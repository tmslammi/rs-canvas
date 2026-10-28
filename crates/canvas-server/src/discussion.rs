//! Discussion notes for one workspace.
//!
//! `DiscussionStore` owns the small, user-inspectable engineering context that
//! belongs beside a drawing: constraints, agreed decisions with their reasons,
//! and open questions. It also owns the path those notes persist to, which is
//! derived from the workspace's document path at construction.
//!
//! Invariants:
//! - Replacing the notes requires the revision the caller read; a newer record
//!   is never overwritten by a stale read.
//! - Persist before publish: a failed write leaves the in-memory record
//!   untouched and reports the error.
//! - `persisted()` is a property of the workspace having a backing file, not
//!   of notes having been written, so the page and agent report the same flag
//!   before and after the first save.

use std::path::{Path, PathBuf};

use rmcp::schemars;
use tokio::sync::Mutex;

use crate::workspace_store::write_atomic;

/// Small, user-inspectable engineering context, separate from document/view state.
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    rmcp::schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Discussion {
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Agreed decisions, including their reasons. Hypotheses belong in questions.
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub questions: Vec<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct DiscussionRecord {
    pub revision: u64,
    pub notes: Discussion,
}

pub struct DiscussionStore {
    record: Mutex<DiscussionRecord>,
    path: Option<PathBuf>,
}

impl DiscussionStore {
    /// Notes live beside the workspace document when there is one; an
    /// in-memory workspace keeps them in memory only.
    pub fn new(doc_path: Option<&Path>) -> Self {
        Self {
            record: Mutex::new(DiscussionRecord::default()),
            path: doc_path.map(|path| path.with_extension("discussion.json")),
        }
    }

    /// Whether these notes have a backing file. This is the same answer as
    /// "the workspace has a document path", exposed here so callers do not
    /// need to know how the note path is derived.
    pub fn persisted(&self) -> bool {
        self.path.is_some()
    }

    /// The current record, cloned out for status/reporting.
    pub async fn record(&self) -> DiscussionRecord {
        self.record.lock().await.clone()
    }

    /// Load persisted notes, if any, keeping the default record on any error.
    pub async fn load(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(json) = tokio::fs::read_to_string(path).await else {
            return;
        };
        if let Ok(notes) = serde_json::from_str(&json) {
            *self.record.lock().await = notes;
        }
    }

    /// Replace context only against the version the caller actually read.
    /// Persist before updating memory so a failed write never reports success.
    pub async fn remember(
        &self,
        notes: Discussion,
        expected: u64,
    ) -> Result<DiscussionRecord, String> {
        if notes
            .constraints
            .iter()
            .chain(&notes.decisions)
            .chain(&notes.questions)
            .count()
            > 100
            || notes
                .constraints
                .iter()
                .chain(&notes.decisions)
                .chain(&notes.questions)
                .any(|s| s.len() > 2000)
        {
            return Err("discussion supports up to 100 entries of at most 2000 bytes each".into());
        }
        let mut current = self.record.lock().await;
        if current.revision != expected {
            return Err(
                "discussion changed; read canvas_status and merge the latest notes before retrying"
                    .into(),
            );
        }
        let updated = DiscussionRecord {
            revision: expected + 1,
            notes,
        };
        if let Some(path) = &self.path {
            let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
            write_atomic(path, json.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
        }
        *current = updated.clone();
        Ok(updated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remember_requires_the_read_revision_and_persists_first() {
        let dir = std::env::temp_dir().join(format!(
            "rs-canvas-discussion-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        let doc_path = dir.join("document.json");
        let store = DiscussionStore::new(Some(&doc_path));
        assert!(store.persisted());

        let notes = Discussion {
            decisions: vec!["keep the page authoritative".into()],
            ..Discussion::default()
        };
        let saved = store.remember(notes.clone(), 0).await.unwrap();
        assert_eq!(saved.revision, 1);
        assert!(doc_path.with_extension("discussion.json").exists());

        assert!(store.remember(notes.clone(), 0).await.is_err());
        assert_eq!(store.record().await.revision, 1);

        let restarted = DiscussionStore::new(Some(&doc_path));
        restarted.load().await;
        assert_eq!(restarted.record().await.notes, notes);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn in_memory_notes_report_no_backing_file() {
        let store = DiscussionStore::new(None);
        assert!(!store.persisted());
        let saved = store
            .remember(
                Discussion {
                    questions: vec!["what changed?".into()],
                    ..Discussion::default()
                },
                0,
            )
            .await
            .unwrap();
        assert_eq!(saved.revision, 1);
    }
}
