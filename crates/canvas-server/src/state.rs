//! Shared server state: workspaces, their document mirrors and on-disk
//! homes, and the connected pages.
//!
//! A workspace is a named, isolated drawing context (ADR-0007): one document
//! mirror, the pages viewing it and everything transient about them. The
//! page owns the interactive document; the mirror is what MCP tool calls
//! read and write. Both sides meet through the existing `canvas-core` merge
//! rules, never through a bespoke protocol. Agent calls always act on the
//! current workspace — the one the user's most recently active page is on.
//! Creating a workspace is the one routing action: it navigates the user's
//! page to the new workspace (ADR-0008).
//!
//! `Workspace` is the composition root for four collaborators:
//! `DocumentMirror` (document, revision, change log, aliases, transactions),
//! `PageHub` (presence, broadcast, view hints, presentation/screenshot state),
//! `WorkspaceStore` (name, epoch, persistence, lifecycle) and
//! `DiscussionStore` (discussion notes). Cross-collaborator workflows —
//! absorbing a page frame, publishing an edit, loading and persisting a
//! workspace — stay here so the ordering of mirror update, broadcast and
//! persist remains explicit.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use canvas_core::model::{Document, Element};
use canvas_core::sync::PageFrame;
use canvas_core::Delta;
use tokio::sync::{broadcast, Mutex};

use crate::aliases::Aliases;
use crate::discussion::DiscussionStore;
pub use crate::discussion::{Discussion, DiscussionRecord};
use crate::document::DocumentMirror;
pub use crate::document::{AgentTransaction, ChangeKind, ChangeRecord, ChangedElement};
use crate::page_hub::PageHub;
pub use crate::page_hub::{PageFrameOutcome, PageMessage, ViewHint};
use crate::workspace_store::{write_atomic, WorkspaceStore};
/// The unnamed workspace: where `--doc` points and where agent calls land
/// when no page is connected.
pub const DEFAULT_WORKSPACE: &str = "default";

/// Workspace names become file names, so keep them boring: lowercase ASCII
/// letters, digits and dashes, starting with a letter or digit.
pub fn valid_workspace_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// What the user allows the agent to do with their camera.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraMode {
    /// No camera moves from the agent at all.
    Off,
    /// Only when the target is not already visible, with a capped zoom.
    Gentle,
    /// Free to frame, still within the camera's own limits.
    Full,
}

/// How quickly presentations play.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pacing {
    Fast,
    Normal,
    Slow,
}

/// The user's live agent preferences, edited on the canvas.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentSettings {
    pub camera: CameraMode,
    pub pacing: Pacing,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            camera: CameraMode::Gentle,
            pacing: Pacing::Normal,
        }
    }
}

impl AgentSettings {
    pub fn hold_scale(&self) -> f64 {
        match self.pacing {
            Pacing::Fast => 0.6,
            Pacing::Normal => 1.0,
            Pacing::Slow => 1.5,
        }
    }

    pub fn zoom_cap(&self) -> f64 {
        match self.camera {
            CameraMode::Off => 1.0,
            CameraMode::Gentle => 2.0,
            CameraMode::Full => 8.0,
        }
    }

    pub fn camera_moves(&self) -> bool {
        !matches!(self.camera, CameraMode::Off)
    }

    pub fn apply_patch(&mut self, patch: &serde_json::Value) -> Result<(), String> {
        let mut updated = self.clone();
        if let Some(camera) = patch.get("camera") {
            updated.camera = serde_json::from_value(camera.clone())
                .map_err(|_| "camera must be off, gentle or full".to_string())?;
        }
        if let Some(pacing) = patch.get("pacing") {
            updated.pacing = serde_json::from_value(pacing.clone())
                .map_err(|_| "pacing must be fast, normal or slow".to_string())?;
        }
        *self = updated;
        Ok(())
    }
}

/// A named, isolated drawing context: one document mirror and the pages
/// viewing it. This is the composition root: every public operation delegates
/// to one collaborator or sequences several of them in the order the bridge
/// has always used.
pub struct Workspace {
    mirror: Arc<DocumentMirror>,
    hub: PageHub,
    store: Arc<WorkspaceStore>,
    discussion: DiscussionStore,
}

impl Workspace {
    fn new(name: &str, doc_path: Option<PathBuf>) -> Self {
        let discussion = DiscussionStore::new(doc_path.as_deref());
        Self {
            mirror: Arc::new(DocumentMirror::new()),
            hub: PageHub::new(),
            store: Arc::new(WorkspaceStore::new(name, doc_path)),
            discussion,
        }
    }

    // Identity and lifecycle ------------------------------------------------

    pub fn name(&self) -> &str {
        self.store.name()
    }

    pub fn context(&self) -> String {
        self.store.context()
    }

    pub fn doc_path(&self) -> Option<&std::path::Path> {
        self.store.doc_path()
    }

    pub fn persisted(&self) -> bool {
        self.store.persisted()
    }

    pub fn created(&self) -> bool {
        self.store.created()
    }

    pub fn mark_created(&self) {
        self.store.mark_created();
    }

    pub fn inherit_created(&self, created: bool) {
        self.store.inherit_created(created);
    }

    pub fn is_retired(&self) -> bool {
        self.store.is_retired()
    }

    /// Retire this workspace. New page admission is refused and the persist
    /// loop wakes to exit.
    pub fn retire(&self) {
        self.store.retire();
    }

    /// Hold the persist write lock across a management operation.
    pub async fn persist_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.store.lock_writes().await
    }

    /// Load the persisted mirror and discussion notes, if any. Called once
    /// when the workspace is created.
    pub async fn load_document(&self) {
        let Some(path) = self.store.doc_path() else {
            return;
        };
        self.discussion.load().await;
        let Ok(bytes) = tokio::fs::read(path).await else {
            return;
        };
        match canvas_core::serialization::from_bytes(&bytes) {
            Ok(doc) => {
                let elements = doc.live_count();
                self.mirror.replace_document(doc).await;
                tracing::info!(elements, path = %path.display(), workspace = %self.name(), "restored document");
            }
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "ignoring unreadable document");
            }
        }
    }

    /// Write the mirror to disk atomically; a no-op without a path.
    pub async fn persist_document(&self) {
        self.store.persist_document(&self.mirror).await;
    }

    /// Queue a persist; bursts within the debounce window coalesce.
    pub async fn schedule_persist(&self) {
        self.store.schedule_persist().await;
    }

    /// The debounced persist loop, spawned per workspace at creation.
    pub async fn persist_loop(self: Arc<Self>) {
        self.store.clone().persist_loop(self.mirror.clone()).await;
    }

    /// In-memory mirrors live only while a page is watching: without a
    /// configured document file there is no home for the drawing between
    /// page sessions — the page's own autosave is the source of truth.
    pub async fn release_when_idle(&self, remaining_pages: usize) {
        if remaining_pages == 0 && !self.store.persisted() {
            let mut doc = self.mirror.doc().lock().await;
            *doc = Document::new();
            self.store.bump_generation();
            self.mirror.clear_transactions().await;
        }
    }

    // Document mirror -------------------------------------------------------

    /// The mirror document. Callers hold this guard across a read/write that
    /// must be atomic with the surrounding merge or transaction.
    pub fn doc(&self) -> &Mutex<Document> {
        self.mirror.doc()
    }

    pub fn revision(&self) -> u64 {
        self.mirror.revision()
    }

    #[cfg(test)]
    pub fn set_revision(&self, revision: u64) {
        self.mirror.set_revision(revision);
    }

    /// The element handles the agent sees.
    pub fn aliases(&self) -> &Mutex<Aliases> {
        self.mirror.aliases()
    }

    /// The mirror as compact JSON, for tests that compare frames.
    #[cfg(test)]
    pub async fn document_json(&self) -> String {
        self.mirror.document_json().await
    }

    /// The element id behind a handle; unknown names pass through.
    pub async fn resolve_id(&self, name: &str) -> String {
        self.mirror.resolve_id(name).await
    }

    /// Resolve a batch of handles.
    pub async fn resolve_ids(&self, names: &[String]) -> Vec<String> {
        self.mirror.resolve_ids(names).await
    }

    /// The mirror as a binary snapshot wire frame. The document payload is the
    /// same binary envelope persistence uses.
    pub async fn snapshot_frame_bytes(&self) -> Result<Vec<u8>, canvas_core::wire::WireError> {
        self.mirror.snapshot_frame_bytes().await
    }

    /// Record a document change: diff old vs new, bump the revision and
    /// append to the bounded change log.
    pub async fn record_change(
        &self,
        before: &Document,
        after: &Document,
        source: &str,
    ) -> (u64, Vec<Element>) {
        self.mirror.record_change(before, after, source).await
    }

    /// Record a committed delta journal.
    pub async fn record_deltas(&self, deltas: &[Delta], source: &str) -> u64 {
        self.mirror.record_deltas(deltas, source).await
    }

    /// Record an additions-only bulk commit.
    pub async fn record_bulk(&self, changed: Vec<String>) -> u64 {
        self.mirror.record_bulk(changed).await
    }

    /// Change records newer than `since`.
    pub async fn changes_since(&self, since: u64) -> Vec<ChangeRecord> {
        self.mirror.changes_since(since).await
    }

    /// Cursor coverage, checked while the caller holds the document lock.
    pub async fn cursor_valid(&self, since: u64) -> bool {
        self.mirror.cursor_valid(since).await
    }

    /// Apply one decoded binary page frame. This is the WebSocket-free seam:
    /// connection code only decodes transport bytes, and tests can drive the
    /// same sequence directly.
    pub async fn apply_page_frame(&self, origin: u64, frame: PageFrame) -> PageFrameOutcome {
        let outcome = match frame {
            PageFrame::Snapshot { base_revision, doc } => {
                self.absorb_document(&doc, base_revision, Some(origin))
                    .await
            }
            PageFrame::Patch { put } => self.absorb_patch_outcome(put, origin).await,
        };
        tracing::debug!(
            changed = outcome.changed,
            revision = outcome.revision,
            base_revision = outcome.base_revision,
            corrections = outcome.corrections.len(),
            accepted = outcome.accepted.len(),
            "applied page frame"
        );
        outcome
    }

    /// Merge an already-parsed page snapshot into the mirror. The page's
    /// `base_revision` is recorded and returned; element-level LWW still
    /// decides the merge. Absence never deletes: only explicit tombstones in
    /// the snapshot do. The outcome names every winner the upload lost and
    /// every live server element it omitted, so the caller can correct the
    /// page instead of guessing from a previous snapshot.
    pub async fn absorb_document(
        &self,
        remote: &Document,
        base_revision: u64,
        origin: Option<u64>,
    ) -> PageFrameOutcome {
        let mut doc = self.mirror.doc().lock().await;
        let result = canvas_core::merge::reconcile_document(&doc, remote);
        let changed = !result.changed.is_empty();
        let (revision, winners) = if changed {
            let (revision, winners) = self.record_change(&doc, &result.document, "page").await;
            *doc = result.document;
            (revision, winners)
        } else {
            (self.mirror.revision(), Vec::new())
        };
        let corrections = result.corrections;
        let accepted = result.accepted;
        drop(doc);
        self.hub
            .broadcast_merge(revision, &winners, &corrections, origin)
            .await;
        if changed {
            self.schedule_persist().await;
        }
        PageFrameOutcome {
            changed,
            revision,
            base_revision: Some(base_revision),
            corrections,
            accepted,
        }
    }

    /// Merge a page patch into the mirror. Changed elements broadcast to the
    /// other pages; elements whose copy lost to a newer mirror version come
    /// back to the origin as corrections, so a concurrent agent edit cannot
    /// leave the page stale. Returns the same outcome shape as a snapshot.
    async fn absorb_patch_outcome(&self, put: Vec<Element>, origin: u64) -> PageFrameOutcome {
        let canvas_core::merge::MergeOutcome {
            changed,
            corrections,
            accepted,
        } = {
            let mut doc = self.mirror.doc().lock().await;
            canvas_core::merge::merge_elements(&mut doc, &put)
        };
        let changes: Vec<ChangedElement> = changed
            .iter()
            .map(|change| ChangedElement {
                id: change.after.id.clone(),
                kinds: crate::document::classify_change(
                    change.before.as_ref(),
                    Some(&change.after),
                ),
            })
            .collect();
        let winners: Vec<Element> = changed.into_iter().map(|change| change.after).collect();
        let changed = !changes.is_empty();
        let revision = if changed {
            self.mirror.record_ids(changes, "page").await
        } else {
            self.mirror.revision()
        };
        self.hub
            .broadcast_merge(revision, &winners, &corrections, Some(origin))
            .await;
        if changed {
            self.schedule_persist().await;
        }
        PageFrameOutcome {
            changed,
            revision,
            base_revision: None,
            corrections,
            accepted,
        }
    }

    /// Broadcast an agent-side edit as a patch and queue the persist.
    pub async fn publish_patch(&self, revision: u64, put: Vec<Element>) {
        self.hub.broadcast_patch(revision, &put, None).await;
        self.schedule_persist().await;
    }

    /// Append a committed agent transaction to the revert history.
    pub async fn push_transaction(&self, transaction: AgentTransaction) {
        self.mirror.push_transaction(transaction).await;
    }

    /// The most recent committed agent transactions, newest first.
    pub async fn recent_transactions(&self, limit: usize) -> Vec<AgentTransaction> {
        self.mirror.recent_transactions(limit).await
    }

    /// A committed agent transaction by receipt id.
    pub async fn transaction(&self, id: &str) -> Option<AgentTransaction> {
        self.mirror.transaction(id).await
    }

    // Pages and page-directed state ----------------------------------------

    /// A page on this workspace did something; it may now be the current one.
    pub fn note_page_activity(&self) {
        self.hub.note_activity();
    }

    pub fn page_count(&self) -> usize {
        self.hub.count()
    }

    /// Monotonic activity stamp used by current-workspace resolution.
    pub fn activity(&self) -> u64 {
        self.hub.activity()
    }

    #[cfg(test)]
    pub fn set_page_count(&self, pages: usize) {
        self.hub.set_count(pages);
    }

    #[cfg(test)]
    pub fn set_activity(&self, activity: u64) {
        self.hub.set_activity(activity);
    }

    /// Admit one page unless the workspace has been retired. Counts and
    /// activity are recorded under the same rules as before.
    pub async fn register_page(&self) -> bool {
        if self.store.is_retired() {
            return false;
        }
        self.hub.connect();
        self.hub.note_activity();
        true
    }

    pub fn release_page(&self) {
        self.hub.disconnect();
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PageMessage> {
        self.hub.subscribe()
    }

    pub fn broadcast(&self, message: PageMessage) {
        self.hub.broadcast(message);
    }

    pub async fn view_hint(&self) -> Option<ViewHint> {
        self.hub.view_hint().await
    }

    pub async fn set_view_hint(&self, hint: ViewHint) {
        self.hub.set_view_hint(hint).await;
    }

    /// Wait out an in-flight presentation so captures show the settled view.
    pub async fn wait_for_presentation(&self, max: Duration) {
        self.hub.wait_for_presentation(max).await;
    }

    pub fn presentation_started(&self) {
        self.hub.presentation_started();
    }

    pub fn presentation_finished(&self) {
        self.hub.presentation_finished();
    }

    pub fn next_presentation_id(&self) -> u64 {
        self.hub.next_presentation_id()
    }

    pub fn subscribe_present_acks(&self) -> broadcast::Receiver<(u64, bool)> {
        self.hub.subscribe_present_acks()
    }

    pub fn acknowledge_presentation(&self, id: u64, cancelled: bool) {
        self.hub.acknowledge_presentation(id, cancelled);
    }

    pub fn next_screenshot_id(&self) -> u64 {
        self.hub.next_screenshot_id()
    }

    pub fn subscribe_screenshot_replies(&self) -> broadcast::Receiver<(u64, Option<String>)> {
        self.hub.subscribe_screenshot_replies()
    }

    pub fn reply_screenshot(&self, id: u64, data_url: Option<String>) {
        self.hub.reply_screenshot(id, data_url);
    }

    // Discussion ------------------------------------------------------------

    pub async fn discussion(&self) -> DiscussionRecord {
        self.discussion.record().await
    }

    pub fn discussion_persisted(&self) -> bool {
        self.discussion.persisted()
    }

    /// Replace context only against the version the caller actually read.
    pub async fn remember(
        &self,
        notes: Discussion,
        expected: u64,
    ) -> Result<DiscussionRecord, String> {
        self.discussion.remember(notes, expected).await
    }
}

/// One entry of the workspace listing that `canvas_status` and
/// `/api/workspaces` report.
#[derive(Clone, Debug, serde::Serialize)]
pub struct WorkspaceInfo {
    pub name: String,
    pub pages: usize,
    /// Live element count; absent for workspaces that exist only as a file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elements: Option<usize>,
    /// Whether the workspace has a backing file.
    pub persisted: bool,
}

#[derive(Debug)]
pub enum WorkspaceManagementError {
    Invalid(String),
    Conflict(String),
    NotFound,
    Io(std::io::Error),
}

pub struct AppState {
    /// When the server started; reported by `canvas_status`.
    pub started: Instant,
    /// The served app directory (fonts for headless rendering live here).
    pub web_dir: PathBuf,
    /// Whether pages exchange documents with the mirror. Tests that only
    /// serve the app turn this off; the agent always runs with it on.
    pub sync: bool,
    /// Live workspaces by name. Idle in-memory entries retain their names;
    /// rename and delete remove retired entries after notifying their pages.
    workspaces: std::sync::Mutex<HashMap<String, Arc<Workspace>>>,
    /// Where the default workspace's mirror persists (the `--doc` argument).
    default_doc_path: Option<PathBuf>,
    /// Where other workspaces' mirrors persist, beside the default document.
    workspaces_dir: Option<PathBuf>,
    /// Connection ids, so a patch broadcast can skip its origin page.
    next_page: AtomicU64,
    workspace_management: Mutex<()>,
    /// The user's live agent preferences (global: they govern the agent,
    /// not any one drawing).
    pub settings: Mutex<AgentSettings>,
    /// Where settings persist; follows the document's directory.
    settings_path: Option<PathBuf>,
}

impl AppState {
    pub fn new(web_dir: PathBuf, doc_path: Option<PathBuf>, sync: bool) -> Self {
        let settings_path = doc_path
            .as_ref()
            .map(|path| path.with_file_name("settings.json"));
        let workspaces_dir = doc_path
            .as_ref()
            .and_then(|path| path.parent().map(|dir| dir.join("workspaces")));
        Self {
            started: Instant::now(),
            web_dir,
            sync,
            workspaces: std::sync::Mutex::new(HashMap::new()),
            default_doc_path: doc_path,
            workspaces_dir,
            next_page: AtomicU64::new(0),
            workspace_management: Mutex::new(()),
            settings: Mutex::new(AgentSettings::default()),
            settings_path,
        }
    }

    fn build_workspace(&self, name: &str) -> Workspace {
        let doc_path = if name == DEFAULT_WORKSPACE {
            self.default_doc_path.clone()
        } else {
            self.workspaces_dir
                .as_ref()
                .map(|dir| dir.join(format!("{name}.rsc")))
        };
        Workspace::new(name, doc_path)
    }

    /// Get or create a workspace, loading its document on first creation.
    pub async fn workspace(&self, name: &str) -> Arc<Workspace> {
        if let Some(ws) = self.workspaces.lock().unwrap().get(name) {
            return ws.clone();
        }
        let _management = self.workspace_management.lock().await;
        self.workspace_unlocked(name).await
    }

    async fn workspace_unlocked(&self, name: &str) -> Arc<Workspace> {
        if let Some(ws) = self.workspaces.lock().unwrap().get(name) {
            return ws.clone();
        }
        let created = Arc::new(self.build_workspace(name));
        created.load_document().await;
        let mut map = self.workspaces.lock().unwrap();
        // A concurrent creator may have won; either way the map holds one
        // instance and only the winner spawns a persist loop.
        map.entry(name.to_string())
            .or_insert_with(|| {
                let ws = created.clone();
                tokio::spawn(ws.clone().persist_loop());
                ws
            })
            .clone()
    }

    /// Create a new, empty workspace for the user to open. Refuses names that
    /// are invalid, reserved, already live or already on disk. An empty
    /// document is written immediately when the workspace has a backing file,
    /// so creation survives a restart before its first edit.
    pub async fn create_workspace(
        &self,
        name: &str,
    ) -> Result<Arc<Workspace>, WorkspaceManagementError> {
        use WorkspaceManagementError::*;
        if !valid_workspace_name(name) {
            return Err(Invalid(
                "Use 1–40 lowercase letters, digits or dashes, starting with a letter or digit."
                    .into(),
            ));
        }
        if name == DEFAULT_WORKSPACE {
            return Err(Conflict("The default workspace already exists.".into()));
        }
        let _management = self.workspace_management.lock().await;
        if self.workspaces.lock().unwrap().contains_key(name) {
            return Err(Conflict(
                "That workspace already exists. Choose another name.".into(),
            ));
        }
        let candidate = self.build_workspace(name);
        if let Some(path) = candidate.doc_path() {
            if tokio::fs::try_exists(path).await.map_err(Io)? {
                return Err(Conflict(
                    "That workspace already exists. Choose another name.".into(),
                ));
            }
        }
        let workspace = self.workspace_unlocked(name).await;
        workspace.mark_created();
        workspace.persist_document().await;
        Ok(workspace)
    }

    /// Serialize connection admission with workspace management so a tab
    /// cannot join a document while it is being renamed or deleted.
    pub async fn register_page(&self, workspace: &Workspace) -> bool {
        let _management = self.workspace_management.lock().await;
        workspace.register_page().await
    }

    pub async fn manage_workspace(
        &self,
        name: &str,
        renamed: Option<&str>,
    ) -> Result<(), WorkspaceManagementError> {
        use WorkspaceManagementError::*;
        if !valid_workspace_name(name) || renamed.is_some_and(|name| !valid_workspace_name(name)) {
            return Err(Invalid(
                "Use 1–40 lowercase letters, digits or dashes, starting with a letter or digit."
                    .into(),
            ));
        }
        if name == DEFAULT_WORKSPACE {
            return Err(Invalid(
                "The default workspace cannot be renamed or deleted.".into(),
            ));
        }
        if renamed == Some(name) {
            return Ok(());
        }
        if renamed == Some(DEFAULT_WORKSPACE) {
            return Err(Conflict("The default workspace already exists.".into()));
        }
        let _management = self.workspace_management.lock().await;
        if let Some(new_name) = renamed {
            let destination = self.build_workspace(new_name);
            if self.workspaces.lock().unwrap().contains_key(new_name)
                || match destination.doc_path() {
                    Some(path) => tokio::fs::try_exists(path).await.map_err(Io)?,
                    None => false,
                }
            {
                return Err(Conflict(
                    "That workspace already exists. Choose another name.".into(),
                ));
            }
        }
        let stored = self.workspaces.lock().unwrap().contains_key(name);
        let path = self
            .build_workspace(name)
            .doc_path()
            .map(std::path::Path::to_path_buf);
        if !stored
            && !match &path {
                Some(path) => tokio::fs::try_exists(path).await.map_err(Io)?,
                None => false,
            }
        {
            return Err(NotFound);
        }
        let source = self.workspace_unlocked(name).await;
        if source.page_count() > 1 {
            return Err(Conflict(
                "This workspace is open in another tab. Close or switch those tabs first.".into(),
            ));
        }
        let _write = source.persist_guard().await;
        let mut doc = source.doc().lock().await;
        // The page uploads its document over the binary wire after navigating.
        // The management route moves the mirror as it stands.
        let merged = doc.clone();
        if let Some(new_name) = renamed {
            let destination = Arc::new(self.build_workspace(new_name));
            destination.inherit_created(source.created());
            if let Some(path) = destination.doc_path() {
                let bytes = canvas_core::serialization::to_bytes(&merged)
                    .map_err(|error| Invalid(error.to_string()))?;
                write_atomic(path, &bytes).await.map_err(Io)?;
            }
            // Remove the old file only once the new document is durable.
            if let Some(path) = source.doc_path() {
                match tokio::fs::remove_file(path).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        if let Some(path) = destination.doc_path() {
                            let _ = tokio::fs::remove_file(path).await;
                        }
                        return Err(Io(error));
                    }
                }
            }
            *destination.doc().lock().await = merged;
            self.workspaces
                .lock()
                .unwrap()
                .insert(new_name.into(), destination.clone());
            tokio::spawn(destination.persist_loop());
        } else if let Some(path) = source.doc_path() {
            match tokio::fs::remove_file(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(Io(error)),
            }
        }
        source.retire();
        *doc = Document::new();
        source.broadcast(PageMessage::control(
            serde_json::json!({
                "type": "workspace_changed", "action": if renamed.is_some() { "rename" } else { "delete" },
                "workspace": renamed.unwrap_or(DEFAULT_WORKSPACE),
            })
            .to_string(),
        ));
        self.workspaces.lock().unwrap().remove(name);
        Ok(())
    }

    /// The workspace agent calls act on: the one the user's most recently
    /// active page is viewing, or `default` when no page is connected.
    pub async fn current_workspace(&self) -> Arc<Workspace> {
        let current = {
            let map = self.workspaces.lock().unwrap();
            map.values()
                .filter(|ws| ws.page_count() > 0)
                .max_by_key(|ws| ws.activity())
                .cloned()
        };
        match current {
            Some(ws) => ws,
            None => self.workspace(DEFAULT_WORKSPACE).await,
        }
    }

    /// Live workspaces plus persisted ones never opened this run. In-memory
    /// husks (no file, no pages, no content) are indistinguishable from
    /// never-created and left out.
    pub async fn list_workspaces(&self) -> Vec<WorkspaceInfo> {
        let workspaces: Vec<Arc<Workspace>> =
            self.workspaces.lock().unwrap().values().cloned().collect();
        let mut infos = Vec::new();
        for ws in workspaces {
            let elements = ws.doc().lock().await.live_count();
            let pages = ws.page_count();
            if ws.name() != DEFAULT_WORKSPACE
                && !ws.created()
                && ws.doc_path().is_none()
                && pages == 0
                && elements == 0
            {
                continue;
            }
            infos.push(WorkspaceInfo {
                name: ws.name().to_string(),
                pages,
                elements: Some(elements),
                persisted: ws.persisted(),
            });
        }
        let mut known: HashSet<String> = infos.iter().map(|info| info.name.clone()).collect();
        if let Some(dir) = &self.workspaces_dir {
            if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) != Some("rsc") {
                        continue;
                    }
                    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                        continue;
                    };
                    if valid_workspace_name(stem) && known.insert(stem.to_string()) {
                        infos.push(WorkspaceInfo {
                            name: stem.to_string(),
                            pages: 0,
                            elements: None,
                            persisted: true,
                        });
                    }
                }
            }
        }
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }

    /// Pages across all workspaces.
    pub fn total_pages(&self) -> usize {
        self.workspaces
            .lock()
            .unwrap()
            .values()
            .map(|ws| ws.page_count())
            .sum()
    }

    /// A page left `workspace`: drop its in-memory mirror when nothing is
    /// watching, and reset ephemeral settings when the server goes idle.
    pub async fn release_workspace(&self, workspace: &Workspace) {
        // Admission must not race a queued cleanup using an old zero-page count.
        let _management = self.workspace_management.lock().await;
        workspace.release_when_idle(workspace.page_count()).await;
        if self.total_pages() == 0 && self.settings_path.is_none() {
            *self.settings.lock().await = AgentSettings::default();
        }
    }

    /// Load persisted agent settings, if any. Called once at startup.
    pub async fn load_settings(&self) {
        let Some(path) = &self.settings_path else {
            return;
        };
        let Ok(json) = tokio::fs::read_to_string(path).await else {
            return;
        };
        match serde_json::from_str::<AgentSettings>(&json) {
            Ok(settings) => *self.settings.lock().await = settings,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "ignoring unreadable settings");
            }
        }
    }

    /// The user's current agent preferences.
    pub async fn current_settings(&self) -> AgentSettings {
        self.settings.lock().await.clone()
    }

    /// Apply a settings patch from the canvas, persist it and tell every page
    /// on every workspace.
    pub async fn apply_settings_patch(
        &self,
        patch: &serde_json::Value,
    ) -> Result<AgentSettings, String> {
        let settings = {
            let mut settings = self.settings.lock().await;
            settings.apply_patch(patch)?;
            settings.clone()
        };
        if let Some(path) = &self.settings_path {
            if let Ok(json) = serde_json::to_string_pretty(&settings) {
                if let Err(error) = write_atomic(path, json.as_bytes()).await {
                    tracing::warn!(%error, path = %path.display(), "could not persist settings");
                }
            }
        }
        let frame = serde_json::json!({ "type": "settings", "settings": settings }).to_string();
        let workspaces: Vec<Arc<Workspace>> =
            self.workspaces.lock().unwrap().values().cloned().collect();
        for ws in workspaces {
            ws.broadcast(PageMessage::control(frame.clone()));
        }
        Ok(settings)
    }

    /// The next connection id.
    pub fn new_page_id(&self) -> u64 {
        self.next_page.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Persist every workspace; called on graceful shutdown.
    pub async fn persist_all(&self) {
        let workspaces: Vec<Arc<Workspace>> =
            self.workspaces.lock().unwrap().values().cloned().collect();
        for ws in workspaces {
            ws.persist_document().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_core::model::{ElementKind, Style};
    use canvas_core::serialization;

    fn shape_at(x: f64) -> canvas_core::model::ElementKind {
        canvas_core::model::ElementKind::Shape {
            shape: canvas_core::model::ShapeKind::Rectangle,
            rect: canvas_core::geometry::Rect::new(x, 0.0, 10.0, 10.0),
            style: canvas_core::model::Style::default(),
            bound_text_id: None,
        }
    }

    fn add_shape(doc: &mut Document, x: f64, version: u64) {
        doc.add(shape_at(x), version);
    }

    fn shape_element(id: &str, index: &str, x: f64) -> Element {
        Element::new(shape_at(x), index.into(), 1).with_id(id)
    }

    fn versioned_shape(id: &str, index: &str, x: f64, version: u64) -> Element {
        let mut element = shape_element(id, index, x);
        element.version = version;
        element.version_nonce = 1;
        element
    }

    fn snapshot(base_revision: u64, elements: impl IntoIterator<Item = Element>) -> PageFrame {
        let mut doc = Document::new();
        for element in elements {
            doc.insert(element);
        }
        PageFrame::Snapshot { base_revision, doc }
    }

    fn patch(put: Vec<Element>) -> PageFrame {
        PageFrame::Patch { put }
    }

    fn control_frame(message: PageMessage) -> serde_json::Value {
        match message {
            PageMessage::Control(text) => serde_json::from_str(&text).unwrap(),
            PageMessage::Document { .. } => panic!("expected a control frame"),
        }
    }

    fn document_put(message: PageMessage) -> (u64, Vec<Element>) {
        match message {
            PageMessage::Document { bytes, .. } => canvas_core::wire::decode_patch(&bytes).unwrap(),
            PageMessage::Control(_) => panic!("expected a document frame"),
        }
    }

    fn text_kind(text: &str, x: f64) -> ElementKind {
        ElementKind::Text {
            rect: canvas_core::geometry::Rect::new(x, 0.0, 100.0, 20.0),
            text: text.into(),
            align: canvas_core::model::TextAlign::Left,
            fixed_width: false,
            container_id: None,
            style: Style::default(),
        }
    }

    fn kinds_of(changed: &[ChangedElement], id: &str) -> Vec<ChangeKind> {
        changed
            .iter()
            .find(|change| change.id == id)
            .unwrap_or_else(|| panic!("changed id {id} was dropped"))
            .kinds
            .clone()
    }

    async fn kinds_between(before: &Document, after: &Document) -> Vec<ChangeKind> {
        let state = AppState::new(PathBuf::from("."), None, true);
        let workspace = state.workspace(DEFAULT_WORKSPACE).await;
        workspace.record_change(before, after, "test").await;
        let records = workspace.changes_since(0).await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].changed.len(), 1);
        records[0].changed[0].kinds.clone()
    }

    #[test]
    fn workspace_names_are_validated() {
        for valid in ["default", "a", "vpc-redesign-2", "x".repeat(40).as_str()] {
            assert!(valid_workspace_name(valid), "{valid} should be valid");
        }
        for invalid in [
            "",
            "-leading-dash",
            "Uppercase",
            "has space",
            "has/slash",
            "has_underscore",
            "ümlaut",
            "x".repeat(41).as_str(),
        ] {
            assert!(
                !valid_workspace_name(invalid),
                "{invalid} should be invalid"
            );
        }
    }

    #[test]
    fn invalid_settings_patch_does_not_apply_valid_fields() {
        let mut settings = AgentSettings::default();
        let before = settings.clone();
        let result = settings.apply_patch(&serde_json::json!({
            "camera": "off", "pacing": "invalid"
        }));
        assert!(result.is_err());
        assert_eq!(settings, before);
    }

    #[tokio::test]
    async fn persistence_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("rs-canvas-state-{}", std::process::id()));
        let path = dir.join("document.rsc");
        let state = AppState::new(PathBuf::from("."), Some(path.clone()), true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        add_shape(&mut *ws.doc().lock().await, 10.0, 1);
        ws.persist_document().await;

        let bytes = tokio::fs::read(&path).await.unwrap();
        assert!(
            canvas_core::envelope::is_binary(&bytes),
            "the mirror is written in the binary envelope"
        );

        let restored = AppState::new(PathBuf::from("."), Some(path), true);
        let restored_ws = restored.workspace(DEFAULT_WORKSPACE).await;
        assert_eq!(restored_ws.doc().lock().await.live_count(), 1);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn json_mirrors_are_ignored() {
        let dir = std::env::temp_dir().join(format!(
            "rs-canvas-json-mirror-{}-{}",
            std::process::id(),
            now_ms()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("document.rsc");
        let mut doc = Document::new();
        add_shape(&mut doc, 10.0, 1);
        tokio::fs::write(&path, serialization::to_json_pretty(&doc).unwrap())
            .await
            .unwrap();

        let state = AppState::new(PathBuf::from("."), Some(path), true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        assert_eq!(
            ws.doc().lock().await.live_count(),
            0,
            "a mirror must be a binary document"
        );
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn future_binary_mirrors_are_ignored() {
        let dir = std::env::temp_dir().join(format!(
            "rs-canvas-future-mirror-{}-{}",
            std::process::id(),
            now_ms()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("document.rsc");
        let bytes =
            canvas_core::envelope::encode(99, canvas_core::envelope::Codec::Postcard, &[]).unwrap();
        tokio::fs::write(&path, bytes).await.unwrap();

        let state = AppState::new(PathBuf::from("."), Some(path), true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        assert_eq!(ws.doc().lock().await.live_count(), 0);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn unreadable_documents_are_ignored() {
        let dir = std::env::temp_dir().join(format!("rs-canvas-bad-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("document.rsc");
        tokio::fs::write(&path, "{not json").await.unwrap();

        let state = AppState::new(PathBuf::from("."), Some(path), true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        assert_eq!(ws.doc().lock().await.live_count(), 0);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn named_workspaces_persist_beside_the_default_document() {
        let dir = std::env::temp_dir().join(format!("rs-canvas-ws-{}", std::process::id()));
        let state = AppState::new(PathBuf::from("."), Some(dir.join("current.rsc")), true);
        let alpha = state.workspace("alpha").await;
        assert_eq!(
            alpha.doc_path(),
            Some(dir.join("workspaces/alpha.rsc").as_path())
        );
        add_shape(&mut *alpha.doc().lock().await, 5.0, 1);
        alpha.persist_document().await;

        let reopened = AppState::new(PathBuf::from("."), Some(dir.join("current.rsc")), true);
        let alpha = reopened.workspace("alpha").await;
        assert_eq!(alpha.doc().lock().await.live_count(), 1);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn creating_a_workspace_is_validated_and_persisted_empty() {
        let dir = std::env::temp_dir().join(format!(
            "rs-workspace-create-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let state = AppState::new(PathBuf::from("."), Some(dir.join("current.rsc")), true);
        assert!(matches!(
            state.create_workspace("Bad Name").await,
            Err(WorkspaceManagementError::Invalid(_))
        ));
        assert!(matches!(
            state.create_workspace(DEFAULT_WORKSPACE).await,
            Err(WorkspaceManagementError::Conflict(_))
        ));

        let created = state.create_workspace("fresh").await.unwrap();
        assert_eq!(created.doc().lock().await.live_count(), 0);
        assert!(created.created());
        assert!(
            dir.join("workspaces/fresh.rsc").exists(),
            "an explicitly created workspace is durable before its first edit"
        );
        assert!(matches!(
            state.create_workspace("fresh").await,
            Err(WorkspaceManagementError::Conflict(_))
        ));

        let restarted = AppState::new(PathBuf::from("."), Some(dir.join("current.rsc")), true);
        let names: Vec<String> = restarted
            .list_workspaces()
            .await
            .iter()
            .map(|info| info.name.clone())
            .collect();
        assert!(names.contains(&"fresh".to_string()), "{names:?}");
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn created_workspaces_are_listed_without_a_backing_file() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let created = state.create_workspace("fresh").await.unwrap();
        assert!(created.doc_path().is_none());
        let names: Vec<String> = state
            .list_workspaces()
            .await
            .iter()
            .map(|info| info.name.clone())
            .collect();
        assert!(
            names.contains(&"fresh".to_string()),
            "an explicit create must be discoverable even in memory-only mode: {names:?}"
        );
    }

    #[tokio::test]
    async fn workspaces_are_isolated() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let alpha = state.workspace("alpha").await;
        let beta = state.workspace("beta").await;
        let mut beta_pages = beta.subscribe();

        let element = Element::new(shape_at(0.0), "a0".into(), 1).with_id("one");
        assert!(
            alpha
                .apply_page_frame(7, patch(vec![element]))
                .await
                .changed
        );

        assert_eq!(alpha.doc().lock().await.live_count(), 1);
        assert_eq!(beta.doc().lock().await.live_count(), 0);
        assert_eq!(alpha.revision(), 1);
        assert_eq!(beta.revision(), 0);
        assert!(
            beta_pages.try_recv().is_err(),
            "alpha's patch must not leak into beta"
        );
    }

    #[tokio::test]
    async fn renaming_preserves_the_document_and_retires_old_connections() {
        let dir = std::env::temp_dir().join(format!(
            "rs-workspace-rename-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let state = AppState::new(PathBuf::from("."), Some(dir.join("current.rsc")), true);
        let old = state.workspace("alpha").await;
        add_shape(&mut *old.doc().lock().await, 42.0, 1);
        old.persist_document().await;
        let before = old.doc().lock().await.clone();
        let mut messages = old.subscribe();
        assert!(state.register_page(&old).await);
        state.manage_workspace("alpha", Some("beta")).await.unwrap();
        assert!(
            !state.register_page(&old).await,
            "retired connections cannot rejoin"
        );
        let frame = control_frame(messages.recv().await.unwrap());
        assert_eq!(frame["action"], "rename");
        assert_eq!(frame["workspace"], "beta");
        old.persist_document().await;
        assert!(
            !dir.join("workspaces/alpha.rsc").exists(),
            "a late persist cannot recreate the old file"
        );
        let renamed = tokio::fs::read(dir.join("workspaces/beta.rsc"))
            .await
            .unwrap();
        assert!(
            canvas_core::envelope::is_binary(&renamed),
            "workspace rename writes the same binary format"
        );
        let restarted = AppState::new(PathBuf::from("."), Some(dir.join("current.rsc")), true);
        assert_eq!(
            *restarted.workspace("beta").await.doc().lock().await,
            before
        );
        assert!(!restarted
            .list_workspaces()
            .await
            .iter()
            .any(|info| info.name == "alpha"));
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn workspace_management_preserves_existing_destinations_and_other_tabs() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let alpha = state.workspace("alpha").await;
        let beta = state.workspace("beta").await;
        add_shape(&mut *alpha.doc().lock().await, 1.0, 1);
        add_shape(&mut *beta.doc().lock().await, 2.0, 1);
        let before_alpha = alpha.doc().lock().await.clone();
        let before_beta = beta.doc().lock().await.clone();
        assert!(matches!(
            state.manage_workspace("alpha", Some("beta")).await,
            Err(WorkspaceManagementError::Conflict(_))
        ));
        alpha.set_page_count(2);
        for renamed in [None, Some("gamma")] {
            assert!(matches!(
                state.manage_workspace("alpha", renamed).await,
                Err(WorkspaceManagementError::Conflict(_))
            ));
        }
        assert_eq!(*alpha.doc().lock().await, before_alpha);
        assert_eq!(*beta.doc().lock().await, before_beta);
        assert!(!alpha.is_retired());
        assert!(matches!(
            state.manage_workspace("default", None).await,
            Err(WorkspaceManagementError::Invalid(_))
        ));
        assert!(matches!(
            state.manage_workspace("alpha", Some("../oops")).await,
            Err(WorkspaceManagementError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn deleting_a_persisted_only_workspace_survives_restart() {
        let dir = std::env::temp_dir().join(format!(
            "rs-workspace-delete-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let initial = AppState::new(PathBuf::from("."), Some(dir.join("current.rsc")), true);
        let old = initial.workspace("alpha").await;
        add_shape(&mut *old.doc().lock().await, 4.0, 1);
        old.persist_document().await;
        let state = AppState::new(PathBuf::from("."), Some(dir.join("current.rsc")), true);
        assert!(state
            .list_workspaces()
            .await
            .iter()
            .any(|info| info.name == "alpha"));
        state.manage_workspace("alpha", None).await.unwrap();
        assert!(!dir.join("workspaces/alpha.rsc").exists());
        assert!(!state
            .list_workspaces()
            .await
            .iter()
            .any(|info| info.name == "alpha"));
        assert!(matches!(
            state.manage_workspace("alpha", None).await,
            Err(WorkspaceManagementError::NotFound)
        ));
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn rapid_page_activity_has_a_strict_order() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let alpha = state.workspace("alpha").await;
        let beta = state.workspace("beta").await;
        alpha.set_page_count(1);
        beta.set_page_count(1);
        for _ in 0..100 {
            alpha.note_page_activity();
            let first = alpha.activity();
            beta.note_page_activity();
            assert!(
                beta.activity() > first,
                "rapid activity must not tie at millisecond precision"
            );
            assert_eq!(state.current_workspace().await.name(), "beta");
            alpha.note_page_activity();
            assert_eq!(state.current_workspace().await.name(), "alpha");
        }
    }

    #[tokio::test]
    async fn current_workspace_follows_the_active_page() {
        let state = AppState::new(PathBuf::from("."), None, true);
        // No pages anywhere: the default workspace, even though it has none.
        let current = state.current_workspace().await;
        assert_eq!(current.name(), DEFAULT_WORKSPACE);

        let alpha = state.workspace("alpha").await;
        alpha.set_page_count(1);
        alpha.set_activity(100);
        assert_eq!(state.current_workspace().await.name(), "alpha");

        let beta = state.workspace("beta").await;
        beta.set_page_count(1);
        beta.set_activity(200);
        assert_eq!(state.current_workspace().await.name(), "beta");

        // A workspace nobody is watching is never current.
        beta.set_page_count(0);
        assert_eq!(state.current_workspace().await.name(), "alpha");
    }

    #[tokio::test]
    async fn the_workspace_listing_skips_never_used_husks() {
        let state = AppState::new(PathBuf::from("."), None, true);
        state.workspace(DEFAULT_WORKSPACE).await;
        state.workspace("touched").await;
        let names: Vec<String> = state
            .list_workspaces()
            .await
            .iter()
            .map(|info| info.name.clone())
            .collect();
        assert_eq!(names, vec![DEFAULT_WORKSPACE]);

        let alpha = state.workspace("alpha").await;
        alpha.set_page_count(1);
        let names: Vec<String> = state
            .list_workspaces()
            .await
            .iter()
            .map(|info| info.name.clone())
            .collect();
        assert!(names.contains(&"alpha".to_string()));
    }

    #[tokio::test]
    async fn a_new_page_cannot_join_halfway_through_idle_cleanup() {
        let state = Arc::new(AppState::new(PathBuf::from("."), None, true));
        let workspace = state.workspace(DEFAULT_WORKSPACE).await;
        let mut doc = workspace.doc().lock().await;
        add_shape(&mut doc, 1.0, 1);
        let releasing = state.clone();
        let old = workspace.clone();
        let cleanup = tokio::spawn(async move {
            releasing.release_workspace(&old).await;
        });
        tokio::task::yield_now().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), state.register_page(&workspace))
                .await
                .is_err(),
            "admission must wait for the old document cleanup to complete"
        );
        drop(doc);
        cleanup.await.unwrap();
        assert_eq!(workspace.doc().lock().await.live_count(), 0);
        assert!(state.register_page(&workspace).await);
    }

    #[tokio::test]
    async fn in_memory_mirrors_clear_when_the_last_page_leaves() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        add_shape(&mut *ws.doc().lock().await, 0.0, 1);

        ws.release_when_idle(1).await;
        assert_eq!(ws.doc().lock().await.live_count(), 1, "a page is watching");
        ws.release_when_idle(0).await;
        assert_eq!(ws.doc().lock().await.live_count(), 0, "no page, no mirror");
    }

    #[tokio::test]
    async fn persisted_mirrors_survive_idle() {
        let dir = std::env::temp_dir().join(format!("rs-canvas-idle-{}", std::process::id()));
        let state = AppState::new(PathBuf::from("."), Some(dir.join("document.rsc")), true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        add_shape(&mut *ws.doc().lock().await, 0.0, 1);
        ws.release_when_idle(0).await;
        assert_eq!(ws.doc().lock().await.live_count(), 1);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn patches_merge_and_broadcast_to_everyone_but_the_origin() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        ws.set_page_count(1);
        let mut from_server = ws.subscribe();
        let element = Element::new(shape_at(0.0), "a0".into(), 1).with_id("one");

        assert!(
            ws.apply_page_frame(7, patch(vec![element.clone()]))
                .await
                .changed
        );
        let message = from_server.recv().await.unwrap();
        assert_eq!(message.except(), Some(7), "the origin page is skipped");
        let (revision, put) = document_put(message);
        assert_eq!(revision, 1);
        assert_eq!(put.len(), 1);
        assert_eq!(ws.revision(), 1);

        assert!(
            !ws.apply_page_frame(7, patch(vec![element])).await.changed,
            "replaying a patch is a no-op"
        );
        assert_eq!(ws.revision(), 1);
    }

    #[tokio::test]
    async fn a_stale_patch_cannot_clobber_a_newer_mirror() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        ws.set_page_count(1);
        let mut newer = Element::new(shape_at(5.0), "a0".into(), 1).with_id("one");
        newer.version = 4;
        ws.apply_page_frame(1, patch(vec![newer])).await;

        let mut from_server = ws.subscribe();
        let older = Element::new(shape_at(0.0), "a0".into(), 1).with_id("one");
        assert!(!ws.apply_page_frame(1, patch(vec![older])).await.changed);
        assert_eq!(
            ws.doc().lock().await.element("one").unwrap().kind,
            shape_at(5.0)
        );

        // The page that sent the stale copy is corrected back to the winner.
        let (_, put) = document_put(from_server.recv().await.unwrap());
        assert_eq!(put.len(), 1);
        assert_eq!(put[0].version, 4);
    }

    #[tokio::test]
    async fn workspaces_broadcast_patches_as_binary_frames() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        ws.set_page_count(1);
        let mut from_server = ws.subscribe();
        let element = Element::new(shape_at(0.0), "a0".into(), 1).with_id("one");

        assert!(
            ws.apply_page_frame(9, patch(vec![element.clone()]))
                .await
                .changed
        );
        let (revision, put) = document_put(from_server.recv().await.unwrap());
        assert_eq!(revision, 1);
        assert_eq!(put, vec![element]);
    }

    #[tokio::test]
    async fn snapshots_carry_the_current_revision_as_a_binary_frame() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        add_shape(&mut *ws.doc().lock().await, 3.0, 1);
        ws.set_revision(5);
        let bytes = ws.snapshot_frame_bytes().await.unwrap();
        let (revision, doc) = canvas_core::wire::decode_snapshot(&bytes).unwrap();
        assert_eq!(revision, 5);
        assert_eq!(doc.live_count(), 1);
    }

    #[tokio::test]
    async fn record_change_classifies_every_kind() {
        let original = shape_element("shape", "a0", 0.0);

        // Added.
        let before = Document::new();
        let mut after = Document::new();
        after.insert(original.clone());
        assert_eq!(
            kinds_between(&before, &after).await,
            vec![ChangeKind::Added]
        );

        // Removed by absence.
        let mut before = Document::new();
        before.insert(original.clone());
        assert_eq!(
            kinds_between(&before, &Document::new()).await,
            vec![ChangeKind::Removed]
        );

        // Removed by tombstone.
        let mut after = Document::new();
        let mut tombstone = original.clone();
        tombstone.is_deleted = true;
        tombstone.touch(2);
        after.insert(tombstone);
        assert_eq!(
            kinds_between(&before, &after).await,
            vec![ChangeKind::Removed]
        );

        // Moved.
        let mut after = before.clone();
        let moved = after.element_mut("shape").unwrap();
        if let ElementKind::Shape { rect, .. } = &mut moved.kind {
            rect.x += 20.0;
        }
        moved.touch(2);
        assert_eq!(
            kinds_between(&before, &after).await,
            vec![ChangeKind::Moved]
        );

        // Resized.
        let mut after = before.clone();
        let resized = after.element_mut("shape").unwrap();
        if let ElementKind::Shape { rect, .. } = &mut resized.kind {
            rect.w += 10.0;
        }
        resized.touch(2);
        assert_eq!(
            kinds_between(&before, &after).await,
            vec![ChangeKind::Resized]
        );

        // Reordered.
        let mut after = before.clone();
        let reordered = after.element_mut("shape").unwrap();
        reordered.index = "a1".into();
        reordered.touch(2);
        assert_eq!(
            kinds_between(&before, &after).await,
            vec![ChangeKind::Reordered]
        );

        // Reparented.
        let mut after = before.clone();
        let reparented = after.element_mut("shape").unwrap();
        reparented.frame_id = Some("frame".into());
        reparented.touch(2);
        assert_eq!(
            kinds_between(&before, &after).await,
            vec![ChangeKind::Reparented]
        );

        // Restyled.
        let mut after = before.clone();
        let restyled = after.element_mut("shape").unwrap();
        if let ElementKind::Shape { style, .. } = &mut restyled.kind {
            style.stroke_width = 6.0;
        }
        restyled.touch(2);
        assert_eq!(
            kinds_between(&before, &after).await,
            vec![ChangeKind::Restyled]
        );

        // Other: a shape-family change outside the named categories.
        let mut after = before.clone();
        let other = after.element_mut("shape").unwrap();
        if let ElementKind::Shape { shape, .. } = &mut other.kind {
            *shape = canvas_core::model::ShapeKind::Ellipse;
        }
        other.touch(2);
        assert_eq!(
            kinds_between(&before, &after).await,
            vec![ChangeKind::Other]
        );

        // Other: a merge-metadata-only touch still reports the id.
        let mut after = before.clone();
        after.element_mut("shape").unwrap().touch(2);
        assert_eq!(
            kinds_between(&before, &after).await,
            vec![ChangeKind::Other]
        );

        // Relabelled: text content and frame name are labels.
        let mut before = Document::new();
        let text = before.add(text_kind("old", 0.0), 1);
        let mut after = before.clone();
        if let ElementKind::Text { text, .. } = &mut after.element_mut(&text).unwrap().kind {
            *text = "new".into();
        }
        after.element_mut(&text).unwrap().touch(2);
        assert_eq!(
            kinds_between(&before, &after).await,
            vec![ChangeKind::Relabelled]
        );

        let mut before = Document::new();
        let frame = before.add(
            ElementKind::Frame {
                rect: canvas_core::geometry::Rect::new(0.0, 0.0, 200.0, 100.0),
                name: "old".into(),
            },
            1,
        );
        let mut after = before.clone();
        if let ElementKind::Frame { name, .. } = &mut after.element_mut(&frame).unwrap().kind {
            *name = "new".into();
        }
        after.element_mut(&frame).unwrap().touch(2);
        assert_eq!(
            kinds_between(&before, &after).await,
            vec![ChangeKind::Relabelled]
        );
    }

    #[tokio::test]
    async fn a_multi_kind_update_reports_every_kind() {
        let mut before = Document::new();
        let id = before.add(text_kind("old", 0.0), 1);
        let mut after = before.clone();
        let changed = after.element_mut(&id).unwrap();
        if let ElementKind::Text {
            rect, text, style, ..
        } = &mut changed.kind
        {
            rect.x += 20.0;
            rect.w += 10.0;
            *text = "new".into();
            style.font_size = 28.0;
        }
        changed.touch(2);

        assert_eq!(
            kinds_between(&before, &after).await,
            vec![
                ChangeKind::Moved,
                ChangeKind::Resized,
                ChangeKind::Relabelled,
                ChangeKind::Restyled
            ]
        );
    }

    #[tokio::test]
    async fn absorb_patch_classifies_before_merge_and_removed_elements() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let workspace = state.workspace(DEFAULT_WORKSPACE).await;
        let element = shape_element("one", "a0", 0.0);

        assert!(
            workspace
                .apply_page_frame(1, patch(vec![element.clone()]))
                .await
                .changed
        );
        let record = workspace.changes_since(0).await.pop().unwrap();
        assert_eq!(kinds_of(&record.changed, "one"), vec![ChangeKind::Added]);

        let mut moved = element.clone();
        moved.version = 2;
        if let ElementKind::Shape { rect, style, .. } = &mut moved.kind {
            rect.x += 50.0;
            style.stroke_width = 5.0;
        }
        assert!(
            workspace
                .apply_page_frame(1, patch(vec![moved.clone()]))
                .await
                .changed
        );
        let record = workspace.changes_since(1).await.pop().unwrap();
        assert_eq!(
            kinds_of(&record.changed, "one"),
            vec![ChangeKind::Moved, ChangeKind::Restyled]
        );

        let mut tombstone = moved;
        tombstone.version = 3;
        tombstone.is_deleted = true;
        tombstone.touch(3);
        assert!(
            workspace
                .apply_page_frame(1, patch(vec![tombstone]))
                .await
                .changed
        );
        let record = workspace.changes_since(2).await.pop().unwrap();
        assert_eq!(kinds_of(&record.changed, "one"), vec![ChangeKind::Removed]);
    }

    #[tokio::test]
    async fn bulk_recordings_are_added() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let workspace = state.workspace(DEFAULT_WORKSPACE).await;
        assert_eq!(workspace.record_bulk(vec!["a".into(), "b".into()]).await, 1);
        let record = workspace.changes_since(0).await.pop().unwrap();
        assert_eq!(record.changed.len(), 2);
        assert_eq!(kinds_of(&record.changed, "a"), vec![ChangeKind::Added]);
        assert_eq!(kinds_of(&record.changed, "b"), vec![ChangeKind::Added]);
    }

    #[tokio::test]
    async fn no_changed_id_is_dropped_from_a_change_record() {
        let mut before = Document::new();
        before.insert(shape_element("gone", "a0", 0.0));
        before.insert(shape_element("stays", "a1", 10.0));
        let mut after = before.clone();
        after.elements.remove("gone");
        let stays = after.element_mut("stays").unwrap();
        if let ElementKind::Shape { rect, .. } = &mut stays.kind {
            rect.x += 5.0;
        }
        stays.touch(2);

        let state = AppState::new(PathBuf::from("."), None, true);
        let workspace = state.workspace(DEFAULT_WORKSPACE).await;
        let (_, put) = workspace.record_change(&before, &after, "page").await;
        assert_eq!(put.len(), 1, "only the surviving element is broadcast");
        let record = workspace.changes_since(0).await.pop().unwrap();
        assert_eq!(record.changed.len(), 2);
        assert_eq!(kinds_of(&record.changed, "gone"), vec![ChangeKind::Removed]);
        assert_eq!(kinds_of(&record.changed, "stays"), vec![ChangeKind::Moved]);
    }

    #[tokio::test]
    async fn the_change_log_stays_bounded_with_kinds() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let workspace = state.workspace(DEFAULT_WORKSPACE).await;
        for index in 0..crate::document::CHANGE_LOG_LIMIT + 5 {
            workspace.record_bulk(vec![format!("id-{index}")]).await;
        }
        let records = workspace.changes_since(0).await;
        assert_eq!(records.len(), crate::document::CHANGE_LOG_LIMIT);
        assert_eq!(records.first().unwrap().revision, 6);
        assert_eq!(
            records.last().unwrap().revision,
            crate::document::CHANGE_LOG_LIMIT as u64 + 5
        );
    }

    #[tokio::test]
    async fn a_stale_reconnect_cannot_overwrite_a_newer_mirror_element() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        ws.set_page_count(1);
        let mut messages = ws.subscribe();

        let newer = versioned_shape("one", "a0", 50.0, 5);
        ws.apply_page_frame(1, patch(vec![newer.clone()])).await;
        let _ = messages.recv().await.unwrap(); // the origin patch broadcast

        let stale = versioned_shape("one", "a0", 0.0, 2);
        let outcome = ws.apply_page_frame(2, snapshot(1, [stale])).await;
        assert!(!outcome.changed, "the stale upload lost");
        assert_eq!(outcome.base_revision, Some(1));
        assert_eq!(
            ws.doc().lock().await.element("one").unwrap().version,
            5,
            "the newer mirror copy survived"
        );
        assert_eq!(outcome.corrections.len(), 1);
        assert_eq!(outcome.corrections[0].kind, newer.kind);

        let (_, correction) = document_put(messages.recv().await.unwrap());
        assert_eq!(correction, vec![newer]);
    }

    #[tokio::test]
    async fn an_explicit_purge_while_disconnected_cannot_revive() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        let live = versioned_shape("one", "a0", 0.0, 3);
        ws.apply_page_frame(1, patch(vec![live.clone()])).await;

        // Undo of a creation purges the entry locally; the page ledger keeps
        // the explicit tombstone and sends it with the reconnect snapshot.
        let mut tombstone = live.clone();
        tombstone.is_deleted = true;
        tombstone.version = 4;
        let outcome = ws
            .apply_page_frame(2, snapshot(1, [tombstone.clone()]))
            .await;
        assert!(outcome.changed, "the explicit ledger record deletes");
        assert!(outcome.corrections.is_empty());
        assert!(ws.doc().lock().await.element("one").unwrap().is_deleted);

        // A later reconnect that only carries the old live copy cannot revive
        // the element: absence is irrelevant and the tombstone wins by LWW.
        let outcome = ws
            .apply_page_frame(2, snapshot(outcome.revision, [live]))
            .await;
        assert!(!outcome.changed);
        assert_eq!(outcome.corrections, vec![tombstone]);
        assert!(ws.doc().lock().await.element("one").unwrap().is_deleted);
    }

    #[tokio::test]
    async fn absence_without_a_ledger_record_is_not_a_deletion() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        let live = versioned_shape("one", "a0", 0.0, 2);
        ws.apply_page_frame(1, patch(vec![live.clone()])).await;
        let revision = ws.revision();

        let outcome = ws.apply_page_frame(2, snapshot(revision, [])).await;
        assert!(!outcome.changed, "absence alone must not delete");
        assert_eq!(ws.doc().lock().await.element("one"), Some(&live));
        assert_eq!(
            outcome.corrections,
            vec![live],
            "the omitted live element is sent back, not removed"
        );
    }

    #[tokio::test]
    async fn snapshot_and_patch_paths_resolve_index_collisions_identically() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let via_snapshot = state.workspace("snapshot").await;
        let via_patch = state.workspace("patch").await;
        let first = versioned_shape("aaa", "a0", 0.0, 1);
        let second = versioned_shape("bbb", "a0", 5.0, 1);

        via_snapshot
            .apply_page_frame(1, snapshot(0, [first.clone(), second.clone()]))
            .await;
        via_patch.apply_page_frame(1, patch(vec![first])).await;
        via_patch.apply_page_frame(1, patch(vec![second])).await;

        assert_eq!(
            *via_snapshot.doc().lock().await,
            *via_patch.doc().lock().await,
            "one collision policy for snapshots and patches"
        );
    }

    #[tokio::test]
    async fn apply_page_frame_drives_connect_upload_edit_and_reconnect() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;

        // Connect with an empty page, then upload its first element.
        let mut page = Document::new();
        let id = page.add(shape_at(0.0), 1);
        let connected = ws
            .apply_page_frame(
                10,
                PageFrame::Snapshot {
                    base_revision: 0,
                    doc: page.clone(),
                },
            )
            .await;
        assert!(connected.changed);
        assert_eq!(connected.revision, 1);
        assert_eq!(connected.accepted, vec![id.clone()]);

        // A local drag reaches the mirror as a patch.
        let mut moved = page.element(&id).unwrap().clone();
        if let ElementKind::Shape { rect, .. } = &mut moved.kind {
            rect.x += 25.0;
        }
        moved.touch(2);
        let edited = ws.apply_page_frame(10, patch(vec![moved.clone()])).await;
        assert!(edited.changed);
        assert_eq!(edited.revision, 2);
        page.insert(moved.clone());

        // While the page is away an agent edits the same element.
        let mut agent = moved.clone();
        if let ElementKind::Shape { rect, .. } = &mut agent.kind {
            rect.x += 50.0;
        }
        agent.touch(3);
        let away = ws.apply_page_frame(99, patch(vec![agent.clone()])).await;
        assert!(away.changed);
        assert_eq!(away.revision, 3);

        // Reconnect: the page uploads its stale copy and base revision. The
        // mirror keeps the agent's newer copy and returns the correction.
        let reconnected = ws
            .apply_page_frame(
                10,
                PageFrame::Snapshot {
                    base_revision: 2,
                    doc: page.clone(),
                },
            )
            .await;
        assert!(!reconnected.changed);
        assert_eq!(reconnected.base_revision, Some(2));
        assert_eq!(reconnected.corrections, vec![agent.clone()]);

        let correction = canvas_core::Patch {
            put: reconnected.corrections.clone(),
        };
        correction.apply_to(&mut page);
        assert_eq!(*ws.doc().lock().await, page);
        assert_eq!(
            ws.revision(),
            3,
            "a pure correction does not invent a revision"
        );

        // A genuinely in-sync reconnect downloads nothing.
        let in_sync = ws
            .apply_page_frame(
                10,
                PageFrame::Snapshot {
                    base_revision: 3,
                    doc: page.clone(),
                },
            )
            .await;
        assert!(!in_sync.changed);
        assert!(in_sync.corrections.is_empty());
    }
}
