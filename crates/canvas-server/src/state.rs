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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use canvas_core::model::{Document, Element};
use canvas_core::serialization;
use tokio::sync::{broadcast, Mutex, Notify};

use crate::aliases::Aliases;
use rmcp::schemars;

/// Small, user-inspectable engineering context, separate from document/view state.
#[derive(
    Clone, Debug, Default, serde::Serialize, serde::Deserialize, rmcp::schemars::JsonSchema,
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

#[derive(Clone)]
pub struct AgentTransaction {
    pub id: String,
    pub changes: Vec<(Option<Element>, Element)>,
}

/// How many recent changes `canvas_read(since)` can recall.
const CHANGE_LOG_LIMIT: usize = 200;

/// How long a burst of edits coalesces before the mirror is written to disk.
static PAGE_ACTIVITY: AtomicU64 = AtomicU64::new(0);

const PERSIST_DEBOUNCE: Duration = Duration::from_millis(500);

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

/// One broadcast frame plus the page that must not receive it (the origin of
/// a patch never gets its own change echoed back).
#[derive(Clone, Debug)]
pub struct PageMessage {
    pub text: String,
    pub except: Option<u64>,
}

impl PageMessage {
    pub fn all(text: String) -> Self {
        Self { text, except: None }
    }

    pub fn except(text: String, page: u64) -> Self {
        Self {
            text,
            except: Some(page),
        }
    }
}

/// One recorded document change, for `canvas_read(since)`.
#[derive(Clone, Debug)]
pub struct ChangeRecord {
    pub revision: u64,
    pub source: String,
    pub changed: Vec<String>,
}

/// The most recent page's transient view state, for `canvas_status`.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct ViewHint {
    #[serde(default)]
    pub camera: Option<serde_json::Value>,
    #[serde(default)]
    pub viewport: Option<serde_json::Value>,
    #[serde(default)]
    pub selection: Vec<String>,
}

/// Per-connection sync state for one page: remembers the last snapshot the
/// page sent, so elements it removes without a tombstone (undo purges them)
/// become real deletions on the wire instead of resurrecting from the mirror.
#[derive(Default)]
pub struct PageSync {
    last: Option<Document>,
}

impl PageSync {
    /// Fold tombstones for disappeared elements into `incoming` (the page's
    /// snapshot about to be absorbed). Returns how many were synthesized.
    pub fn prepare(&self, incoming: &mut Document, now_ms: u64) -> usize {
        let Some(previous) = &self.last else {
            return 0;
        };
        let mut synthesized = 0;
        for (id, element) in &previous.elements {
            if element.is_deleted || incoming.elements.contains_key(id) {
                continue;
            }
            let mut tombstone = element.clone();
            tombstone.is_deleted = true;
            tombstone.touch(now_ms);
            incoming.elements.insert(id.clone(), tombstone);
            synthesized += 1;
        }
        synthesized
    }

    /// Remember the page's document as it actually is (before synthesis), so
    /// the next disappearance is detectable and tombstones don't pile up.
    pub fn commit(&mut self, page_document: Document) {
        self.last = Some(page_document);
    }
}

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
/// viewing it. Everything an agent call can touch — the document, its
/// revision and change log, element handles, view hints, the page broadcast
/// and pending present/screenshot requests — is scoped to one of these.
pub struct Workspace {
    pub name: String,
    /// Identity of this in-memory document lifetime; handles never cross it.
    pub epoch: String,
    generation: AtomicU64,
    pub transactions: Mutex<Vec<AgentTransaction>>,
    pub discussion: Mutex<DiscussionRecord>,
    /// The document mirror.
    pub doc: Mutex<Document>,
    /// Where the mirror is persisted; `None` keeps it in memory only.
    pub doc_path: Option<PathBuf>,
    /// Connected pages on this workspace.
    pub pages: AtomicUsize,
    /// Server-to-page messages (JSON text over the WebSocket).
    pub to_pages: broadcast::Sender<PageMessage>,
    /// A persist is queued; bursts coalesce into one write.
    persist_pending: AtomicBool,
    persist_write: Mutex<()>,
    pub retired: AtomicBool,
    /// Created explicitly by an agent (or any future create RPC) rather than
    /// lazily by a page naming the workspace. Keeps an empty workspace in the
    /// listing even before its first page or element exists.
    created: AtomicBool,
    /// Wakes the persist loop.
    persist_notify: Notify,
    /// Page acknowledgements of a finished presentation: (id, cancelled).
    pub present_acks: broadcast::Sender<(u64, bool)>,
    /// Presentation ids, so each timeline can be matched to its ack.
    pub present_counter: AtomicU64,
    /// True while a presentation is playing on some page.
    pub present_active: AtomicBool,
    /// Fired when a presentation finishes or is cancelled.
    pub present_finished: Notify,
    /// Page replies to a screenshot request: (id, data URL or None).
    pub screenshot_replies: broadcast::Sender<(u64, Option<String>)>,
    /// Screenshot request ids.
    pub screenshot_counter: AtomicU64,
    /// Bumped on every document mutation; the agent's change cursor.
    pub revision: AtomicU64,
    /// Recent changes, newest last (bounded).
    change_log: Mutex<Vec<ChangeRecord>>,
    /// Transient view state from the most recent page report.
    pub view_hint: Mutex<Option<ViewHint>>,
    /// Short handles for element ids, as the agent sees them.
    pub aliases: Mutex<Aliases>,
    /// Monotonic order of the most recent page activity; drives
    /// current-workspace resolution without clock ties or clock changes.
    last_activity: AtomicU64,
}

impl Workspace {
    fn new(name: &str, doc_path: Option<PathBuf>) -> Self {
        let (to_pages, _) = broadcast::channel(64);
        let (present_acks, _) = broadcast::channel(16);
        let (screenshot_replies, _) = broadcast::channel(16);
        Self {
            name: name.to_string(),
            epoch: canvas_core::id::new_element_id(),
            generation: AtomicU64::new(0),
            transactions: Mutex::new(Vec::new()),
            discussion: Mutex::new(DiscussionRecord::default()),
            doc: Mutex::new(Document::new()),
            doc_path,
            pages: AtomicUsize::new(0),
            to_pages,
            persist_pending: AtomicBool::new(false),
            persist_write: Mutex::new(()),
            retired: AtomicBool::new(false),
            created: AtomicBool::new(false),
            persist_notify: Notify::new(),
            present_acks,
            present_counter: AtomicU64::new(0),
            present_active: AtomicBool::new(false),
            present_finished: Notify::new(),
            screenshot_replies,
            screenshot_counter: AtomicU64::new(0),
            revision: AtomicU64::new(0),
            change_log: Mutex::new(Vec::new()),
            view_hint: Mutex::new(None),
            aliases: Mutex::new(Aliases::default()),
            last_activity: AtomicU64::new(0),
        }
    }

    /// A page on this workspace did something; it may now be the current one.
    pub fn note_page_activity(&self) {
        let activity = PAGE_ACTIVITY.fetch_add(1, Ordering::SeqCst) + 1;
        self.last_activity.fetch_max(activity, Ordering::SeqCst);
    }

    pub fn context(&self) -> String {
        format!("{}:{}", self.epoch, self.generation.load(Ordering::SeqCst))
    }

    /// Load the persisted mirror, if any. Called once when the workspace is
    /// created.
    pub async fn load_document(&self) {
        let Some(path) = &self.doc_path else {
            return;
        };
        if let Ok(json) = tokio::fs::read_to_string(path.with_extension("discussion.json")).await {
            if let Ok(notes) = serde_json::from_str(&json) {
                *self.discussion.lock().await = notes;
            }
        }
        let Ok(json) = tokio::fs::read_to_string(path).await else {
            return;
        };
        match serialization::from_json(&json) {
            Ok(doc) => {
                let elements = doc.live_count();
                *self.doc.lock().await = doc;
                tracing::info!(elements, path = %path.display(), workspace = %self.name, "restored document");
            }
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "ignoring unreadable document");
            }
        }
    }

    /// The mirror as compact JSON.
    pub async fn document_json(&self) -> String {
        let doc = self.doc.lock().await;
        serialization::to_json(&doc).unwrap_or_default()
    }

    /// The element id behind a handle; unknown names pass through.
    pub async fn resolve_id(&self, name: &str) -> String {
        self.aliases.lock().await.resolve(name)
    }

    /// Resolve a batch of handles.
    pub async fn resolve_ids(&self, names: &[String]) -> Vec<String> {
        let aliases = self.aliases.lock().await;
        names.iter().map(|name| aliases.resolve(name)).collect()
    }

    /// The `{"type":"snapshot","rev":N,"doc":{...}}` frame for the mirror.
    /// The document is embedded as an object, not an escaped string: one
    /// parse on the page and no escaping overhead.
    pub async fn snapshot_frame(&self) -> String {
        let revision = self.revision.load(Ordering::SeqCst);
        self.snapshot_json(revision).await
    }

    async fn snapshot_json(&self, revision: u64) -> String {
        #[derive(serde::Serialize)]
        struct Snapshot<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            rev: u64,
            doc: &'a Document,
        }
        let doc = self.doc.lock().await;
        serde_json::to_string(&Snapshot {
            kind: "snapshot",
            rev: revision,
            doc: &doc,
        })
        .unwrap_or_default()
    }

    /// The `{"type":"patch","rev":N,"put":[...]}` frame.
    pub fn patch_frame(revision: u64, put: &[Element]) -> String {
        #[derive(serde::Serialize)]
        struct Patch<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            rev: u64,
            put: &'a [Element],
        }
        serde_json::to_string(&Patch {
            kind: "patch",
            rev: revision,
            put,
        })
        .unwrap_or_default()
    }

    /// Merge an already-parsed page document into the mirror. Returns whether
    /// the mirror changed. Connect-time path: the merged result is broadcast
    /// as a full snapshot, because the page may be missing server-side edits.
    /// `origin` excludes the uploading page from that broadcast: it already
    /// holds the state it just sent.
    pub async fn absorb_document(&self, remote: &Document, origin: Option<u64>) -> bool {
        let mut doc = self.doc.lock().await;
        let before = doc.clone();
        let merged = canvas_core::merge::reconcile(&doc, remote);
        if merged == *doc {
            return false;
        }
        *doc = merged;
        let (revision, _) = self.record_change(&before, &doc, "page").await;
        drop(doc);
        self.broadcast_snapshot_except(revision, origin).await;
        true
    }

    /// Merge a page patch into the mirror. Changed elements broadcast to the
    /// other pages; elements whose copy lost to a newer mirror version come
    /// back to the origin as corrections, so a concurrent agent edit cannot
    /// leave the page stale. Returns whether the mirror changed.
    pub async fn absorb_patch(&self, put: Vec<Element>, origin: u64) -> bool {
        let mut changed_ids: Vec<String> = Vec::new();
        let mut corrections: Vec<Element> = Vec::new();
        {
            let mut doc = self.doc.lock().await;
            for element in &put {
                if canvas_core::merge_element(&mut doc, element) {
                    changed_ids.push(element.id.clone());
                } else if let Some(stored) = doc.elements.get(&element.id) {
                    if stored != element && !corrections.iter().any(|e| e.id == element.id) {
                        corrections.push(stored.clone());
                    }
                }
            }
        }
        if changed_ids.is_empty() && corrections.is_empty() {
            return false;
        }

        let revision = if changed_ids.is_empty() {
            self.revision.load(Ordering::SeqCst)
        } else {
            self.record_ids(changed_ids.clone(), "page").await
        };
        let winners: Vec<Element> = {
            let doc = self.doc.lock().await;
            changed_ids
                .iter()
                .filter_map(|id| doc.element(id).cloned())
                .collect()
        };

        if changed_ids.is_empty() {
            // Nothing moved in the mirror; only the origin needs the truth.
            let _ = self
                .to_pages
                .send(PageMessage::all(Self::patch_frame(revision, &corrections)));
        } else if corrections.is_empty() {
            // Normal case: the origin already holds these changes.
            let _ = self.to_pages.send(PageMessage::except(
                Self::patch_frame(revision, &winners),
                origin,
            ));
        } else {
            // The page's copy lost to a concurrent edit; converge everyone.
            let mut all = winners;
            all.extend(corrections);
            let _ = self
                .to_pages
                .send(PageMessage::all(Self::patch_frame(revision, &all)));
        }
        if !changed_ids.is_empty() {
            self.schedule_persist().await;
        }
        !changed_ids.is_empty()
    }

    /// Broadcast an agent-side edit as a patch and queue the persist.
    pub async fn publish_patch(&self, revision: u64, put: Vec<Element>) {
        let _ = self
            .to_pages
            .send(PageMessage::all(Self::patch_frame(revision, &put)));
        self.schedule_persist().await;
    }

    /// Broadcast the whole mirror to every page but `except` (the uploader
    /// already holds what it sent) and queue the persist.
    pub async fn broadcast_snapshot_except(&self, revision: u64, except: Option<u64>) {
        let frame = self.snapshot_json(revision).await;
        let message = match except {
            Some(id) => PageMessage::except(frame, id),
            None => PageMessage::all(frame),
        };
        let _ = self.to_pages.send(message);
        self.schedule_persist().await;
    }

    /// Record a document change: diff old vs new, bump the revision and
    /// append to the bounded change log. Returns the new revision and the
    /// elements as they now stand (tombstones included), ready to broadcast.
    pub async fn record_change(
        &self,
        before: &Document,
        after: &Document,
        source: &str,
    ) -> (u64, Vec<Element>) {
        let mut changed: Vec<String> = Vec::new();
        for (id, after_element) in &after.elements {
            match before.elements.get(id) {
                Some(before_element) if before_element == after_element => {}
                _ => changed.push(id.clone()),
            }
        }
        for id in before.elements.keys() {
            if !after.elements.contains_key(id) {
                changed.push(id.clone());
            }
        }
        let put: Vec<Element> = changed
            .iter()
            .filter_map(|id| after.element(id).cloned())
            .collect();
        if changed.is_empty() {
            return (self.revision.load(Ordering::SeqCst), put);
        }
        let revision = self.record_ids(changed, source).await;
        (revision, put)
    }

    /// Record an additions-only bulk commit: bump the revision and change log
    /// without per-element receipt work. Bulk boards skip the revert history
    /// (which would retain every element); wrap them in a frame to remove
    /// them later with one clear.
    pub async fn record_bulk(&self, changed: Vec<String>) -> u64 {
        self.record_ids(changed, "agent").await
    }

    /// Record a change by the ids it touched.
    async fn record_ids(&self, changed: Vec<String>, source: &str) -> u64 {
        let revision = self.revision.fetch_add(1, Ordering::SeqCst) + 1;
        let mut log = self.change_log.lock().await;
        log.push(ChangeRecord {
            revision,
            source: source.to_string(),
            changed,
        });
        let overflow = log.len().saturating_sub(CHANGE_LOG_LIMIT);
        if overflow > 0 {
            log.drain(0..overflow);
        }
        revision
    }

    /// Change records newer than `since`.
    pub async fn changes_since(&self, since: u64) -> Vec<ChangeRecord> {
        self.change_log
            .lock()
            .await
            .iter()
            .filter(|record| record.revision > since)
            .cloned()
            .collect()
    }

    /// Cursor coverage, checked while the caller holds the document lock.
    pub async fn cursor_valid(&self, since: u64) -> bool {
        let revision = self.revision.load(Ordering::SeqCst);
        let log = self.change_log.lock().await;
        since <= revision
            && log.first().map_or(since == revision, |first| {
                since >= first.revision.saturating_sub(1)
            })
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
        let mut current = self.discussion.lock().await;
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
        if let Some(path) = &self.doc_path {
            let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
            write_atomic(&path.with_extension("discussion.json"), &json)
                .await
                .map_err(|e| e.to_string())?;
        }
        *current = updated.clone();
        Ok(updated)
    }

    /// Queue a persist; bursts within `PERSIST_DEBOUNCE` coalesce into one
    /// write. The loop is spawned when the workspace is created.
    pub async fn schedule_persist(&self) {
        self.persist_pending.store(true, Ordering::SeqCst);
        self.persist_notify.notify_one();
    }

    /// The debounced persist loop, spawned per workspace at creation.
    pub async fn persist_loop(self: Arc<Self>) {
        loop {
            self.persist_notify.notified().await;
            if self.retired.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(PERSIST_DEBOUNCE).await;
            if self.persist_pending.swap(false, Ordering::SeqCst) {
                self.persist_document().await;
            }
        }
    }

    /// In-memory mirrors live only while a page is watching: without a
    /// configured document file there is no home for the drawing between
    /// page sessions — the page's own autosave is the source of truth.
    pub async fn release_when_idle(&self, remaining_pages: usize) {
        if remaining_pages == 0 && self.doc_path.is_none() {
            let mut doc = self.doc.lock().await;
            *doc = Document::new();
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.transactions.lock().await.clear();
        }
    }

    /// Wait out an in-flight presentation so captures show the settled view.
    pub async fn wait_for_presentation(&self, max: Duration) {
        if !self.present_active.load(Ordering::SeqCst) {
            return;
        }
        let _ = tokio::time::timeout(max, self.present_finished.notified()).await;
    }

    /// Mark playback started (screenshots will wait for it).
    pub fn presentation_started(&self) {
        self.present_active.store(true, Ordering::SeqCst);
    }

    /// Mark playback over and wake any waiting screenshot.
    pub fn presentation_finished(&self) {
        self.present_active.store(false, Ordering::SeqCst);
        self.present_finished.notify_one();
    }

    /// Write the mirror to disk atomically; a no-op without a path.
    pub async fn persist_document(&self) {
        let _write = self.persist_write.lock().await;
        if self.retired.load(Ordering::SeqCst) {
            return;
        }
        let Some(path) = &self.doc_path else {
            return;
        };
        let json = self.document_json().await;
        if let Err(error) = write_atomic(path, &json).await {
            tracing::warn!(%error, path = %path.display(), "could not persist document");
        }
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
                .map(|dir| dir.join(format!("{name}.json")))
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
        if let Some(path) = &candidate.doc_path {
            if tokio::fs::try_exists(path).await.map_err(Io)? {
                return Err(Conflict(
                    "That workspace already exists. Choose another name.".into(),
                ));
            }
        }
        let workspace = self.workspace_unlocked(name).await;
        workspace.created.store(true, Ordering::SeqCst);
        workspace.persist_document().await;
        Ok(workspace)
    }

    /// Serialize connection admission with workspace management so a tab
    /// cannot join a document while it is being renamed or deleted.
    pub async fn register_page(&self, workspace: &Workspace) -> bool {
        let _management = self.workspace_management.lock().await;
        if workspace.retired.load(Ordering::SeqCst) {
            return false;
        }
        workspace.pages.fetch_add(1, Ordering::SeqCst);
        workspace.note_page_activity();
        true
    }

    pub async fn manage_workspace(
        &self,
        name: &str,
        renamed: Option<&str>,
        incoming: Option<Document>,
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
                || match &destination.doc_path {
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
        let path = self.build_workspace(name).doc_path;
        if !stored
            && incoming.is_none()
            && !match &path {
                Some(path) => tokio::fs::try_exists(path).await.map_err(Io)?,
                None => false,
            }
        {
            return Err(NotFound);
        }
        let source = self.workspace_unlocked(name).await;
        if source.pages.load(Ordering::SeqCst) > 1 {
            return Err(Conflict(
                "This workspace is open in another tab. Close or switch those tabs first.".into(),
            ));
        }
        let _write = source.persist_write.lock().await;
        let mut doc = source.doc.lock().await;
        let merged = incoming
            .as_ref()
            .map(|incoming| canvas_core::merge::reconcile(&doc, incoming))
            .unwrap_or_else(|| doc.clone());
        if let Some(new_name) = renamed {
            let destination = Arc::new(self.build_workspace(new_name));
            destination
                .created
                .store(source.created.load(Ordering::SeqCst), Ordering::SeqCst);
            if let Some(path) = &destination.doc_path {
                let json =
                    serialization::to_json(&merged).map_err(|error| Invalid(error.to_string()))?;
                write_atomic(path, &json).await.map_err(Io)?;
            }
            // Remove the old file only once the new document is durable.
            if let Some(path) = &source.doc_path {
                match tokio::fs::remove_file(path).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        if let Some(path) = &destination.doc_path {
                            let _ = tokio::fs::remove_file(path).await;
                        }
                        return Err(Io(error));
                    }
                }
            }
            *destination.doc.lock().await = merged;
            self.workspaces
                .lock()
                .unwrap()
                .insert(new_name.into(), destination.clone());
            tokio::spawn(destination.persist_loop());
        } else if let Some(path) = &source.doc_path {
            match tokio::fs::remove_file(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(Io(error)),
            }
        }
        source.retired.store(true, Ordering::SeqCst);
        *doc = Document::new();
        source.persist_notify.notify_one();
        let _ = source.to_pages.send(PageMessage::all(serde_json::json!({
            "type": "workspace_changed", "action": if renamed.is_some() { "rename" } else { "delete" },
            "workspace": renamed.unwrap_or(DEFAULT_WORKSPACE),
        }).to_string()));
        self.workspaces.lock().unwrap().remove(name);
        Ok(())
    }

    /// The workspace agent calls act on: the one the user's most recently
    /// active page is viewing, or `default` when no page is connected.
    pub async fn current_workspace(&self) -> Arc<Workspace> {
        let current = {
            let map = self.workspaces.lock().unwrap();
            map.values()
                .filter(|ws| ws.pages.load(Ordering::SeqCst) > 0)
                .max_by_key(|ws| ws.last_activity.load(Ordering::SeqCst))
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
            let elements = ws.doc.lock().await.live_count();
            let pages = ws.pages.load(Ordering::SeqCst);
            if ws.name != DEFAULT_WORKSPACE
                && !ws.created.load(Ordering::SeqCst)
                && ws.doc_path.is_none()
                && pages == 0
                && elements == 0
            {
                continue;
            }
            infos.push(WorkspaceInfo {
                name: ws.name.clone(),
                pages,
                elements: Some(elements),
                persisted: ws.doc_path.is_some(),
            });
        }
        let mut known: HashSet<String> = infos.iter().map(|info| info.name.clone()).collect();
        if let Some(dir) = &self.workspaces_dir {
            if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
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
            .map(|ws| ws.pages.load(Ordering::SeqCst))
            .sum()
    }

    /// A page left `workspace`: drop its in-memory mirror when nothing is
    /// watching, and reset ephemeral settings when the server goes idle.
    pub async fn release_workspace(&self, workspace: &Workspace) {
        // Admission must not race a queued cleanup using an old zero-page count.
        let _management = self.workspace_management.lock().await;
        workspace
            .release_when_idle(workspace.pages.load(Ordering::SeqCst))
            .await;
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
                if let Err(error) = write_atomic(path, &json).await {
                    tracing::warn!(%error, path = %path.display(), "could not persist settings");
                }
            }
        }
        let frame = serde_json::json!({ "type": "settings", "settings": settings }).to_string();
        let workspaces: Vec<Arc<Workspace>> =
            self.workspaces.lock().unwrap().values().cloned().collect();
        for ws in workspaces {
            let _ = ws.to_pages.send(PageMessage::all(frame.clone()));
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

async fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temp = path.with_extension("json.tmp");
    tokio::fs::write(&temp, contents).await?;
    tokio::fs::rename(&temp, path).await
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let path = dir.join("document.json");
        let state = AppState::new(PathBuf::from("."), Some(path.clone()), true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        add_shape(&mut *ws.doc.lock().await, 10.0, 1);
        ws.persist_document().await;

        let restored = AppState::new(PathBuf::from("."), Some(path), true);
        let restored_ws = restored.workspace(DEFAULT_WORKSPACE).await;
        assert_eq!(restored_ws.doc.lock().await.live_count(), 1);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn unreadable_documents_are_ignored() {
        let dir = std::env::temp_dir().join(format!("rs-canvas-bad-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("document.json");
        tokio::fs::write(&path, "{not json").await.unwrap();

        let state = AppState::new(PathBuf::from("."), Some(path), true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        assert_eq!(ws.doc.lock().await.live_count(), 0);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn named_workspaces_persist_beside_the_default_document() {
        let dir = std::env::temp_dir().join(format!("rs-canvas-ws-{}", std::process::id()));
        let state = AppState::new(PathBuf::from("."), Some(dir.join("current.json")), true);
        let alpha = state.workspace("alpha").await;
        assert_eq!(
            alpha.doc_path.as_deref(),
            Some(dir.join("workspaces/alpha.json").as_path())
        );
        add_shape(&mut *alpha.doc.lock().await, 5.0, 1);
        alpha.persist_document().await;

        let reopened = AppState::new(PathBuf::from("."), Some(dir.join("current.json")), true);
        let alpha = reopened.workspace("alpha").await;
        assert_eq!(alpha.doc.lock().await.live_count(), 1);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn creating_a_workspace_is_validated_and_persisted_empty() {
        let dir = std::env::temp_dir().join(format!(
            "rs-workspace-create-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let state = AppState::new(PathBuf::from("."), Some(dir.join("current.json")), true);
        assert!(matches!(
            state.create_workspace("Bad Name").await,
            Err(WorkspaceManagementError::Invalid(_))
        ));
        assert!(matches!(
            state.create_workspace(DEFAULT_WORKSPACE).await,
            Err(WorkspaceManagementError::Conflict(_))
        ));

        let created = state.create_workspace("fresh").await.unwrap();
        assert_eq!(created.doc.lock().await.live_count(), 0);
        assert!(created.created.load(Ordering::SeqCst));
        assert!(
            dir.join("workspaces/fresh.json").exists(),
            "an explicitly created workspace is durable before its first edit"
        );
        assert!(matches!(
            state.create_workspace("fresh").await,
            Err(WorkspaceManagementError::Conflict(_))
        ));

        let restarted = AppState::new(PathBuf::from("."), Some(dir.join("current.json")), true);
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
        assert_eq!(created.doc_path, None);
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
        let mut beta_pages = beta.to_pages.subscribe();

        let element = Element::new(shape_at(0.0), "a0".into(), 1).with_id("one");
        assert!(alpha.absorb_patch(vec![element], 7).await);

        assert_eq!(alpha.doc.lock().await.live_count(), 1);
        assert_eq!(beta.doc.lock().await.live_count(), 0);
        assert_eq!(alpha.revision.load(Ordering::SeqCst), 1);
        assert_eq!(beta.revision.load(Ordering::SeqCst), 0);
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
        let state = AppState::new(PathBuf::from("."), Some(dir.join("current.json")), true);
        let old = state.workspace("alpha").await;
        add_shape(&mut *old.doc.lock().await, 42.0, 1);
        old.persist_document().await;
        let before = old.doc.lock().await.clone();
        let mut messages = old.to_pages.subscribe();
        assert!(state.register_page(&old).await);
        state
            .manage_workspace("alpha", Some("beta"), None)
            .await
            .unwrap();
        assert!(
            !state.register_page(&old).await,
            "retired connections cannot rejoin"
        );
        let frame: serde_json::Value =
            serde_json::from_str(&messages.recv().await.unwrap().text).unwrap();
        assert_eq!(frame["action"], "rename");
        assert_eq!(frame["workspace"], "beta");
        old.persist_document().await;
        assert!(
            !dir.join("workspaces/alpha.json").exists(),
            "a late persist cannot recreate the old file"
        );
        let restarted = AppState::new(PathBuf::from("."), Some(dir.join("current.json")), true);
        assert_eq!(*restarted.workspace("beta").await.doc.lock().await, before);
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
        add_shape(&mut *alpha.doc.lock().await, 1.0, 1);
        add_shape(&mut *beta.doc.lock().await, 2.0, 1);
        let before_alpha = alpha.doc.lock().await.clone();
        let before_beta = beta.doc.lock().await.clone();
        assert!(matches!(
            state.manage_workspace("alpha", Some("beta"), None).await,
            Err(WorkspaceManagementError::Conflict(_))
        ));
        alpha.pages.store(2, Ordering::SeqCst);
        for renamed in [None, Some("gamma")] {
            assert!(matches!(
                state.manage_workspace("alpha", renamed, None).await,
                Err(WorkspaceManagementError::Conflict(_))
            ));
        }
        assert_eq!(*alpha.doc.lock().await, before_alpha);
        assert_eq!(*beta.doc.lock().await, before_beta);
        assert!(!alpha.retired.load(Ordering::SeqCst));
        assert!(matches!(
            state.manage_workspace("default", None, None).await,
            Err(WorkspaceManagementError::Invalid(_))
        ));
        assert!(matches!(
            state.manage_workspace("alpha", Some("../oops"), None).await,
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
        let initial = AppState::new(PathBuf::from("."), Some(dir.join("current.json")), true);
        let old = initial.workspace("alpha").await;
        add_shape(&mut *old.doc.lock().await, 4.0, 1);
        old.persist_document().await;
        let state = AppState::new(PathBuf::from("."), Some(dir.join("current.json")), true);
        assert!(state
            .list_workspaces()
            .await
            .iter()
            .any(|info| info.name == "alpha"));
        state.manage_workspace("alpha", None, None).await.unwrap();
        assert!(!dir.join("workspaces/alpha.json").exists());
        assert!(!state
            .list_workspaces()
            .await
            .iter()
            .any(|info| info.name == "alpha"));
        assert!(matches!(
            state.manage_workspace("alpha", None, None).await,
            Err(WorkspaceManagementError::NotFound)
        ));
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn rapid_page_activity_has_a_strict_order() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let alpha = state.workspace("alpha").await;
        let beta = state.workspace("beta").await;
        alpha.pages.store(1, Ordering::SeqCst);
        beta.pages.store(1, Ordering::SeqCst);
        for _ in 0..100 {
            alpha.note_page_activity();
            let first = alpha.last_activity.load(Ordering::SeqCst);
            beta.note_page_activity();
            assert!(
                beta.last_activity.load(Ordering::SeqCst) > first,
                "rapid activity must not tie at millisecond precision"
            );
            assert_eq!(state.current_workspace().await.name, "beta");
            alpha.note_page_activity();
            assert_eq!(state.current_workspace().await.name, "alpha");
        }
    }

    #[tokio::test]
    async fn current_workspace_follows_the_active_page() {
        let state = AppState::new(PathBuf::from("."), None, true);
        // No pages anywhere: the default workspace, even though it has none.
        let current = state.current_workspace().await;
        assert_eq!(current.name, DEFAULT_WORKSPACE);

        let alpha = state.workspace("alpha").await;
        alpha.pages.store(1, Ordering::SeqCst);
        alpha.last_activity.store(100, Ordering::SeqCst);
        assert_eq!(state.current_workspace().await.name, "alpha");

        let beta = state.workspace("beta").await;
        beta.pages.store(1, Ordering::SeqCst);
        beta.last_activity.store(200, Ordering::SeqCst);
        assert_eq!(state.current_workspace().await.name, "beta");

        // A workspace nobody is watching is never current.
        beta.pages.store(0, Ordering::SeqCst);
        assert_eq!(state.current_workspace().await.name, "alpha");
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
        alpha.pages.store(1, Ordering::SeqCst);
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
        let mut doc = workspace.doc.lock().await;
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
        assert_eq!(workspace.doc.lock().await.live_count(), 0);
        assert!(state.register_page(&workspace).await);
    }

    #[tokio::test]
    async fn in_memory_mirrors_clear_when_the_last_page_leaves() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        add_shape(&mut *ws.doc.lock().await, 0.0, 1);

        ws.release_when_idle(1).await;
        assert_eq!(ws.doc.lock().await.live_count(), 1, "a page is watching");
        ws.release_when_idle(0).await;
        assert_eq!(ws.doc.lock().await.live_count(), 0, "no page, no mirror");
    }

    #[tokio::test]
    async fn persisted_mirrors_survive_idle() {
        let dir = std::env::temp_dir().join(format!("rs-canvas-idle-{}", std::process::id()));
        let state = AppState::new(PathBuf::from("."), Some(dir.join("document.json")), true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        add_shape(&mut *ws.doc.lock().await, 0.0, 1);
        ws.release_when_idle(0).await;
        assert_eq!(ws.doc.lock().await.live_count(), 1);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn patches_merge_and_broadcast_to_everyone_but_the_origin() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        let mut from_server = ws.to_pages.subscribe();
        let element = Element::new(shape_at(0.0), "a0".into(), 1).with_id("one");

        assert!(ws.absorb_patch(vec![element.clone()], 7).await);
        let message = from_server.recv().await.unwrap();
        assert_eq!(message.except, Some(7), "the origin page is skipped");
        let frame: serde_json::Value = serde_json::from_str(&message.text).unwrap();
        assert_eq!(frame["type"], "patch");
        assert_eq!(frame["put"].as_array().unwrap().len(), 1);
        assert_eq!(ws.revision.load(Ordering::SeqCst), 1);

        assert!(
            !ws.absorb_patch(vec![element], 7).await,
            "replaying a patch is a no-op"
        );
        assert_eq!(ws.revision.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_stale_patch_cannot_clobber_a_newer_mirror() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        let mut newer = Element::new(shape_at(5.0), "a0".into(), 1).with_id("one");
        newer.version = 4;
        ws.absorb_patch(vec![newer], 1).await;

        let mut from_server = ws.to_pages.subscribe();
        let older = Element::new(shape_at(0.0), "a0".into(), 1).with_id("one");
        assert!(!ws.absorb_patch(vec![older], 1).await);
        assert_eq!(
            ws.doc.lock().await.element("one").unwrap().kind,
            shape_at(5.0)
        );

        // The page that sent the stale copy is corrected back to the winner.
        let frame: serde_json::Value =
            serde_json::from_str(&from_server.recv().await.unwrap().text).unwrap();
        assert_eq!(frame["type"], "patch");
        let put = frame["put"].as_array().unwrap();
        assert_eq!(put.len(), 1);
        assert_eq!(put[0]["version"], 4);
    }

    #[tokio::test]
    async fn snapshot_frames_embed_the_document_as_an_object() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let ws = state.workspace(DEFAULT_WORKSPACE).await;
        add_shape(&mut *ws.doc.lock().await, 0.0, 1);
        let frame: serde_json::Value = serde_json::from_str(&ws.snapshot_frame().await).unwrap();
        assert_eq!(frame["type"], "snapshot");
        assert!(frame["doc"]["elements"].is_object());
        assert_eq!(frame["rev"], 0);
    }

    #[test]
    fn page_sync_tombstones_elements_the_page_purged() {
        let mut first = Document::new();
        let id = first.add(shape_at(0.0), 1);
        let mut sync = PageSync::default();
        sync.commit(first.clone());

        // Undo of a creation purges the entry: it is absent, not tombstoned.
        let mut second = Document::new();
        assert_eq!(sync.prepare(&mut second, 2), 1);
        let tombstone = second.element(&id).expect("synthesized tombstone");
        assert!(tombstone.is_deleted);
        assert!(tombstone.version > first.element(&id).unwrap().version);

        // Once committed, the base forgets it: no repeated tombstones.
        sync.commit(second);
        let mut third = Document::new();
        assert_eq!(sync.prepare(&mut third, 3), 0);
        assert!(third.element(&id).is_none());
    }

    #[test]
    fn page_sync_leaves_tombstones_and_new_elements_alone() {
        let mut previous = Document::new();
        let kept = previous.add(shape_at(0.0), 1);
        let mut sync = PageSync::default();
        sync.commit(previous);

        let mut incoming = Document::new();
        let mut deleted =
            canvas_core::model::Element::new(shape_at(0.0), "a1".into(), 2).with_id(kept.clone());
        deleted.is_deleted = true;
        incoming.insert(deleted);
        let fresh = incoming.add(shape_at(20.0), 2);

        assert_eq!(sync.prepare(&mut incoming, 3), 0);
        assert!(incoming.element(&kept).unwrap().is_deleted);
        assert!(incoming.element(&fresh).is_some());
    }

    #[test]
    fn first_page_snapshot_never_tombstones() {
        let mut incoming = Document::new();
        incoming.add(shape_at(0.0), 1);
        assert_eq!(PageSync::default().prepare(&mut incoming, 2), 0);
    }
}
