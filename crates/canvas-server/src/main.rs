//! The rs-canvas server: static app, WebSocket bridge to the open page, and
//! the MCP endpoint agent clients use to drive the canvas.
//!
//! Binding is `127.0.0.1` only: the agent bridge is a local dev surface, not
//! a networked service.

mod aliases;
mod discussion;
mod doc_ops;
mod document;
#[cfg(feature = "embed-web")]
mod embedded;
mod mcp;
mod page_hub;
mod render;
mod script;
mod state;
mod verbs;
mod workspace_api;
mod workspace_store;
mod ws;

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, HeaderValue};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::state::{AppState, DEFAULT_WORKSPACE};

const DEFAULT_PORT: u16 = 5173;

struct Args {
    /// `None` uses the embedded front end in release builds, or the working
    /// directory in development builds.
    web: Option<PathBuf>,
    port: u16,
    doc: Option<PathBuf>,
    sync: bool,
    /// `None` opens a browser only for embedded (installed) builds.
    open: Option<bool>,
}

fn parse_args() -> Args {
    let mut args = Args {
        web: None,
        port: DEFAULT_PORT,
        doc: None,
        sync: true,
        open: None,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                print_version();
                std::process::exit(0);
            }
            "--web" => args.web = Some(PathBuf::from(next_value(&mut iter, "--web"))),
            "--port" => {
                let raw = next_value(&mut iter, "--port");
                args.port = raw.parse().unwrap_or_else(|_| {
                    eprintln!("rs-canvas: --port must be a number, got {raw:?}");
                    std::process::exit(2);
                });
            }
            "--doc" => args.doc = Some(PathBuf::from(next_value(&mut iter, "--doc"))),
            "--no-sync" => args.sync = false,
            "--open" => args.open = Some(true),
            "--no-open" => args.open = Some(false),
            other => {
                eprintln!("rs-canvas: unknown argument {other:?}");
                eprintln!("Try `rs-canvas --help`.");
                std::process::exit(2);
            }
        }
    }
    args
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> String {
    args.next().unwrap_or_else(|| {
        eprintln!("rs-canvas: {flag} needs a value");
        std::process::exit(2);
    })
}

fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn print_version() {
    match option_env!("RS_CANVAS_GIT_SHA") {
        Some(sha) => println!("rs-canvas {} ({sha})", version()),
        None => println!("rs-canvas {}", version()),
    }
}

fn print_help() {
    println!("rs-canvas {}", version());
    println!();
    println!("A local-first, hand-drawn canvas for sketching and architecture diagrams.");
    println!();
    println!("Usage: rs-canvas [options]");
    println!();
    println!("Options:");
    println!("  --web <dir>    Serve the front end from <dir> instead of the embedded copy");
    println!("  --port <port>  Listen on 127.0.0.1:<port> (default {DEFAULT_PORT})");
    println!("  --doc <file>   Persist the default workspace binary document to <file>");
    println!("  --no-sync      Do not persist or sync the document");
    println!("  --open         Open the app in the default browser");
    println!("  --no-open      Do not open a browser");
    println!("  -V, --version  Print the version");
    println!("  -h, --help     Print this help");
    println!();
    println!("Installed builds keep documents in ~/.rs-canvas and expose MCP at /mcp.");
    println!("Set RS_CANVAS_HOME to use a different document directory.");
}

/// Where an installed build stores documents when `--doc` is omitted.
#[cfg(feature = "embed-web")]
fn installed_doc_path() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("RS_CANVAS_HOME") {
        return Some(PathBuf::from(home).join("current.rsc"));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".rs-canvas").join("current.rsc"))
}

/// Development builds keep the historical behaviour: no `--doc` means no
/// server-side persistence.
#[cfg(not(feature = "embed-web"))]
fn installed_doc_path() -> Option<PathBuf> {
    None
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "canvas_server=info,tower_http=warn".into()),
        )
        .init();

    let Args {
        web,
        port,
        doc,
        sync,
        open,
    } = parse_args();

    // Release builds are the only ones that assume a per-user document and
    // open a browser; `--doc` and `--no-open` stay available to override.
    let doc = doc.or_else(installed_doc_path);
    let open = open.unwrap_or(cfg!(feature = "embed-web"));
    let web_dir = web.clone().unwrap_or_else(|| PathBuf::from("."));

    let state = Arc::new(AppState::new(web_dir, doc.clone(), sync));
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
        .route("/api/workspaces/{name}/rename", post(workspace_api::rename))
        .route("/api/workspaces/{name}", delete(workspace_api::delete))
        .nest_service("/mcp", mcp_service)
        .with_state(state.clone());

    let app = match web.as_deref() {
        Some(dir) => app.fallback_service(ServeDir::new(dir)),
        None => {
            #[cfg(feature = "embed-web")]
            {
                app.fallback(embedded::handler)
            }
            #[cfg(not(feature = "embed-web"))]
            {
                app.fallback_service(ServeDir::new("."))
            }
        }
    };
    let app = app.layer(no_store);

    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("rs-canvas: cannot listen on 127.0.0.1:{port}: {error}");
            std::process::exit(1);
        }
    };
    let url = format!("http://127.0.0.1:{port}");
    tracing::info!("rs-canvas {} at {url} (MCP at {url}/mcp)", version());
    if open {
        open_browser(&url);
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("canvas server failed");

    // The debounced writers may still owe a write.
    state.persist_all().await;
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = Command::new("xdg-open");

    if let Err(error) = command.arg(url).spawn() {
        tracing::warn!("could not open a browser: {error}");
    }
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
