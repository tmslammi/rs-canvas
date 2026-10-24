//! The MCP surface: local agent clients drive the canvas through these tools.
//!
//! View tools (camera, highlights) are transient; they never touch the
//! document, history or storage. Drawing tools edit the mirrored document.
//! The server is created per MCP session but shares `AppState`. Every tool
//! call acts on the current workspace — the one the user's most recently
//! active page is viewing (ADR-0007) — except `canvas_workspace_create`,
//! which opens a new workspace and moves that page with it (ADR-0008).

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use canvas_core::camera::Camera;
use canvas_core::geometry::{Point, Rect};
use canvas_core::model::{Document, Element, ElementKind, ShapeKind};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};

use crate::doc_ops;
use crate::render;
use crate::script::{self, ScriptBeat};
use crate::state::{
    AppState, ChangeKind, ChangedElement, PageMessage, Workspace, WorkspaceManagementError,
};
use crate::verbs;

const INSTRUCTIONS: &str =
    "rs-canvas is a hand-drawn architecture canvas open in the user's browser. \
Use canvas_status to check that the local bridge and the page are up before acting. \
canvas_read shows the current drawing as compact lines, each starting with a short \
handle (e1, e2, ...); every tool accepts handles or raw ids. It is the primary view \
of the workspace; canvas_screenshot is only for rendered appearance or when the user \
asks to see it. canvas_draw creates one \
element and canvas_pen draws a freehand stroke with per-point pressure and ink style \
(width, colour, roughness, dash, opacity, fill/shading). \
Camera and presentation tools move the user's view; they never change the drawing. \
The user may keep several workspaces (named, isolated canvases); every tool acts on \
the one the user is currently viewing — canvas_status names it and lists the rest. \
canvas_workspace_create starts a new workspace and brings the user's page to it, so \
later calls act there. Give a new discussion its own session workspace unless the \
user is pointing at an existing drawing or workspace. \
Think together: sketch the smallest useful idea, explore one concrete tradeoff or \
failure case, and revise existing elements with canvas_script update beats. \
Read selected elements, ids or a frame rather than unrelated content. Reads return \
context and revision; pass them to scripts to guard against stale plans. \
Scripts return edit receipts with changed lines and a transaction for canvas_revert. \
canvas_status.discussion preserves constraints, decision reasons and open questions; \
canvas_remember replaces that note with a discussion revision precondition. \
canvas_glyphs discovers valid glyph ids. Camera preferences and user interruption \
govern presentation; cancelling playback does not revert the committed document.";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct CreateWorkspaceArgs {
    /// 1–40 lowercase letters, digits or dashes, starting with a letter or digit.
    name: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DrawArgs {
    /// rectangle, ellipse, diamond, sticky, frame, glyph or text.
    kind: String,
    /// Label text: bound to shapes and stickies, required for text.
    #[serde(default)]
    text: Option<String>,
    /// Frame name, for kind "frame".
    #[serde(default)]
    name: Option<String>,
    /// Glyph library id, for kind "glyph".
    #[serde(default)]
    glyph: Option<String>,
    /// Explicit left edge; omit for automatic placement.
    #[serde(default)]
    x: Option<f64>,
    /// Explicit top edge; omit for automatic placement.
    #[serde(default)]
    y: Option<f64>,
    #[serde(default)]
    w: Option<f64>,
    #[serde(default)]
    h: Option<f64>,
    /// Place beside this element id (with `side`).
    #[serde(default)]
    near: Option<String>,
    /// right (default), left, top, bottom or inside of `near`.
    #[serde(default)]
    side: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PenArgs {
    /// The stroke's path in world coordinates, in draw order (at least 2).
    points: Vec<PenPoint>,
    /// Default pressure for samples without their own, 0..1 (default 0.5).
    #[serde(default)]
    pressure: Option<f64>,
    /// Ink width in world px, 0.5..64 (default 2).
    #[serde(default)]
    stroke_width: Option<f64>,
    /// Ink colour, any CSS colour (default "#ece4d6").
    #[serde(default)]
    stroke_color: Option<String>,
    /// 0 = architect, 1 = artist, 2 = cartoonist (default 1).
    #[serde(default)]
    roughness: Option<u8>,
    /// solid (default), dashed or dotted.
    #[serde(default)]
    dash: Option<DashArg>,
    /// Ink opacity, 0.05..1 (default 1).
    #[serde(default)]
    opacity: Option<f64>,
    /// Shading colour for the closed path; omit or "transparent" for none.
    #[serde(default)]
    fill_color: Option<String>,
    /// hachure (default), crosshatch or solid.
    #[serde(default)]
    fill_style: Option<FillStyleArg>,
}

/// One world-space sample of a pen stroke.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PenPoint {
    x: f64,
    y: f64,
    /// Pressure at this sample, 0..1; overrides the stroke-level default.
    #[serde(default)]
    pressure: Option<f64>,
}

/// Stroke dash pattern as the agent names it.
#[derive(Clone, Copy, Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum DashArg {
    Solid,
    Dashed,
    Dotted,
}

impl From<DashArg> for canvas_core::model::StrokeDash {
    fn from(dash: DashArg) -> Self {
        match dash {
            DashArg::Solid => Self::Solid,
            DashArg::Dashed => Self::Dashed,
            DashArg::Dotted => Self::Dotted,
        }
    }
}

/// Fill treatment as the agent names it.
#[derive(Clone, Copy, Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum FillStyleArg {
    Hachure,
    CrossHatch,
    Solid,
}

impl From<FillStyleArg> for canvas_core::model::FillPattern {
    fn from(fill: FillStyleArg) -> Self {
        match fill {
            FillStyleArg::Hachure => Self::Hachure,
            FillStyleArg::CrossHatch => Self::CrossHatch,
            FillStyleArg::Solid => Self::Solid,
        }
    }
}

/// The ink style a pen call asked for, as a full `Style` with defaults.
fn pen_style(args: &PenArgs) -> canvas_core::model::Style {
    let mut style = canvas_core::model::Style::default();
    if let Some(color) = args
        .stroke_color
        .as_deref()
        .map(str::trim)
        .filter(|color| !color.is_empty())
    {
        style.stroke_color = color.to_string();
    }
    if let Some(color) = args
        .fill_color
        .as_deref()
        .map(str::trim)
        .filter(|color| !color.is_empty())
    {
        style.fill_color = color.to_string();
    }
    if let Some(fill_style) = args.fill_style {
        style.fill_style = fill_style.into();
    }
    if let Some(dash) = args.dash {
        style.dash = dash.into();
    }
    let patch = canvas_core::model::StylePatch {
        stroke_width: args.stroke_width,
        roughness: args.roughness,
        opacity: args.opacity,
        ..canvas_core::model::StylePatch::default()
    };
    patch.apply_to(&mut style);
    style
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ScriptArgs {
    /// Ordered beats; each beat is `{draw: {...}}` or `{connect: {...}}`.
    beats: Vec<ScriptBeat>,
    /// Context from canvas_status/read; asserts the current workspace, never routes it.
    #[serde(default)]
    context: Option<String>,
    /// Optional optimistic precondition for the observed document revision.
    #[serde(default)]
    revision: Option<u64>,
}

/// One bulk shape: `k` is r=rectangle, e=ellipse, d=diamond, s=sticky,
/// t=text, f=frame. `f` overrides the call's shared fill, `t` is the bound
/// label (text content for t, frame name for f).
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BulkShape {
    k: String,
    x: f64,
    y: f64,
    #[serde(default)]
    w: Option<f64>,
    #[serde(default)]
    h: Option<f64>,
    #[serde(default)]
    f: Option<String>,
    #[serde(default)]
    t: Option<String>,
}

/// One bulk connector: indices into this call's shapes, in order
/// (explicit shapes first, then grid nodes).
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BulkConnect {
    a: usize,
    b: usize,
}

/// A parametric grid: a whole board in tens of tokens. `count` nodes fill
/// row-major (`cols` defaults to a square); `kinds` cycles shape codes
/// (default "r"); `palette` cycles fills; `links` is none/right/down/both
/// (default right); `frame` draws an owning frame around the grid.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BulkGrid {
    count: usize,
    #[serde(default)]
    cols: Option<usize>,
    #[serde(default)]
    x0: Option<f64>,
    #[serde(default)]
    y0: Option<f64>,
    #[serde(default)]
    dx: Option<f64>,
    #[serde(default)]
    dy: Option<f64>,
    #[serde(default)]
    w: Option<f64>,
    #[serde(default)]
    h: Option<f64>,
    #[serde(default)]
    kinds: Option<String>,
    #[serde(default)]
    palette: Option<Vec<String>>,
    #[serde(default)]
    links: Option<String>,
    #[serde(default)]
    rough: Option<u8>,
    #[serde(default)]
    frame: Option<String>,
}

/// Bulk board construction: thousands of explicit shapes or one parametric
/// grid per call. See `canvas_bulk`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BulkArgs {
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    revision: Option<u64>,
    /// Shared fill for shapes without their own `f`.
    #[serde(default)]
    fill: Option<String>,
    /// Shared ink (stroke) colour.
    #[serde(default)]
    ink: Option<String>,
    /// Shared roughness 0-2.
    #[serde(default)]
    rough: Option<u8>,
    /// Owning frame name for the explicit shapes.
    #[serde(default)]
    frame: Option<String>,
    #[serde(default)]
    shapes: Vec<BulkShape>,
    #[serde(default)]
    connects: Vec<BulkConnect>,
    #[serde(default)]
    grid: Option<BulkGrid>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ScreenshotArgs {
    /// Live viewport scale, 0.1–4 (default 1); ignored when rendering headlessly.
    #[serde(default)]
    scale: Option<f64>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    /// Only report what changed after this revision (from an earlier read or
    /// canvas_status). Omit for the full scene graph.
    #[serde(default)]
    since: Option<u64>,
    /// Read only these handles/ids, including their owned labels.
    #[serde(default)]
    ids: Option<Vec<String>>,
    /// Read the user's current selection. Empty selection is reported explicitly.
    #[serde(default)]
    selected: bool,
    /// Read a frame and its descendants.
    #[serde(default)]
    frame: Option<String>,
    /// Case-insensitive label/name search; combines with an explicit scope.
    #[serde(default)]
    query: Option<String>,
    /// Include exact element JSON with stable ids for the matching elements.
    #[serde(default)]
    detail: bool,
    /// Maximum returned elements, 1..500; defaults to 100. Use offset to continue.
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
    /// Required with offset > 0 to reject pagination over a changed document.
    #[serde(default)]
    revision: Option<u64>,
    /// Context from the previous read, especially when using since or offset.
    #[serde(default)]
    context: Option<String>,
    /// Whole-document summary (counts by kind, bounds, top fills) in a few
    /// hundred tokens at any scale. Ignores scope and pagination.
    #[serde(default)]
    summary: bool,
    /// Read what the user is looking at: the view rect and zoom, selected
    /// handles, visible elements, elements just outside the view and an
    /// off-screen summary. Needs a page reporting view state; cannot be
    /// combined with ids, frame, query, selected or since.
    #[serde(default)]
    attention: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct RevertArgs {
    /// Transaction returned by a successful drawing tool; retained for 50 edits this run.
    transaction: String,
    context: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct RememberArgs {
    context: String,
    /// Discussion revision from canvas_status; protects against overwriting newer context.
    revision: u64,
    notes: crate::state::Discussion,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
struct GlyphArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    category: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AnnotateArgs {
    /// Element id to mark.
    id: String,
    /// cross, check or question.
    mark: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EmphasisArgs {
    /// Element ids to change.
    ids: Vec<String>,
    /// dim or emphasize.
    mode: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ElementIdsArgs {
    /// Element ids.
    ids: Vec<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct FocusArgs {
    /// Element id to frame; omit to fit the whole drawing.
    #[serde(default)]
    id: Option<String>,
    /// Screen-space padding around the target, in CSS px (default 64).
    #[serde(default)]
    pad: Option<f64>,
    /// Set false to jump instantly instead of gliding.
    #[serde(default)]
    animate: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PointArgs {
    /// Element id to highlight.
    id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ZoomArgs {
    /// One of: in, out, fit (whole drawing), reset (100%).
    mode: String,
}

/// Deliver a view command to the pages on the current workspace. View
/// commands are transient: they never touch the document, history or
/// storage. The user's camera settings are enforced here, so an agent that
/// ignores `canvas_status` still cannot take over the view.
async fn deliver_view(
    state: &AppState,
    workspace: &Workspace,
    mut message: serde_json::Value,
) -> Result<CallToolResult, McpError> {
    let action = message
        .get("action")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    let settings = state.current_settings().await;
    if matches!(action.as_str(), "focus" | "zoom") {
        if !settings.camera_moves() {
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                serde_json::json!({
                    "ok": false,
                    "moved": false,
                    "reason": "the user turned agent camera moves off in the canvas \
                               settings; use canvas_point or captions instead",
                })
                .to_string(),
            )]));
        }
        message["zoom_cap"] = settings.zoom_cap().into();
    }
    let pages = workspace.pages.load(Ordering::SeqCst);
    if pages == 0 {
        return Ok(CallToolResult::error(vec![ContentBlock::text(
            "No canvas page is connected. Ask the user to open http://localhost:5173 \
             (the dev server must be running).",
        )]));
    }
    let _ = workspace
        .to_pages
        .send(PageMessage::all(message.to_string()));
    Ok(CallToolResult::success(vec![ContentBlock::text(
        serde_json::json!({ "ok": true, "delivered_to_pages": pages }).to_string(),
    )]))
}

/// Wait for any page to report the presentation done (or cancelled). Falls
/// back to "not cancelled" when pages disappear or the timeline overruns.
async fn wait_for_present_ack(
    acks: &mut tokio::sync::broadcast::Receiver<(u64, bool)>,
    presentation_id: u64,
    total_hold_ms: u64,
) -> bool {
    let deadline = std::time::Duration::from_millis(total_hold_ms.saturating_add(10_000));
    let received = tokio::time::timeout(deadline, async {
        loop {
            match acks.recv().await {
                Ok((id, cancelled)) if id == presentation_id => return cancelled,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    })
    .await;
    received.unwrap_or(false)
}

/// Wait for a page's screenshot reply; `None` when the page reported failure.
async fn wait_for_screenshot(
    replies: &mut tokio::sync::broadcast::Receiver<(u64, Option<String>)>,
    screenshot_id: u64,
) -> Option<String> {
    loop {
        match replies.recv().await {
            Ok((id, data_url)) if id == screenshot_id => return data_url,
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

fn image_from_data_url(data_url: &str) -> Result<CallToolResult, McpError> {
    match data_url.strip_prefix("data:image/png;base64,") {
        Some(data) if !data.is_empty() => Ok(CallToolResult::success(vec![ContentBlock::image(
            data,
            "image/png",
        )])),
        _ => Err(McpError::internal_error(
            "the page returned an unexpected screenshot format",
            None,
        )),
    }
}

/// The `canvas_read(since)` report: change records plus the current state of
/// everything they touched.
fn describe_changes(
    doc: &canvas_core::model::Document,
    records: &[crate::state::ChangeRecord],
    since: u64,
    revision: u64,
    ids: &mut dyn canvas_core::describe::IdMap,
) -> String {
    if records.is_empty() {
        return format!("revision {revision}\nno changes since revision {since}");
    }
    let mut text = format!("revision {revision}\nchanges since revision {since}:");
    for record in records {
        text.push_str(&format!(
            "\n- rev {} ({}): {} changed",
            record.revision,
            record.source,
            record.changed.len()
        ));
        let counts: Vec<String> = record
            .kind_counts()
            .into_iter()
            .map(|(kind, count)| format!("{count} {}", kind.label()))
            .collect();
        if !counts.is_empty() {
            text.push_str(&format!(" ({})", counts.join(", ")));
        }
    }
    // One current-state line per id, with the union of the kinds seen for it
    // across the whole window.
    let mut changes: Vec<ChangedElement> = Vec::new();
    let mut positions: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for record in records {
        for change in &record.changed {
            match positions.get(&change.id) {
                Some(&position) => {
                    let existing = &mut changes[position];
                    for kind in &change.kinds {
                        if !existing.kinds.contains(kind) {
                            existing.kinds.push(*kind);
                        }
                    }
                }
                None => {
                    positions.insert(change.id.clone(), changes.len());
                    changes.push(change.clone());
                }
            }
        }
    }
    let mut live = Vec::new();
    let mut removed = Vec::new();
    for change in &mut changes {
        change.kinds.sort_unstable();
        change.kinds.dedup();
        let kinds = kind_list(&change.kinds);
        match doc
            .element(&change.id)
            .filter(|element| !element.is_deleted)
        {
            Some(element) => {
                let line = canvas_core::describe::describe_element_with(doc, element, ids);
                if kinds.is_empty() {
                    live.push(line);
                } else {
                    live.push(format!("{line} ({kinds})"));
                }
            }
            None => {
                let handle = ids.map(&change.id);
                if kinds.is_empty() {
                    removed.push(handle);
                } else {
                    removed.push(format!("{handle} ({kinds})"));
                }
            }
        }
    }
    if !live.is_empty() {
        text.push_str("\nchanged now:\n");
        for line in live {
            text.push_str(&line);
            text.push('\n');
        }
    }
    if !removed.is_empty() {
        text.push_str(&format!("removed: {}\n", removed.join(" ")));
    }
    text
}

fn kind_list(kinds: &[ChangeKind]) -> String {
    kinds
        .iter()
        .map(|kind| kind.label())
        .collect::<Vec<_>>()
        .join(", ")
}

/// How many visible and nearby element lines an attention read prints.
const ATTENTION_VISIBLE_CAP: usize = 100;
const ATTENTION_NEARBY_CAP: usize = 25;
const ATTENTION_NEARBY_MARGIN: f64 = 0.1;

#[derive(serde::Deserialize)]
struct HintCamera {
    x: f64,
    y: f64,
    zoom: f64,
}

#[derive(serde::Deserialize)]
struct HintViewport {
    w: f64,
    h: f64,
}

/// The camera and viewport from a page hint, or `None` when the hint is
/// missing, malformed or cannot produce a finite world view.
fn hint_view(hint: &crate::state::ViewHint) -> Option<(Camera, (f64, f64))> {
    let camera: HintCamera = serde_json::from_value(hint.camera.clone()?).ok()?;
    let viewport: HintViewport = serde_json::from_value(hint.viewport.clone()?).ok()?;
    if !camera.x.is_finite()
        || !camera.y.is_finite()
        || !camera.zoom.is_finite()
        || camera.zoom <= 0.0
        || !viewport.w.is_finite()
        || !viewport.h.is_finite()
        || viewport.w <= 0.0
        || viewport.h <= 0.0
    {
        return None;
    }
    let camera = Camera {
        scroll: Point::new(camera.x, camera.y),
        zoom: camera.zoom,
    };
    Some((camera, (viewport.w, viewport.h)))
}

fn attention_kind_name(element: &Element) -> &'static str {
    match &element.kind {
        ElementKind::Shape { shape, .. } => match shape {
            ShapeKind::Rectangle => "rect",
            ShapeKind::Ellipse => "ellipse",
            ShapeKind::Diamond => "diamond",
            ShapeKind::Sticky => "sticky",
        },
        ElementKind::Freehand { .. } => "stroke",
        ElementKind::Glyph { .. } => "glyph",
        ElementKind::Arrow { .. } => "arrow",
        ElementKind::Text { .. } => "text",
        ElementKind::StickyStack { .. } => "stack",
        ElementKind::Frame { .. } => "frame",
    }
}

/// The `canvas_read(attention)` report: the user's current world view, the
/// selected handles, visible and barely-off-screen element lines, and a
/// summary of everything outside the view. All names go through `ids`.
fn describe_attention(
    doc: &Document,
    hint: &crate::state::ViewHint,
    live_page: bool,
    revision: u64,
    ids: &mut dyn canvas_core::describe::IdMap,
) -> Option<String> {
    let (camera, viewport) = hint_view(hint)?;
    let view = camera.viewport(viewport.0, viewport.1);
    let margin_x = view.w * ATTENTION_NEARBY_MARGIN;
    let margin_y = view.h * ATTENTION_NEARBY_MARGIN;
    let around = Rect::new(
        view.x - margin_x,
        view.y - margin_y,
        view.w + 2.0 * margin_x,
        view.h + 2.0 * margin_y,
    );

    let live: Vec<&Element> = doc.ordered_live();
    let visible: Vec<&Element> = live
        .iter()
        .copied()
        .filter(|element| element.bounds().intersects(&view))
        .collect();
    let nearby: Vec<&Element> = live
        .iter()
        .copied()
        .filter(|element| {
            let bounds = element.bounds();
            !bounds.intersects(&view) && bounds.intersects(&around)
        })
        .collect();
    let offscreen: Vec<&Element> = live
        .iter()
        .copied()
        .filter(|element| !element.bounds().intersects(&view))
        .collect();

    let selected: Vec<String> = hint.selection.iter().map(|id| ids.map(id)).collect();
    let mut text = format!(
        "revision {revision}\nview {:.0},{:.0} {:.0}x{:.0} zoom={} page={}\n",
        view.x,
        view.y,
        view.w,
        view.h,
        camera.zoom,
        if live_page { "live" } else { "none" }
    );
    text.push_str(&format!(
        "selected {}\n",
        if selected.is_empty() {
            "none".to_string()
        } else {
            selected.join(" ")
        }
    ));
    text.push_str(&format!(
        "visible {} showing {}\n",
        visible.len(),
        visible.len().min(ATTENTION_VISIBLE_CAP)
    ));
    for element in visible.iter().take(ATTENTION_VISIBLE_CAP) {
        text.push_str(&canvas_core::describe::describe_element_with(
            doc, element, ids,
        ));
        text.push('\n');
    }
    text.push_str(&format!(
        "nearby {} showing {}\n",
        nearby.len(),
        nearby.len().min(ATTENTION_NEARBY_CAP)
    ));
    for element in nearby.iter().take(ATTENTION_NEARBY_CAP) {
        text.push_str(&canvas_core::describe::describe_element_with(
            doc, element, ids,
        ));
        text.push('\n');
    }
    if offscreen.is_empty() {
        text.push_str("offscreen 0\n");
    } else {
        let bounds = offscreen
            .iter()
            .map(|element| element.bounds())
            .reduce(|acc, rect| acc.union(&rect))
            .unwrap_or(Rect::ZERO);
        let mut kinds: std::collections::BTreeMap<&'static str, usize> =
            std::collections::BTreeMap::new();
        for element in &offscreen {
            *kinds.entry(attention_kind_name(element)).or_default() += 1;
        }
        let mut top: Vec<(&str, usize)> = kinds.into_iter().collect();
        top.sort_by_key(|(name, count)| (std::cmp::Reverse(*count), *name));
        let kinds: Vec<String> = top
            .into_iter()
            .take(6)
            .map(|(name, count)| format!("{name}:{count}"))
            .collect();
        text.push_str(&format!(
            "offscreen {} bounds={:.0},{:.0} {:.0}x{:.0} kinds={}\n",
            offscreen.len(),
            bounds.x,
            bounds.y,
            bounds.w,
            bounds.h,
            kinds.join(" ")
        ));
    }
    Some(text)
}

#[derive(Clone)]
pub struct CanvasServer {
    state: Arc<AppState>,
    /// Last workspace observed on this MCP session; guards writes that do not
    /// pass an explicit context token.
    observed: Arc<tokio::sync::Mutex<Option<String>>>,
}

impl CanvasServer {
    /// The workspace this call acts on: the one the user is looking at.
    async fn current(&self) -> Arc<Workspace> {
        self.state.current_workspace().await
    }

    /// Apply an edit to the workspace mirror. On success the change is
    /// recorded and broadcast as an element patch; on failure the document
    /// and the change log stay untouched. Returns the edit's own result.
    async fn apply_edit<T>(
        &self,
        workspace: &Workspace,
        source: &str,
        run: impl FnOnce(&mut Document) -> Result<T, String>,
    ) -> Result<T, String> {
        self.apply_transaction(workspace, source, None, None, run)
            .await
            .map(|(result, _)| result)
    }

    async fn apply_transaction<T>(
        &self,
        workspace: &Workspace,
        source: &str,
        context: Option<&str>,
        revision: Option<u64>,
        run: impl FnOnce(&mut Document) -> Result<T, String>,
    ) -> Result<(T, serde_json::Value), String> {
        let observed = self.observed.lock().await.clone();
        let expected = context.or(observed.as_deref());
        let current = self.current().await;
        if current.context() != workspace.context()
            || expected.is_some_and(|token| token != workspace.context())
        {
            return Err(
                "stale_context: the current workspace changed; read canvas_status before retrying"
                    .into(),
            );
        }
        let mut doc = workspace.doc.lock().await;
        if workspace.retired.load(Ordering::SeqCst) {
            return Err("This workspace was renamed or deleted. Reconnect the page.".into());
        }
        if revision.is_some_and(|rev| rev != workspace.revision.load(Ordering::SeqCst)) {
            return Err("stale_revision: read the affected elements before retrying".into());
        }
        let before = doc.clone();
        let mut working = before.clone();
        let result = run(&mut working)?;
        let (revision, put) = workspace.record_change(&before, &working, source).await;
        let mut aliases = workspace.aliases.lock().await;
        let changed: Vec<String> = put
            .iter()
            .filter(|e| !e.is_deleted)
            .map(|e| canvas_core::describe::describe_element_with(&working, e, &mut *aliases))
            .collect();
        let removed: Vec<String> = put
            .iter()
            .filter(|e| e.is_deleted)
            .map(|e| aliases.handle(&e.id))
            .collect();
        let transaction = if put.is_empty() {
            None
        } else {
            Some(format!("{}:{revision}", workspace.context()))
        };
        if let Some(id) = &transaction {
            let changes = put
                .iter()
                .map(|e| (before.element(&e.id).cloned(), e.clone()))
                .collect();
            let mut history = workspace.transactions.lock().await;
            history.push(crate::state::AgentTransaction {
                id: id.clone(),
                changes,
            });
            if history.len() > 50 {
                history.remove(0);
            }
        }
        *doc = working;
        drop(aliases);
        let receipt = serde_json::json!({
            "workspace": workspace.name, "context": workspace.context(), "revision": revision,
            "transaction": transaction, "changed": changed, "removed": removed,
        });
        // Publish while serializing edits so a later revision cannot overtake this one.
        workspace.publish_patch(revision, put).await;
        Ok((result, receipt))
    }

    /// The agent-facing identity of one element: its handle and compact line.
    async fn describe_one(&self, workspace: &Workspace, id: &str) -> serde_json::Value {
        let doc = workspace.doc.lock().await;
        let mut aliases = workspace.aliases.lock().await;
        let handle = aliases.handle(id);
        let line = doc
            .element(id)
            .map(|element| {
                canvas_core::describe::describe_element_with(&doc, element, &mut *aliases)
            })
            .unwrap_or_default();
        serde_json::json!({ "id": handle, "line": line })
    }
}

#[tool_router]
impl CanvasServer {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            observed: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    #[tool(
        description = "Report canvas server status: whether the bridge is reachable, \
                       which workspace is current (the one the user is viewing), how \
                       many browser pages are connected to it, the mirrored document's \
                       element count, and the other workspaces that exist."
    )]
    async fn canvas_status(&self) -> Result<CallToolResult, McpError> {
        let workspace = self.current().await;
        // Read the count and drop the lock: list_workspaces locks every
        // workspace's document, including this one.
        let (elements, revision) = {
            let doc = workspace.doc.lock().await;
            (doc.live_count(), workspace.revision.load(Ordering::SeqCst))
        };
        let hints = workspace.view_hint.lock().await.clone();
        let settings = self.state.current_settings().await;
        let workspaces = self.state.list_workspaces().await;
        let discussion = workspace.discussion.lock().await.clone();
        let recent_edits: Vec<_> = workspace
            .transactions
            .lock()
            .await
            .iter()
            .rev()
            .take(5)
            .map(|edit| serde_json::json!({"transaction":edit.id,"elements":edit.changes.len()}))
            .collect();
        *self.observed.lock().await = Some(workspace.context());
        let body = serde_json::json!({
            "server": "rs-canvas",
            "version": env!("CARGO_PKG_VERSION"),
            "workspace": workspace.name,
            "context": workspace.context(),
            "discussion": discussion,
            "discussion_persisted": workspace.doc_path.is_some(),
            "recent_edits": recent_edits,
            "pages_connected": workspace.pages.load(Ordering::SeqCst),
            "uptime_ms": self.state.started.elapsed().as_millis() as u64,
            "revision": revision,
            "document": {
                "elements": elements,
            },
            "settings": settings,
            "workspaces": workspaces,
            "hints": hints.map(|hint| serde_json::json!({
                "note": "transient view state from the most recent page; never persisted or merged",
                "camera": hint.camera,
                "viewport": hint.viewport,
                "selection": hint.selection,
            })),
        });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "Create a named workspace and open it for the user: the page \
                       showing the current workspace saves its drawing and switches to \
                       the new one, so later calls act there. Use it once at the start \
                       of a session that has no existing drawing to work on, then reuse \
                       that workspace for the session. Names are 1–40 lowercase \
                       letters, digits or dashes, starting with a letter or digit; the \
                       name must not exist. With no page connected the workspace is \
                       created but the agent stays on the current workspace until the \
                       user opens it. Waits briefly for the page to arrive and reports \
                       whether it did; read canvas_status before drawing if it did not."
    )]
    async fn canvas_workspace_create(
        &self,
        Parameters(args): Parameters<CreateWorkspaceArgs>,
    ) -> Result<CallToolResult, McpError> {
        let source = self.current().await;
        let workspace = match self.state.create_workspace(&args.name).await {
            Ok(workspace) => workspace,
            Err(WorkspaceManagementError::Invalid(message))
            | Err(WorkspaceManagementError::Conflict(message)) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(message)]));
            }
            Err(WorkspaceManagementError::NotFound) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "Could not create the workspace.",
                )]));
            }
            Err(WorkspaceManagementError::Io(error)) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "Could not create the workspace: {error}"
                ))]));
            }
        };
        let watching = source.pages.load(Ordering::SeqCst);
        let mut switched = false;
        if watching > 0 {
            let message = serde_json::json!({
                "type": "workspace_changed",
                "action": "switch",
                "workspace": workspace.name,
            })
            .to_string();
            let _ = source.to_pages.send(PageMessage::all(message));
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while tokio::time::Instant::now() < deadline {
                if workspace.pages.load(Ordering::SeqCst) > 0 {
                    switched = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            if switched {
                // This call put the user on the new workspace, so it is a
                // valid observation for subsequent context guards.
                *self.observed.lock().await = Some(workspace.context());
            }
        }
        let elements = workspace.doc.lock().await.live_count();
        let note = if switched {
            "The page is on the new workspace; later calls act there."
        } else if watching > 0 {
            "The workspace was created, but the page did not switch before the timeout; read canvas_status before drawing."
        } else {
            "No page is connected; the workspace exists but the agent still targets the current workspace."
        };
        let body = serde_json::json!({
            "workspace": workspace.name,
            "context": workspace.context(),
            "switched": switched,
            "pages_connected": workspace.pages.load(Ordering::SeqCst),
            "persisted": workspace.doc_path.is_some(),
            "document": { "elements": elements },
            "note": note,
        });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "Read the current drawing as compact lines: `handle kind x,y wxh`, \
                       then membership and labels. Every line starts with a short handle \
                       (e1, e2, ...) that all id-taking tools accept. Read it before \
                       drawing to learn handles and available space. Pass `since` with a \
                       revision from an earlier read or canvas_status to hear only what \
                       changed while you were away. Scope with selected, ids or frame; \
                       query searches labels/names. Defaults to 100 elements, capped at 500; \
                       offset pagination needs the first read's context and unchanged revision. \
                       detail adds exact element JSON, including full stroke points. \
                       summary returns whole-document counts by kind, bounds and top fills \
                       in a few hundred tokens at any scale: start there on large boards. \
                       attention reads what the user is looking at: the view rect and zoom, \
                       selected handles, visible elements, elements just outside the view \
                       and an off-screen summary; it needs a page reporting view state and \
                       cannot be combined with ids, frame, query, selected or since. \
                       This is the primary way to understand the workspace; a screenshot \
                       adds only rendered appearance."
    )]
    async fn canvas_read(
        &self,
        Parameters(args): Parameters<ReadArgs>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.current().await;
        let doc = workspace.doc.lock().await;
        let revision = workspace.revision.load(Ordering::SeqCst);
        let failure = |message: &str| Ok(CallToolResult::error(vec![ContentBlock::text(message)]));
        if args.attention {
            if args.ids.is_some() || args.frame.is_some() || args.query.is_some() || args.selected {
                return failure(
                    "attention cannot be combined with ids, frame, query or selected; it reads the current view as a whole",
                );
            }
            if args.since.is_some() {
                return failure(
                    "attention cannot be combined with since; read the current view or the change log, not both",
                );
            }
            let hint = workspace.view_hint.lock().await.clone();
            let live_page = workspace.pages.load(Ordering::SeqCst) > 0;
            let mut aliases = workspace.aliases.lock().await;
            let Some(text) = hint.as_ref().and_then(|hint| {
                describe_attention(&doc, hint, live_page, revision, &mut *aliases)
            }) else {
                return failure(
                    "no_view: no page is reporting view state; connect a page or read without attention",
                );
            };
            *self.observed.lock().await = Some(workspace.context());
            return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "{text}\nworkspace {:?} context {:?}",
                workspace.name,
                workspace.context()
            ))]));
        }
        if args.summary {
            let mut body = canvas_core::bulk::summarize(&doc);
            body["revision"] = serde_json::json!(revision);
            body["context"] = serde_json::json!(workspace.context());
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                body.to_string(),
            )]));
        }
        if args.since.is_some()
            && args.context.is_none()
            && self
                .observed
                .lock()
                .await
                .as_deref()
                .is_some_and(|context| context != workspace.context())
        {
            return failure("stale_context: revision cursor belongs to another document; read canvas_status and start a new read");
        }
        if args
            .context
            .as_deref()
            .is_some_and(|context| context != workspace.context())
        {
            return failure("stale_context: read canvas_status and start a new read");
        }
        let scoped = args.ids.is_some()
            || args.selected
            || args.frame.is_some()
            || args.query.is_some()
            || args.detail;
        if args.since.is_some() && (scoped || args.offset.unwrap_or(0) > 0) {
            return failure("since cannot be combined with a scope, detail or pagination");
        }
        if args.offset.unwrap_or(0) > 0
            && (args.context.is_none() || args.revision != Some(revision))
        {
            return failure("resync_required: pagination needs context and the unchanged revision from the first read");
        }
        if args.revision.is_some_and(|rev| rev != revision) {
            return failure("resync_required: document revision changed; restart the scoped read");
        }
        if args.since.is_some_and(|since| since > revision) {
            return failure(
                "resync_required: cursor is newer than this document; start a full or scoped read",
            );
        }
        let mut aliases = workspace.aliases.lock().await;
        let text = if let Some(since) = args.since {
            if !workspace.cursor_valid(since).await {
                return failure("resync_required: change history no longer covers that cursor; start a full or scoped read");
            }
            let records = workspace.changes_since(since).await;
            describe_changes(&doc, &records, since, revision, &mut *aliases)
        } else {
            let mut roots = std::collections::BTreeSet::new();
            let has_scope = args.ids.is_some() || args.selected || args.frame.is_some();
            if let Some(ids) = &args.ids {
                for id in ids {
                    let id = aliases.resolve(id);
                    if doc.element(&id).is_none_or(|e| e.is_deleted) {
                        return failure("unknown or deleted element in ids; read the relevant frame or selection again");
                    }
                    roots.insert(id);
                }
            }
            if args.selected {
                if let Some(hint) = workspace.view_hint.lock().await.as_ref() {
                    roots.extend(
                        hint.selection
                            .iter()
                            .filter(|id| doc.element(id).is_some_and(|e| !e.is_deleted))
                            .cloned(),
                    );
                }
            }
            if let Some(frame) = &args.frame {
                let frame = aliases.resolve(frame);
                if !doc
                    .element(&frame)
                    .is_some_and(|e| !e.is_deleted && e.is_frame())
                {
                    return failure(
                        "frame must name a live frame; read the document summary again",
                    );
                }
                roots.insert(frame);
            }
            // A scoped owner includes owned labels and frame descendants.
            loop {
                let count = roots.len();
                for element in doc.ordered_live() {
                    if element
                        .frame_id
                        .as_ref()
                        .is_some_and(|id| roots.contains(id))
                        || matches!(&element.kind, canvas_core::ElementKind::Text { container_id: Some(id), .. } if roots.contains(id))
                    {
                        roots.insert(element.id.clone());
                    }
                }
                if roots.len() == count {
                    break;
                }
            }
            let query = args.query.as_deref().map(str::to_lowercase);
            let matches: Vec<_> = doc
                .ordered_live()
                .into_iter()
                .filter(|e| !has_scope || roots.contains(&e.id))
                .filter(|e| {
                    query.as_ref().is_none_or(|q| {
                        let text = match &e.kind {
                            canvas_core::ElementKind::Text { text, .. } => text.clone(),
                            canvas_core::ElementKind::Frame { name, .. } => name.clone(),
                            _ => canvas_core::edit::owned_label(&doc, &e.id)
                                .and_then(|id| canvas_core::describe::text_of(&doc, &id))
                                .unwrap_or_default(),
                        };
                        text.to_lowercase().contains(q)
                    })
                })
                .collect();
            let limit = args.limit.unwrap_or(100).clamp(1, 500);
            let offset = args.offset.unwrap_or(0);
            let mut text = format!(
                "revision {revision}\n{} elements{}\n",
                matches.len(),
                if has_scope || query.is_some() {
                    " matched"
                } else {
                    ""
                }
            );
            for element in matches.iter().skip(offset).take(limit) {
                text.push_str(&canvas_core::describe::describe_element_with(
                    &doc,
                    element,
                    &mut *aliases,
                ));
                text.push('\n');
                if args.detail {
                    text.push_str(
                        &serde_json::to_string(element)
                            .map_err(|e| McpError::internal_error(e.to_string(), None))?,
                    );
                    text.push('\n');
                }
            }
            if offset.saturating_add(limit) < matches.len() {
                text.push_str(&format!(
                    "next offset={} revision={revision}\n",
                    offset.saturating_add(limit)
                ));
            }
            text
        };
        *self.observed.lock().await = Some(workspace.context());
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "{text}\nworkspace {:?} context {:?}",
            workspace.name,
            workspace.context()
        ))]))
    }

    #[tool(
        description = "Revert an agent transaction from this server run. Checks every touched element against the committed versions; refuses to overwrite later human edits. Creates fresh merge-safe edits, and keeps unrelated work. Returns a new transaction receipt."
    )]
    async fn canvas_revert(
        &self,
        Parameters(args): Parameters<RevertArgs>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.current().await;
        let transaction = workspace
            .transactions
            .lock()
            .await
            .iter()
            .find(|t| t.id == args.transaction)
            .cloned();
        let Some(transaction) = transaction else {
            return Ok(CallToolResult::error(vec![ContentBlock::text("unknown or expired transaction; only the last 50 agent edits in this document lifetime are reversible")]));
        };
        let outcome = self.apply_transaction(&workspace, "agent-revert", Some(&args.context), None, |doc| {
            let touched: std::collections::HashSet<_> = transaction.changes.iter().map(|(_, after)| after.id.clone()).collect();
            for (before, after) in &transaction.changes {
                if doc.element(&after.id) != Some(after) {
                    return Err(format!("revert_conflict: element {:?} changed after the experiment; read it and revise explicitly", after.id));
                }
                if before.is_none() {
                    for element in doc.ordered_live().into_iter().filter(|e| !touched.contains(&e.id)) {
                        let dependent = element.frame_id.as_deref() == Some(&after.id)
                            || matches!(&element.kind, canvas_core::ElementKind::Text { container_id: Some(owner), .. } if owner == &after.id)
                            || matches!(&element.kind, canvas_core::ElementKind::Arrow { start, end, .. } if [&start.binding, &end.binding].into_iter().flatten().any(|b| b.element_id == after.id));
                        if dependent { return Err("revert_conflict: later elements depend on this experiment; revise them explicitly first".into()); }
                    }
                }
            }
            for (before, after) in &transaction.changes {
                let mut restored = before.clone().unwrap_or_else(|| { let mut e = after.clone(); e.is_deleted = true; e });
                restored.version = after.version;
                restored.touch(now_ms());
                doc.apply(&canvas_core::Delta::Update { before: after.clone(), after: restored }).map_err(|e| e.to_string())?;
            }
            Ok(())
        }).await;
        match outcome {
            Ok((_, receipt)) => Ok(CallToolResult::success(vec![ContentBlock::text(
                receipt.to_string(),
            )])),
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "Save a small workspace discussion note: constraints, agreed decisions with reasons, and open questions. Replace the notes after merging current canvas_status.discussion. Pass its discussion revision and context; newer notes or a workspace switch cause an error. Notes persist beside a saved document, or stay in memory if there is no backing file."
    )]
    async fn canvas_remember(
        &self,
        Parameters(args): Parameters<RememberArgs>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.current().await;
        let _doc = workspace.doc.lock().await;
        if workspace.context() != args.context || self.current().await.context() != args.context {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "stale_context: read canvas_status before saving discussion notes",
            )]));
        }
        match workspace.remember(args.notes, args.revision).await {
            Ok(record) => {
                let body = serde_json::json!({
                    "type": "discussion", "workspace": workspace.name, "context": workspace.context(),
                    "discussion": record, "discussion_persisted": workspace.doc_path.is_some(),
                });
                let _ = workspace.to_pages.send(PageMessage::all(body.to_string()));
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    body.to_string(),
                )]))
            }
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "Discover glyph IDs, display names and categories. Optional case-insensitive query and exact category filter; use before drawing an unfamiliar cloud service."
    )]
    async fn canvas_glyphs(
        &self,
        Parameters(args): Parameters<GlyphArgs>,
    ) -> Result<CallToolResult, McpError> {
        let query = args.query.unwrap_or_default().to_lowercase();
        let entries: Vec<_> = canvas_core::glyph_catalog()
            .into_iter()
            .filter(|g| {
                args.category
                    .as_deref()
                    .is_none_or(|category| category == g.category)
                    && (g.id.to_lowercase().contains(&query)
                        || g.name.to_lowercase().contains(&query))
            })
            .collect();
        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string(&entries)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?,
        )]))
    }

    #[tool(
        description = "Draw one element on the user's canvas, immediately visible. \
                       kind: rectangle, ellipse, diamond, sticky, frame, glyph or text. \
                       Shapes and stickies take an optional `text` label; text needs one. \
                       Placement is automatic — pass `near` and `side` to put it beside \
                       an element, or x/y to place explicitly. Returns the new element's \
                       handle and compact line."
    )]
    async fn canvas_draw(
        &self,
        Parameters(args): Parameters<DrawArgs>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.current().await;
        let near = match args.near.as_deref() {
            Some(near) => Some(workspace.resolve_id(near).await),
            None => None,
        };
        let request = doc_ops::DrawRequest {
            kind: args.kind,
            text: args.text,
            name: args.name,
            glyph: args.glyph,
            x: args.x,
            y: args.y,
            w: args.w,
            h: args.h,
            near,
            side: args.side,
        };
        let created = self
            .apply_edit(&workspace, "agent", |doc| {
                doc_ops::draw(doc, &request, now_ms())
            })
            .await;
        match created {
            Ok(id) => {
                let elements = workspace.doc.lock().await.live_count();
                let mut body = self.describe_one(&workspace, &id).await;
                body["elements"] = serde_json::json!(elements);
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    body.to_string(),
                )]))
            }
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "Draw a freehand pen stroke through world-space `points` — an \
                       ordered path of {x, y, pressure} samples, in draw order (at \
                       least 2). This is the agent's pencil: the same hand-drawn \
                       brush as strokes drawn by hand, with the controls the brush \
                       needs for detail. Per-sample `pressure` (0..1, or a flat \
                       `pressure` default) tapers and thickens the stroke — vary it \
                       for hair, limbs and folds. Style fields set the ink: \
                       stroke_width (0.5–64), stroke_color, roughness (0–2), dash, \
                       opacity, and fill_color/fill_style to shade the closed path \
                       with hachure, crosshatch or solid. Returns the new stroke's id."
    )]
    async fn canvas_pen(
        &self,
        Parameters(args): Parameters<PenArgs>,
    ) -> Result<CallToolResult, McpError> {
        let default_pressure = args.pressure.unwrap_or(0.5);
        let points: Vec<Point> = args
            .points
            .iter()
            .map(|sample| Point::new(sample.x, sample.y))
            .collect();
        let pressures: Vec<f64> = args
            .points
            .iter()
            .map(|sample| sample.pressure.unwrap_or(default_pressure))
            .collect();
        let request = doc_ops::PenRequest {
            points,
            pressures,
            style: pen_style(&args),
        };
        let workspace = self.current().await;
        let result = self
            .apply_edit(&workspace, "agent", |doc| {
                doc_ops::pen(doc, &request, now_ms())
            })
            .await;
        match result {
            Ok(id) => {
                let elements = workspace.doc.lock().await.live_count();
                let mut body = self.describe_one(&workspace, &id).await;
                body["elements"] = serde_json::json!(elements);
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    body.to_string(),
                )]))
            }
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(description = "Build a small diagram in one call from ordered beats. \
                       Draw beats: {\"draw\": {\"as\": \"handle\", \"kind\": ..., \"text\": ..., \
                       \"name\": ..., \"glyph\": ..., \"near\": \"handle\", \"side\": ..., \
                       \"around\": [handles]}}. Connect beats: {\"connect\": {\"from\": ..., \
                       \"to\": ..., \"label\": ..., \"arrow\": true}}. Any beat may add \
                       presentation: \"caption\": text shown on the canvas, \"focus\": handle, \
                       \"point\": handle, \"hold\": milliseconds to pause. A script with any \
                       presentation field plays back beat by beat on the user's canvas — \
                       elements reveal in order with the camera following — and this call \
                       returns when playback finishes or the user takes over. Without \
                       presentation fields the whole script lands at once. Relative placement searches for free space; explicit coordinates are honored. \
                       Update beats: {\"update\": {\"id\": handle, \"text\": ..., \"x\": ..., \"y\": ..., \"w\": ..., \"h\": ..., \"frame\": ..., \"detach\": true, \"from\": ..., \"to\": ..., \"style\": {...}}}. \
                       Frame moves carry children; owned labels and connectors follow. \
                       Pass context from a read to guard against workspace switches. Nothing commits if any beat fails.")]
    async fn canvas_script(
        &self,
        Parameters(args): Parameters<ScriptArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Fail fast: per-beat indexing is O(n²), so huge beat lists hang
        // until the tool timeout instead of finishing. Bulk boards belong in
        // canvas_bulk (explicit shapes or a parametric grid).
        if args.beats.len() > 300 {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "too many beats ({} > 300): describe this board with canvas_bulk instead — \
                     explicit shapes/connects for custom layouts, or one grid for regular \
                     boards — then revise the result with small scripts",
                args.beats.len(),
            ))]));
        }
        let workspace = self.current().await;
        let beat_count = args.beats.len();
        let context = args.context;
        let revision = args.revision;
        let mut beats = args.beats;
        for beat in &mut beats {
            if let Some(update) = &mut beat.update {
                update.id = workspace.resolve_id(&update.id).await;
                if let Some(id) = update.frame.take() {
                    update.frame = Some(workspace.resolve_id(&id).await);
                }
                if let Some(id) = update.from.take() {
                    update.from = Some(workspace.resolve_id(&id).await);
                }
                if let Some(id) = update.to.take() {
                    update.to = Some(workspace.resolve_id(&id).await);
                }
            }
            if let Some(draw) = &mut beat.draw {
                if let Some(frame) = draw.frame.take() {
                    draw.frame = Some(workspace.resolve_id(&frame).await);
                }
                if let Some(near) = draw.near.take() {
                    draw.near = Some(workspace.resolve_id(&near).await);
                }
                if let Some(around) = &mut draw.around {
                    *around = workspace.resolve_ids(around).await;
                }
            }
            if let Some(connect) = &mut beat.connect {
                connect.from = workspace.resolve_id(&connect.from).await;
                connect.to = workspace.resolve_id(&connect.to).await;
            }
            if let Some(annotate) = &mut beat.annotate {
                annotate.target = workspace.resolve_id(&annotate.target).await;
            }
            if let Some(emphasis) = &mut beat.emphasis {
                emphasis.targets = workspace.resolve_ids(&emphasis.targets).await;
            }
            if let Some(tidy) = &mut beat.tidy {
                *tidy = workspace.resolve_ids(tidy).await;
            }
            if let Some(clear) = &mut beat.clear {
                *clear = workspace.resolve_ids(clear).await;
            }
            if let Some(focus) = beat.focus.take() {
                beat.focus = Some(workspace.resolve_id(&focus).await);
            }
            if let Some(point) = beat.point.take() {
                beat.point = Some(workspace.resolve_id(&point).await);
            }
        }
        let outcome = self
            .apply_transaction(&workspace, "agent", context.as_deref(), revision, |doc| {
                script::execute(doc, &beats, now_ms())
            })
            .await;
        let (outcome, receipt) = match outcome {
            Ok(outcome) => outcome,
            Err(message) => return Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        };

        let created: serde_json::Map<String, serde_json::Value> = {
            let mut aliases = workspace.aliases.lock().await;
            outcome
                .created
                .iter()
                .map(|(handle, id)| (handle.clone(), serde_json::json!(aliases.handle(id))))
                .collect()
        };

        let mut played = false;
        let mut cancelled = false;
        let pages = workspace.pages.load(Ordering::SeqCst);
        if outcome.presentation && pages > 0 {
            let settings = self.state.current_settings().await;
            let hold_scale = settings.hold_scale();
            let camera_moves = settings.camera_moves();
            let presentation_id = workspace.present_counter.fetch_add(1, Ordering::SeqCst) + 1;
            let holds: Vec<u64> = outcome
                .steps
                .iter()
                .map(|step| ((step.hold_ms.unwrap_or(0) as f64) * hold_scale).round() as u64)
                .collect();
            let total_hold: u64 = holds.iter().sum();
            let steps: Vec<serde_json::Value> = outcome
                .steps
                .iter()
                .zip(&holds)
                .map(|(step, hold)| {
                    serde_json::json!({
                        "reveal": step.created,
                        "focus": if camera_moves { step.focus.clone() } else { None },
                        "point": step.point,
                        "caption": step.caption,
                        "hold_ms": hold,
                    })
                })
                .collect();
            let mut acks = workspace.present_acks.subscribe();
            let frame = serde_json::json!({
                "type": "present",
                "id": presentation_id,
                "steps": steps,
                "zoom_cap": settings.zoom_cap(),
            })
            .to_string();
            workspace.presentation_started();
            let _ = workspace.to_pages.send(PageMessage::all(frame));
            played = true;
            cancelled = wait_for_present_ack(&mut acks, presentation_id, total_hold).await;
            workspace.presentation_finished();
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::json!({
                "created": created,
                "edit": receipt,
                "elements": outcome.elements,
                "beats": beat_count,
                "played": played,
                "cancelled": cancelled,
            })
            .to_string(),
        )]))
    }

    #[tool(
        description = "Build a board in one call: thousands of explicit shapes or one \
                       parametric grid. Shapes: {k, x, y, w, h, f, t} with k one of \
                       r=rectangle, e=ellipse, d=diamond, s=sticky, t=text, f=frame; \
                       w/h default 160x100 (text auto-sizes); f overrides the call's \
                       shared fill; t is the bound label (text content for t, frame \
                       name for f). Connects: {a, b} index into this call's shapes in \
                       order (explicit shapes first, then grid nodes), wired as bound \
                       elbow connectors. Grid: {count, cols, x0, y0, dx, dy, w, h, \
                       kinds, palette, links, rough, frame} — count nodes row-major \
                       (cols defaults to a square), kinds cycles shape codes \
                       (default r), palette cycles fills, links is none/right/down/both \
                       (default right), frame draws an owning frame around the grid. \
                       Caps: 20,000 explicit shapes, 150,000 grid nodes per call. \
                       Lands at once, never plays back. Receipt reports counts and \
                       bounds, not per-element lines. Bulk commits skip the revert \
                       history: pass frame (or grid.frame) to remove the board later \
                       with one canvas_clear on the frame. For a handful of elements \
                       prefer canvas_draw/canvas_script. Pass context/revision as usual."
    )]
    async fn canvas_bulk(
        &self,
        Parameters(args): Parameters<BulkArgs>,
    ) -> Result<CallToolResult, McpError> {
        use canvas_core::bulk::{
            build_grid, build_shapes, connect_indices, BulkStats, BulkStyle, GridSpec, KeyChain,
            LinkMode, ShapeCode, ShapeSpec,
        };
        use canvas_core::geometry::Rect;

        let failure =
            |message: String| Ok(CallToolResult::error(vec![ContentBlock::text(message)]));
        if args.shapes.len() > 20_000 {
            return failure(format!(
                "too many explicit shapes ({} > 20,000): describe the regular parts with grid instead",
                args.shapes.len()
            ));
        }
        if args.grid.as_ref().is_some_and(|grid| grid.count > 150_000) {
            return failure(format!(
                "grid too large ({} > 150,000 nodes): split the board across calls",
                args.grid.as_ref().map(|grid| grid.count).unwrap_or(0)
            ));
        }
        if args.shapes.is_empty() && args.grid.is_none() {
            return failure("nothing to build: pass shapes, grid, or both".into());
        }
        // Materialize specs before touching the document so malformed calls
        // fail without committing anything.
        let defaults = BulkStyle {
            fill: args.fill.clone(),
            ink: args.ink.clone(),
            roughness: args.rough,
        };
        let mut specs: Vec<ShapeSpec> = Vec::with_capacity(args.shapes.len());
        for (position, shape) in args.shapes.iter().enumerate() {
            let Some(kind) = ShapeCode::parse(shape.k.as_str()) else {
                return failure(format!(
                    "shape #{position}: unknown kind {:?}; use r, e, d, s, t or f",
                    shape.k
                ));
            };
            for value in [shape.x, shape.y] {
                if !value.is_finite() {
                    return failure(format!("shape #{position}: x and y must be finite"));
                }
            }
            let (w, h) = (shape.w.unwrap_or(160.0), shape.h.unwrap_or(100.0));
            if kind != ShapeCode::Text && (!w.is_finite() || !h.is_finite()) {
                return failure(format!("shape #{position}: w and h must be finite"));
            }
            specs.push(ShapeSpec {
                kind,
                rect: Rect::new(shape.x, shape.y, w, h),
                fill: shape.f.clone(),
                label: shape.t.clone(),
            });
        }
        for (position, connect) in args.connects.iter().enumerate() {
            if connect.a == connect.b {
                return failure(format!(
                    "connect #{position}: cannot link a shape to itself"
                ));
            }
        }
        let grid = match args.grid.as_ref() {
            None => None,
            Some(grid) => {
                let links = match grid.links.as_deref() {
                    None => LinkMode::Right,
                    Some(mode) => match LinkMode::parse(mode) {
                        Some(parsed) => parsed,
                        None => {
                            return failure(format!(
                                "grid links must be none, right, down or both; got {mode:?}"
                            ));
                        }
                    },
                };
                Some(GridSpec {
                    count: grid.count,
                    cols: grid.cols,
                    x0: grid.x0.unwrap_or(20.0),
                    y0: grid.y0.unwrap_or(20.0),
                    dx: grid.dx.unwrap_or(220.0),
                    dy: grid.dy.unwrap_or(170.0),
                    w: grid.w.unwrap_or(140.0),
                    h: grid.h.unwrap_or(90.0),
                    kinds: grid.kinds.clone().unwrap_or("r".into()),
                    palette: grid.palette.clone().unwrap_or(vec![
                        "#ffec99".into(),
                        "#b2f2bb".into(),
                        "#a5d8ff".into(),
                        "#ffc9c9".into(),
                        "#d0bfff".into(),
                    ]),
                    links,
                    roughness: grid.rough.unwrap_or(1),
                    frame: grid.frame.clone(),
                })
            }
        };

        // Same guards as apply_transaction, but the commit below stays lean:
        // no per-element receipt lines, no revert-history entry (both scale
        // with the change and would cost megabytes/gigabytes on huge boards).
        let observed = self.observed.lock().await.clone();
        let expected = args.context.clone().or(observed);
        let workspace = self.current().await;
        if expected.is_some_and(|token| token != workspace.context()) {
            return failure(
                "stale_context: the current workspace changed; read canvas_status before retrying"
                    .into(),
            );
        }
        let now = now_ms();
        // One lock scope: guards, build, and revision bump stay atomic, so
        // chained keys never collide with a concurrent writer.
        let (stats, put, revision, frame_id) = {
            let mut doc = workspace.doc.lock().await;
            if workspace.retired.load(Ordering::SeqCst) {
                return failure(
                    "This workspace was renamed or deleted. Reconnect the page.".into(),
                );
            }
            if args
                .revision
                .is_some_and(|rev| rev != workspace.revision.load(Ordering::SeqCst))
            {
                return failure(
                    "stale_revision: read the affected elements before retrying".into(),
                );
            }
            let mut chain = KeyChain::new(&doc);
            let mut stats = BulkStats::default();
            if let Err(message) = build_shapes(
                &mut doc, &mut chain, &specs, &defaults, now, None, &mut stats,
            ) {
                return failure(message);
            }
            // An explicit frame wraps the call's own shapes: one canvas_clear
            // on it later removes the whole batch (bulk skips revert history).
            let mut frame_id: Option<String> = None;
            if let Some(name) = args.frame.as_deref() {
                if let Some(bounds) = stats.bounds {
                    let rect = bounds.expand(24.0);
                    let frame_kind = canvas_core::model::ElementKind::Frame {
                        rect,
                        name: name.to_string(),
                    };
                    let index = match chain.next_key() {
                        Ok(index) => index,
                        Err(message) => return failure(message),
                    };
                    let id = canvas_core::id::new_element_id();
                    let frame = canvas_core::model::Element::new_with_id(
                        frame_kind,
                        index,
                        now,
                        id.clone(),
                    );
                    for shape_id in stats.shape_ids.clone() {
                        if let Some(element) = doc.element_mut(&shape_id) {
                            element.frame_id = Some(id.clone());
                        }
                    }
                    doc.insert(frame);
                    stats.new_ids.push(id.clone());
                    frame_id = Some(id.clone());
                    stats.bounds = Some(rect);
                }
            }
            if let Some(grid) = &grid {
                if let Err(message) =
                    build_grid(&mut doc, &mut chain, grid, &defaults, now, &mut stats)
                {
                    return failure(message);
                }
            }
            // Explicit connects address shapes-then-grid-nodes in order; grid
            // links were already wired during expansion.
            let pairs: Vec<(usize, usize)> = args
                .connects
                .iter()
                .map(|connect| (connect.a, connect.b))
                .collect();
            let ids = stats.shape_ids.clone();
            if let Err(message) =
                connect_indices(&mut doc, &mut chain, &pairs, &ids, now, &mut stats)
            {
                return failure(message);
            }
            let put: Vec<canvas_core::model::Element> = stats
                .new_ids
                .iter()
                .filter_map(|id| doc.element(id).cloned())
                .collect();
            let revision = workspace.record_bulk(stats.new_ids.clone()).await;
            (stats, put, revision, frame_id)
        };
        workspace.publish_patch(revision, put).await;
        let mut aliases = workspace.aliases.lock().await;
        let sample: Vec<String> = stats
            .shape_ids
            .iter()
            .take(5)
            .map(|id| aliases.handle(id))
            .collect();
        let frame = frame_id
            .as_deref()
            .map(|id| aliases.handle(id))
            .unwrap_or_default();
        drop(aliases);
        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::json!({
                "workspace": workspace.name,
                "context": workspace.context(),
                "revision": revision,
                "transaction": serde_json::Value::Null,
                "bulk": {
                    "shapes": stats.shapes,
                    "texts": stats.texts,
                    "connects": stats.connects,
                    "bounds": stats.bounds.map(|r| [r.x, r.y, r.w, r.h]),
                    "sample": sample,
                    "frame": frame,
                },
                "note": "Bulk commits skip the revert history; pass frame (or grid.frame) to remove the board later with one canvas_clear on the frame.",
            })
            .to_string(),
        )]))
    }

    #[tool(
        description = "Put a cross, check or question mark on an element — a normal drawing \
                       element that saves, merges and moves like any other. Use it to strike \
                       out a dead idea, tick a good one, or flag a question while discussing. \
                       Returns the ids of the mark's elements (a cross is two strokes)."
    )]
    async fn canvas_annotate(
        &self,
        Parameters(args): Parameters<AnnotateArgs>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.current().await;
        let id = workspace.resolve_id(&args.id).await;
        let result = self
            .apply_edit(&workspace, "agent", |doc| {
                verbs::annotate(doc, &id, &args.mark, now_ms())
            })
            .await;
        match result {
            Ok(ids) => {
                let elements = workspace.doc.lock().await.live_count();
                let mut aliases = workspace.aliases.lock().await;
                let handles: Vec<String> = ids.iter().map(|id| aliases.handle(id)).collect();
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    serde_json::json!({ "ids": handles, "elements": elements }).to_string(),
                )]))
            }
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "Change emphasis through the style model: `dim` fades elements into \
                       the background, `emphasize` restores opacity and thickens the stroke. \
                       Use it to park an abandoned branch or spotlight the live one."
    )]
    async fn canvas_emphasis(
        &self,
        Parameters(args): Parameters<EmphasisArgs>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.current().await;
        let ids = workspace.resolve_ids(&args.ids).await;
        let result = self
            .apply_edit(&workspace, "agent", |doc| {
                verbs::emphasis(doc, &ids, &args.mode, now_ms())
            })
            .await;
        match result {
            Ok(changed) => {
                let elements = workspace.doc.lock().await.live_count();
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    serde_json::json!({ "changed": changed, "elements": elements }).to_string(),
                )]))
            }
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "Arrange a set of elements into a tidy grid at their current top-left \
                       corner, in reading order. Bound labels travel with their elements and \
                       bound arrows re-resolve, so nothing detaches. Arrows are skipped: they \
                       move with their endpoints."
    )]
    async fn canvas_tidy(
        &self,
        Parameters(args): Parameters<ElementIdsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.current().await;
        let ids = workspace.resolve_ids(&args.ids).await;
        let result = self
            .apply_edit(&workspace, "agent", |doc| verbs::tidy(doc, &ids, now_ms()))
            .await;
        match result {
            Ok(moved) => {
                let elements = workspace.doc.lock().await.live_count();
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    serde_json::json!({ "moved": moved, "elements": elements }).to_string(),
                )]))
            }
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "Delete a set of elements (tombstones, so the merge model and other \
                       viewers see the deletion). Frames take their children, containers \
                       take their labels, and connectors bound to cleared elements go \
                       with them. Use it to clear away a discussion that has moved on."
    )]
    async fn canvas_clear(
        &self,
        Parameters(args): Parameters<ElementIdsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.current().await;
        let ids = workspace.resolve_ids(&args.ids).await;
        let result = self
            .apply_edit(&workspace, "agent", |doc| verbs::clear(doc, &ids, now_ms()))
            .await;
        match result {
            Ok(cleared) => {
                let elements = workspace.doc.lock().await.live_count();
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    serde_json::json!({ "cleared": cleared, "elements": elements }).to_string(),
                )]))
            }
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "Look at the rendered canvas: returns a PNG image of what the user \
                       is looking at right now, camera included, after any in-flight \
                       explanation settles. With no page open it renders the whole drawing \
                       headlessly. Use canvas_read for geometry, content and handles; reach \
                       for this only when the rendered result itself matters (text wrapping, \
                       ink, exact camera framing) or the user asks to see it. It can also \
                       check your own layout, spacing, labels and overlaps."
    )]
    async fn canvas_screenshot(
        &self,
        Parameters(args): Parameters<ScreenshotArgs>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.current().await;
        workspace
            .wait_for_presentation(Duration::from_secs(15))
            .await;
        let pages = workspace.pages.load(Ordering::SeqCst);
        if pages > 0 {
            let id = workspace.screenshot_counter.fetch_add(1, Ordering::SeqCst) + 1;
            let mut replies = workspace.screenshot_replies.subscribe();
            let scale = args.scale.unwrap_or(1.0).clamp(0.1, 4.0);
            let frame = serde_json::json!({
                "type": "screenshot",
                "id": id,
                "scale": scale,
            })
            .to_string();
            let _ = workspace.to_pages.send(PageMessage::all(frame));
            if let Ok(Some(data_url)) = tokio::time::timeout(
                Duration::from_secs(8),
                wait_for_screenshot(&mut replies, id),
            )
            .await
            {
                return image_from_data_url(&data_url);
            }
            // The page did not answer (busy or gone): show the mirror instead.
        }
        let doc = workspace.doc.lock().await;
        match render::render_png(&doc, &self.state.web_dir) {
            Ok(png) => Ok(CallToolResult::success(vec![ContentBlock::image(
                base64::engine::general_purpose::STANDARD.encode(png),
                "image/png",
            )])),
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "Move the user's view: glide the camera to frame one element, or \
                       the whole drawing when id is omitted. Use it to show the user \
                       something or to set up context. A frame id frames everything \
                       inside it. Set animate=false to jump instantly."
    )]
    async fn canvas_focus(
        &self,
        Parameters(args): Parameters<FocusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.current().await;
        let mut message = serde_json::json!({ "type": "view", "action": "focus" });
        if let Some(id) = args.id {
            message["id"] = workspace.resolve_id(&id).await.into();
        }
        if let Some(pad) = args.pad {
            message["pad"] = pad.into();
        }
        if let Some(animate) = args.animate {
            message["animate"] = animate.into();
        }
        deliver_view(&self.state, &workspace, message).await
    }

    #[tool(
        description = "Pulse a fading highlight around one element without moving the \
                       camera. Use it to point at something while explaining a drawing."
    )]
    async fn canvas_point(
        &self,
        Parameters(args): Parameters<PointArgs>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.current().await;
        let id = workspace.resolve_id(&args.id).await;
        let message = serde_json::json!({
            "type": "view",
            "action": "point",
            "id": id,
        });
        deliver_view(&self.state, &workspace, message).await
    }

    #[tool(
        description = "Zoom the user's view: `in`, `out`, `fit` (frame the whole drawing) \
                       or `reset` (back to 100%)."
    )]
    async fn canvas_zoom(
        &self,
        Parameters(args): Parameters<ZoomArgs>,
    ) -> Result<CallToolResult, McpError> {
        if !matches!(args.mode.as_str(), "in" | "out" | "fit" | "reset") {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Unknown zoom mode {:?}; expected one of in, out, fit, reset.",
                args.mode
            ))]));
        }
        let workspace = self.current().await;
        let message = serde_json::json!({
            "type": "view",
            "action": "zoom",
            "mode": args.mode,
        });
        deliver_view(&self.state, &workspace, message).await
    }
}

#[tool_handler]
impl ServerHandler for CanvasServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rs-canvas", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error_text(result: &CallToolResult) -> String {
        result.content[0]
            .as_text()
            .expect("text content")
            .text
            .clone()
    }

    fn point(x: f64, y: f64) -> PenPoint {
        PenPoint {
            x,
            y,
            pressure: None,
        }
    }

    fn pen_args(points: Vec<PenPoint>) -> PenArgs {
        PenArgs {
            points,
            pressure: None,
            stroke_width: None,
            stroke_color: None,
            roughness: None,
            dash: None,
            opacity: None,
            fill_color: None,
            fill_style: None,
        }
    }

    #[tokio::test]
    async fn status_reports_an_empty_document_and_no_pages() {
        let server = CanvasServer::new(Arc::new(AppState::new(
            std::path::PathBuf::from("."),
            None,
            true,
        )));
        let result = tokio::time::timeout(Duration::from_secs(1), server.canvas_status())
            .await
            .expect("status must not deadlock while listing workspaces")
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&error_text(&result)).unwrap();
        assert_eq!(body["server"], "rs-canvas");
        assert_eq!(body["pages_connected"], 0);
        assert_eq!(body["document"]["elements"], 0);
        assert_eq!(body["workspace"], "default");
        assert_eq!(
            body["workspaces"].as_array().unwrap().len(),
            1,
            "only the default workspace exists yet"
        );
    }

    #[tokio::test]
    async fn view_tools_need_an_open_page() {
        let server = CanvasServer::new(Arc::new(AppState::new(
            std::path::PathBuf::from("."),
            None,
            true,
        )));
        let result = server
            .canvas_zoom(Parameters(ZoomArgs { mode: "in".into() }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(error_text(&result).contains("No canvas page is connected"));
    }

    #[tokio::test]
    async fn focus_delivers_a_view_command_to_open_pages() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        let mut from_server = workspace.to_pages.subscribe();
        workspace.pages.store(1, Ordering::SeqCst);
        let server = CanvasServer::new(state);

        let result = server
            .canvas_focus(Parameters(FocusArgs {
                id: Some("abc".into()),
                pad: Some(24.0),
                animate: Some(false),
            }))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));

        let message: serde_json::Value =
            serde_json::from_str(&from_server.recv().await.unwrap().text).unwrap();
        assert_eq!(message["type"], "view");
        assert_eq!(message["action"], "focus");
        assert_eq!(message["id"], "abc");
        assert_eq!(message["pad"], 24.0);
        assert_eq!(message["animate"], false);
        assert_eq!(message["zoom_cap"], 2.0, "gentle camera caps the zoom");
    }

    #[tokio::test]
    async fn zoom_rejects_unknown_modes() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        let mut from_server = workspace.to_pages.subscribe();
        workspace.pages.store(1, Ordering::SeqCst);
        let server = CanvasServer::new(state);

        let result = server
            .canvas_zoom(Parameters(ZoomArgs {
                mode: "sideways".into(),
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(error_text(&result).contains("Unknown zoom mode"));
        assert!(from_server.try_recv().is_err(), "nothing was broadcast");
    }

    #[tokio::test]
    async fn point_carries_the_element_id() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        let mut from_server = workspace.to_pages.subscribe();
        workspace.pages.store(1, Ordering::SeqCst);
        let server = CanvasServer::new(state);

        server
            .canvas_point(Parameters(PointArgs { id: "db".into() }))
            .await
            .unwrap();
        let message: serde_json::Value =
            serde_json::from_str(&from_server.recv().await.unwrap().text).unwrap();
        assert_eq!(message["action"], "point");
        assert_eq!(message["id"], "db");
        assert!(message.get("animate").is_none());
    }

    #[tokio::test]
    async fn canvas_draw_creates_and_broadcasts_an_element() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        let mut from_server = workspace.to_pages.subscribe();
        workspace.pages.store(1, Ordering::SeqCst);
        let server = CanvasServer::new(state.clone());

        let result = server
            .canvas_draw(Parameters(DrawArgs {
                kind: "sticky".into(),
                text: Some("Queue".into()),
                name: None,
                glyph: None,
                x: None,
                y: None,
                w: None,
                h: None,
                near: None,
                side: None,
            }))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        let created: serde_json::Value = serde_json::from_str(&error_text(&result)).unwrap();
        assert_eq!(created["elements"], 2, "sticky plus its bound label");
        assert!(workspace.doc.lock().await.live_count() == 2);

        let frame: serde_json::Value =
            serde_json::from_str(&from_server.recv().await.unwrap().text).unwrap();
        assert_eq!(frame["type"], "patch");
        assert_eq!(
            frame["put"].as_array().unwrap().len(),
            2,
            "sticky plus its bound label travel in one patch"
        );
        assert!(frame["rev"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn canvas_pen_creates_and_broadcasts_a_stroke() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        let mut from_server = workspace.to_pages.subscribe();
        workspace.pages.store(1, Ordering::SeqCst);
        let server = CanvasServer::new(state.clone());

        let result = server
            .canvas_pen(Parameters(pen_args(vec![
                point(0.0, 0.0),
                point(40.0, 20.0),
                point(80.0, 0.0),
            ])))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        let created: serde_json::Value = serde_json::from_str(&error_text(&result)).unwrap();
        assert_eq!(created["elements"], 1);
        assert!(created["id"].is_string());

        let frame: serde_json::Value =
            serde_json::from_str(&from_server.recv().await.unwrap().text).unwrap();
        assert_eq!(frame["type"], "patch");
        assert_eq!(frame["put"].as_array().unwrap().len(), 1);
        assert!(frame["rev"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn canvas_pen_rejects_a_degenerate_stroke_readably() {
        let server = CanvasServer::new(Arc::new(AppState::new(
            std::path::PathBuf::from("."),
            None,
            true,
        )));
        let result = server
            .canvas_pen(Parameters(pen_args(vec![point(1.0, 1.0)])))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(error_text(&result).contains("at least 2 points"));
    }

    #[tokio::test]
    async fn canvas_pen_applies_pressure_and_ink_style() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        let server = CanvasServer::new(state.clone());
        let result = server
            .canvas_pen(Parameters(PenArgs {
                points: vec![
                    PenPoint {
                        x: 0.0,
                        y: 0.0,
                        pressure: Some(0.1),
                    },
                    PenPoint {
                        x: 30.0,
                        y: 20.0,
                        pressure: Some(1.0),
                    },
                    PenPoint {
                        x: 60.0,
                        y: 0.0,
                        pressure: None,
                    },
                ],
                pressure: Some(0.2),
                stroke_width: Some(7.0),
                stroke_color: Some(" #ff8787 ".into()),
                roughness: Some(2),
                dash: Some(DashArg::Dashed),
                opacity: Some(0.5),
                fill_color: Some("#ffec99".into()),
                fill_style: Some(FillStyleArg::CrossHatch),
            }))
            .await
            .unwrap();
        let created: serde_json::Value = serde_json::from_str(&error_text(&result)).unwrap();
        let id = workspace.resolve_id(created["id"].as_str().unwrap()).await;
        let doc = workspace.doc.lock().await;
        let element = doc.element(&id).expect("created stroke");
        let canvas_core::model::ElementKind::Freehand {
            pressures, style, ..
        } = &element.kind
        else {
            panic!("expected freehand, got {:?}", element.kind);
        };
        assert_eq!(
            pressures,
            &vec![0.1, 1.0, 0.2],
            "per-point pressure wins, the stroke default fills the gap"
        );
        assert_eq!(style.stroke_width, 7.0);
        assert_eq!(style.stroke_color, "#ff8787");
        assert_eq!(style.roughness, 2);
        assert_eq!(style.dash, canvas_core::model::StrokeDash::Dashed);
        assert_eq!(style.opacity, 0.5);
        assert_eq!(style.fill_color, "#ffec99");
        assert_eq!(
            style.fill_style,
            canvas_core::model::FillPattern::CrossHatch
        );
    }

    #[tokio::test]
    async fn canvas_pen_strokes_show_up_in_the_scene_graph() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        workspace.pages.store(1, Ordering::SeqCst);
        let server = CanvasServer::new(state);
        let result = server
            .canvas_pen(Parameters(pen_args(vec![
                point(0.0, 0.0),
                point(10.0, 10.0),
            ])))
            .await
            .unwrap();
        let created: serde_json::Value = serde_json::from_str(&error_text(&result)).unwrap();
        let id = created["id"].as_str().unwrap();
        let described = error_text(
            &server
                .canvas_read(Parameters(ReadArgs::default()))
                .await
                .unwrap(),
        );
        assert!(described.contains("stroke"), "{described}");
        assert!(described.contains(id), "{described}");
    }

    #[tokio::test]
    async fn canvas_read_describes_the_mirror() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let server = CanvasServer::new(state.clone());
        server
            .canvas_draw(Parameters(DrawArgs {
                kind: "rectangle".into(),
                text: Some("API".into()),
                name: None,
                glyph: None,
                x: Some(0.0),
                y: Some(0.0),
                w: None,
                h: None,
                near: None,
                side: None,
            }))
            .await
            .unwrap();

        let result = server
            .canvas_read(Parameters(ReadArgs::default()))
            .await
            .unwrap();
        let described = error_text(&result);
        assert!(described.contains("rect"), "{described}");
        assert!(described.contains("\"API\""), "{described}");
        assert!(described.starts_with("revision 1"), "{described}");
    }

    #[tokio::test]
    async fn canvas_read_since_reports_only_changes() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        let server = CanvasServer::new(state.clone());
        let draw = |kind: &str| DrawArgs {
            kind: kind.into(),
            text: None,
            name: None,
            glyph: None,
            x: None,
            y: None,
            w: None,
            h: None,
            near: None,
            side: None,
        };
        server
            .canvas_draw(Parameters(draw("rectangle")))
            .await
            .unwrap();
        let first = error_text(
            &server
                .canvas_read(Parameters(ReadArgs::default()))
                .await
                .unwrap(),
        );
        let revision: u64 = first
            .lines()
            .next()
            .unwrap()
            .trim_start_matches("revision ")
            .parse()
            .unwrap();

        // Nothing happened since: an empty report.
        let quiet = error_text(
            &server
                .canvas_read(Parameters(ReadArgs {
                    since: Some(revision),
                    ..Default::default()
                }))
                .await
                .unwrap(),
        );
        assert!(quiet.contains("no changes"), "{quiet}");

        // An agent draw, then a page-style change: both are reported.
        server
            .canvas_draw(Parameters(draw("ellipse")))
            .await
            .unwrap();
        let mut doc = workspace.doc.lock().await;
        let before = doc.clone();
        let id = doc.add(
            canvas_core::model::ElementKind::Shape {
                shape: canvas_core::model::ShapeKind::Diamond,
                rect: canvas_core::geometry::Rect::new(0.0, 0.0, 10.0, 10.0),
                style: canvas_core::model::Style::default(),
                bound_text_id: None,
            },
            3,
        );
        workspace.record_change(&before, &doc, "page").await;
        drop(doc);
        let handle = workspace.aliases.lock().await.handle(&id);

        let report = error_text(
            &server
                .canvas_read(Parameters(ReadArgs {
                    since: Some(revision),
                    ..Default::default()
                }))
                .await
                .unwrap(),
        );
        assert!(report.contains("(agent)"), "{report}");
        assert!(report.contains("(page)"), "{report}");
        assert!(report.contains("changed now:"), "{report}");
        assert!(report.contains(&handle), "{report}");
    }

    #[test]
    fn change_reports_count_kinds_and_label_elements() {
        let mut doc = Document::new();
        let moved = doc.add(
            canvas_core::model::ElementKind::Shape {
                shape: canvas_core::model::ShapeKind::Rectangle,
                rect: canvas_core::geometry::Rect::new(0.0, 0.0, 100.0, 60.0),
                style: canvas_core::model::Style::default(),
                bound_text_id: None,
            },
            1,
        );
        let records = vec![
            crate::state::ChangeRecord {
                revision: 13,
                source: "agent".into(),
                changed: vec![ChangedElement {
                    id: moved.clone(),
                    kinds: vec![ChangeKind::Moved],
                }],
            },
            crate::state::ChangeRecord {
                revision: 14,
                source: "page".into(),
                changed: vec![
                    ChangedElement {
                        id: moved.clone(),
                        kinds: vec![ChangeKind::Restyled],
                    },
                    ChangedElement {
                        id: "gone".into(),
                        kinds: vec![ChangeKind::Removed],
                    },
                ],
            },
        ];
        let mut ids = crate::aliases::Aliases::default();
        let report = describe_changes(&doc, &records, 12, 14, &mut ids);

        assert!(
            report.contains("- rev 13 (agent): 1 changed (1 moved)"),
            "{report}"
        );
        assert!(
            report.contains("- rev 14 (page): 2 changed (1 removed, 1 restyled)"),
            "{report}"
        );
        assert!(
            report.contains("(moved, restyled)"),
            "kinds are unioned across the window: {report}"
        );
        assert!(report.contains("(removed)"), "{report}");
        assert!(report.contains("removed: "), "{report}");
    }

    #[tokio::test]
    async fn canvas_read_since_reports_kinds_per_revision_and_element() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        let server = CanvasServer::new(state.clone());
        let draw = |kind: &str| DrawArgs {
            kind: kind.into(),
            text: None,
            name: None,
            glyph: None,
            x: None,
            y: None,
            w: None,
            h: None,
            near: None,
            side: None,
        };
        server
            .canvas_draw(Parameters(draw("rectangle")))
            .await
            .unwrap();
        server
            .canvas_draw(Parameters(draw("ellipse")))
            .await
            .unwrap();
        let revision = workspace.revision.load(Ordering::SeqCst);
        assert_eq!(revision, 2);

        {
            let mut doc = workspace.doc.lock().await;
            let ids: Vec<String> = doc
                .ordered_live()
                .iter()
                .map(|element| element.id.clone())
                .collect();
            let before = doc.clone();
            let moved = doc.element_mut(&ids[0]).unwrap();
            if let canvas_core::model::ElementKind::Shape { rect, .. } = &mut moved.kind {
                rect.x += 30.0;
            }
            moved.touch(now_ms());
            let removed = doc.element_mut(&ids[1]).unwrap();
            removed.is_deleted = true;
            removed.touch(now_ms());
            workspace.record_change(&before, &doc, "page").await;
        }

        let report = error_text(
            &server
                .canvas_read(Parameters(ReadArgs {
                    since: Some(revision),
                    ..Default::default()
                }))
                .await
                .unwrap(),
        );
        assert!(
            report.contains("- rev 3 (page): 2 changed (1 removed, 1 moved)"),
            "{report}"
        );
        assert!(report.contains("changed now:"), "{report}");
        assert!(
            report.contains("(moved)"),
            "live element carries its kind: {report}"
        );
        assert!(
            report.contains("(removed)"),
            "removed element carries its kind: {report}"
        );
        assert!(report.contains("removed: "), "{report}");
    }

    fn bulk_shape(k: &str, x: f64, y: f64) -> BulkShape {
        BulkShape {
            k: k.into(),
            x,
            y,
            w: None,
            h: None,
            f: None,
            t: None,
        }
    }

    fn bulk_args() -> BulkArgs {
        BulkArgs {
            context: None,
            revision: None,
            fill: None,
            ink: None,
            rough: None,
            frame: None,
            shapes: Vec::new(),
            connects: Vec::new(),
            grid: None,
        }
    }

    #[tokio::test]
    async fn canvas_bulk_builds_shapes_and_wires_them() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        let server = CanvasServer::new(state);
        let mut args = bulk_args();
        args.shapes = vec![
            bulk_shape("r", 0.0, 0.0),
            bulk_shape("e", 220.0, 0.0),
            bulk_shape("d", 440.0, 0.0),
        ];
        args.shapes[0].t = Some("DB".into());
        args.connects = vec![BulkConnect { a: 0, b: 1 }, BulkConnect { a: 1, b: 2 }];
        args.frame = Some("backend".into());
        let receipt = error_text(&server.canvas_bulk(Parameters(args)).await.unwrap());
        let body: serde_json::Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(body["bulk"]["shapes"], 3, "{receipt}");
        assert_eq!(body["bulk"]["texts"], 1, "{receipt}");
        assert_eq!(body["bulk"]["connects"], 2, "{receipt}");
        assert!(body["bulk"]["bounds"].is_array(), "{receipt}");
        assert!(body["transaction"].is_null(), "{receipt}");
        // 3 shapes + 1 label + 2 arrows + 1 frame.
        assert_eq!(workspace.doc.lock().await.live_count(), 7);
        // The frame reverts the whole batch: one clear takes shapes, the
        // label and the bound arrows with it.
        let frame = body["bulk"]["frame"].as_str().unwrap().to_string();
        assert!(!frame.is_empty(), "{receipt}");
        let cleared = error_text(
            &server
                .canvas_clear(Parameters(ElementIdsArgs { ids: vec![frame] }))
                .await
                .unwrap(),
        );
        assert!(cleared.contains('7'), "{cleared}");
        assert_eq!(workspace.doc.lock().await.live_count(), 0);
    }

    #[tokio::test]
    async fn canvas_bulk_grid_wires_a_board_without_row_wrap() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        let server = CanvasServer::new(state);
        let mut args = bulk_args();
        args.grid = Some(BulkGrid {
            count: 10,
            cols: Some(4),
            x0: None,
            y0: None,
            dx: None,
            dy: None,
            w: None,
            h: None,
            kinds: Some("red".into()),
            palette: None,
            links: Some("both".into()),
            rough: None,
            frame: None,
        });
        let receipt = error_text(&server.canvas_bulk(Parameters(args)).await.unwrap());
        let body: serde_json::Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(body["bulk"]["shapes"], 10, "{receipt}");
        assert_eq!(body["bulk"]["connects"], 13, "{receipt}");
        assert_eq!(workspace.doc.lock().await.live_count(), 23);
    }

    #[tokio::test]
    async fn canvas_bulk_rejects_bad_kinds_and_dangling_links() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let server = CanvasServer::new(state);
        let mut args = bulk_args();
        args.shapes = vec![bulk_shape("z", 0.0, 0.0)];
        let err = error_text(&server.canvas_bulk(Parameters(args)).await.unwrap());
        assert!(err.contains("unknown kind"), "{err}");
    }

    #[tokio::test]
    async fn canvas_script_deflects_huge_beat_lists_to_bulk() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let server = CanvasServer::new(state);
        let beat: ScriptBeat = serde_json::from_value(serde_json::json!({
            "draw": {"kind": "rectangle", "x": 0.0, "y": 0.0},
        }))
        .unwrap();
        let beats = vec![beat.clone(); 301];
        let err = error_text(
            &server
                .canvas_script(Parameters(ScriptArgs {
                    beats,
                    context: None,
                    revision: None,
                }))
                .await
                .unwrap(),
        );
        assert!(err.contains("canvas_bulk"), "{err}");
    }

    #[tokio::test]
    async fn canvas_read_summary_counts_any_scale() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let server = CanvasServer::new(state.clone());
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        let mut args = bulk_args();
        args.grid = Some(BulkGrid {
            count: 50,
            cols: None,
            x0: None,
            y0: None,
            dx: None,
            dy: None,
            w: None,
            h: None,
            kinds: None,
            palette: None,
            links: Some("right".into()),
            rough: None,
            frame: None,
        });
        server.canvas_bulk(Parameters(args)).await.unwrap();
        let summary = error_text(
            &server
                .canvas_read(Parameters(ReadArgs {
                    summary: true,
                    ..Default::default()
                }))
                .await
                .unwrap(),
        );
        let body: serde_json::Value = serde_json::from_str(&summary).unwrap();
        assert_eq!(
            body["live"],
            50 + body["arrows"].as_u64().unwrap(),
            "{summary}"
        );
        assert!(body["bounds"].is_array(), "{summary}");
        assert!(body["revision"].is_u64(), "{summary}");
        assert!(workspace.doc.lock().await.live_count() > 0);
    }

    fn attention_rect(doc: &mut Document, x: f64, y: f64, w: f64, h: f64) -> String {
        doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(x, y, w, h),
                style: canvas_core::model::Style::default(),
                bound_text_id: None,
            },
            0,
        )
    }

    #[tokio::test]
    async fn canvas_read_attention_reports_the_view_and_nearby_elements() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        workspace.pages.store(1, Ordering::SeqCst);
        let (inside, nearby) = {
            let mut doc = workspace.doc.lock().await;
            (
                attention_rect(&mut doc, 20.0, 20.0, 20.0, 20.0),
                attention_rect(&mut doc, 105.0, 20.0, 10.0, 10.0),
            )
        };
        {
            let mut doc = workspace.doc.lock().await;
            attention_rect(&mut doc, 400.0, 400.0, 10.0, 10.0);
        }
        *workspace.view_hint.lock().await = Some(crate::state::ViewHint {
            camera: Some(serde_json::json!({ "x": 0.0, "y": 0.0, "zoom": 1.0 })),
            viewport: Some(serde_json::json!({ "w": 100.0, "h": 100.0 })),
            selection: vec![inside.clone()],
        });
        let server = CanvasServer::new(state);
        let text = error_text(
            &server
                .canvas_read(Parameters(ReadArgs {
                    attention: true,
                    ..Default::default()
                }))
                .await
                .unwrap(),
        );
        let (inside_handle, nearby_handle) = {
            let mut aliases = workspace.aliases.lock().await;
            (aliases.handle(&inside), aliases.handle(&nearby))
        };
        assert!(text.contains("view 0,0 100x100 zoom=1 page=live"), "{text}");
        assert!(
            text.contains(&format!("selected {inside_handle}")),
            "{text}"
        );
        assert!(text.contains("visible 1 showing 1"), "{text}");
        assert!(
            text.contains(&format!("{inside_handle} rect 20,20 20x20")),
            "{text}"
        );
        assert!(text.contains("nearby 1 showing 1"), "{text}");
        assert!(
            text.contains(&format!("{nearby_handle} rect 105,20 10x10")),
            "{text}"
        );
        assert!(
            text.contains("offscreen 2 bounds=105,20 305x390 kinds=rect:2"),
            "{text}"
        );
        assert!(!text.contains("rect 400,400"), "{text}");
    }

    #[tokio::test]
    async fn canvas_read_attention_caps_visible_and_nearby_lines() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        {
            let mut doc = workspace.doc.lock().await;
            for i in 0..101 {
                attention_rect(&mut doc, (i % 10) as f64, (i / 10) as f64, 1.0, 1.0);
            }
            for i in 0..30 {
                attention_rect(&mut doc, 105.0, i as f64, 1.0, 1.0);
            }
        }
        *workspace.view_hint.lock().await = Some(crate::state::ViewHint {
            camera: Some(serde_json::json!({ "x": 0.0, "y": 0.0, "zoom": 1.0 })),
            viewport: Some(serde_json::json!({ "w": 100.0, "h": 100.0 })),
            selection: Vec::new(),
        });
        let server = CanvasServer::new(state);
        let text = error_text(
            &server
                .canvas_read(Parameters(ReadArgs {
                    attention: true,
                    ..Default::default()
                }))
                .await
                .unwrap(),
        );
        assert!(text.contains("visible 101 showing 100"), "{text}");
        assert!(text.contains("nearby 30 showing 25"), "{text}");
        assert_eq!(
            text.matches("rect 105,").count(),
            25,
            "nearby is capped at 25 lines: {text}"
        );
    }

    #[tokio::test]
    async fn canvas_read_attention_without_a_view_hint_is_a_tool_error() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        let server = CanvasServer::new(state);
        let args = ReadArgs {
            attention: true,
            ..Default::default()
        };
        let missing = server
            .canvas_read(Parameters(args))
            .await
            .expect("a missing hint must not panic");
        assert_eq!(missing.is_error, Some(true));
        assert_eq!(
            error_text(&missing),
            "no_view: no page is reporting view state; connect a page or read without attention"
        );

        *workspace.view_hint.lock().await = Some(crate::state::ViewHint {
            camera: None,
            viewport: Some(serde_json::json!({ "w": 100.0, "h": 100.0 })),
            selection: Vec::new(),
        });
        let incomplete = server
            .canvas_read(Parameters(ReadArgs {
                attention: true,
                ..Default::default()
            }))
            .await
            .unwrap();
        assert_eq!(incomplete.is_error, Some(true));
        assert!(
            error_text(&incomplete).starts_with("no_view:"),
            "{incomplete:?}"
        );
    }

    #[tokio::test]
    async fn canvas_read_attention_rejects_scopes_and_since() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        *workspace.view_hint.lock().await = Some(crate::state::ViewHint {
            camera: Some(serde_json::json!({ "x": 0.0, "y": 0.0, "zoom": 1.0 })),
            viewport: Some(serde_json::json!({ "w": 100.0, "h": 100.0 })),
            selection: Vec::new(),
        });
        let server = CanvasServer::new(state);
        let attention = || ReadArgs {
            attention: true,
            ..Default::default()
        };
        let cases = [
            ReadArgs {
                ids: Some(vec!["e1".into()]),
                ..attention()
            },
            ReadArgs {
                frame: Some("e1".into()),
                ..attention()
            },
            ReadArgs {
                query: Some("api".into()),
                ..attention()
            },
            ReadArgs {
                selected: true,
                ..attention()
            },
            ReadArgs {
                since: Some(0),
                ..attention()
            },
        ];
        for args in cases {
            let result = server.canvas_read(Parameters(args)).await.unwrap();
            assert_eq!(result.is_error, Some(true));
            let text = error_text(&result);
            assert!(text.contains("attention cannot be combined"), "{text}");
        }
    }

    #[tokio::test]
    async fn canvas_read_without_attention_has_no_view_section() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        {
            let mut doc = workspace.doc.lock().await;
            attention_rect(&mut doc, 0.0, 0.0, 10.0, 10.0);
        }
        *workspace.view_hint.lock().await = Some(crate::state::ViewHint {
            camera: Some(serde_json::json!({ "x": 0.0, "y": 0.0, "zoom": 1.0 })),
            viewport: Some(serde_json::json!({ "w": 100.0, "h": 100.0 })),
            selection: Vec::new(),
        });
        let server = CanvasServer::new(state);
        let text = error_text(
            &server
                .canvas_read(Parameters(ReadArgs::default()))
                .await
                .unwrap(),
        );
        assert!(text.contains("rect 0,0 10x10"), "{text}");
        for marker in ["view 0,0", "selected", "visible ", "nearby ", "offscreen "] {
            assert!(!text.contains(marker), "unexpected {marker:?} in {text}");
        }
    }

    #[tokio::test]
    async fn status_reports_revision_and_ephemeral_view_hints() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        *workspace.view_hint.lock().await = Some(crate::state::ViewHint {
            camera: Some(serde_json::json!({ "x": 10.0, "y": 20.0, "zoom": 1.5 })),
            viewport: Some(serde_json::json!({ "w": 800.0, "h": 600.0 })),
            selection: vec!["db".into()],
        });
        let server = CanvasServer::new(state);
        let result = tokio::time::timeout(Duration::from_secs(1), server.canvas_status())
            .await
            .expect("status must not deadlock while listing workspaces")
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&error_text(&result)).unwrap();
        assert_eq!(body["revision"], 0);
        assert_eq!(body["hints"]["selection"][0], "db");
        assert_eq!(body["hints"]["camera"]["zoom"], 1.5);
        assert!(body["hints"]["note"]
            .as_str()
            .unwrap()
            .contains("transient"));
        assert_eq!(body["settings"]["camera"], "gentle");
        assert_eq!(body["settings"]["pacing"], "normal");
    }

    #[tokio::test]
    async fn camera_off_blocks_focus_and_reports_why() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        state
            .apply_settings_patch(&serde_json::json!({ "camera": "off" }))
            .await
            .unwrap();
        let mut from_server = workspace.to_pages.subscribe();
        workspace.pages.store(1, Ordering::SeqCst);
        let server = CanvasServer::new(state);

        let result = server
            .canvas_focus(Parameters(FocusArgs {
                id: Some("x".into()),
                pad: None,
                animate: None,
            }))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&error_text(&result)).unwrap();
        assert_eq!(body["ok"], false);
        assert_eq!(body["moved"], false);
        assert!(body["reason"].as_str().unwrap().contains("camera"));
        assert!(
            from_server.try_recv().is_err(),
            "camera-off focus must not broadcast"
        );
    }

    #[tokio::test]
    async fn pacing_scales_presentation_holds() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        state
            .apply_settings_patch(&serde_json::json!({ "pacing": "fast" }))
            .await
            .unwrap();
        let mut from_server = workspace.to_pages.subscribe();
        workspace.pages.store(1, Ordering::SeqCst);
        let ack_state = workspace.clone();
        let (frame_tx, frame_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            while let Ok(message) = from_server.recv().await {
                let frame: serde_json::Value = serde_json::from_str(&message.text).unwrap();
                if frame["type"] == "present" {
                    let id = frame["id"].as_u64().unwrap();
                    let _ = ack_state.present_acks.send((id, false));
                    let _ = frame_tx.send(frame);
                    break;
                }
            }
        });

        let server = CanvasServer::new(state);
        let beats: Vec<ScriptBeat> = serde_json::from_value(serde_json::json!([
            { "draw": { "as": "a", "kind": "sticky" }, "caption": "Hello", "hold": 1000 }
        ]))
        .unwrap();
        server
            .canvas_script(Parameters(ScriptArgs {
                beats,
                context: None,
                revision: None,
            }))
            .await
            .unwrap();

        let frame = frame_rx.await.expect("present frame");
        assert_eq!(
            frame["steps"][0]["hold_ms"], 600,
            "fast pacing scales holds"
        );
        assert_eq!(frame["zoom_cap"], 2.0);
    }

    #[tokio::test]
    async fn canvas_draw_rejects_bad_kinds_readably() {
        let server = CanvasServer::new(Arc::new(AppState::new(
            std::path::PathBuf::from("."),
            None,
            true,
        )));
        let result = server
            .canvas_draw(Parameters(DrawArgs {
                kind: "hexagon".into(),
                text: None,
                name: None,
                glyph: None,
                x: None,
                y: None,
                w: None,
                h: None,
                near: None,
                side: None,
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(error_text(&result).contains("unknown kind"));
    }

    #[tokio::test]
    async fn canvas_script_builds_atomically_and_returns_handles() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        let server = CanvasServer::new(state.clone());
        let beats: Vec<ScriptBeat> = serde_json::from_value(serde_json::json!([
            { "draw": { "as": "client", "kind": "sticky", "text": "Client" } },
            { "draw": { "as": "db", "kind": "sticky", "text": "DB" } },
            { "connect": { "from": "client", "to": "db", "label": "SQL" } }
        ]))
        .unwrap();

        let result = server
            .canvas_script(Parameters(ScriptArgs {
                beats,
                context: None,
                revision: None,
            }))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&error_text(&result)).unwrap();
        assert!(body["created"]["client"].is_string());
        assert!(body["created"]["db"].is_string());
        assert_eq!(body["beats"], 3);
        assert_eq!(
            workspace.doc.lock().await.live_count(),
            6,
            "2 stickies, 2 labels, 1 arrow, 1 arrow label"
        );
    }

    #[tokio::test]
    async fn a_failed_script_leaves_the_mirror_untouched() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        let server = CanvasServer::new(state.clone());
        let beats: Vec<ScriptBeat> = serde_json::from_value(serde_json::json!([
            { "draw": { "as": "a", "kind": "sticky" } },
            { "connect": { "from": "a", "to": "ghost" } }
        ]))
        .unwrap();

        let result = server
            .canvas_script(Parameters(ScriptArgs {
                beats,
                context: None,
                revision: None,
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(error_text(&result).contains("beat 2"));
        assert_eq!(workspace.doc.lock().await.live_count(), 0);
    }

    #[tokio::test]
    async fn canvas_script_waits_for_the_page_to_finish_playing() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        let mut from_server = workspace.to_pages.subscribe();
        workspace.pages.store(1, Ordering::SeqCst);
        let ack_state = workspace.clone();
        tokio::spawn(async move {
            while let Ok(message) = from_server.recv().await {
                let frame: serde_json::Value = serde_json::from_str(&message.text).unwrap();
                if frame["type"] == "present" {
                    let id = frame["id"].as_u64().unwrap();
                    let _ = ack_state.present_acks.send((id, false));
                    break;
                }
            }
        });

        let server = CanvasServer::new(state);
        let beats: Vec<ScriptBeat> = serde_json::from_value(serde_json::json!([
            { "draw": { "as": "a", "kind": "sticky" }, "caption": "Hello", "hold": 40 }
        ]))
        .unwrap();
        let result = server
            .canvas_script(Parameters(ScriptArgs {
                beats,
                context: None,
                revision: None,
            }))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&error_text(&result)).unwrap();
        assert_eq!(body["played"], true);
        assert_eq!(body["cancelled"], false);
        assert!(body["created"]["a"].is_string());
    }

    #[tokio::test]
    async fn presentations_are_skipped_when_no_page_is_open() {
        let server = CanvasServer::new(Arc::new(AppState::new(
            std::path::PathBuf::from("."),
            None,
            true,
        )));
        let beats: Vec<ScriptBeat> = serde_json::from_value(serde_json::json!([
            { "draw": { "as": "a", "kind": "sticky" }, "caption": "Hello", "hold": 40 }
        ]))
        .unwrap();
        let result = server
            .canvas_script(Parameters(ScriptArgs {
                beats,
                context: None,
                revision: None,
            }))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&error_text(&result)).unwrap();
        assert_eq!(body["played"], false);
    }

    #[tokio::test]
    async fn screenshot_renders_headlessly_without_a_page() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let server = CanvasServer::new(state);

        let result = server
            .canvas_screenshot(Parameters(ScreenshotArgs { scale: None }))
            .await
            .unwrap();
        assert_eq!(
            result.is_error,
            Some(true),
            "an empty canvas cannot be seen"
        );
        assert!(error_text(&result).contains("empty"));

        server
            .canvas_draw(Parameters(DrawArgs {
                kind: "rectangle".into(),
                text: Some("API".into()),
                name: None,
                glyph: None,
                x: None,
                y: None,
                w: None,
                h: None,
                near: None,
                side: None,
            }))
            .await
            .unwrap();
        let result = server
            .canvas_screenshot(Parameters(ScreenshotArgs { scale: None }))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        match &result.content[0] {
            ContentBlock::Image(image) => {
                assert_eq!(image.mime_type, "image/png");
                assert!(image.data.len() > 100, "png data looks empty");
            }
            other => panic!("expected an image block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn screenshot_prefers_the_live_page() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        let mut from_server = workspace.to_pages.subscribe();
        workspace.pages.store(1, Ordering::SeqCst);
        let reply_state = workspace.clone();
        tokio::spawn(async move {
            while let Ok(message) = from_server.recv().await {
                let frame: serde_json::Value = serde_json::from_str(&message.text).unwrap();
                if frame["type"] == "screenshot" {
                    let id = frame["id"].as_u64().unwrap();
                    let _ = reply_state
                        .screenshot_replies
                        .send((id, Some("data:image/png;base64,QUJD".into())));
                    break;
                }
            }
        });

        let server = CanvasServer::new(state);
        let result = server
            .canvas_screenshot(Parameters(ScreenshotArgs { scale: Some(1.0) }))
            .await
            .unwrap();
        match &result.content[0] {
            ContentBlock::Image(image) => assert_eq!(image.data, "QUJD"),
            other => panic!("expected an image block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failed_emphasis_does_not_change_document_or_revision() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace("default").await;
        let id = workspace.doc.lock().await.add(
            canvas_core::model::ElementKind::Shape {
                shape: canvas_core::model::ShapeKind::Rectangle,
                rect: canvas_core::geometry::Rect::new(0.0, 0.0, 100.0, 50.0),
                style: canvas_core::model::Style::default(),
                bound_text_id: None,
            },
            1,
        );
        let before = workspace.doc.lock().await.clone();
        let mut messages = workspace.to_pages.subscribe();
        let server = CanvasServer::new(state);
        let result = server
            .canvas_emphasis(Parameters(EmphasisArgs {
                ids: vec![id, "missing".into()],
                mode: "dim".into(),
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert_eq!(*workspace.doc.lock().await, before);
        assert_eq!(workspace.revision.load(Ordering::SeqCst), 0);
        assert!(messages.try_recv().is_err());
    }

    #[tokio::test]
    async fn discussion_verbs_work_as_standalone_tools() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let workspace = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        let server = CanvasServer::new(state.clone());
        let draw = |kind: &str, text: Option<&str>| DrawArgs {
            kind: kind.into(),
            text: text.map(str::to_string),
            name: None,
            glyph: None,
            x: None,
            y: None,
            w: None,
            h: None,
            near: None,
            side: None,
        };
        let first = serde_json::from_str::<serde_json::Value>(&error_text(
            &server
                .canvas_draw(Parameters(draw("sticky", Some("API"))))
                .await
                .unwrap(),
        ))
        .unwrap();
        let first_id = workspace.resolve_id(first["id"].as_str().unwrap()).await;
        server
            .canvas_draw(Parameters(draw("sticky", Some("DB"))))
            .await
            .unwrap();

        let annotated = serde_json::from_str::<serde_json::Value>(&error_text(
            &server
                .canvas_annotate(Parameters(AnnotateArgs {
                    id: first_id.clone(),
                    mark: "cross".into(),
                }))
                .await
                .unwrap(),
        ))
        .unwrap();
        assert_eq!(annotated["ids"].as_array().unwrap().len(), 2);

        let dimmed = serde_json::from_str::<serde_json::Value>(&error_text(
            &server
                .canvas_emphasis(Parameters(EmphasisArgs {
                    ids: vec![first_id.clone()],
                    mode: "dim".into(),
                }))
                .await
                .unwrap(),
        ))
        .unwrap();
        assert_eq!(dimmed["changed"], 1);

        let tidied = serde_json::from_str::<serde_json::Value>(&error_text(
            &server
                .canvas_tidy(Parameters(ElementIdsArgs {
                    ids: vec![first_id.clone()],
                }))
                .await
                .unwrap(),
        ))
        .unwrap();
        assert!(tidied["moved"].as_u64().is_some());

        let before_clear = workspace.doc.lock().await.live_count();
        let cleared = serde_json::from_str::<serde_json::Value>(&error_text(
            &server
                .canvas_clear(Parameters(ElementIdsArgs {
                    ids: vec![first_id.clone()],
                }))
                .await
                .unwrap(),
        ))
        .unwrap();
        assert_eq!(cleared["cleared"], 2, "the sticky and its label");
        assert!(workspace.doc.lock().await.live_count() < before_clear);
        assert!(
            workspace
                .doc
                .lock()
                .await
                .element(&first_id)
                .unwrap()
                .is_deleted
        );
    }

    #[tokio::test]
    async fn tools_act_on_the_workspace_the_user_is_viewing() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let beta = state.workspace("beta").await;
        beta.pages.store(1, Ordering::SeqCst);
        beta.note_page_activity();
        let server = CanvasServer::new(state.clone());

        server
            .canvas_draw(Parameters(DrawArgs {
                kind: "sticky".into(),
                text: Some("on beta".into()),
                name: None,
                glyph: None,
                x: None,
                y: None,
                w: None,
                h: None,
                near: None,
                side: None,
            }))
            .await
            .unwrap();

        assert_eq!(
            beta.doc.lock().await.live_count(),
            2,
            "the sticky and its label land on the current workspace"
        );
        let default = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        assert_eq!(default.doc.lock().await.live_count(), 0);

        let status = server.canvas_status().await.unwrap();
        let body: serde_json::Value = serde_json::from_str(&error_text(&status)).unwrap();
        assert_eq!(body["workspace"], "beta");
        let names: Vec<&str> = body["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|info| info["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"beta"), "{names:?}");
        assert!(names.contains(&"default"), "{names:?}");
    }

    #[tokio::test]
    async fn workspace_create_without_a_page_is_listed_but_does_not_reroute_the_agent() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let server = CanvasServer::new(state.clone());

        let result = server
            .canvas_workspace_create(Parameters(CreateWorkspaceArgs {
                name: "fresh".into(),
            }))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&error_text(&result)).unwrap();
        assert_eq!(body["workspace"], "fresh");
        assert_eq!(body["switched"], false);
        assert_eq!(state.current_workspace().await.name, "default");

        let status = server.canvas_status().await.unwrap();
        let body: serde_json::Value = serde_json::from_str(&error_text(&status)).unwrap();
        let names: Vec<&str> = body["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|info| info["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"fresh"), "{names:?}");
    }

    #[tokio::test]
    async fn workspace_create_rejects_invalid_reserved_and_existing_names() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let server = CanvasServer::new(state);

        for (name, expected) in [("Bad Name", "lowercase"), ("default", "default workspace")] {
            let result = server
                .canvas_workspace_create(Parameters(CreateWorkspaceArgs { name: name.into() }))
                .await
                .unwrap();
            assert_eq!(result.is_error, Some(true), "{name}");
            assert!(error_text(&result).contains(expected), "{name}");
        }
        server
            .canvas_workspace_create(Parameters(CreateWorkspaceArgs {
                name: "fresh".into(),
            }))
            .await
            .unwrap();
        let duplicate = server
            .canvas_workspace_create(Parameters(CreateWorkspaceArgs {
                name: "fresh".into(),
            }))
            .await
            .unwrap();
        assert_eq!(duplicate.is_error, Some(true));
        assert!(error_text(&duplicate).contains("already exists"));
    }

    #[tokio::test]
    async fn workspace_create_opens_the_workspace_and_later_tools_land_there() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let source = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        source.pages.store(1, Ordering::SeqCst);
        source.note_page_activity();
        let mut from_server = source.to_pages.subscribe();
        let page_state = state.clone();
        let page = tokio::spawn(async move {
            let message = from_server.recv().await.unwrap();
            let frame: serde_json::Value = serde_json::from_str(&message.text).unwrap();
            assert_eq!(frame["type"], "workspace_changed");
            assert_eq!(frame["action"], "switch");
            let target = page_state
                .workspace(frame["workspace"].as_str().unwrap())
                .await;
            target.pages.store(1, Ordering::SeqCst);
            target.note_page_activity();
            target
        });
        let server = CanvasServer::new(state.clone());

        let result = server
            .canvas_workspace_create(Parameters(CreateWorkspaceArgs {
                name: "fresh".into(),
            }))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&error_text(&result)).unwrap();
        assert_eq!(body["switched"], true);
        assert_eq!(body["pages_connected"], 1);
        let target = page.await.unwrap();

        server
            .canvas_draw(Parameters(DrawArgs {
                kind: "sticky".into(),
                text: Some("on the new workspace".into()),
                name: None,
                glyph: None,
                x: None,
                y: None,
                w: None,
                h: None,
                near: None,
                side: None,
            }))
            .await
            .unwrap();

        assert_eq!(
            target.doc.lock().await.live_count(),
            2,
            "the sticky and its label land where the page followed"
        );
        assert_eq!(source.doc.lock().await.live_count(), 0);
        assert_eq!(state.current_workspace().await.name, "fresh");
    }

    #[tokio::test]
    async fn with_no_pages_the_default_workspace_is_current() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let beta = state.workspace("beta").await;
        beta.note_page_activity();
        let server = CanvasServer::new(state.clone());

        server
            .canvas_draw(Parameters(DrawArgs {
                kind: "sticky".into(),
                text: Some("headless".into()),
                name: None,
                glyph: None,
                x: None,
                y: None,
                w: None,
                h: None,
                near: None,
                side: None,
            }))
            .await
            .unwrap();

        assert_eq!(
            beta.doc.lock().await.live_count(),
            0,
            "a workspace nobody is watching is never current"
        );
        let default = state.workspace(crate::state::DEFAULT_WORKSPACE).await;
        assert_eq!(default.doc.lock().await.live_count(), 2);
    }
}

#[cfg(test)]
#[path = "conversation_tests.rs"]
mod conversation_tests;
