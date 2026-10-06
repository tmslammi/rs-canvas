//! The WebSocket bridge to the open page.
//!
//! Carries view commands (agent to page), document snapshots (both ways) and
//! later the agent's presentation beats. Snapshots are merged through
//! `canvas-core`'s reconcile rules, so both sides can edit at once.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use tokio::sync::broadcast::error::RecvError;

use crate::state::AppState;

pub async fn handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| connected(socket, state))
}

async fn connected(mut socket: WebSocket, state: Arc<AppState>) {
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
        let snapshot = state.document_json().await;
        socket.send(Message::Text(hello.into())).await.is_ok()
            && socket
                .send(Message::Text(AppState::snapshot_frame(&snapshot).into()))
                .await
                .is_ok()
    } else {
        socket.send(Message::Text(hello.into())).await.is_ok()
    };

    if ready {
        let mut to_pages = state.to_pages.subscribe();
        let mut page_sync = crate::state::PageSync::default();
        loop {
            tokio::select! {
                outgoing = to_pages.recv() => match outgoing {
                    Ok(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                },
                incoming = socket.recv() => match incoming {
                    None | Some(Ok(Message::Close(_))) | Some(Err(_)) => break,
                    Some(Ok(Message::Text(text))) => {
                        if !state.sync {
                            continue;
                        }
                        if let Ok(frame) = serde_json::from_str::<serde_json::Value>(&text) {
                            match frame.get("type").and_then(|v| v.as_str()) {
                                Some("snapshot") => {
                                    if let Some(json) = frame.get("json").and_then(|v| v.as_str())
                                    {
                                        if let Ok(mut incoming) =
                                            canvas_core::serialization::from_json(json)
                                        {
                                            // The page's doc as it actually is: purged
                                            // elements are absent, so diffing against the
                                            // previous snapshot is what makes a deletion
                                            // expressible on the wire.
                                            let page_document = incoming.clone();
                                            page_sync
                                                .prepare(&mut incoming, crate::state::now_ms());
                                            if state.absorb_document(&incoming).await {
                                                state.publish_document().await;
                                            }
                                            page_sync.commit(page_document);
                                        }
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
                                    if let Ok(hint) =
                                        serde_json::from_value::<crate::state::ViewHint>(
                                            frame.clone(),
                                        )
                                    {
                                        *state.view_hint.lock().await = Some(hint);
                                    }
                                }
                                Some("settings") => {
                                    if let Some(patch) = frame.get("patch") {
                                        if let Err(error) =
                                            state.apply_settings_patch(patch).await
                                        {
                                            tracing::warn!(%error, "rejected settings patch");
                                        }
                                    }
                                }
                                _ => {}
                            }
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
