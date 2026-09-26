//! Rust-owned persistence: the current document autosaves to IndexedDB.

use idb::builder::{DatabaseBuilder, ObjectStoreBuilder};
use idb::{Database, KeyPath, Query, TransactionMode};
use wasm_bindgen::JsValue;

const DB_NAME: &str = "rs-canvas";
const STORE: &str = "documents";
const KEY: &str = "current";

fn error_string<E: std::fmt::Debug>(error: E) -> String {
    format!("{error:?}")
}

async fn open() -> Result<Database, String> {
    DatabaseBuilder::new(DB_NAME)
        .version(1)
        .add_object_store(ObjectStoreBuilder::new(STORE).key_path(Some(KeyPath::new_single("id"))))
        .build()
        .await
        .map_err(error_string)
}

/// Persist a serialized document as the current autosave.
pub async fn save_document(json: &str) -> Result<(), String> {
    let db = open().await?;
    let transaction = db
        .transaction(&[STORE], TransactionMode::ReadWrite)
        .map_err(error_string)?;
    let store = transaction.object_store(STORE).map_err(error_string)?;
    let value = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&value, &"id".into(), &JsValue::from_str(KEY));
    let _ = js_sys::Reflect::set(&value, &"json".into(), &JsValue::from_str(json));
    let _ = js_sys::Reflect::set(
        &value,
        &"updated".into(),
        &JsValue::from_f64(js_sys::Date::now()),
    );
    store
        .put(&value.into(), None)
        .map_err(error_string)?
        .await
        .map_err(error_string)?;
    transaction
        .commit()
        .map_err(error_string)?
        .await
        .map_err(error_string)?;
    Ok(())
}

/// Load the current autosave, if any.
pub async fn load_document() -> Result<Option<String>, String> {
    let db = open().await?;
    let transaction = db
        .transaction(&[STORE], TransactionMode::ReadOnly)
        .map_err(error_string)?;
    let store = transaction.object_store(STORE).map_err(error_string)?;
    let value = store
        .get(Query::Key(JsValue::from_str(KEY)))
        .map_err(error_string)?
        .await
        .map_err(error_string)?;
    let Some(value) = value else {
        return Ok(None);
    };
    let json = js_sys::Reflect::get(&value, &"json".into()).map_err(error_string)?;
    Ok(json.as_string())
}
