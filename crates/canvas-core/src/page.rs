//! The page's browser-free sync and playback session.
//!
//! A page is more than an [`crate::Editor`]: between the editor's live
//! document and the server's binary frames sit the page's base revision, its
//! purge ledger, frames that arrived while a gesture was in flight, and the
//! outbound projection of committed deltas. Agent presentation pacing is also
//! page state — the document is final while a script plays; only the reveal
//! frontier and captions move. None of that needs a canvas, a clock or a DOM,
//! so it lives here and is unit-tested natively.
//!
//! [`PageSession`] deliberately does not own the live [`Document`]. The editor
//! remains the single owner of committed content (ADR-0013); the session takes
//! the editor as a parameter and applies through its public merge/patch entry
//! points. It owns the wire-facing metadata and the frames around it:
//!
//! - **Inbound**: [`PageSession::apply_snapshot`] and
//!   [`PageSession::apply_patch`] decode a frame, apply it and report the ids
//!   that changed. A frame that lands mid-gesture is deferred; deferred frames
//!   are drained by [`PageSession::apply_pending`] at gesture end, after the
//!   local edit has committed (ADR-0011).
//! - **Outbound**: [`PageSession::take_outbound_patch`] projects drained deltas
//!   into one patch and records purges in the ledger; the same deltas never
//!   produce a second frame. [`PageSession::snapshot_frame`] folds the ledger
//!   into a full snapshot so an absent element is never mistaken for a
//!   deletion.
//! - **Playback**: a presentation script is parsed, revealed step by step and
//!   finished/cancelled into an outcome queue. Camera and highlight cues are
//!   returned to the caller, which owns the actual camera and renderer.
//!
//! Screenshot capture stays in the browser adapter: it is a Canvas2D render of
//! the live viewport, not page session state. The server-side request ids live
//! in `canvas-server`'s `PageHub` (ADR-0012).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::delta::Delta;
use crate::editor::Editor;
use crate::merge::reconcile_document;
use crate::model::{Document, Element, ElementId};
use crate::patch::Patch;
use crate::sync::PurgeLedger;
use crate::wire::{self, WireError};

/// Default cap for focus zoom during a presentation. A focus beat on a small
/// element should not cause a dramatic jump.
pub const DEFAULT_PRESENT_ZOOM_CAP: f64 = 2.0;

fn default_zoom_cap() -> f64 {
    DEFAULT_PRESENT_ZOOM_CAP
}

/// One step of an agent presentation timeline.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct PresentStep {
    #[serde(default)]
    pub reveal: Vec<ElementId>,
    #[serde(default)]
    pub focus: Option<ElementId>,
    #[serde(default)]
    pub point: Option<ElementId>,
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub hold_ms: u64,
}

/// A parsed agent presentation script.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PresentScript {
    pub id: u64,
    pub steps: Vec<PresentStep>,
    #[serde(default = "default_zoom_cap")]
    pub zoom_cap: f64,
}

/// A finished or cancelled playback the shell must acknowledge.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct PresentOutcome {
    pub id: u64,
    pub cancelled: bool,
}

/// The camera/highlight cues for the step that just began. The caller applies
/// them through the editor; the session never touches a camera.
#[derive(Clone, Debug, PartialEq)]
pub struct PresentationCues {
    pub focus: Option<ElementId>,
    pub point: Option<ElementId>,
    pub zoom_cap: f64,
}

/// What [`PageSession::advance_presentation`] decided this frame.
#[derive(Clone, Debug, PartialEq)]
pub enum PresentationAdvance {
    /// No presentation is playing.
    Idle,
    /// The current step is still holding; the pump should keep running.
    Holding,
    /// The timeline moved to the next step. Apply these cues, then repaint.
    Step(PresentationCues),
    /// The last step's hold elapsed. The outcome is queued and the reveal set
    /// is gone; the caller repaints the final document.
    Finished,
}

/// Playback of an agent explanation: the document is already final; this is
/// the reveal frontier, caption and camera cue over it. View state only.
#[derive(Debug)]
struct Presentation {
    id: u64,
    steps: Vec<PresentStep>,
    index: usize,
    step_started_ms: u64,
    /// Elements not yet revealed.
    hidden: BTreeSet<ElementId>,
    caption: Option<String>,
    /// The server's zoom cap for focus beats.
    zoom_cap: f64,
}

/// A decoded server snapshot, ready to reconcile into the live document.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotFrame {
    pub revision: u64,
    pub doc: Document,
}

/// A decoded server patch, ready to merge element by element.
#[derive(Clone, Debug, PartialEq)]
pub struct PatchFrame {
    pub revision: u64,
    pub put: Vec<Element>,
}

/// The result of handing a frame to the session.
#[derive(Clone, Debug, PartialEq)]
pub enum Receive<T> {
    /// A gesture is in flight; the bytes are queued for the gesture's end.
    Deferred,
    /// Decoded and ready for the caller to apply.
    Ready(T),
}

/// A frame that was deferred during a gesture and is now ready.
#[derive(Clone, Debug, PartialEq)]
pub enum PendingFrame {
    Snapshot(SnapshotFrame),
    Patch(PatchFrame),
}

/// What applying a decoded frame did to the live editor.
#[derive(Clone, Debug, PartialEq)]
pub enum FrameApply {
    /// A gesture is in flight; the frame is queued for its end.
    Deferred,
    /// The frame applied, but the document did not change.
    Unchanged,
    /// The document changed; these ids need their layers repainted.
    Changed { ids: Vec<ElementId> },
}

impl FrameApply {
    pub fn changed(&self) -> bool {
        matches!(self, FrameApply::Changed { .. })
    }
}

/// The page's sync-facing state and agent playback state.
///
/// The live document stays with the editor. Every method that needs it takes
/// it as a parameter, which keeps this type independent of rendering, input
/// and the browser.
#[derive(Debug, Default)]
pub struct PageSession {
    /// The last server revision this page has seen (a binary frame header or
    /// the `synced` acknowledgement). Outbound snapshots carry it as the
    /// page's base revision; merging never depends on it.
    wire_revision: u64,
    /// Explicit deletions whose element entry was purged locally (undo of a
    /// creation). Folded into snapshots and autosaves; absence alone is never
    /// a deletion.
    purge_ledger: PurgeLedger,
    /// A server snapshot that arrived mid-gesture, applied on pointer-up.
    /// The latest snapshot wins; a patch queue follows it in order.
    pending_snapshot: Option<Vec<u8>>,
    /// Server patches that arrived mid-gesture, applied on pointer-up.
    pending_patches: Vec<Vec<u8>>,
    /// Agent explanation playback in flight, if any (view state only).
    presentation: Option<Presentation>,
    /// Finished/cancelled playbacks the shell must acknowledge.
    present_outcomes: Vec<PresentOutcome>,
}

impl PageSession {
    pub fn new() -> Self {
        Self::default()
    }

    // -- base revision -----------------------------------------------------

    pub fn wire_revision(&self) -> u64 {
        self.wire_revision
    }

    /// Record the server revision carried by the `synced` control frame.
    /// Binary document decoding updates the revision itself; the
    /// acknowledgement covers a snapshot that needed no correction, so the
    /// next snapshot uploads a truthful base.
    pub fn set_wire_revision(&mut self, revision: u64) {
        self.wire_revision = revision;
    }

    // -- inbound frames ----------------------------------------------------

    /// Decode a binary snapshot frame. When `interacting` is true the bytes are
    /// queued instead: replacing the document under an in-flight drag would
    /// corrupt the gesture. The base revision advances only once a frame is
    /// actually ready to apply.
    pub fn receive_snapshot(
        &mut self,
        bytes: &[u8],
        interacting: bool,
    ) -> Result<Receive<SnapshotFrame>, WireError> {
        if interacting {
            self.pending_snapshot = Some(bytes.to_vec());
            return Ok(Receive::Deferred);
        }
        let (revision, doc) = wire::decode_snapshot(bytes)?;
        self.wire_revision = revision;
        Ok(Receive::Ready(SnapshotFrame { revision, doc }))
    }

    /// Decode a binary patch frame, deferring mid-gesture exactly like a
    /// snapshot. Patches queue in arrival order.
    pub fn receive_patch(
        &mut self,
        bytes: &[u8],
        interacting: bool,
    ) -> Result<Receive<PatchFrame>, WireError> {
        if interacting {
            self.pending_patches.push(bytes.to_vec());
            return Ok(Receive::Deferred);
        }
        let (revision, put) = wire::decode_patch(bytes)?;
        self.wire_revision = revision;
        Ok(Receive::Ready(PatchFrame { revision, put }))
    }

    /// Decode and clear every frame deferred during a gesture, in the order
    /// they must be applied: the latest snapshot first, then patches in
    /// arrival order. A malformed deferred frame is dropped (it can no longer
    /// surface to the caller) and does not advance the revision.
    pub fn drain_pending(&mut self) -> Vec<PendingFrame> {
        let mut frames = Vec::new();
        if let Some(bytes) = self.pending_snapshot.take() {
            if let Ok((revision, doc)) = wire::decode_snapshot(&bytes) {
                self.wire_revision = revision;
                frames.push(PendingFrame::Snapshot(SnapshotFrame { revision, doc }));
            }
        }
        for bytes in std::mem::take(&mut self.pending_patches) {
            if let Ok((revision, put)) = wire::decode_patch(&bytes) {
                self.wire_revision = revision;
                frames.push(PendingFrame::Patch(PatchFrame { revision, put }));
            }
        }
        frames
    }

    /// Decode and apply a snapshot frame through the editor's merge entry
    /// point. Mid-gesture frames defer exactly like [`Self::receive_snapshot`].
    pub fn apply_snapshot(
        &mut self,
        editor: &mut Editor,
        bytes: &[u8],
    ) -> Result<FrameApply, WireError> {
        match self.receive_snapshot(bytes, editor.is_interacting())? {
            Receive::Deferred => Ok(FrameApply::Deferred),
            Receive::Ready(frame) => Ok(self.apply_snapshot_frame(editor, frame)),
        }
    }

    /// Decode and apply a patch frame through the editor's per-element merge
    /// entry point. Mid-gesture frames defer exactly like
    /// [`Self::receive_patch`].
    pub fn apply_patch(
        &mut self,
        editor: &mut Editor,
        bytes: &[u8],
    ) -> Result<FrameApply, WireError> {
        match self.receive_patch(bytes, editor.is_interacting())? {
            Receive::Deferred => Ok(FrameApply::Deferred),
            Receive::Ready(frame) => Ok(self.apply_patch_frame(editor, frame)),
        }
    }

    /// Apply one already-decoded snapshot: reconcile with the live document,
    /// install the merge result (keeping undo history and view state), and
    /// report the changed ids. The editor refreshes its scene here; the caller
    /// still owns the repaint.
    pub fn apply_snapshot_frame(&self, editor: &mut Editor, frame: SnapshotFrame) -> FrameApply {
        let outcome = reconcile_document(editor.document(), &frame.doc);
        let ids = outcome
            .changed
            .into_iter()
            .map(|change| change.after.id)
            .collect();
        if editor.merge_document(outcome.document) {
            FrameApply::Changed { ids }
        } else {
            FrameApply::Unchanged
        }
    }

    /// Apply one already-decoded patch through the editor's LWW merge path and
    /// report the ids whose layers need rebuilding.
    pub fn apply_patch_frame(&self, editor: &mut Editor, frame: PatchFrame) -> FrameApply {
        let changed = editor.apply_patch(&Patch { put: frame.put });
        if changed.is_empty() {
            FrameApply::Unchanged
        } else {
            FrameApply::Changed { ids: changed }
        }
    }

    /// Apply every frame deferred during a gesture, in order, after the local
    /// edit has committed. Each result describes what the caller must repaint.
    pub fn apply_pending(&mut self, editor: &mut Editor) -> Vec<FrameApply> {
        self.drain_pending()
            .into_iter()
            .map(|frame| match frame {
                PendingFrame::Snapshot(frame) => self.apply_snapshot_frame(editor, frame),
                PendingFrame::Patch(frame) => self.apply_patch_frame(editor, frame),
            })
            .collect()
    }

    // -- outbound frames ---------------------------------------------------

    /// Project drained deltas into one patch and record purges in the ledger
    /// with the exact tombstone this patch carries, so a later reconnect
    /// snapshot is byte-for-byte the same deletion. `None` when there is
    /// nothing to send; the caller's outbox is already drained.
    pub fn take_outbound_patch(&mut self, deltas: &[Delta], now_ms: u64) -> Option<Patch> {
        if deltas.is_empty() {
            return None;
        }
        let patch = Patch::from_deltas(deltas, now_ms);
        self.purge_ledger.record(deltas, &patch);
        Some(patch)
    }

    /// Drop drained deltas without projecting them; the reconnect snapshot
    /// carries the document instead. Purges still enter the ledger so the
    /// snapshot cannot forget an explicit deletion.
    pub fn discard_outbound(&mut self, deltas: &[Delta], now_ms: u64) {
        if deltas.is_empty() {
            return;
        }
        let projected = Patch::from_deltas(deltas, now_ms);
        self.purge_ledger.record(deltas, &projected);
    }

    /// Drain one outbound binary patch frame: header (the page's base
    /// revision), then the frozen v1 element list. Empty when the deltas
    /// produced no patch. Encoding failure yields empty bytes, matching the
    /// wire path's "nothing to send" case.
    pub fn take_outbound_frame(&mut self, deltas: &[Delta], now_ms: u64) -> Vec<u8> {
        let Some(patch) = self.take_outbound_patch(deltas, now_ms) else {
            return Vec::new();
        };
        wire::encode_patch(self.wire_revision, &patch.put).unwrap_or_default()
    }

    /// A full binary snapshot frame whose header carries the page's base
    /// revision and whose payload folds in the purge ledger.
    pub fn snapshot_frame(&self, live: &Document) -> Vec<u8> {
        let doc = self.synced_document(live);
        wire::encode_snapshot(self.wire_revision, &doc).unwrap_or_default()
    }

    /// The live document with explicit purge tombstones folded in. Absence in
    /// the live document is not a deletion; only the ledger adds one.
    pub fn synced_document(&self, live: &Document) -> Document {
        self.purge_ledger.materialize(live)
    }

    /// Forget explicit purges (opening a file or replacing the document).
    pub fn clear_purge_ledger(&mut self) {
        self.purge_ledger.clear();
    }

    // -- presentation ------------------------------------------------------

    /// Start an agent presentation timeline (JSON: `{id, steps:[{reveal,
    /// focus, point, caption, hold_ms}], zoom_cap}`). The document is already
    /// final; this stages the reveal, caption and camera cues over it.
    /// Returns the first step's cues, or `None` for an invalid/empty script
    /// (an existing playback is left untouched).
    pub fn start_presentation(&mut self, json: &str, now_ms: u64) -> Option<PresentationCues> {
        let script: PresentScript = serde_json::from_str(json).ok()?;
        if script.steps.is_empty() {
            return None;
        }
        self.cancel_presentation();
        let mut hidden: BTreeSet<ElementId> = BTreeSet::new();
        for step in &script.steps {
            hidden.extend(step.reveal.iter().cloned());
        }
        self.presentation = Some(Presentation {
            id: script.id,
            steps: script.steps,
            index: 0,
            step_started_ms: now_ms,
            hidden,
            caption: None,
            zoom_cap: script.zoom_cap,
        });
        Some(self.begin_current_step(now_ms))
    }

    /// Advance playback to `now_ms`: hold, move to the next step, or finish.
    /// The pump stays alive for the whole timeline; pure holds cost a frame
    /// tick, not a repaint.
    pub fn advance_presentation(&mut self, now_ms: u64) -> PresentationAdvance {
        let Some(presentation) = self.presentation.as_ref() else {
            return PresentationAdvance::Idle;
        };
        let Some(step) = presentation.steps.get(presentation.index) else {
            return PresentationAdvance::Idle;
        };
        if now_ms < presentation.step_started_ms.saturating_add(step.hold_ms) {
            return PresentationAdvance::Holding;
        }
        let last = presentation.index + 1 >= presentation.steps.len();
        if last {
            let id = presentation.id;
            self.presentation = None;
            self.present_outcomes.push(PresentOutcome {
                id,
                cancelled: false,
            });
            return PresentationAdvance::Finished;
        }
        if let Some(presentation) = self.presentation.as_mut() {
            presentation.index += 1;
        }
        PresentationAdvance::Step(self.begin_current_step(now_ms))
    }

    /// Any direct user input wins over the agent's explanation. Queues a
    /// cancelled outcome when something was playing.
    pub fn cancel_presentation(&mut self) -> Option<PresentOutcome> {
        let presentation = self.presentation.take()?;
        let outcome = PresentOutcome {
            id: presentation.id,
            cancelled: true,
        };
        self.present_outcomes.push(outcome.clone());
        Some(outcome)
    }

    /// Drain the finished/cancelled playbacks the shell must acknowledge.
    pub fn take_presentation_outcomes(&mut self) -> Vec<PresentOutcome> {
        std::mem::take(&mut self.present_outcomes)
    }

    pub fn is_presenting(&self) -> bool {
        self.presentation.is_some()
    }

    /// Elements the current playback has not revealed yet, borrowed for the
    /// render loop's per-layer skip check.
    pub fn hidden_elements(&self) -> Option<&BTreeSet<ElementId>> {
        self.presentation
            .as_ref()
            .map(|presentation| &presentation.hidden)
    }

    pub fn hidden_count(&self) -> usize {
        self.presentation
            .as_ref()
            .map_or(0, |presentation| presentation.hidden.len())
    }

    pub fn caption(&self) -> Option<&str> {
        self.presentation
            .as_ref()
            .and_then(|presentation| presentation.caption.as_deref())
    }

    /// Apply the current step: reveal its elements, set the caption and
    /// restart its hold. Returns the camera/highlight cues for the caller.
    fn begin_current_step(&mut self, now_ms: u64) -> PresentationCues {
        let Some(presentation) = self.presentation.as_mut() else {
            return PresentationCues {
                focus: None,
                point: None,
                zoom_cap: DEFAULT_PRESENT_ZOOM_CAP,
            };
        };
        let Some(step) = presentation.steps.get(presentation.index).cloned() else {
            return PresentationCues {
                focus: None,
                point: None,
                zoom_cap: presentation.zoom_cap,
            };
        };
        for id in &step.reveal {
            presentation.hidden.remove(id);
        }
        presentation.caption = step.caption;
        presentation.step_started_ms = now_ms;
        PresentationCues {
            focus: step.focus,
            point: step.point,
            zoom_cap: presentation.zoom_cap,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Editor;
    use crate::geometry::Rect;
    use crate::model::{ElementKind, ShapeKind, Style};

    fn element(id: &str, version: u64) -> Element {
        let mut element = Element::new(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(0.0, 0.0, 10.0, 10.0),
                style: Style::default(),
                bound_text_id: None,
            },
            "a0".into(),
            1,
        )
        .with_id(id);
        element.version = version;
        element
    }

    fn script() -> String {
        serde_json::json!({
            "id": 3,
            "steps": [
                {
                    "reveal": ["a"],
                    "focus": "a",
                    "point": "a",
                    "caption": "First",
                    "hold_ms": 100
                },
                {
                    "reveal": ["b"],
                    "caption": "Second",
                    "hold_ms": 50
                }
            ]
        })
        .to_string()
    }

    #[test]
    fn snapshot_applied_mid_gesture_defers_until_gesture_end() {
        let mut page = PageSession::new();
        let mut doc = Document::new();
        doc.insert(element("one", 1));
        let bytes = wire::encode_snapshot(5, &doc).unwrap();

        assert_eq!(
            page.receive_snapshot(&bytes, true).unwrap(),
            Receive::Deferred
        );
        assert_eq!(page.wire_revision(), 0, "deferred frames are not applied");

        let frames = page.drain_pending();
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            PendingFrame::Snapshot(frame) => {
                assert_eq!(frame.revision, 5);
                assert_eq!(frame.doc, doc);
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }
        assert_eq!(page.wire_revision(), 5);
        assert!(page.drain_pending().is_empty(), "frames drain once");
    }

    #[test]
    fn frame_api_applies_a_snapshot_and_reports_changes() {
        let mut page = PageSession::new();
        let mut editor = Editor::new(Document::new());
        let mut doc = Document::new();
        doc.insert(element("one", 1));
        let bytes = wire::encode_snapshot(5, &doc).unwrap();

        let first = page.apply_snapshot(&mut editor, &bytes).unwrap();
        assert!(first.changed());
        assert!(editor.document().element("one").is_some());
        assert_eq!(page.wire_revision(), 5);

        let again = page.apply_snapshot(&mut editor, &bytes).unwrap();
        assert_eq!(again, FrameApply::Unchanged);
    }

    #[test]
    fn frame_api_defers_a_snapshot_mid_gesture_and_applies_it_on_release() {
        let mut page = PageSession::new();
        let mut editor = Editor::new(Document::new());
        editor.set_tool(crate::tool::Tool::Rectangle);
        editor.pointer_down(
            crate::geometry::Point::new(10.0, 10.0),
            0,
            crate::editor::Modifiers::default(),
            1,
        );
        assert!(editor.is_interacting());

        let mut doc = Document::new();
        doc.insert(element("one", 1));
        let bytes = wire::encode_snapshot(5, &doc).unwrap();
        assert_eq!(
            page.apply_snapshot(&mut editor, &bytes).unwrap(),
            FrameApply::Deferred
        );
        assert!(
            editor.document().element("one").is_none(),
            "the document is untouched during the gesture"
        );
        assert_eq!(page.wire_revision(), 0);

        // Release commits the local gesture; only then may the remote frame
        // land, so the merge sees the final local state.
        editor.pointer_up(
            crate::geometry::Point::new(30.0, 30.0),
            crate::editor::Modifiers::default(),
            2,
        );
        let applied = page.apply_pending(&mut editor);
        assert_eq!(applied.len(), 1);
        assert!(applied[0].changed());
        assert!(editor.document().element("one").is_some());
        assert_eq!(page.wire_revision(), 5);
    }

    #[test]
    fn deferred_patches_apply_after_the_latest_snapshot_in_order() {
        let mut page = PageSession::new();
        let mut doc = Document::new();
        doc.insert(element("one", 1));
        let snapshot = wire::encode_snapshot(5, &doc).unwrap();
        let first = wire::encode_patch(6, &[element("two", 1)]).unwrap();
        let second = wire::encode_patch(7, &[element("three", 1)]).unwrap();

        assert_eq!(
            page.receive_snapshot(&snapshot, true).unwrap(),
            Receive::Deferred
        );
        assert_eq!(page.receive_patch(&first, true).unwrap(), Receive::Deferred);
        assert_eq!(
            page.receive_patch(&second, true).unwrap(),
            Receive::Deferred
        );
        assert_eq!(page.wire_revision(), 0);

        let frames = page.drain_pending();
        assert_eq!(frames.len(), 3);
        assert!(matches!(&frames[0], PendingFrame::Snapshot(frame) if frame.revision == 5));
        assert!(matches!(&frames[1], PendingFrame::Patch(frame) if frame.revision == 6));
        assert!(matches!(&frames[2], PendingFrame::Patch(frame) if frame.revision == 7));
        assert_eq!(page.wire_revision(), 7);
    }

    #[test]
    fn outbound_patches_drain_once() {
        let mut page = PageSession::new();
        page.set_wire_revision(4);
        let mut editor = Editor::new(Document::new());
        editor.insert_glyph(crate::glyphs::catalog()[0].id, 1);

        let first = page.take_outbound_frame(&editor.drain_outbox(), 1);
        assert!(!first.is_empty(), "the committed edit produced a patch");
        let (revision, put) = wire::decode_patch(&first).unwrap();
        assert_eq!(revision, 4, "the patch carries the page's base");
        assert!(!put.is_empty());

        let second = page.take_outbound_frame(&editor.drain_outbox(), 2);
        assert!(second.is_empty(), "the outbox was drained exactly once");
        assert!(page.take_outbound_patch(&[], 3).is_none());
    }

    #[test]
    fn purge_ledger_survives_snapshot_production() {
        let mut page = PageSession::new();
        page.set_wire_revision(3);
        let live = element("one", 1);
        let deltas = vec![Delta::Purge {
            element: live.clone(),
        }];
        let patch = page.take_outbound_patch(&deltas, 5).expect("a purge patch");
        assert!(patch.put[0].is_deleted);

        // The live document no longer has the entry; the snapshot still does.
        let empty = Document::new();
        let synced = page.synced_document(&empty);
        assert!(synced.element("one").expect("ledger tombstone").is_deleted);

        let bytes = page.snapshot_frame(&empty);
        let (revision, snapshot) = wire::decode_snapshot(&bytes).unwrap();
        assert_eq!(revision, 3);
        let tombstone = snapshot.element("one").expect("snapshot tombstone");
        assert!(tombstone.is_deleted);
        assert_eq!(tombstone.version, patch.put[0].version);

        page.clear_purge_ledger();
        assert!(!page.synced_document(&empty).elements.contains_key("one"));
    }

    #[test]
    fn base_revision_round_trips_through_frames() {
        let mut page = PageSession::new();
        page.set_wire_revision(7);
        let snapshot = page.snapshot_frame(&Document::new());
        assert_eq!(wire::parse(&snapshot).unwrap().revision, 7);

        let patch = wire::encode_patch(9, &[]).unwrap();
        match page.receive_patch(&patch, false).unwrap() {
            Receive::Ready(frame) => assert_eq!(frame.revision, 9),
            Receive::Deferred => panic!("an idle page applies immediately"),
        }
        assert_eq!(page.wire_revision(), 9);
        let snapshot = page.snapshot_frame(&Document::new());
        assert_eq!(wire::parse(&snapshot).unwrap().revision, 9);

        // A deferred patch does not move the base until it is applied.
        let deferred = wire::encode_patch(11, &[]).unwrap();
        assert_eq!(
            page.receive_patch(&deferred, true).unwrap(),
            Receive::Deferred
        );
        assert_eq!(page.wire_revision(), 9);
        page.drain_pending();
        assert_eq!(page.wire_revision(), 11);
    }

    #[test]
    fn presentation_plays_step_by_step_and_finishes() {
        let mut page = PageSession::new();
        let cues = page
            .start_presentation(&script(), 1_000)
            .expect("valid script");
        assert_eq!(cues.focus.as_deref(), Some("a"));
        assert_eq!(cues.point.as_deref(), Some("a"));
        assert_eq!(cues.zoom_cap, DEFAULT_PRESENT_ZOOM_CAP);
        assert_eq!(page.hidden_count(), 1, "the first step reveals a at once");
        assert_eq!(page.caption(), Some("First"));
        assert!(page.is_presenting());

        assert_eq!(
            page.advance_presentation(1_099),
            PresentationAdvance::Holding
        );
        let next = page.advance_presentation(1_100);
        match next {
            PresentationAdvance::Step(cues) => {
                assert_eq!(cues.focus, None);
                assert_eq!(cues.point, None);
            }
            other => panic!("expected the second step, got {other:?}"),
        }
        assert_eq!(page.hidden_count(), 0, "the second beat revealed b");
        assert_eq!(page.caption(), Some("Second"));

        assert_eq!(
            page.advance_presentation(1_149),
            PresentationAdvance::Holding
        );
        assert_eq!(
            page.advance_presentation(1_150),
            PresentationAdvance::Finished
        );
        assert!(!page.is_presenting());
        assert_eq!(page.hidden_count(), 0);
        assert_eq!(
            page.take_presentation_outcomes(),
            vec![PresentOutcome {
                id: 3,
                cancelled: false
            }]
        );
        assert!(page.take_presentation_outcomes().is_empty());
        assert_eq!(page.advance_presentation(2_000), PresentationAdvance::Idle);
    }

    #[test]
    fn cancelling_queues_one_cancelled_outcome_and_reveals_everything() {
        let mut page = PageSession::new();
        page.start_presentation(&script(), 0);
        assert_eq!(
            page.cancel_presentation(),
            Some(PresentOutcome {
                id: 3,
                cancelled: true
            })
        );
        assert!(!page.is_presenting());
        assert!(page.hidden_elements().is_none());
        assert_eq!(
            page.take_presentation_outcomes(),
            vec![PresentOutcome {
                id: 3,
                cancelled: true
            }]
        );
        assert_eq!(page.cancel_presentation(), None);
    }

    #[test]
    fn a_new_script_cancels_the_previous_playback() {
        let mut page = PageSession::new();
        page.start_presentation(&script(), 0);
        let replacement = serde_json::json!({
            "id": 4,
            "steps": [{ "caption": "Only", "hold_ms": 10 }]
        })
        .to_string();
        page.start_presentation(&replacement, 5)
            .expect("valid script");
        assert_eq!(
            page.take_presentation_outcomes(),
            vec![PresentOutcome {
                id: 3,
                cancelled: true
            }]
        );
        assert_eq!(page.caption(), Some("Only"));
    }

    #[test]
    fn invalid_or_empty_scripts_leave_playback_untouched() {
        let mut page = PageSession::new();
        assert!(page.start_presentation("not json", 0).is_none());
        assert!(page
            .start_presentation(r#"{"id":1,"steps":[]}"#, 0)
            .is_none());
        assert!(page.take_presentation_outcomes().is_empty());

        page.start_presentation(&script(), 0);
        assert!(page
            .start_presentation(r#"{"id":2,"steps":[]}"#, 5)
            .is_none());
        assert!(page.is_presenting(), "a rejected script keeps the old one");
        assert_eq!(page.caption(), Some("First"));
    }
}
