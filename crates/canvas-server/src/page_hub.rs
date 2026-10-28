//! Page presence, broadcast, and transient page-facing state for one workspace.
//!
//! `PageHub` owns the connected-page count, the broadcast channel, the most
//! recent view hint, and the pending presentation/screenshot request state. It
//! is deliberately independent of the document mirror: callers hand it an
//! already-encoded binary patch or a JSON control string, so a broadcast can
//! never read the document behind the workspace's back.
//!
//! Invariants:
//! - `pages` counts a connection from admission until disconnect.
//! - A document broadcast carries one binary wire frame (ADR-0010); a control
//!   frame is a JSON text frame. There is exactly one encoding per message
//!   kind, so no page can receive a frame it cannot read.
//! - The origin page is skipped for unchanged winners, but corrections are
//!   broadcast to everyone.
//! - View hints and presentation/screenshot ids are transient; they are never
//!   persisted or merged into the document.
//! - Connection ids are allocated by `AppState` and only compared within one
//!   hub (`except`), so they stay global rather than becoming per-workspace
//!   state.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use canvas_core::model::Element;
use tokio::sync::{broadcast, Mutex, Notify};

/// Monotonic order of page activity across all workspaces.
static PAGE_ACTIVITY: AtomicU64 = AtomicU64::new(0);

/// One broadcast frame plus the page that must not receive it (the origin of
/// a patch never gets its own change echoed back).
///
/// A control frame is a JSON text frame. A document frame is a binary wire
/// frame (ADR-0010). There is exactly one encoding per message kind.
#[derive(Clone, Debug)]
pub enum PageMessage {
    Control(String),
    Document { bytes: Vec<u8>, except: Option<u64> },
}

impl PageMessage {
    pub fn control(text: String) -> Self {
        Self::Control(text)
    }

    pub fn document(bytes: Vec<u8>, except: Option<u64>) -> Self {
        Self::Document { bytes, except }
    }

    pub fn except(&self) -> Option<u64> {
        match self {
            PageMessage::Control(_) => None,
            PageMessage::Document { except, .. } => *except,
        }
    }
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

/// What applying one page frame did to the mirror.
#[derive(Clone, Debug, Default)]
pub struct PageFrameOutcome {
    /// The mirror changed.
    pub changed: bool,
    /// The mirror's revision after the frame.
    pub revision: u64,
    /// The page's base revision for snapshots; `None` for patches.
    pub base_revision: Option<u64>,
    /// Elements the page must apply to match the merged mirror: live elements
    /// its snapshot omitted, or winners its stale copies lost to.
    pub corrections: Vec<Element>,
    /// Incoming IDs whose copy won the merge.
    pub accepted: Vec<canvas_core::model::ElementId>,
}

/// One workspace's live pages and page-directed state.
pub struct PageHub {
    /// Connected pages on this workspace.
    pages: AtomicUsize,
    /// Server-to-page messages: JSON control frames or binary document frames.
    to_pages: broadcast::Sender<PageMessage>,
    /// Transient view state from the most recent page report.
    view_hint: Mutex<Option<ViewHint>>,
    /// Page acknowledgements of a finished presentation: (id, cancelled).
    present_acks: broadcast::Sender<(u64, bool)>,
    /// Presentation ids, so each timeline can be matched to its ack.
    present_counter: AtomicU64,
    /// True while a presentation is playing on some page.
    present_active: AtomicBool,
    /// Fired when a presentation finishes or is cancelled.
    present_finished: Notify,
    /// Page replies to a screenshot request: (id, data URL or None).
    screenshot_replies: broadcast::Sender<(u64, Option<String>)>,
    /// Screenshot request ids.
    screenshot_counter: AtomicU64,
    /// Monotonic order of the most recent page activity; drives
    /// current-workspace resolution without clock ties or clock changes.
    last_activity: AtomicU64,
}

impl PageHub {
    pub fn new() -> Self {
        let (to_pages, _) = broadcast::channel(64);
        let (present_acks, _) = broadcast::channel(16);
        let (screenshot_replies, _) = broadcast::channel(16);
        Self {
            pages: AtomicUsize::new(0),
            to_pages,
            view_hint: Mutex::new(None),
            present_acks,
            present_counter: AtomicU64::new(0),
            present_active: AtomicBool::new(false),
            present_finished: Notify::new(),
            screenshot_replies,
            screenshot_counter: AtomicU64::new(0),
            last_activity: AtomicU64::new(0),
        }
    }

    /// Number of pages currently connected to this workspace.
    pub fn count(&self) -> usize {
        self.pages.load(Ordering::SeqCst)
    }

    /// Admit one page. Retirement is checked by the caller before this call,
    /// so the count can never resurrect a retired workspace.
    pub fn connect(&self) {
        self.pages.fetch_add(1, Ordering::SeqCst);
    }

    /// Drop one page from the count.
    pub fn disconnect(&self) {
        self.pages.fetch_sub(1, Ordering::SeqCst);
    }

    /// The current page count, seeded directly in tests that only need the
    /// accounting state.
    #[cfg(test)]
    pub fn set_count(&self, pages: usize) {
        self.pages.store(pages, Ordering::SeqCst);
    }

    /// A page on this workspace did something; it may now be the current one.
    pub fn note_activity(&self) {
        let activity = PAGE_ACTIVITY.fetch_add(1, Ordering::SeqCst) + 1;
        self.last_activity.fetch_max(activity, Ordering::SeqCst);
    }

    /// The monotonic activity stamp used by current-workspace resolution.
    pub fn activity(&self) -> u64 {
        self.last_activity.load(Ordering::SeqCst)
    }

    /// Seed the activity stamp in tests that assert resolution ordering.
    #[cfg(test)]
    pub fn set_activity(&self, activity: u64) {
        self.last_activity.store(activity, Ordering::SeqCst);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PageMessage> {
        self.to_pages.subscribe()
    }

    /// Send a control frame (or a fully encoded document frame) to every page.
    pub fn broadcast(&self, message: PageMessage) {
        let _ = self.to_pages.send(message);
    }

    /// Broadcast a patch to every page but `except` as one binary frame.
    pub async fn broadcast_patch(&self, revision: u64, put: &[Element], except: Option<u64>) {
        if self.count() == 0 {
            return;
        }
        match canvas_core::wire::encode_patch(revision, put) {
            Ok(bytes) => self.broadcast(PageMessage::document(bytes, except)),
            Err(error) => {
                tracing::warn!(%error, "could not encode binary patch; dropping broadcast");
            }
        }
    }

    /// Broadcast merge winners and/or corrections. The origin is skipped when
    /// it already holds every winner; corrections always reach it.
    pub async fn broadcast_merge(
        &self,
        revision: u64,
        winners: &[Element],
        corrections: &[Element],
        origin: Option<u64>,
    ) {
        if winners.is_empty() && corrections.is_empty() {
            return;
        }
        if winners.is_empty() {
            self.broadcast_patch(revision, corrections, None).await;
        } else if corrections.is_empty() {
            self.broadcast_patch(revision, winners, origin).await;
        } else {
            let mut all = winners.to_vec();
            for correction in corrections {
                if !all.iter().any(|element| element.id == correction.id) {
                    all.push(correction.clone());
                }
            }
            self.broadcast_patch(revision, &all, None).await;
        }
    }

    /// The most recent view hint, cloned out for status reads.
    pub async fn view_hint(&self) -> Option<ViewHint> {
        self.view_hint.lock().await.clone()
    }

    /// Record a view hint from a page. Hints are last-writer-wins, transient
    /// state: an older tab may overwrite a newer one, exactly as before.
    pub async fn set_view_hint(&self, hint: ViewHint) {
        *self.view_hint.lock().await = Some(hint);
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

    /// Allocate the id for one presentation timeline.
    pub fn next_presentation_id(&self) -> u64 {
        self.present_counter.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn subscribe_present_acks(&self) -> broadcast::Receiver<(u64, bool)> {
        self.present_acks.subscribe()
    }

    /// Record a page's presentation acknowledgement.
    pub fn acknowledge_presentation(&self, id: u64, cancelled: bool) {
        let _ = self.present_acks.send((id, cancelled));
    }

    /// Allocate the id for one screenshot request.
    pub fn next_screenshot_id(&self) -> u64 {
        self.screenshot_counter.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn subscribe_screenshot_replies(&self) -> broadcast::Receiver<(u64, Option<String>)> {
        self.screenshot_replies.subscribe()
    }

    /// Record a page's screenshot reply; `None` means the page failed.
    pub fn reply_screenshot(&self, id: u64, data_url: Option<String>) {
        let _ = self.screenshot_replies.send((id, data_url));
    }
}

impl Default for PageHub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_core::geometry::Rect;
    use canvas_core::model::{ElementKind, ShapeKind, Style};

    fn element(id: &str) -> Element {
        Element::new(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(0.0, 0.0, 10.0, 10.0),
                style: Style::default(),
                bound_text_id: None,
            },
            "a0".into(),
            1,
        )
        .with_id(id)
    }

    #[tokio::test]
    async fn page_accounting_follows_connection_lifetime() {
        let hub = PageHub::new();
        assert_eq!(hub.count(), 0);
        hub.connect();
        hub.connect();
        assert_eq!(hub.count(), 2);
        hub.disconnect();
        assert_eq!(hub.count(), 1);
        hub.disconnect();
        assert_eq!(hub.count(), 0);
    }

    #[tokio::test]
    async fn document_broadcasts_are_binary_and_control_broadcasts_are_json() {
        let hub = PageHub::new();
        let mut receiver = hub.subscribe();
        let put = vec![element("one")];

        // No pages: nothing is encoded or sent.
        hub.broadcast_patch(1, &put, None).await;
        assert!(receiver.try_recv().is_err());

        hub.connect();
        hub.broadcast_patch(1, &put, Some(7)).await;
        let message = receiver.recv().await.unwrap();
        assert_eq!(message.except(), Some(7));
        let PageMessage::Document { bytes, .. } = message else {
            panic!("expected a document frame");
        };
        let (revision, decoded) = canvas_core::wire::decode_patch(&bytes).unwrap();
        assert_eq!(revision, 1);
        assert_eq!(decoded, put);

        hub.broadcast(PageMessage::control("{\"type\":\"synced\"}".into()));
        let message = receiver.recv().await.unwrap();
        assert_eq!(message.except(), None);
        assert!(matches!(message, PageMessage::Control(_)));
    }

    #[tokio::test]
    async fn corrections_reach_the_origin_and_winners_skip_it() {
        let hub = PageHub::new();
        hub.connect();
        let mut receiver = hub.subscribe();
        let winner = element("winner");
        let correction = element("correction");

        hub.broadcast_merge(1, std::slice::from_ref(&winner), &[], Some(4))
            .await;
        let message = receiver.recv().await.unwrap();
        assert_eq!(message.except(), Some(4));

        hub.broadcast_merge(2, std::slice::from_ref(&winner), &[correction], Some(4))
            .await;
        let message = receiver.recv().await.unwrap();
        assert_eq!(message.except(), None, "corrections go to every page");
    }
}
