//! The WebSocket bridge to the open page.
//!
//! Steady state carries element-granular patches both ways: a page's gesture
//! becomes one patch, an agent edit becomes one patch, and each is merged by
//! `canvas-core`'s per-element LWW rules. Full snapshots remain for connect
//! and for resync after a broadcast gap. View commands and presentation
//! frames ride the same socket.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;

use canvas_core::model::Element;

use crate::state::{now_ms, AppState};

pub async fn handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| connected(socket, state))
}

async fn connected(mut socket: WebSocket, state: Arc<AppState>) {
    let id = state.new_page_id();
    let pages = state.pages.fetch_add(1, Ordering::SeqCst) + 1;
    tracing::info!(pages, "page connected");

    let settings = state.current_settings().await;
    let hello = serde_json::json!({
        "type": "hello",
        "server": "rs-canvas",
        "version": env!("CARGO_PKG_VERSION"),
        "sync": state.sync,
        "settings": settings,
    })
    .to_string();

    // Hand the page the current mirror; it reconciles with its local
    // document, so an autosave and the server copy never clobber each other.
    let ready = if state.sync {
        let snapshot = state.snapshot_frame().await;
        socket.send(Message::Text(hello.into())).await.is_ok()
            && socket.send(Message::Text(snapshot.into())).await.is_ok()
    } else {
        socket.send(Message::Text(hello.into())).await.is_ok()
    };

    if ready {
        let mut to_pages = state.to_pages.subscribe();
        let mut page_sync = crate::state::PageSync::default();
        loop {
            tokio::select! {
                outgoing = to_pages.recv() => match outgoing {
                    Ok(message) => {
                        // A patch never echoes back to the page that sent it.
                        if message.except == Some(id) {
                            continue;
                        }
                        if socket.send(Message::Text(message.text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        // Frames were dropped: only a full snapshot can repair
                        // the page, so send one now.
                        let snapshot = state.snapshot_frame().await;
                        if socket.send(Message::Text(snapshot.into())).await.is_err() {
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
                        if let Ok(frame) = serde_json::from_str::<Value>(&text) {
                            handle_frame(&state, &mut page_sync, id, &frame).await;
                        }
                    }
                    Some(Ok(_)) => {}
                },
            }
        }
    }

    let remaining = state.pages.fetch_sub(1, Ordering::SeqCst) - 1;
    state.release_when_idle(remaining).await;
    tracing::info!(pages = remaining, "page disconnected");
}

/// One incoming page frame. Patches merge per element; snapshots go through
/// the full reconcile, with `PageSync` synthesizing tombstones for elements
/// the page purged (undo of a creation).
async fn handle_frame(
    state: &Arc<AppState>,
    page_sync: &mut crate::state::PageSync,
    id: u64,
    frame: &Value,
) {
    match frame.get("type").and_then(|value| value.as_str()) {
        Some("snapshot") => {
            let Some(doc) = frame.get("doc") else {
                return;
            };
            let Ok(mut incoming) = canvas_core::serialization::from_value(doc.clone()) else {
                return;
            };
            // The page's doc as it actually is: purged elements are absent,
            // so diffing against the previous snapshot is what makes a
            // deletion expressible on the wire.
            let page_document = incoming.clone();
            page_sync.prepare(&mut incoming, now_ms());
            state.absorb_document(&incoming).await;
            page_sync.commit(page_document);
        }
        Some("patch") => {
            let Some(put) = frame.get("put") else {
                return;
            };
            if let Ok(elements) = serde_json::from_value::<Vec<Element>>(put.clone()) {
                state.absorb_patch(elements, id).await;
            }
        }
        Some("present_done") => {
            if let Some(id) = frame.get("id").and_then(|v| v.as_u64()) {
                let cancelled = frame
                    .get("cancelled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let _ = state.present_acks.send((id, cancelled));
            }
        }
        Some("screenshot_done") => {
            if let Some(id) = frame.get("id").and_then(|v| v.as_u64()) {
                let data_url = frame
                    .get("data_url")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let _ = state.screenshot_replies.send((id, data_url));
            }
        }
        Some("hint") => {
            if let Ok(hint) = serde_json::from_value::<crate::state::ViewHint>(frame.clone()) {
                *state.view_hint.lock().await = Some(hint);
            }
        }
        Some("settings") => {
            if let Some(patch) = frame.get("patch") {
                if let Err(error) = state.apply_settings_patch(patch).await {
                    tracing::warn!(%error, "rejected settings patch");
                }
            }
        }
        _ => {}
    }
}
