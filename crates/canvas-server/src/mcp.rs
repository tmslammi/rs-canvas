//! The MCP surface: opencode drives the canvas through these tools.
//!
//! View tools (camera, highlights) are transient; they never touch the
//! document, history or storage. Drawing tools arrive with their tickets.
//! The server is created per MCP session but shares `AppState`. Every tool
//! call acts on the current workspace — the one the user's most recently
//! active page is viewing (ADR-0007) — never on an agent-owned one.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use canvas_core::geometry::Point;
use canvas_core::model::Document;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};

use crate::doc_ops;
use crate::render;
use crate::script::{self, ScriptBeat};
use crate::state::{AppState, PageMessage, Workspace};
use crate::verbs;

const INSTRUCTIONS: &str =
    "rs-canvas is a hand-drawn architecture canvas open in the user's browser. \
Use canvas_status to check that the local bridge and the page are up before acting. \
canvas_read shows the current drawing as compact lines, each starting with a short \
handle (e1, e2, ...); every tool accepts handles or raw ids. canvas_draw creates one \
element and canvas_pen draws a freehand stroke with per-point pressure and ink style \
(width, colour, roughness, dash, opacity, fill/shading). \
Camera and presentation tools move the user's view; they never change the drawing. \
The user may keep several workspaces (named, isolated canvases); every tool acts on \
the one the user is currently viewing — canvas_status names it and lists the rest.";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
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
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ScreenshotArgs {
    /// Live viewport scale, 0.1–4 (default 1); ignored when rendering headlessly.
    #[serde(default)]
    scale: Option<f64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ReadArgs {
    /// Only report what changed after this revision (from an earlier read or
    /// canvas_status). Omit for the full scene graph.
    #[serde(default)]
    since: Option<u64>,
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
    let workspace = state.current_workspace().await;
    let pages = workspace.pages.load(Ordering::SeqCst);
    if pages == 0 {
        return Ok(CallToolResult::error(vec![ContentBlock::text(
            "No canvas page is connected. Ask the user to open http://localhost:5173 \
             (the dev server must be running).",
        )]));
    }
    let _ = workspace.to_pages.send(PageMessage::all(message.to_string()));
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
    }
    let mut seen = std::collections::HashSet::new();
    let mut changed_ids: Vec<String> = Vec::new();
    for record in records {
        for id in &record.changed {
            if seen.insert(id.clone()) {
                changed_ids.push(id.clone());
            }
        }
    }
    let mut live = Vec::new();
    let mut removed = Vec::new();
    for id in &changed_ids {
        match doc.element(id).filter(|element| !element.is_deleted) {
            Some(element) => {
                live.push(canvas_core::describe::describe_element_with(
                    doc, element, ids,
                ));
            }
            None => removed.push(ids.map(id)),
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

#[derive(Clone)]
pub struct CanvasServer {
    state: Arc<AppState>,
}

impl CanvasServer {
    /// The workspace this call acts on: the one the user is looking at.
    async fn current(&self) -> Arc<Workspace> {
        self.state.current_workspace().await
    }

    /// Apply an edit to the workspace mirror. On success the change is
    /// recorded and broadcast as an element patch; on failure the document
    /// and the change log stay untouched. Returns the edit's own result.
    async fn apply_edit<T, E>(
        &self,
        workspace: &Workspace,
        source: &str,
        run: impl FnOnce(&mut Document) -> Result<T, E>,
    ) -> Result<T, E> {
        let (result, change) = {
            let mut doc = workspace.doc.lock().await;
            let before = doc.clone();
            let result = run(&mut doc);
            let change = if result.is_ok() {
                Some(workspace.record_change(&before, &doc, source).await)
            } else {
                None
            };
            (result, change)
        };
        if let Some((revision, put)) = change {
            workspace.publish_patch(revision, put).await;
        }
        result
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
        Self { state }
    }

    #[tool(
        description = "Report canvas server status: whether the bridge is reachable, \
                       which workspace is current (the one the user is viewing), how \
                       many browser pages are connected to it, the mirrored document's \
                       element count, and the other workspaces that exist."
    )]
    async fn canvas_status(&self) -> Result<CallToolResult, McpError> {
        let workspace = self.current().await;
        let doc = workspace.doc.lock().await;
        let revision = workspace.revision.load(Ordering::SeqCst);
        let hints = workspace.view_hint.lock().await.clone();
        let settings = self.state.current_settings().await;
        let workspaces = self.state.list_workspaces().await;
        let body = serde_json::json!({
            "server": "rs-canvas",
            "version": env!("CARGO_PKG_VERSION"),
            "workspace": workspace.name,
            "pages_connected": workspace.pages.load(Ordering::SeqCst),
            "uptime_ms": self.state.started.elapsed().as_millis() as u64,
            "revision": revision,
            "document": {
                "elements": doc.ordered_live().len(),
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
        description = "Read the current drawing as compact lines: `handle kind x,y wxh`, \
                       then membership and labels. Every line starts with a short handle \
                       (e1, e2, ...) that all id-taking tools accept. Read it before \
                       drawing to learn handles and available space. Pass `since` with a \
                       revision from an earlier read or canvas_status to hear only what \
                       changed while you were away."
    )]
    async fn canvas_read(
        &self,
        Parameters(args): Parameters<ReadArgs>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.current().await;
        let revision = workspace.revision.load(Ordering::SeqCst);
        let doc = workspace.doc.lock().await;
        let text = match args.since {
            None => {
                let mut aliases = workspace.aliases.lock().await;
                format!(
                    "revision {revision}\n{}",
                    canvas_core::describe::describe_with(&doc, &mut *aliases)
                )
            }
            Some(since) => {
                let records = workspace.changes_since(since).await;
                let mut aliases = workspace.aliases.lock().await;
                describe_changes(&doc, &records, since, revision, &mut *aliases)
            }
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
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
                       presentation fields the whole script lands at once. Placement is \
                       automatic and never overlaps; nothing is drawn if any beat fails.")]
    async fn canvas_script(
        &self,
        Parameters(args): Parameters<ScriptArgs>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.current().await;
        let beat_count = args.beats.len();
        let mut beats = args.beats;
        for beat in &mut beats {
            if let Some(draw) = &mut beat.draw {
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
            .apply_edit(&workspace, "agent", |doc| {
                script::execute(doc, &beats, now_ms())
            })
            .await;
        let outcome = match outcome {
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
                "elements": outcome.elements,
                "beats": beat_count,
                "played": played,
                "cancelled": cancelled,
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
            .apply_edit(&workspace, "agent", |doc| {
                verbs::tidy(doc, &ids, now_ms())
            })
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
                       viewers see the deletion). Frames take their children and containers \
                       take their labels. Use it to clear away a discussion that has moved on."
    )]
    async fn canvas_clear(
        &self,
        Parameters(args): Parameters<ElementIdsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.current().await;
        let ids = workspace.resolve_ids(&args.ids).await;
        let result = self
            .apply_edit(&workspace, "agent", |doc| {
                verbs::clear(doc, &ids, now_ms())
            })
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
        description = "Look at the canvas: returns a PNG image of what the user is looking \
                       at right now, camera included, after any in-flight explanation \
                       settles. With no page open it renders the whole drawing headlessly. \
                       Use it to check your own layout, spacing, labels and overlaps."
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
        let mut message = serde_json::json!({ "type": "view", "action": "focus" });
        if let Some(id) = args.id {
            message["id"] = self.current().await.resolve_id(&id).await.into();
        }
        if let Some(pad) = args.pad {
            message["pad"] = pad.into();
        }
        if let Some(animate) = args.animate {
            message["animate"] = animate.into();
        }
        deliver_view(&self.state, message).await
    }

    #[tool(
        description = "Pulse a fading highlight around one element without moving the \
                       camera. Use it to point at something while explaining a drawing."
    )]
    async fn canvas_point(
        &self,
        Parameters(args): Parameters<PointArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = self.current().await.resolve_id(&args.id).await;
        let message = serde_json::json!({
            "type": "view",
            "action": "point",
            "id": id,
        });
        deliver_view(&self.state, message).await
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
        let message = serde_json::json!({
            "type": "view",
            "action": "zoom",
            "mode": args.mode,
        });
        deliver_view(&self.state, message).await
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
        let result = server.canvas_status().await.unwrap();
        let body: serde_json::Value = serde_json::from_str(&error_text(&result)).unwrap();
        assert_eq!(body["server"], "rs-canvas");
        assert_eq!(body["pages_connected"], 0);
        assert_eq!(body["document"]["elements"], 0);
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
        let mut from_server = state.to_pages.subscribe();
        state.pages.store(1, Ordering::SeqCst);
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
        let mut from_server = state.to_pages.subscribe();
        state.pages.store(1, Ordering::SeqCst);
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
        let mut from_server = state.to_pages.subscribe();
        state.pages.store(1, Ordering::SeqCst);
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
        let mut from_server = state.to_pages.subscribe();
        state.pages.store(1, Ordering::SeqCst);
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
        assert!(state.doc.lock().await.live_count() == 2);

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
        let mut from_server = state.to_pages.subscribe();
        state.pages.store(1, Ordering::SeqCst);
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
        let id = state.resolve_id(created["id"].as_str().unwrap()).await;
        let doc = state.doc.lock().await;
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
        state.pages.store(1, Ordering::SeqCst);
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
                .canvas_read(Parameters(ReadArgs { since: None }))
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
            .canvas_read(Parameters(ReadArgs { since: None }))
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
                .canvas_read(Parameters(ReadArgs { since: None }))
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
        let mut doc = state.doc.lock().await;
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
        state.record_change(&before, &doc, "page").await;
        drop(doc);
        let handle = state.aliases.lock().await.handle(&id);

        let report = error_text(
            &server
                .canvas_read(Parameters(ReadArgs {
                    since: Some(revision),
                }))
                .await
                .unwrap(),
        );
        assert!(report.contains("(agent)"), "{report}");
        assert!(report.contains("(page)"), "{report}");
        assert!(report.contains("changed now:"), "{report}");
        assert!(report.contains(&handle), "{report}");
    }

    #[tokio::test]
    async fn status_reports_revision_and_ephemeral_view_hints() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        *state.view_hint.lock().await = Some(crate::state::ViewHint {
            camera: Some(serde_json::json!({ "x": 10.0, "y": 20.0, "zoom": 1.5 })),
            viewport: Some(serde_json::json!({ "w": 800.0, "h": 600.0 })),
            selection: vec!["db".into()],
        });
        let server = CanvasServer::new(state);
        let result = server.canvas_status().await.unwrap();
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
        state
            .apply_settings_patch(&serde_json::json!({ "camera": "off" }))
            .await
            .unwrap();
        let mut from_server = state.to_pages.subscribe();
        state.pages.store(1, Ordering::SeqCst);
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
        state
            .apply_settings_patch(&serde_json::json!({ "pacing": "fast" }))
            .await
            .unwrap();
        let mut from_server = state.to_pages.subscribe();
        state.pages.store(1, Ordering::SeqCst);
        let ack_state = state.clone();
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
            .canvas_script(Parameters(ScriptArgs { beats }))
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
        let server = CanvasServer::new(state.clone());
        let beats: Vec<ScriptBeat> = serde_json::from_value(serde_json::json!([
            { "draw": { "as": "client", "kind": "sticky", "text": "Client" } },
            { "draw": { "as": "db", "kind": "sticky", "text": "DB" } },
            { "connect": { "from": "client", "to": "db", "label": "SQL" } }
        ]))
        .unwrap();

        let result = server
            .canvas_script(Parameters(ScriptArgs { beats }))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&error_text(&result)).unwrap();
        assert!(body["created"]["client"].is_string());
        assert!(body["created"]["db"].is_string());
        assert_eq!(body["beats"], 3);
        assert_eq!(
            state.doc.lock().await.live_count(),
            6,
            "2 stickies, 2 labels, 1 arrow, 1 arrow label"
        );
    }

    #[tokio::test]
    async fn a_failed_script_leaves_the_mirror_untouched() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let server = CanvasServer::new(state.clone());
        let beats: Vec<ScriptBeat> = serde_json::from_value(serde_json::json!([
            { "draw": { "as": "a", "kind": "sticky" } },
            { "connect": { "from": "a", "to": "ghost" } }
        ]))
        .unwrap();

        let result = server
            .canvas_script(Parameters(ScriptArgs { beats }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(error_text(&result).contains("beat 2"));
        assert_eq!(state.doc.lock().await.live_count(), 0);
    }

    #[tokio::test]
    async fn canvas_script_waits_for_the_page_to_finish_playing() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
        let mut from_server = state.to_pages.subscribe();
        state.pages.store(1, Ordering::SeqCst);
        let ack_state = state.clone();
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
            .canvas_script(Parameters(ScriptArgs { beats }))
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
            .canvas_script(Parameters(ScriptArgs { beats }))
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
        let mut from_server = state.to_pages.subscribe();
        state.pages.store(1, Ordering::SeqCst);
        let reply_state = state.clone();
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
    async fn discussion_verbs_work_as_standalone_tools() {
        let state = Arc::new(AppState::new(std::path::PathBuf::from("."), None, true));
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
        let first_id = state.resolve_id(first["id"].as_str().unwrap()).await;
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

        let before_clear = state.doc.lock().await.live_count();
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
        assert!(state.doc.lock().await.live_count() < before_clear);
        assert!(
            state
                .doc
                .lock()
                .await
                .element(&first_id)
                .unwrap()
                .is_deleted
        );
    }
}
