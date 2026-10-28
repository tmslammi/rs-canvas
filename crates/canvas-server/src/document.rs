//! The server-side document mirror.
//!
//! `DocumentMirror` owns everything that is scoped to one workspace's document
//! lifetime: the merged `Document`, its monotonically increasing revision, the
//! bounded change log behind `canvas_read(since)`, the agent-facing element
//! handles, and the agent transaction/revert journal. It never talks to pages
//! or to disk; `Workspace` composes those steps around it.
//!
//! Invariants:
//! - The revision only advances when a change is actually recorded. A merge
//!   that produces no winners, or a correction-only reconnect, leaves it alone.
//! - The change log is append-only and bounded; a cursor older than the log's
//!   first entry is invalid, not silently truncated.
//! - Aliases are assigned on first sight and never reused, even after the
//!   element is gone, so an old agent handle still resolves meaningfully.
//! - The transaction journal is bounded to the last 50 committed agent edits;
//!   revert replays the recorded deltas instead of retaining snapshots.
//!
//! Lock order when more than one is needed: document, then change log, then
//! aliases, then transaction journal. Callers that already hold the document
//! lock must use the reference-based recording methods.

use std::sync::atomic::{AtomicU64, Ordering};

use canvas_core::model::{Document, Element, ElementKind, Style};
use canvas_core::{serialization, Delta};
use tokio::sync::Mutex;

use crate::aliases::Aliases;

/// One committed agent edit, kept for `canvas_revert`.
#[derive(Clone)]
pub struct AgentTransaction {
    pub id: String,
    /// The exact committed journal. Revert replays its inverse instead of
    /// retaining full before/after snapshots of every touched element.
    pub deltas: Vec<Delta>,
}

/// How many recent changes `canvas_read(since)` can recall.
pub(crate) const CHANGE_LOG_LIMIT: usize = 200;

/// How many agent transactions `canvas_revert` can reach.
const TRANSACTION_LIMIT: usize = 50;

/// What kind of change one element underwent, as far as a before/after
/// comparison can tell. One element may carry several kinds (a move that
/// also resizes and relabels, for example).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangeKind {
    Added,
    Removed,
    Moved,
    Resized,
    Relabelled,
    Restyled,
    Reparented,
    Reordered,
    Other,
}

impl ChangeKind {
    /// Reporting order for revision summaries and per-element lists.
    pub const ALL: [ChangeKind; 9] = [
        ChangeKind::Added,
        ChangeKind::Removed,
        ChangeKind::Moved,
        ChangeKind::Resized,
        ChangeKind::Relabelled,
        ChangeKind::Restyled,
        ChangeKind::Reparented,
        ChangeKind::Reordered,
        ChangeKind::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ChangeKind::Added => "added",
            ChangeKind::Removed => "removed",
            ChangeKind::Moved => "moved",
            ChangeKind::Resized => "resized",
            ChangeKind::Relabelled => "relabelled",
            ChangeKind::Restyled => "restyled",
            ChangeKind::Reparented => "reparented",
            ChangeKind::Reordered => "reordered",
            ChangeKind::Other => "other",
        }
    }
}

/// One element in a change record: its id and every kind detected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedElement {
    pub id: String,
    pub kinds: Vec<ChangeKind>,
}

/// One recorded document change, for `canvas_read(since)`.
#[derive(Clone, Debug)]
pub struct ChangeRecord {
    pub revision: u64,
    pub source: String,
    pub changed: Vec<ChangedElement>,
}

impl ChangeRecord {
    /// Per-kind element counts for this revision, in reporting order.
    pub fn kind_counts(&self) -> Vec<(ChangeKind, usize)> {
        ChangeKind::ALL
            .iter()
            .filter_map(|kind| {
                let count = self
                    .changed
                    .iter()
                    .filter(|change| change.kinds.contains(kind))
                    .count();
                (count > 0).then_some((*kind, count))
            })
            .collect()
    }
}

/// The authoritative merged document plus the bookkeeping that belongs to its
/// lifetime.
pub struct DocumentMirror {
    doc: Mutex<Document>,
    revision: AtomicU64,
    change_log: Mutex<Vec<ChangeRecord>>,
    aliases: Mutex<Aliases>,
    transactions: Mutex<Vec<AgentTransaction>>,
}

impl DocumentMirror {
    pub fn new() -> Self {
        Self {
            doc: Mutex::new(Document::new()),
            revision: AtomicU64::new(0),
            change_log: Mutex::new(Vec::new()),
            aliases: Mutex::new(Aliases::default()),
            transactions: Mutex::new(Vec::new()),
        }
    }

    /// The mirror document. Callers must hold this guard for any read/write
    /// that is meant to be atomic with the surrounding merge or edit.
    pub fn doc(&self) -> &Mutex<Document> {
        &self.doc
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    /// Test-only direct revision seeding, for frame encodings that only care
    /// about the number carried on the wire.
    #[cfg(test)]
    pub fn set_revision(&self, revision: u64) {
        self.revision.store(revision, Ordering::SeqCst);
    }

    /// The element handles the agent sees.
    pub fn aliases(&self) -> &Mutex<Aliases> {
        &self.aliases
    }

    /// The mirror as compact JSON, for tests that compare frames.
    #[cfg(test)]
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

    /// The mirror as a binary snapshot wire frame. The document payload is the
    /// same binary envelope persistence uses.
    pub async fn snapshot_frame_bytes(&self) -> Result<Vec<u8>, canvas_core::wire::WireError> {
        let revision = self.revision();
        let doc = self.doc.lock().await;
        canvas_core::wire::encode_snapshot(revision, &doc)
    }

    /// Replace the mirror document, used when a workspace is loaded from
    /// disk.
    pub async fn replace_document(&self, document: Document) {
        *self.doc.lock().await = document;
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
        let mut changed: Vec<ChangedElement> = Vec::new();
        for (id, after_element) in &after.elements {
            match before.elements.get(id) {
                Some(before_element) if before_element == after_element => {}
                before_element => changed.push(ChangedElement {
                    id: id.clone(),
                    kinds: classify_change(before_element, Some(after_element)),
                }),
            }
        }
        for (id, before_element) in &before.elements {
            if !after.elements.contains_key(id) {
                changed.push(ChangedElement {
                    id: id.clone(),
                    kinds: classify_change(Some(before_element), None),
                });
            }
        }
        let put: Vec<Element> = changed
            .iter()
            .filter_map(|change| after.element(&change.id).cloned())
            .collect();
        if changed.is_empty() {
            return (self.revision(), put);
        }
        let revision = self.record_ids(changed, source).await;
        (revision, put)
    }

    /// Record a committed delta journal: bump the revision and change log
    /// from the elements the commands actually touched. The classification is
    /// derived from the deltas' before/after copies, so delta-native commits
    /// keep the same rich `canvas_read(since)` report as page snapshots.
    pub async fn record_deltas(&self, deltas: &[Delta], source: &str) -> u64 {
        let mut changed: Vec<ChangedElement> = Vec::new();
        for delta in deltas {
            let (id, kinds) = match delta {
                Delta::Add { element } => {
                    (element.id.clone(), classify_change(None, Some(element)))
                }
                Delta::Purge { element } => {
                    (element.id.clone(), classify_change(Some(element), None))
                }
                Delta::Remove { before, tombstone } => {
                    let after = tombstone.materialize(before);
                    (
                        before.id.clone(),
                        classify_change(Some(before), Some(&after)),
                    )
                }
                Delta::Update { before, after } => {
                    (after.id.clone(), classify_change(Some(before), Some(after)))
                }
            };
            if let Some(existing) = changed.iter_mut().find(|entry| entry.id == id) {
                for kind in kinds {
                    if !existing.kinds.contains(&kind) {
                        existing.kinds.push(kind);
                    }
                }
            } else {
                changed.push(ChangedElement { id, kinds });
            }
        }
        self.record_ids(changed, source).await
    }

    /// Record an additions-only bulk commit: bump the revision and change log
    /// without per-element receipt work. Bulk boards skip the revert history
    /// (which would retain every element); wrap them in a frame to remove
    /// them later with one clear.
    pub async fn record_bulk(&self, changed: Vec<String>) -> u64 {
        let changed = changed
            .into_iter()
            .map(|id| ChangedElement {
                id,
                kinds: vec![ChangeKind::Added],
            })
            .collect();
        self.record_ids(changed, "agent").await
    }

    /// Record a change by the elements it touched and the kinds detected.
    pub(crate) async fn record_ids(&self, changed: Vec<ChangedElement>, source: &str) -> u64 {
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
        let revision = self.revision();
        let log = self.change_log.lock().await;
        since <= revision
            && log.first().map_or(since == revision, |first| {
                since >= first.revision.saturating_sub(1)
            })
    }

    /// Append a committed agent transaction, keeping only the recent window.
    pub async fn push_transaction(&self, transaction: AgentTransaction) {
        let mut history = self.transactions.lock().await;
        history.push(transaction);
        if history.len() > TRANSACTION_LIMIT {
            history.remove(0);
        }
    }

    /// The most recent committed agent transactions, newest first.
    pub async fn recent_transactions(&self, limit: usize) -> Vec<AgentTransaction> {
        self.transactions
            .lock()
            .await
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect()
    }

    /// A committed agent transaction by receipt id, if still in the window.
    pub async fn transaction(&self, id: &str) -> Option<AgentTransaction> {
        self.transactions
            .lock()
            .await
            .iter()
            .find(|transaction| transaction.id == id)
            .cloned()
    }

    /// Drop the transaction journal. Used when an unpersisted mirror is
    /// released, so a later page starts a fresh document lifetime.
    pub async fn clear_transactions(&self) {
        self.transactions.lock().await.clear();
    }

    /// Serialize the mirror for the persist path.
    pub async fn to_bytes(&self) -> Result<Vec<u8>, serialization::SchemaError> {
        let doc = self.doc.lock().await;
        serialization::to_bytes(&doc)
    }
}

impl Default for DocumentMirror {
    fn default() -> Self {
        Self::new()
    }
}

/// Classify one element from its before/after copies. Every changed id gets
/// at least one kind, so a change can never be silently dropped.
pub(crate) fn classify_change(
    before: Option<&Element>,
    after: Option<&Element>,
) -> Vec<ChangeKind> {
    let Some(after) = after else {
        return vec![ChangeKind::Removed];
    };
    let Some(before) = before else {
        return if after.is_deleted {
            vec![ChangeKind::Removed]
        } else {
            vec![ChangeKind::Added]
        };
    };
    if after.is_deleted {
        return if before.is_deleted {
            vec![ChangeKind::Other]
        } else {
            vec![ChangeKind::Removed]
        };
    }
    if before.is_deleted {
        return vec![ChangeKind::Added];
    }

    let mut kinds = Vec::new();
    let before_bounds = before.bounds();
    let after_bounds = after.bounds();
    if before_bounds.x != after_bounds.x || before_bounds.y != after_bounds.y {
        kinds.push(ChangeKind::Moved);
    }
    if before_bounds.w != after_bounds.w || before_bounds.h != after_bounds.h {
        kinds.push(ChangeKind::Resized);
    }
    if label_changed(before, after) {
        kinds.push(ChangeKind::Relabelled);
    }
    if style_of(before) != style_of(after) {
        kinds.push(ChangeKind::Restyled);
    }
    if parent_of(before) != parent_of(after) {
        kinds.push(ChangeKind::Reparented);
    }
    if before.index != after.index {
        kinds.push(ChangeKind::Reordered);
    }
    if other_change(before, after) {
        kinds.push(ChangeKind::Other);
    }
    if kinds.is_empty() {
        // Only merge metadata changed (version, nonce, timestamp).
        kinds.push(ChangeKind::Other);
    }
    kinds
}

fn label_changed(before: &Element, after: &Element) -> bool {
    match (&before.kind, &after.kind) {
        (ElementKind::Text { text: before, .. }, ElementKind::Text { text: after, .. })
        | (ElementKind::Frame { name: before, .. }, ElementKind::Frame { name: after, .. }) => {
            before != after
        }
        _ => false,
    }
}

fn style_of(element: &Element) -> Option<&Style> {
    match &element.kind {
        ElementKind::Shape { style, .. }
        | ElementKind::Freehand { style, .. }
        | ElementKind::Glyph { style, .. }
        | ElementKind::Arrow { style, .. }
        | ElementKind::Text { style, .. }
        | ElementKind::StickyStack { style, .. } => Some(style),
        ElementKind::Frame { .. } => None,
    }
}

/// Parent/owner membership: a frame, plus the container of a bound label.
fn parent_of(element: &Element) -> (Option<&str>, Option<&str>) {
    let container = match &element.kind {
        ElementKind::Text { container_id, .. } => container_id.as_deref(),
        _ => None,
    };
    (element.frame_id.as_deref(), container)
}

/// Properties outside bounds, label, style, membership and order. Geometry is
/// deliberately excluded here (endpoint positions move with an arrow, freehand
/// samples move with a stroke), so a plain move is not also reported as
/// `other`.
fn other_change(before: &Element, after: &Element) -> bool {
    match (&before.kind, &after.kind) {
        (
            ElementKind::Shape {
                shape: before_shape,
                bound_text_id: before_label,
                ..
            },
            ElementKind::Shape {
                shape: after_shape,
                bound_text_id: after_label,
                ..
            },
        ) => before_shape != after_shape || before_label != after_label,
        (
            ElementKind::Freehand {
                pressures: before, ..
            },
            ElementKind::Freehand {
                pressures: after, ..
            },
        ) => before != after,
        (
            ElementKind::Glyph {
                glyph: before_glyph,
                caption_id: before_caption,
                ..
            },
            ElementKind::Glyph {
                glyph: after_glyph,
                caption_id: after_caption,
                ..
            },
        ) => before_glyph != after_glyph || before_caption != after_caption,
        (
            ElementKind::Arrow {
                start: before_start,
                end: before_end,
                routing: before_routing,
                start_arrowhead: before_start_head,
                end_arrowhead: before_end_head,
                label_id: before_label,
                ..
            },
            ElementKind::Arrow {
                start: after_start,
                end: after_end,
                routing: after_routing,
                start_arrowhead: after_start_head,
                end_arrowhead: after_end_head,
                label_id: after_label,
                ..
            },
        ) => {
            before_routing != after_routing
                || before_start_head != after_start_head
                || before_end_head != after_end_head
                || before_label != after_label
                || before_start.binding != after_start.binding
                || before_end.binding != after_end.binding
        }
        (
            ElementKind::Text {
                align: before_align,
                fixed_width: before_fixed,
                ..
            },
            ElementKind::Text {
                align: after_align,
                fixed_width: after_fixed,
                ..
            },
        ) => before_align != after_align || before_fixed != after_fixed,
        (ElementKind::StickyStack { .. }, ElementKind::StickyStack { .. }) => false,
        (ElementKind::Frame { .. }, ElementKind::Frame { .. }) => false,
        _ => true,
    }
}
