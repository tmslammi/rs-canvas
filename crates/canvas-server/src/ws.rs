//! The WebSocket bridge to the open page.
//!
//! Steady state carries element-granular patches both ways: a page's gesture
//! becomes one patch, an agent edit becomes one patch, and each is merged by
//! `canvas-core`'s per-element LWW rules. Full snapshots remain for connect
//! and for resync after a broadcast gap. View commands and presentation
//! frames ride the same socket.
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

use canvas_core::model::Element;
use canvas_core::wire;

use crate::state::{
    now_ms, valid_workspace_name, AppState, PageMessage, Workspace, DEFAULT_WORKSPACE,
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
        let mut page_sync = crate::state::PageSync::default();
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
                            let synced = handle_frame(&state, &workspace, &mut page_sync, id, &frame).await;
                            if first_sync_pending {
                                if let Some(mirror_leads) = synced {
                                    first_sync_pending = false;
                                    fallback_armed = false;
                                    if mirror_leads {
                                        // The page lacks live elements the
                                        // server kept while it was away: the
                                        // upload just merged in, so one
                                        // snapshot covers both worlds. A
                                        // converged page gets no download.
                                        if !send_snapshot(&mut socket, &workspace, binary_wire).await {
                                            break;
                                        }
                                    }
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
                        let synced =
                            handle_binary_frame(&workspace, &mut page_sync, id, &bytes).await;
                        if first_sync_pending {
                            if let Some(mirror_leads) = synced {
                                first_sync_pending = false;
                                fallback_armed = false;
                                if mirror_leads
                                    && !send_snapshot(&mut socket, &workspace, binary_wire).await
                                {
                                    break;
                                }
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

/// One incoming binary frame. Returns `Some(mirror_leads)` for snapshots, with
/// the same meaning as the JSON handler.
async fn handle_binary_frame(
    workspace: &Arc<Workspace>,
    page_sync: &mut crate::state::PageSync,
    id: u64,
    bytes: &[u8],
) -> Option<bool> {
    match wire::kind(bytes) {
        Ok(wire::FrameKind::Snapshot) => {
            let Ok((_, incoming)) = wire::decode_snapshot(bytes) else {
                tracing::warn!("ignoring malformed binary snapshot frame");
                return None;
            };
            Some(absorb_page_snapshot(workspace, page_sync, id, incoming).await)
        }
        Ok(wire::FrameKind::Patch) => {
            match wire::decode_patch(bytes) {
                Ok((_, put)) => {
                    workspace.absorb_patch(put, id).await;
                }
                Err(error) => tracing::warn!(%error, "ignoring malformed binary patch frame"),
            }
            None
        }
        Err(error) => {
            tracing::warn!(%error, "ignoring malformed binary frame");
            None
        }
    }
}

/// Merge one page snapshot into the mirror and update the per-connection
/// purge base. Returns whether the mirror holds live elements the page lacks,
/// which means the page needs a download.
async fn absorb_page_snapshot(
    workspace: &Arc<Workspace>,
    page_sync: &mut crate::state::PageSync,
    id: u64,
    mut incoming: canvas_core::Document,
) -> bool {
    // The page's doc as it actually is: purged elements are absent, so
    // diffing against the previous snapshot is what makes a deletion
    // expressible on the wire.
    let page_document = incoming.clone();
    // Does the mirror hold any live element the page lacks? It does when the
    // page left with content and the server carried on (agent edits with no
    // page connected) — such a page must catch up with a download. An
    // in-sync boot already holds everything and stays silent; deletions made
    // while the page was away reach it through the patch-correction path.
    let mirror_leads = {
        let mirror = workspace.doc.lock().await;
        mirror
            .elements
            .values()
            .filter(|element| !element.is_deleted)
            .any(|element| page_document.element(&element.id).is_none())
    };
    page_sync.prepare(&mut incoming, now_ms());
    workspace.absorb_document(&incoming, Some(id)).await;
    page_sync.commit(page_document);
    mirror_leads
}

/// One incoming page frame. Patches merge per element; snapshots go through
/// the full reconcile, with `PageSync` synthesizing tombstones for elements
/// the page purged (undo of a creation).
///
/// Returns `Some(mirror_leads)` for snapshot frames — whether the mirror
/// holds live elements the page's upload lacks — so the connection can skip
/// the initial download when the page is already in sync; `None` for
/// everything else.
async fn handle_frame(
    state: &Arc<AppState>,
    workspace: &Arc<Workspace>,
    page_sync: &mut crate::state::PageSync,
    id: u64,
    frame: &Value,
) -> Option<bool> {
    match frame.get("type").and_then(|value| value.as_str()) {
        Some("snapshot") => {
            let doc = frame.get("doc")?;
            let Ok(incoming) = canvas_core::serialization::from_value(doc.clone()) else {
                return None;
            };
            Some(absorb_page_snapshot(workspace, page_sync, id, incoming).await)
        }
        Some("patch") => {
            let put = frame.get("put")?;
            if let Ok(elements) = serde_json::from_value::<Vec<Element>>(put.clone()) {
                workspace.absorb_patch(elements, id).await;
            }
            None
        }
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
