//! Serving a built single-page application beside the API.
//!
//! The end-to-end shape — the image bakes the bundle and the module points `web.dir` at it —
//! is a Nix concern. What is tested here is the half a Rust test can hold: a configured
//! directory is served, a path that is not a file gets `index.html` so a client-side route
//! survives a reload, and neither the probes nor the API prefix are shadowed by the fallback.

use std::path::PathBuf;

use api::startup::Web;
use axum::http::StatusCode;
use tempfile::TempDir;

use crate::helpers::{get_call, raw_call_to, router_with_web};

/// A built bundle on disk, the way `web/bundle` arrives in a service's source tree.
///
/// Returned alongside the `TempDir` that owns it: dropping that ends the directory, so every
/// test has to hold on to both.
fn bundle() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("create a temp dir");
    std::fs::write(
        dir.path().join("index.html"),
        "<!doctype html><title>spa</title>",
    )
    .expect("write index.html");
    std::fs::create_dir_all(dir.path().join("assets")).expect("create assets/");
    std::fs::write(dir.path().join("assets/app.js"), "console.log('app');")
        .expect("write an asset");
    let path = dir.path().to_path_buf();
    (dir, path)
}

fn served(dir: PathBuf) -> Web {
    Web::Served {
        dir,
        index: "index.html".to_owned(),
    }
}

#[tokio::test]
async fn a_file_in_the_bundle_is_served_as_itself() {
    let (_dir, path) = bundle();
    let (status, body) = get_call(router_with_web(served(path)), "/assets/app.js").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "console.log('app');");
}

#[tokio::test]
async fn a_client_side_route_falls_back_to_index_html() {
    // The path a browser asks for after a reload at a route the client router owns. Without
    // the fallback it is a 404, and the front end fails on exactly the URLs a user shares.
    let (_dir, path) = bundle();
    let (status, body) = get_call(router_with_web(served(path)), "/counter/42").await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("<title>spa</title>"),
        "the fallback must serve the bundle's entry point, got: {body}"
    );
}

#[tokio::test]
async fn the_fallback_does_not_swallow_the_probes_or_the_api_prefix() {
    // Routed paths are matched before the fallback, and this is what pins that: the bundle
    // answers everything else, so a fallback mounted carelessly would turn a readiness probe
    // into `index.html` with a 200 — a replica that reports ready by serving its own front
    // end.
    let (_dir, path) = bundle();
    let router = router_with_web(served(path));

    let (live, body) = get_call(router.clone(), "/healthz").await;
    assert_eq!(live, StatusCode::OK);
    assert_eq!(body, "ok", "the probe must not be answered by the bundle");

    let (ready, _) = get_call(router.clone(), "/readyz").await;
    assert_eq!(
        ready,
        StatusCode::SERVICE_UNAVAILABLE,
        "readiness must still consult the database, not the bundle"
    );

    // An unknown Connect method is still a Connect error, not `index.html`.
    let (unknown, json) =
        raw_call_to(router, "/api/health.v1.HealthService/NoSuchMethod", "{}").await;
    assert_eq!(unknown, StatusCode::NOT_FOUND);
    assert_eq!(json["code"], "unimplemented");
}

#[tokio::test]
async fn no_directory_means_no_fallback() {
    // An API-only service must not answer its root with a bundle it does not have. This is
    // the state every scaffolded service starts in.
    let (status, _) = get_call(router_with_web(Web::Absent), "/counter/42").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_missing_file_is_a_404_and_not_the_entry_point() {
    // The stale-asset case: a hashed bundle from a previous build, an ES module import that
    // asks for a file, a favicon a browser remembers. These are missing *files*, not client
    // routes. Answering them with index.html and a 200 turns a clean 404 into a parse error
    // inside a file the network tab says loaded — which is exactly what nobody can debug.
    let (_dir, path) = bundle();
    let (status, body) = get_call(router_with_web(served(path)), "/assets/app.4f2c9d.js").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        !body.contains("<title>spa</title>"),
        "a missing asset must not be served the entry point, got: {body}"
    );
}

#[tokio::test]
async fn a_file_that_exists_with_an_extension_is_still_served() {
    // The other side of the rule above: the extension is only decisive when there is no such
    // file. ServeDir answers the existing file before the fallback runs.
    let (_dir, path) = bundle();
    let (status, body) = get_call(router_with_web(served(path)), "/assets/app.js").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "console.log('app');");
}
