//! Serving the built web app from the same process as the API.
//!
//! # Why one process and not a reverse proxy
//!
//! The obvious shape is nginx or Caddy in front of two containers. It is what a hosted
//! deployment looks like and it is the wrong default here.
//!
//! This product is installed on-premise, often somewhere with a change-control process
//! and sometimes somewhere with no internet at all. Every image is a thing to transfer,
//! scan, patch and explain. A proxy container in the compose file would also sit behind
//! *their* proxy — TLS is terminated there, see the workspace manifest — so it adds a
//! hop that changes nothing except the number of things that can be misconfigured.
//!
//! One binary serves both, which also makes the property the cookie design depends on
//! structural rather than configured: the app and the API are the same origin because
//! they are the same socket. There is no CORS configuration in this repository, and no
//! way to accidentally introduce a permissive one.
//!
//! An operator who wants a proxy still puts one in front. Nothing here prevents it.
//!
//! # The SPA fallback, and why `/api` is excluded from it
//!
//! Client-side routing means `/resources/<uuid>` must return `index.html` on a hard
//! reload — the server has no such file and the router sorts it out once the app boots.
//! The danger is doing that for `/api/v1/typo` as well: the client asks for JSON, gets
//! 200 and a page of HTML, and reports something other than "that endpoint does not
//! exist". `uops_api` therefore registers a catch-all under `/api` that answers 404 in
//! problem+json, and it matches before this fallback is ever reached.

use std::path::{Path, PathBuf};

use axum::Router;
use tower_http::services::{ServeDir, ServeFile};

/// Where the built web app lives, if it is being served.
///
/// `None` is a perfectly good answer: `cargo run -p uops-server` during development
/// serves the API alone and the Vite dev server proxies to it. The variable exists so
/// that the container can point at `/srv/web` without the binary having to guess.
#[must_use]
pub fn root_from_env() -> Option<PathBuf> {
    let raw = std::env::var("UOPS_WEB_ROOT").ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(PathBuf::from(raw))
}

/// Add static file serving to the router.
///
/// # Errors
///
/// When `root` has no `index.html`. Starting anyway would give every unmatched path a
/// 404 from a static file server, which looks exactly like a routing bug and is
/// actually an empty or wrongly mounted directory — a distinction worth making at boot,
/// once, rather than in a support ticket.
pub fn serve(app: Router, root: &Path) -> Result<Router, String> {
    let index = root.join("index.html");
    if !index.is_file() {
        return Err(format!(
            "UOPS_WEB_ROOT is {} but there is no index.html in it — the web build is \
             missing or the directory is mounted somewhere else",
            root.display()
        ));
    }

    // ServeDir answers real files; anything it does not have falls to index.html, which
    // is what makes a deep link survive a reload.
    let files = ServeDir::new(root).fallback(ServeFile::new(index));
    Ok(app.fallback_service(files))
}
