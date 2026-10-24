//! The WebSocket bridge to the open page.
//!
//! Steady state carries element-granular patches both ways: a page's gesture
//! becomes one patch, an agent edit becomes one patch, and each is merged by
//! `canvas-core`'s per-element LWW rules. A page uploads a snapshot on connect
//! with the server revision it was based on and its explicit purge ledger;
//! the workspace returns any elements the upload lost or omitted as a
//! correction patch. Full snapshots remain for a lagged broadcast or a client
//! that never uploads. View commands and presentation frames ride the same
//! socket.
//!
//! A connection belongs to exactly one workspace, named by the `?ws=` query
//! parameter (ADR-0007); without one it lands on `default`.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;

use canvas_core::sync::PageFrame;
use canvas_core::wire;

use crate::state::{
    valid_workspace_name, AppState, PageFrameOutcome, PageMessage, Workspace, DEFAULT_WORKSPACE,
};

/// `wire=json` suppresses the binary capability advertisement so a test (or a
/// deliberately conservative client) can exercise the legacy JSON path
/// against this server. An old server simply never advertises in the first
/// place; the page falls back the same way.
#[derive(serde::Deserialize)]
pub struct WsQuery {
    ws: Option<String>,
    wire: Option<String>,
}

pub async fn handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Query(query): Query<WsQuery>,
) -> Response {
    let name = query
        .ws
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| DEFAULT_WORKSPACE.to_string());
    if !valid_workspace_name(&name) {
        return (
            StatusCode::BAD_REQUEST,
            format!("invalid workspace name {name:?}; use lowercase letters, digits and dashes"),
        )
            .into_response();
    }
    let binary_allowed = query.wire.as_deref() != Some("json");
    let workspace = state.workspace(&name).await;
    ws.on_upgrade(move |socket| connected(socket, state, workspace, binary_allowed))
}

async fn connected(
    mut socket: WebSocket,
    state: Arc<AppState>,
    workspace: Arc<Workspace>,
    binary_allowed: bool,
) {
    let id = state.new_page_id();
    if !state.register_page(&workspace).await {
        return;
    }
    let pages = workspace.pages.load(Ordering::SeqCst);
    tracing::info!(pages, workspace = %workspace.name, "page connected");
    // Subscribe before capturing initial state; concurrent edits/notes can then
    // be replayed rather than disappearing between hello and subscription.
    let mut to_pages = workspace.to_pages.subscribe();

    let settings = state.current_settings().await;
    let mut hello = serde_json::json!({
        "type": "hello",
        "server": "rs-canvas",
        "version": env!("CARGO_PKG_VERSION"),
        "sync": state.sync,
        "workspace": workspace.name,
        "settings": settings,
        "discussion": workspace.discussion.lock().await.clone(),
        "discussion_persisted": workspace.doc_path.is_some(),
    });
    if binary_allowed {
        // Capability negotiation: a page that sees this sends document frames
        // as binary; a page that does not keeps sending and receiving JSON.
        hello["binary_sync"] = wire::VERSION.into();
    }
    let hello = hello.to_string();

    // The page uploads its document first (every boot sends a snapshot);
    // the server only sends its mirror back when the page actually needs
    // it — an in-sync reload exchanges hello plus a small upload, never a
    // full download. Previously every connect downloaded the whole mirror
    // even when the page already held it (tens of MB on huge boards).
    let ready = socket.send(Message::Text(hello.into())).await.is_ok();

    if ready {
        // The connection starts on the JSON path; its first binary document
        // frame promotes it. New pages wait for hello before uploading, so
        // this happens before any document frame the server sends them.
        let mut binary_wire = false;
        let mut binary_guard = None;
        // First upload not yet reconciled; a fallback timer below covers
        // clients that connect without ever uploading.
        let mut first_sync_pending = state.sync;
        let fallback = tokio::time::sleep(std::time::Duration::from_secs(10));
        tokio::pin!(fallback);
        // Avoid an allocation per loop lap: only needed until first sync.
        let mut fallback_armed = state.sync;
        loop {
            tokio::select! {
                outgoing = to_pages.recv() => match outgoing {
                    Ok(message) => {
                        // A patch never echoes back to the page that sent it.
                        if message.except == Some(id) {
                            continue;
                        }
                        if !send_page_message(&mut socket, message, binary_wire).await {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        // Frames were dropped: only a full snapshot can repair
                        // the page, so send one now.
                        if !send_snapshot(&mut socket, &workspace, binary_wire).await {
                            break;
                        }
                        let discussion = serde_json::json!({
                            "type": "discussion", "discussion": workspace.discussion.lock().await.clone(),
                            "discussion_persisted": workspace.doc_path.is_some(),
                        }).to_string();
                        if socket.send(Message::Text(discussion.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Closed) => break,
                },
                incoming = socket.recv() => match incoming {
                    None | Some(Ok(Message::Close(_))) | Some(Err(_)) => break,
                    Some(Ok(Message::Text(text))) => {
                        if !state.sync {
                            continue;
                        }
                        if workspace.retired.load(Ordering::SeqCst) { break; }
                        if let Ok(frame) = serde_json::from_str::<Value>(&text) {
                            workspace.note_page_activity();
                            let synced = handle_frame(&state, &workspace, id, &frame).await;
                            if let Some(outcome) = synced {
                                if first_sync_pending {
                                    first_sync_pending = false;
                                    fallback_armed = false;
                                }
                                if !send_revision_ack(&mut socket, outcome.revision).await {
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if !state.sync || !binary_allowed {
                            continue;
                        }
                        if workspace.retired.load(Ordering::SeqCst) { break; }
                        if !binary_wire {
                            // Set the local flag before the workspace count:
                            // a broadcast that sees the count must also see a
                            // connection ready to read the binary frame.
                            binary_wire = true;
                            binary_guard = Some(workspace.promote_binary_page());
                        }
                        workspace.note_page_activity();
                        let synced = handle_binary_frame(&workspace, id, &bytes).await;
                        if let Some(outcome) = synced {
                            if first_sync_pending {
                                first_sync_pending = false;
                                fallback_armed = false;
                            }
                            if !send_revision_ack(&mut socket, outcome.revision).await {
                                break;
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                },
                _ = &mut fallback, if fallback_armed => {
                    // No upload arrived: fall back to the legacy eager
                    // snapshot so old or quiet clients still converge.
                    fallback_armed = false;
                    first_sync_pending = false;
                    if state.sync && !send_snapshot(&mut socket, &workspace, binary_wire).await {
                        break;
                    }
                },
            }
        }
        drop(binary_guard);
    }

    workspace.pages.fetch_sub(1, Ordering::SeqCst);
    state.release_workspace(&workspace).await;
    let remaining = workspace.pages.load(Ordering::SeqCst);
    tracing::info!(pages = remaining, workspace = %workspace.name, "page disconnected");
}

/// Send one broadcast frame in an encoding this page can read. A binary page
/// falls back to JSON for a frame encoded before it was promoted; a legacy
/// page always gets JSON.
async fn send_page_message(socket: &mut WebSocket, message: PageMessage, binary: bool) -> bool {
    if binary {
        if let Some(bytes) = message.binary {
            return socket.send(Message::Binary(bytes.into())).await.is_ok();
        }
    }
    match message.json {
        Some(json) => socket.send(Message::Text(json.into())).await.is_ok(),
        None => true,
    }
}

/// Send the mirror snapshot as a binary frame to a capable page, or JSON to a
/// legacy one. A binary encoding failure falls back to JSON rather than
/// dropping the resync.
async fn send_snapshot(socket: &mut WebSocket, workspace: &Workspace, binary: bool) -> bool {
    if binary {
        match workspace.snapshot_frame_bytes().await {
            Ok(bytes) => return socket.send(Message::Binary(bytes.into())).await.is_ok(),
            Err(error) => {
                tracing::warn!(%error, "could not encode binary snapshot; sending JSON instead");
            }
        }
    }
    let snapshot = workspace.snapshot_frame().await;
    socket.send(Message::Text(snapshot.into())).await.is_ok()
}

/// Acknowledge an applied snapshot upload with the mirror revision, so the
/// page's next snapshot carries an accurate base even when no correction was
/// needed (and therefore no document frame was sent back).
async fn send_revision_ack(socket: &mut WebSocket, revision: u64) -> bool {
    let frame = serde_json::json!({ "type": "synced", "rev": revision }).to_string();
    socket.send(Message::Text(frame.into())).await.is_ok()
}

/// One incoming binary frame. Snapshot uploads return their outcome so the
/// connection can finish first-sync bookkeeping; the workspace itself has
/// already broadcast any corrections.
async fn handle_binary_frame(
    workspace: &Arc<Workspace>,
    id: u64,
    bytes: &[u8],
) -> Option<PageFrameOutcome> {
    match PageFrame::from_binary(bytes) {
        Ok(frame) => {
            let snapshot = frame.is_snapshot();
            let outcome = workspace.apply_page_frame(id, frame).await;
            snapshot.then_some(outcome)
        }
        Err(error) => {
            tracing::warn!(%error, "ignoring malformed binary frame");
            None
        }
    }
}

/// One incoming page frame. Both document frames route through
/// `Workspace::apply_page_frame`, the same seam tests drive without a socket.
/// Snapshot uploads return their outcome so first-sync bookkeeping can finish;
/// control frames return `None`.
async fn handle_frame(
    state: &Arc<AppState>,
    workspace: &Arc<Workspace>,
    id: u64,
    frame: &Value,
) -> Option<PageFrameOutcome> {
    if let Some(page_frame) = PageFrame::from_json(frame) {
        let snapshot = page_frame.is_snapshot();
        let outcome = workspace.apply_page_frame(id, page_frame).await;
        return snapshot.then_some(outcome);
    }
    match frame.get("type").and_then(|value| value.as_str()) {
        Some("present_done") => {
            if let Some(id) = frame.get("id").and_then(|v| v.as_u64()) {
                let cancelled = frame
                    .get("cancelled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let _ = workspace.present_acks.send((id, cancelled));
            }
            None
        }
        Some("screenshot_done") => {
            if let Some(id) = frame.get("id").and_then(|v| v.as_u64()) {
                let data_url = frame
                    .get("data_url")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let _ = workspace.screenshot_replies.send((id, data_url));
            }
            None
        }
        Some("hint") => {
            if let Ok(hint) = serde_json::from_value::<crate::state::ViewHint>(frame.clone()) {
                *workspace.view_hint.lock().await = Some(hint);
            }
            None
        }
        Some("settings") => {
            if let Some(patch) = frame.get("patch") {
                if let Err(error) = state.apply_settings_patch(patch).await {
                    tracing::warn!(%error, "rejected settings patch");
                }
            }
            None
        }
        _ => None,
    }
}
