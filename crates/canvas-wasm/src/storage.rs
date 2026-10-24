//! Rust-owned persistence: the current document autosaves to IndexedDB.
//!
//! Records written since the binary format store a `Uint8Array` under `bytes`;
//! records from older builds store a JSON string under `json`. Loads read
//! either and the next save replaces the whole record with binary.

use idb::builder::{DatabaseBuilder, ObjectStoreBuilder};
use idb::{Database, KeyPath, Query, TransactionMode};
use js_sys::Uint8Array;
use wasm_bindgen::{JsCast, JsValue};

const DB_NAME: &str = "rs-canvas";
const STORE: &str = "documents";
/// v2: the handmade-dark theme flipped the default ink to light, so old
/// autosaves are ignored rather than loaded invisible.
///
/// Each workspace autosaves under its own key: `current-v2` for the default
/// workspace and `current-v2:workspace:<name>` for a named one.
pub const DEFAULT_KEY: &str = "current-v2";

fn autosave_key(workspace: &str) -> String {
    if workspace.is_empty() || workspace == "default" {
        DEFAULT_KEY.to_string()
    } else {
        format!("{DEFAULT_KEY}:workspace:{workspace}")
    }
}

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
pub async fn save_document(bytes: &[u8], workspace: &str) -> Result<(), String> {
    let db = open().await?;
    let transaction = db
        .transaction(&[STORE], TransactionMode::ReadWrite)
        .map_err(error_string)?;
    let store = transaction.object_store(STORE).map_err(error_string)?;
    let value = js_sys::Object::new();
    let _ = js_sys::Reflect::set(
        &value,
        &"id".into(),
        &JsValue::from_str(&autosave_key(workspace)),
    );
    let _ = js_sys::Reflect::set(&value, &"bytes".into(), &Uint8Array::from(bytes));
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

/// Load the current autosave, if any, as raw document bytes.
///
/// Binary records are returned as stored. A legacy JSON string is returned as
/// its UTF-8 bytes; `serialization::from_bytes` auto-detects it and the next
/// autosave upgrades the record.
pub async fn load_document(workspace: &str) -> Result<Option<Vec<u8>>, String> {
    let db = open().await?;
    let transaction = db
        .transaction(&[STORE], TransactionMode::ReadOnly)
        .map_err(error_string)?;
    let store = transaction.object_store(STORE).map_err(error_string)?;
    let value = store
        .get(Query::Key(JsValue::from_str(&autosave_key(workspace))))
        .map_err(error_string)?
        .await
        .map_err(error_string)?;
    let Some(value) = value else {
        return Ok(None);
    };
    let bytes = js_sys::Reflect::get(&value, &"bytes".into()).map_err(error_string)?;
    if bytes.is_instance_of::<Uint8Array>() {
        return Ok(Some(Uint8Array::from(bytes).to_vec()));
    }
    let json = js_sys::Reflect::get(&value, &"json".into()).map_err(error_string)?;
    Ok(json.as_string().map(String::into_bytes))
}

/// Discover autosaves written before the browser catalogue existed.
pub async fn list_workspaces() -> Result<Vec<String>, String> {
    let db = open().await?;
    let transaction = db
        .transaction(&[STORE], TransactionMode::ReadOnly)
        .map_err(error_string)?;
    let store = transaction.object_store(STORE).map_err(error_string)?;
    let keys = store
        .get_all_keys(None, None)
        .map_err(error_string)?
        .await
        .map_err(error_string)?;
    Ok(keys
        .into_iter()
        .filter_map(|key| key.as_string())
        .filter_map(|key| workspace_from_key(&key))
        .collect())
}

fn workspace_from_key(key: &str) -> Option<String> {
    if key == DEFAULT_KEY {
        Some("default".into())
    } else {
        key.strip_prefix(&format!("{DEFAULT_KEY}:workspace:"))
            .map(str::to_string)
    }
}

pub async fn delete_workspace(workspace: &str) -> Result<(), String> {
    let db = open().await?;
    let transaction = db
        .transaction(&[STORE], TransactionMode::ReadWrite)
        .map_err(error_string)?;
    let store = transaction.object_store(STORE).map_err(error_string)?;
    store
        .delete(Query::Key(JsValue::from_str(&autosave_key(workspace))))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_keys_are_isolated_and_preserve_default_autosaves() {
        assert_eq!(autosave_key(""), DEFAULT_KEY);
        assert_eq!(autosave_key("default"), DEFAULT_KEY);
        assert_ne!(autosave_key("alpha"), autosave_key("beta"));
        assert_ne!(autosave_key("alpha"), autosave_key("default"));
    }
}
