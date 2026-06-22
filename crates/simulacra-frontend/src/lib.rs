//! simulacra-frontend — static SPA assets served via tower-http's `ServeDir`.
//!
//! Public API: [`frontend_router`] returns an [`axum::Router`] rooted at `/`
//! that serves the crate's `assets/` directory. Mount it via
//! `simulacra-server::build_router` alongside the GraphQL gateway and the existing
//! REST control plane so a single binary serves the UI and the API on one
//! origin.

use axum::Router;
use axum::http::{HeaderName, HeaderValue, header};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

/// Build a router that serves the bundled `assets/` directory.
///
/// In v1 this is a runtime read from the source tree's `assets/` directory
/// (relative to `CARGO_MANIFEST_DIR`). v2+ may switch to `include_dir!`
/// embedding behind a cargo feature flag for single-binary distribution.
///
/// Disables HTTP caching on every response so a plain page reload picks up
/// asset edits immediately. Production deploys should swap this for a
/// versioned/immutable cache strategy.
pub fn frontend_router() -> Router {
    let assets_dir = format!("{}/assets", env!("CARGO_MANIFEST_DIR"));
    Router::new()
        .fallback_service(ServeDir::new(assets_dir))
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("pragma"),
            HeaderValue::from_static("no-cache"),
        ))
}
