use super::*;
use crate::sync::*;
use axum::{
    http::{header, StatusCode},
    response::IntoResponse,
};
use dashmap::mapref::entry::Entry;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::Mutex;
use yrs::{Doc, Options, StateVector};

const MAX_TRIM_UPDATE_LIMIT: i64 = 50;

async fn load_doc(db: &SQLite) -> Result<Doc, sqlx::Error> {
    let doc = Doc::with_options(Options {
        skip_gc: true,
        ..Default::default()
    });

    if db.count().await? == 0 {
        let update = doc.encode_state_as_update_v1(&StateVector::default());
        println!("count1: {}", update.len());
        db.insert(&update).await?;
    } else {
        let updates = db.all(0).await?;
        println!("count2: {}", updates.len());

        let mut trx = doc.transact();
        for update in updates {
            match Update::decode_v1(&update.blob) {
                Ok(update) => trx.apply_update(update),
                Err(err) => info!("failed to decode update: {:?}", err),
            }
        }
        trx.commit();
    }

    Ok(doc)
}

async fn flush_document(db: &SQLite) -> Result<Doc, sqlx::Error> {
    let doc = load_doc(&db).await?;

    // println!(
    //     "{}",
    //     serde_json::to_string(&doc.transact().get_map("blocks").to_json()).unwrap()
    // );

    let update = doc.encode_state_as_update_v1(&StateVector::default());

    db.insert(&update).await?;

    let clock = db.max_id().await?;
    println!("clock: {}, {}", clock, update.len());
    db.delete_before(clock).await?;

    Ok(doc)
}

async fn create_doc(context: Arc<Context>, workspace: String) -> (Doc, SQLite) {
    let db = init(context.db_conn.clone(), &workspace).await.unwrap();
    let doc = flush_document(&db).await.unwrap();

    (doc, db)
}

pub async fn init_doc(context: Arc<Context>, workspace: String) {
    if let Entry::Vacant(entry) = context.doc.entry(workspace.clone()) {
        let (mut doc, db) = create_doc(context.clone(), workspace.clone()).await;

        if let Entry::Vacant(entry) = context.subscribes.entry(workspace.clone()) {
            let sub = doc.observe_update_v1(move |mut trx, e| {
                let db = db.clone();
                let update = trx.encode_update_v1();

                assert_eq!(update, e.update);

                tokio::spawn(async move {
                    db.insert(&update).await.unwrap();
                    if db.count().await.unwrap() > MAX_TRIM_UPDATE_LIMIT {
                        flush_document(&db).await.unwrap();
                    }
                });
            });
            entry.insert(sub.into());
        }

        entry.insert(Mutex::new(doc));
    };
}

pub fn parse_doc<T>(any: T) -> impl IntoResponse
where
    T: Serialize,
{
    use serde_json::to_string;
    if let Ok(data) = to_string(&any) {
        ([(header::CONTENT_TYPE, "application/json")], data).into_response()
    } else {
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }
}

mod tests {
    use super::*;
    #[tokio::test]
    async fn doc_load_test() -> anyhow::Result<()> {
        let doc = Doc::default();

        {
            let mut trx = doc.transact();
            let mut block = jwst::Block::new(&mut trx, "test", "text");
            block.content().insert(&mut trx, "test", "test");
            trx.commit();
        }

        let new_doc = {
            let update = doc.encode_state_as_update_v1(&StateVector::default());
            let doc = Doc::default();
            let mut trx = doc.transact();
            match Update::decode_v1(&update) {
                Ok(update) => trx.apply_update(update),
                Err(err) => info!("failed to decode update: {:?}", err),
            }
            trx.commit();
            doc
        };

        assert_json_diff::assert_json_eq!(
            doc.transact().get_map("blocks").to_json(),
            new_doc.transact().get_map("blocks").to_json()
        );

        Ok(())
    }
}