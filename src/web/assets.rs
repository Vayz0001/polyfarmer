//! Static assets (CSS, JS, fonts) embedded into the binary via rust-embed and
//! served under `/assets/*`.
//!
//! The files aren't fingerprinted, so they are served with a content-hash `ETag`
//! and `Cache-Control: no-cache` (always revalidate): browsers keep them but ask
//! "changed?" each time and get a body-less `304` when not. Path traversal is
//! impossible by construction: rust-embed only resolves paths inside its embedded
//! folder (the filesystem-backed debug build canonicalises and rejects escapes).

use axum::{
    extract::Path,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Assets;

/// Serve an embedded asset by path, with a guessed content-type and an ETag.
pub async fn serve(Path(path): Path<String>, headers: HeaderMap) -> Response {
    let Some(content) = Assets::get(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let etag = format!("\"{}\"", hex::encode(content.metadata.sha256_hash()));
    let cache = [(header::ETAG, etag.clone()), (header::CACHE_CONTROL, "no-cache".to_string())];

    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|inm| inm.split(',').any(|t| t.trim().trim_start_matches("W/") == etag));
    if unchanged {
        return (StatusCode::NOT_MODIFIED, cache).into_response();
    }
    let mime = mime_guess::from_path(&path).first_or_octet_stream();
    ([(header::CONTENT_TYPE, mime.as_ref().to_string())], cache, content.data).into_response()
}
