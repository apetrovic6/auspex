//! Server reflection is mounted where tools look for it — when, and only when, something
//! chose to serve it.
//!
//! These assert *routing*, not schema content. `ServerReflectionInfo` is a bidirectional
//! streaming RPC, so a unary JSON POST is not a conversation it can complete — but the
//! status still separates "this path is served" from "this path is not", and that is the
//! whole of the defect these exist for: the service was absent, so every client reported a
//! connection problem rather than a missing service.
//!
//! The pool's *contents* are proven elsewhere, by the generated code itself: the descriptors
//! are embedded at build time under `reflect_mode=bridge`, so a package missing from them is
//! a package missing from `proto/`.
//!
//! Three groups below, and the last two are newer than the first. Reflection served is what
//! `helpers::router` builds and what the `local` profile asks for; reflection withheld is
//! what a deployment gets; and the configuration itself is asserted against the committed
//! files, because a default that is only correct in the type is a default a `[reflection]`
//! section somebody added in the wrong file can undo. `docs/adr/0019` records the decision.

use std::path::Path;

use api::config::Settings;
use axum::http::StatusCode;

use crate::helpers::{raw_call, raw_call_to, router_without_reflection};

/// The empty-message body every case below posts.
///
/// A real `ServerReflectionInfo` call streams; this is enough to reach the router, which is
/// what is under test.
const EMPTY: &str = "{}";

const V1: &str = "grpc.reflection.v1.ServerReflection/ServerReflectionInfo";
const V1ALPHA: &str = "grpc.reflection.v1alpha.ServerReflection/ServerReflectionInfo";

// ---------------------------------------------------------------------------
// Served, which is what the `local` profile asks for
// ---------------------------------------------------------------------------

/// Where `buf curl` and the generated clients look: they derive the reflection path from
/// the base URI they were given, so for this server that is under the API prefix.
#[tokio::test]
async fn reflection_answers_under_the_api_prefix() {
    let (status, _) = raw_call(&format!("/api/{V1}"), EMPTY).await;
    assert_ne!(
        status,
        StatusCode::NOT_FOUND,
        "reflection is not mounted inside {}",
        api::API_PREFIX
    );
}

/// Where gRPC-native tooling looks — grpcurl, Bruno's gRPC mode — because the gRPC protocol
/// defines a path as `/package.Service/Method` and has no notion of a base path. This is the
/// mount whose absence made reflection look like a connection failure.
#[tokio::test]
async fn reflection_answers_at_the_server_root() {
    let (status, _) = raw_call(&format!("/{V1}"), EMPTY).await;
    assert_ne!(
        status,
        StatusCode::NOT_FOUND,
        "reflection is not mounted at the root, where grpcurl and Bruno ask for it"
    );
}

/// The superseded service name, still what several released clients send.
#[tokio::test]
async fn the_v1alpha_service_name_answers_too() {
    let (root, _) = raw_call(&format!("/{V1ALPHA}"), EMPTY).await;
    assert_ne!(
        root,
        StatusCode::NOT_FOUND,
        "v1alpha is not mounted at the root"
    );

    let (nested, _) = raw_call(&format!("/api/{V1ALPHA}"), EMPTY).await;
    assert_ne!(
        nested,
        StatusCode::NOT_FOUND,
        "v1alpha is not mounted under the prefix"
    );
}

/// The control, without which none of the above proves anything: an unserved path at the
/// root really does answer 404, so `assert_ne!` above can fail.
#[tokio::test]
async fn an_unmounted_root_path_is_still_not_found() {
    let (status, _) = raw_call("/grpc.reflection.v1.NoSuchService/NoSuchMethod", EMPTY).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Withheld, which is what a deployment gets
// ---------------------------------------------------------------------------

/// The production-shaped stack: nothing turned reflection on, so nothing answers for it.
///
/// Not a second `an_unmounted_root_path_is_still_not_found` — that posts to a path never
/// routed under any configuration, to prove a 404 is reachable at all. This posts to the four
/// that *are* routed when reflection is served, so it fails if `Withheld` mounts them anyway.
/// It is also the only test here that exercises the flag: the three above serve reflection
/// through `helpers::router` and pass either way.
#[tokio::test]
async fn withheld_reflection_answers_at_neither_path() {
    for method in api::startup::reflection_methods() {
        let root = format!("/{method}");
        let (status, _) = raw_call_to(router_without_reflection(), &root, EMPTY).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{root} is routed although nothing enabled reflection"
        );

        let nested = format!("{}/{method}", api::API_PREFIX);
        let (status, body) = raw_call_to(router_without_reflection(), &nested, EMPTY).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{nested} answers although nothing enabled reflection"
        );
        // The dispatcher was reached and had nothing to dispatch — the answer any unregistered
        // method gets. Without this, a nest that had stopped routing entirely would look
        // exactly like reflection being withheld.
        assert_eq!(
            body["code"], "unimplemented",
            "{nested} is a 404 for the wrong reason"
        );
    }
}

// ---------------------------------------------------------------------------
// Where the decision is actually made
// ---------------------------------------------------------------------------

/// The committed configuration, read the way the binary reads it.
///
/// `CARGO_MANIFEST_DIR` rather than a relative path: this is about the files a deployment
/// ships, not about where cargo happened to start the process.
fn committed_settings(profile: &str) -> Settings {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("config");
    settings::load_from(&dir, api::config::CONFIG_NAME, profile)
        .expect("the committed configuration should load")
}

#[test]
fn the_committed_configuration_serves_reflection_in_the_local_profile_only() {
    assert!(
        committed_settings(settings::DEFAULT_PROFILE)
            .reflection
            .enabled,
        "the local profile should serve reflection — it is how a developer asks a new \
         endpoint what it offers, and a 404 there reads as an unreachable server"
    );

    // No `api.prod.toml` exists, which is the point: this is the base file alone, and the
    // base file is what every profile without an overlay of its own gets.
    assert!(
        !committed_settings("prod").reflection.enabled,
        "a profile with no overlay of its own must not serve the schema; reflection is opted \
         into per environment, never inherited"
    );
}

#[test]
fn a_configuration_with_no_reflection_section_does_not_serve_it() {
    // The case a committed `enabled = false` does not cover: a configuration predating this
    // key, where nothing says either way. `#[serde(default)]` on the section is what makes
    // absence mean off, and it is one attribute away from meaning the opposite.
    //
    // JSON because the default under test is serde's, not the TOML parser's — the loader goes
    // through this same `Deserialize` impl whatever the file format was.
    let settings: Settings = serde_json::from_str(
        r#"{
            "application": { "host": "127.0.0.1", "port": 0 },
            "database": {
                "host": "127.0.0.1",
                "port": 5432,
                "username": "unused",
                "database_name": "unused",
                "max_connections": 1
            }
        }"#,
    )
    .expect("a configuration without a [reflection] section should still load");

    assert!(
        !settings.reflection.enabled,
        "a configuration that never heard of reflection served it anyway, which is the \
         silent inheritance this key exists to end"
    );
}
