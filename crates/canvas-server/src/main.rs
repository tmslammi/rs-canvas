//! The rs-canvas server: static app, WebSocket bridge to the open page, and
//! the MCP endpoint opencode uses to drive the canvas.
//!
//! Binding is `127.0.0.1` only: the agent bridge is a local dev surface, not
//! a networked service.

mod aliases;
mod doc_ops;
mod mcp;
mod render;
mod script;
mod state;
mod verbs;
mod ws;

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, HeaderValue};
use axum::routing::get;
use axum::{Json, Router};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::state::{AppState, DEFAULT_WORKSPACE};

struct Args {
    web: PathBuf,
    port: u16,
    doc: Option<PathBuf>,
    sync: bool,
}

fn parse_args() -> Args {
    let mut web = PathBuf::from(".");
    let mut port = 5173;
    let mut doc = None;
    let mut sync = true;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--web" => {
                if let Some(value) = args.next() {
                    web = PathBuf::from(value);
                }
            }
            "--port" => {
                if let Some(value) = args.next() {
                    port = value.parse().expect("--port must be a number");
                }
            }
            "--doc" => doc = args.next().map(PathBuf::from),
            "--no-sync" => sync = false,
            other => {
                panic!("unknown argument {other:?} (expected --web, --port, --doc or --no-sync)")
            }
        }
    }
    Args {
        web,
        port,
        doc,
        sync,
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "canvas_server=info,tower_http=warn".into()),
        )
        .init();

    let args = parse_args();
    let state = Arc::new(AppState::new(args.web.clone(), args.doc.clone(), args.sync));
    // Create the default workspace up front so its document is loaded before
    // anyone asks; every other workspace is born when a page names it.
    state.workspace(DEFAULT_WORKSPACE).await;
    state.load_settings().await;

    // The page's document is authoritative; the factory runs per MCP session
    // and every instance shares the same process-wide state.
    let factory_state = state.clone();
    let mcp_service: StreamableHttpService<mcp::CanvasServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(mcp::CanvasServer::new(factory_state.clone())),
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default(),
        );

    // Serve the app as-is; a stale wasm or main.js in the browser looks
    // exactly like "the new feature isn't there".
    let no_store = SetResponseHeaderLayer::overriding(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );

    let app = Router::new()
        .route("/ws", get(ws::handler))
        .route("/api/workspaces", get(workspaces))
        .nest_service("/mcp", mcp_service)
        .fallback_service(ServeDir::new(&args.web))
        .layer(no_store)
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port))
        .await
        .expect("cannot bind the canvas server port");
    tracing::info!("rs-canvas: http://localhost:{} (MCP at /mcp)", args.port);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("canvas server failed");

    // The debounced writers may still owe a write.
    state.persist_all().await;
}

/// The workspace listing the page's switcher shows: live workspaces plus
/// persisted ones not opened this run.
async fn workspaces(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "workspaces": state.list_workspaces().await }))
}

/// Ctrl-C or SIGTERM; either ends the process gracefully so the debounced
/// mirror write lands before exit.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}
