//! Shared server state: the document mirror, its on-disk home, and the
//! connected pages.
//!
//! The page owns the interactive document; this mirror is what MCP tool calls
//! read and write. Both sides meet through the existing `canvas-core` merge
//! rules, never through a bespoke protocol.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use canvas_core::model::{Document, Element};
use canvas_core::serialization;
use tokio::sync::{broadcast, Mutex, Notify};

use crate::aliases::Aliases;

/// How many recent changes `canvas_read(since)` can recall.
const CHANGE_LOG_LIMIT: usize = 200;

/// How long a burst of edits coalesces before the mirror is written to disk.
const PERSIST_DEBOUNCE: Duration = Duration::from_millis(500);

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
        if let Some(camera) = patch.get("camera") {
            self.camera = serde_json::from_value(camera.clone())
                .map_err(|_| "camera must be off, gentle or full".to_string())?;
        }
        if let Some(pacing) = patch.get("pacing") {
            self.pacing = serde_json::from_value(pacing.clone())
                .map_err(|_| "pacing must be fast, normal or slow".to_string())?;
        }
        Ok(())
    }
}

pub struct AppState {
    /// When the server started; reported by `canvas_status`.
    pub started: Instant,
    /// The document mirror.
    pub doc: Mutex<Document>,
    /// Where the mirror is persisted; `None` keeps it in memory only.
    pub doc_path: Option<PathBuf>,
    /// The served app directory (fonts for headless rendering live here).
    pub web_dir: PathBuf,
    /// Whether pages exchange documents with the mirror. Tests that only
    /// serve the app turn this off; the agent always runs with it on.
    pub sync: bool,
    /// Connected pages.
    pub pages: AtomicUsize,
    /// Connection ids, so a patch broadcast can skip its origin page.
    next_page: AtomicU64,
    /// Server-to-page messages (JSON text over the WebSocket).
    pub to_pages: broadcast::Sender<PageMessage>,
    /// A persist is queued; bursts coalesce into one write.
    persist_pending: AtomicBool,
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
    /// The user's live agent preferences.
    pub settings: Mutex<AgentSettings>,
    /// Where settings persist; follows the document's directory.
    settings_path: Option<PathBuf>,
}

impl AppState {
    pub fn new(web_dir: PathBuf, doc_path: Option<PathBuf>, sync: bool) -> Self {
        let (to_pages, _) = broadcast::channel(64);
        let (present_acks, _) = broadcast::channel(16);
        let (screenshot_replies, _) = broadcast::channel(16);
        let settings_path = doc_path
            .as_ref()
            .map(|path| path.with_file_name("settings.json"));
        Self {
            started: Instant::now(),
            doc: Mutex::new(Document::new()),
            doc_path,
            web_dir,
            sync,
            pages: AtomicUsize::new(0),
            next_page: AtomicU64::new(0),
            to_pages,
            persist_pending: AtomicBool::new(false),
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
            settings: Mutex::new(AgentSettings::default()),
            settings_path,
        }
    }

    /// Load the persisted mirror, if any. Called once at startup.
    pub async fn load_document(&self) {
        let Some(path) = &self.doc_path else {
            return;
        };
        let Ok(json) = tokio::fs::read_to_string(path).await else {
            return;
        };
        match serialization::from_json(&json) {
            Ok(doc) => {
                let elements = doc.live_count();
                *self.doc.lock().await = doc;
                tracing::info!(elements, path = %path.display(), "restored document");
            }
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "ignoring unreadable document");
            }
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

    /// Apply a settings patch from the canvas, persist it and tell every page.
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
        let _ = self.to_pages.send(PageMessage::all(
            serde_json::json!({ "type": "settings", "settings": settings }).to_string(),
        ));
        Ok(settings)
    }

    /// The mirror as compact JSON.
    pub async fn document_json(&self) -> String {
        let doc = self.doc.lock().await;
        serialization::to_json(&doc).unwrap_or_default()
    }

    /// The next connection id.
    pub fn new_page_id(&self) -> u64 {
        self.next_page.fetch_add(1, Ordering::SeqCst) + 1
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
    pub async fn absorb_document(&self, remote: &Document) -> bool {
        let before = {
            let mut doc = self.doc.lock().await;
            let before = doc.clone();
            let merged = canvas_core::merge::reconcile(&doc, remote);
            if merged == *doc {
                return false;
            }
            *doc = merged;
            before
        };
        let doc = self.doc.lock().await;
        let (revision, _) = self.record_change(&before, &doc, "page").await;
        drop(doc);
        self.broadcast_snapshot(revision).await;
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

    /// Broadcast the whole mirror (connect/resync path) and queue the persist.
    pub async fn broadcast_snapshot(&self, revision: u64) {
        let frame = self.snapshot_json(revision).await;
        let _ = self.to_pages.send(PageMessage::all(frame));
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
        let revision = self.record_ids(changed, source).await;
        (revision, put)
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

    /// Queue a persist; bursts within `PERSIST_DEBOUNCE` coalesce into one
    /// write. The loop runs from `main` (and in tests that need it).
    pub async fn schedule_persist(&self) {
        self.persist_pending.store(true, Ordering::SeqCst);
        self.persist_notify.notify_one();
    }

    /// The debounced persist loop. Call once at startup.
    pub async fn persist_loop(self: std::sync::Arc<Self>) {
        loop {
            self.persist_notify.notified().await;
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
            *self.doc.lock().await = Document::new();
            *self.settings.lock().await = AgentSettings::default();
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
        let Some(path) = &self.doc_path else {
            return;
        };
        let json = self.document_json().await;
        if let Err(error) = write_atomic(path, &json).await {
            tracing::warn!(%error, path = %path.display(), "could not persist document");
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

    #[tokio::test]
    async fn persistence_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("rs-canvas-state-{}", std::process::id()));
        let path = dir.join("document.json");
        let state = AppState::new(PathBuf::from("."), Some(path.clone()), true);
        {
            let mut doc = state.doc.lock().await;
            doc.add(
                canvas_core::model::ElementKind::Shape {
                    shape: canvas_core::model::ShapeKind::Rectangle,
                    rect: canvas_core::geometry::Rect::new(10.0, 20.0, 30.0, 40.0),
                    style: canvas_core::model::Style::default(),
                    bound_text_id: None,
                },
                1,
            );
        }
        state.persist_document().await;

        let restored = AppState::new(PathBuf::from("."), Some(path), true);
        restored.load_document().await;
        assert_eq!(restored.doc.lock().await.live_count(), 1);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn unreadable_documents_are_ignored() {
        let dir = std::env::temp_dir().join(format!("rs-canvas-bad-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("document.json");
        tokio::fs::write(&path, "{not json").await.unwrap();

        let state = AppState::new(PathBuf::from("."), Some(path), true);
        state.load_document().await;
        assert_eq!(state.doc.lock().await.live_count(), 0);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn in_memory_mirrors_clear_when_the_last_page_leaves() {
        let state = AppState::new(PathBuf::from("."), None, true);
        state.doc.lock().await.add(
            canvas_core::model::ElementKind::Shape {
                shape: canvas_core::model::ShapeKind::Rectangle,
                rect: canvas_core::geometry::Rect::new(0.0, 0.0, 10.0, 10.0),
                style: canvas_core::model::Style::default(),
                bound_text_id: None,
            },
            1,
        );

        state.release_when_idle(1).await;
        assert_eq!(state.doc.lock().await.live_count(), 1, "a page is watching");
        state.release_when_idle(0).await;
        assert_eq!(state.doc.lock().await.live_count(), 0, "no page, no mirror");
    }

    #[tokio::test]
    async fn persisted_mirrors_survive_idle() {
        let dir = std::env::temp_dir().join(format!("rs-canvas-idle-{}", std::process::id()));
        let state = AppState::new(PathBuf::from("."), Some(dir.join("document.json")), true);
        state.doc.lock().await.add(
            canvas_core::model::ElementKind::Shape {
                shape: canvas_core::model::ShapeKind::Rectangle,
                rect: canvas_core::geometry::Rect::new(0.0, 0.0, 10.0, 10.0),
                style: canvas_core::model::Style::default(),
                bound_text_id: None,
            },
            1,
        );
        state.release_when_idle(0).await;
        assert_eq!(state.doc.lock().await.live_count(), 1);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    fn shape_at(x: f64) -> canvas_core::model::ElementKind {
        canvas_core::model::ElementKind::Shape {
            shape: canvas_core::model::ShapeKind::Rectangle,
            rect: canvas_core::geometry::Rect::new(x, 0.0, 10.0, 10.0),
            style: canvas_core::model::Style::default(),
            bound_text_id: None,
        }
    }

    #[tokio::test]
    async fn patches_merge_and_broadcast_to_everyone_but_the_origin() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let mut from_server = state.to_pages.subscribe();
        let element = Element::new(shape_at(0.0), "a0".into(), 1).with_id("one");

        assert!(state.absorb_patch(vec![element.clone()], 7).await);
        let message = from_server.recv().await.unwrap();
        assert_eq!(message.except, Some(7), "the origin page is skipped");
        let frame: serde_json::Value = serde_json::from_str(&message.text).unwrap();
        assert_eq!(frame["type"], "patch");
        assert_eq!(frame["put"].as_array().unwrap().len(), 1);
        assert_eq!(state.revision.load(Ordering::SeqCst), 1);

        assert!(
            !state.absorb_patch(vec![element], 7).await,
            "replaying a patch is a no-op"
        );
        assert_eq!(state.revision.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_stale_patch_cannot_clobber_a_newer_mirror() {
        let state = AppState::new(PathBuf::from("."), None, true);
        let mut newer = Element::new(shape_at(5.0), "a0".into(), 1).with_id("one");
        newer.version = 4;
        state.absorb_patch(vec![newer], 1).await;

        let mut from_server = state.to_pages.subscribe();
        let older = Element::new(shape_at(0.0), "a0".into(), 1).with_id("one");
        assert!(!state.absorb_patch(vec![older], 1).await);
        assert_eq!(
            state.doc.lock().await.element("one").unwrap().kind,
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
        state.doc.lock().await.add(shape_at(0.0), 1);
        let frame: serde_json::Value = serde_json::from_str(&state.snapshot_frame().await).unwrap();
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
