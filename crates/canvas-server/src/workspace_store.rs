//! Identity, persistence and lifecycle for one workspace.
//!
//! `WorkspaceStore` owns the workspace name and document-lifetime epoch, the
//! path the mirror persists to, the explicit-creation flag, the retired flag,
//! and the debounced persist queue/loop. It does not read or merge the
//! document; the persist loop asks `DocumentMirror` to serialize under its own
//! lock, preserving the old lock order.
//!
//! Invariants:
//! - `epoch:generation` identifies one in-memory document lifetime. Handles
//!   never cross it; `release_when_idle` bumps the generation when a mirror
//!   with no backing file is dropped.
//! - `retire()` is terminal: it rejects new page admission and wakes the
//!   persist loop so it can exit without recreating a removed file.
//! - Persist requests coalesce. The loop waits out `PERSIST_DEBOUNCE`, takes
//!   the pending flag once, and writes at most once per burst.
//! - The persist write lock is held across serialization and the atomic file
//!   replace, so rename/delete can order a document swap against a late writer.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, MutexGuard, Notify};

use crate::document::DocumentMirror;

/// How long a burst of edits coalesces before the mirror is written to disk.
const PERSIST_DEBOUNCE: Duration = Duration::from_millis(500);

pub struct WorkspaceStore {
    name: String,
    /// Identity of this in-memory document lifetime; handles never cross it.
    epoch: String,
    generation: AtomicU64,
    /// Where the mirror is persisted; `None` keeps it in memory only.
    doc_path: Option<PathBuf>,
    /// Created explicitly by an agent (or any future create RPC) rather than
    /// lazily by a page naming the workspace. Keeps an empty workspace in the
    /// listing even before its first page or element exists.
    created: AtomicBool,
    retired: AtomicBool,
    /// A persist is queued; bursts coalesce into one write.
    persist_pending: AtomicBool,
    persist_write: Mutex<()>,
    /// Wakes the persist loop.
    persist_notify: Notify,
    /// Test-only count of completed writes, for coalescing assertions.
    #[cfg(test)]
    persist_writes: AtomicU64,
}

impl WorkspaceStore {
    pub fn new(name: &str, doc_path: Option<PathBuf>) -> Self {
        Self {
            name: name.to_string(),
            epoch: canvas_core::id::new_element_id(),
            generation: AtomicU64::new(0),
            doc_path,
            created: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            persist_pending: AtomicBool::new(false),
            persist_write: Mutex::new(()),
            persist_notify: Notify::new(),
            #[cfg(test)]
            persist_writes: AtomicU64::new(0),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where the mirror is persisted; `None` keeps it in memory only.
    pub fn doc_path(&self) -> Option<&Path> {
        self.doc_path.as_deref()
    }

    /// Whether the workspace has a backing file.
    pub fn persisted(&self) -> bool {
        self.doc_path.is_some()
    }

    /// `epoch:generation`, the token agent calls use to detect a workspace
    /// switch or a recycled in-memory document.
    pub fn context(&self) -> String {
        format!("{}:{}", self.epoch, self.generation())
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Start a new in-memory document lifetime. Called when a mirror with no
    /// backing file is released; handles from before must stop resolving.
    pub fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn created(&self) -> bool {
        self.created.load(Ordering::SeqCst)
    }

    pub fn mark_created(&self) {
        self.created.store(true, Ordering::SeqCst);
    }

    /// Carry the explicit-creation flag across a rename.
    pub fn inherit_created(&self, created: bool) {
        self.created.store(created, Ordering::SeqCst);
    }

    pub fn is_retired(&self) -> bool {
        self.retired.load(Ordering::SeqCst)
    }

    /// Retire the workspace and wake the persist loop so it exits rather than
    /// recreating a deleted file. Irreversible.
    pub fn retire(&self) {
        self.retired.store(true, Ordering::SeqCst);
        self.persist_notify.notify_one();
    }

    /// Queue a persist; bursts within `PERSIST_DEBOUNCE` coalesce into one
    /// write. The loop is spawned when the workspace is created.
    pub async fn schedule_persist(&self) {
        self.persist_pending.store(true, Ordering::SeqCst);
        self.persist_notify.notify_one();
    }

    /// Hold the write lock across a management operation (rename/delete) that
    /// must not race a queued persist.
    pub async fn lock_writes(&self) -> MutexGuard<'_, ()> {
        self.persist_write.lock().await
    }

    /// The debounced persist loop, spawned per workspace at creation.
    pub async fn persist_loop(self: Arc<Self>, mirror: Arc<DocumentMirror>) {
        loop {
            self.persist_notify.notified().await;
            if self.is_retired() {
                break;
            }
            tokio::time::sleep(PERSIST_DEBOUNCE).await;
            if self.persist_pending.swap(false, Ordering::SeqCst) {
                self.persist_document(&mirror).await;
            }
        }
    }

    /// Write the mirror to disk atomically; a no-op without a path.
    pub async fn persist_document(&self, mirror: &DocumentMirror) {
        let _write = self.persist_write.lock().await;
        if self.is_retired() {
            return;
        }
        let Some(path) = &self.doc_path else {
            return;
        };
        let bytes = match mirror.to_bytes().await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "could not serialize document");
                return;
            }
        };
        if let Err(error) = write_atomic(path, &bytes).await {
            tracing::warn!(%error, path = %path.display(), "could not persist document");
        } else {
            #[cfg(test)]
            self.persist_writes.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The number of writes completed by this store (test-only).
    #[cfg(test)]
    pub fn persist_writes(&self) -> u64 {
        self.persist_writes.load(Ordering::SeqCst)
    }
}

pub(crate) async fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temp = path.with_extension("tmp");
    tokio::fs::write(&temp, contents).await?;
    tokio::fs::rename(&temp, path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rs-canvas-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ))
    }

    #[tokio::test]
    async fn persist_requests_coalesce_into_one_write_per_burst() {
        let dir = temp_dir("persist-coalesce");
        let path = dir.join("document.json");
        let store = Arc::new(WorkspaceStore::new("default", Some(path.clone())));
        let mirror = Arc::new(DocumentMirror::new());
        let loop_store = store.clone();
        let loop_mirror = mirror.clone();
        let persist = tokio::spawn(async move { loop_store.persist_loop(loop_mirror).await });

        for _ in 0..5 {
            store.schedule_persist().await;
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while store.persist_writes() == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the queued burst should write");
        assert_eq!(store.persist_writes(), 1, "the burst coalesced");
        assert!(path.exists());

        store.retire();
        persist.await.unwrap();
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn a_retired_store_cannot_recreate_its_file() {
        let dir = temp_dir("persist-retired");
        let path = dir.join("document.json");
        let store = WorkspaceStore::new("alpha", Some(path.clone()));
        let mirror = DocumentMirror::new();
        store.mark_created();
        assert!(store.created());
        store.retire();
        store.persist_document(&mirror).await;
        assert!(!path.exists());
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
