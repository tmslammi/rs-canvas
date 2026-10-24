//! The staged front end compiled into release builds.
//!
//! `web/dist` is produced by `npm run build`; the release workflow builds it
//! before compiling with `--features embed-web`.

use axum::body::Body;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../web/dist/"]
struct Assets;

pub async fn handler(uri: Uri) -> Response {
    let requested = uri.path().trim_start_matches('/');
    let path = if requested.is_empty() {
        "index.html"
    } else {
        requested
    };

    match Assets::get(path) {
        Some(asset) => {
            let content_type = mime_guess::from_path(path).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, content_type.to_string())],
                Body::from(asset.data.into_owned()),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
