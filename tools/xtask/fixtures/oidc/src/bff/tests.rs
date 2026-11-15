//! BFF route tests against the fake OIDC provider.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

use crate::bff::cookies::{CookieKey, FlowValue, SessionValue};
use crate::bff::{BffSetup, bff_router, parse_public_url};
use crate::testing::{SigningKey, TestIdp};

const PUBLIC: &str = "https://app.example.com:8443";
const CALLBACK: &str = "https://app.example.com:8443/auth/callback";
const CLEAR_FLOW: &str = "__Host-login-flow=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0";
const CLEAR_SESSION: &str = "__Host-session=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0";

fn test_key() -> CookieKey {
    CookieKey::fixed(1)
}

fn setup(issuer: &str) -> BffSetup {
    BffSetup {
        issuer: issuer.to_owned(),
        public_url: parse_public_url(PUBLIC).unwrap(),
        client_id: "test-client".to_owned(),
        scopes: vec!["openid".to_owned(), "profile".to_owned()],
        login_audience: Some("api-audience".to_owned()),
        cookie_key: test_key(),
    }
}

fn bff(idp: &TestIdp) -> axum::Router {
    bff_router(setup(idp.issuer()))
}

fn cookies(response: &axum::response::Response) -> Vec<&str> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect()
}

fn cookie_value(response: &axum::response::Response, name: &str) -> String {
    let prefix = format!("{name}=");
    cookies(response)
        .into_iter()
        .find_map(|c| {
            c.strip_prefix(&prefix)
                .map(|v| v.split(';').next().unwrap().to_owned())
        })
        .expect("cookie")
}

fn assert_clear(response: &axum::response::Response, expected: &str) {
    assert!(
        cookies(response).contains(&expected),
        "exact clear cookie absent: {:?}",
        cookies(response)
    );
}

async fn send(
    app: &axum::Router,
    request: axum::http::request::Builder,
) -> axum::response::Response {
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn session_request(cookie: &str) -> axum::http::request::Builder {
    Request::post("/auth/session")
        .header("sec-fetch-site", "same-origin")
        .header("cookie", format!("__Host-session={cookie}"))
}

fn callback_request(code: &str, state: &str, flow: &str) -> axum::http::request::Builder {
    Request::get(format!("/auth/callback?code={code}&state={state}"))
        .header("cookie", format!("__Host-login-flow={flow}"))
}

async fn login_flow(app: &axum::Router) -> (String, FlowValue) {
    let response = send(app, Request::get("/auth/login")).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    let location = url::Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
    let params: std::collections::HashMap<_, _> = location.query_pairs().into_owned().collect();
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["client_id"], "test-client");
    assert_eq!(params["redirect_uri"], CALLBACK);
    assert_eq!(params["scope"], "openid profile");
    assert_eq!(params["audience"], "api-audience");
    assert_eq!(params["code_challenge_method"], "S256");
    let cookie = cookie_value(&response, "__Host-login-flow");
    assert_eq!(
        cookies(&response),
        vec![format!(
            "__Host-login-flow={cookie}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=600"
        )]
    );
    let flow = FlowValue::from_cookie_value(&cookie, &test_key()).unwrap();
    assert_eq!(params["state"], flow.state);
    (cookie, flow)
}

#[tokio::test]
async fn full_login_round_trip() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let app = bff(&idp);
    let (cookie, flow) = login_flow(&app).await;
    // The provider rejects an exchange whose redirect_uri differs from this registered URL.
    let (code, _) = idp.issue_code(CALLBACK, &flow.state);
    let response = send(&app, callback_request(&code, &flow.state, &cookie)).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(response.headers()["location"], "/");
    assert_clear(&response, CLEAR_FLOW);
    let session = cookie_value(&response, "__Host-session");
    assert!(cookies(&response).contains(
        &format!("__Host-session={session}; HttpOnly; Secure; SameSite=Strict; Path=/").as_str()
    ));
    let response = send(&app, session_request(&session)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["name"], "Test User");
    assert!(body["expires_in"].as_u64().unwrap() > 0);
    assert!(body.get("refresh_token").is_none());
    assert_eq!(idp.token_hits(), 2);
}

#[tokio::test]
async fn state_mismatch_is_400_and_clears_flow() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let app = bff(&idp);
    let (cookie, flow) = login_flow(&app).await;
    let (code, _) = idp.issue_code(CALLBACK, &flow.state);
    let response = send(&app, callback_request(&code, "wrong-state", &cookie)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(cookies(&response), vec![CLEAR_FLOW]);
    assert_eq!(idp.token_hits(), 0);
}

#[tokio::test]
async fn callback_replay_same_code_and_flow_is_refused() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let app = bff(&idp);
    let (cookie, flow) = login_flow(&app).await;
    let (code, _) = idp.issue_code(CALLBACK, &flow.state);
    let first = send(&app, callback_request(&code, &flow.state, &cookie)).await;
    assert_eq!(first.status(), StatusCode::FOUND);
    assert_eq!(first.headers()["location"], "/");
    let replay = send(&app, callback_request(&code, &flow.state, &cookie)).await;
    assert_eq!(replay.status(), StatusCode::FOUND);
    assert_eq!(replay.headers()["location"], "/?login=denied");
    assert_eq!(cookies(&replay), vec![CLEAR_FLOW]);
    assert_eq!(idp.token_hits(), 2);
}

#[tokio::test]
async fn missing_flow_cookie_is_400_and_clears_flow() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let response = send(
        &bff(&idp),
        Request::get("/auth/callback?code=code&state=state"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(cookies(&response), vec![CLEAR_FLOW]);
    assert_eq!(idp.token_hits(), 0);
}

#[tokio::test]
async fn provider_token_5xx_is_502_without_session() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let app = bff(&idp);
    let (cookie, flow) = login_flow(&app).await;
    let (code, _) = idp.issue_code(CALLBACK, &flow.state);
    idp.fail_token_requests();
    let response = send(&app, callback_request(&code, &flow.state, &cookie)).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(cookies(&response), vec![CLEAR_FLOW]);
    assert_eq!(idp.token_hits(), 1);
}

#[tokio::test]
async fn provider_error_redirects_to_login_denied() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let response = send(
        &bff(&idp),
        Request::get("/auth/callback?error=access_denied"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(response.headers()["location"], "/?login=denied");
    assert_eq!(cookies(&response), vec![CLEAR_FLOW]);
}

#[tokio::test]
async fn rotation_old_cookie_refused_after_rotation() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let app = bff(&idp);
    let _ = idp.seed_refresh_token("refresh-old", "Test User");
    let old_cookie = SessionValue::new("refresh-old".to_owned()).to_cookie_value(&test_key());
    let response = send(&app, session_request(&old_cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let new_cookie = cookie_value(&response, "__Host-session");
    assert!(cookies(&response).contains(
        &format!("__Host-session={new_cookie}; HttpOnly; Secure; SameSite=Strict; Path=/").as_str()
    ));
    let rotated = SessionValue::from_cookie_value(&new_cookie, &test_key()).unwrap();
    assert_ne!(
        rotated.refresh_token, "refresh-old",
        "response must store the rotated token"
    );
    let response = send(&app, session_request(&old_cookie)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(cookies(&response), vec![CLEAR_SESSION]);
    // A fresh response cookie must remain usable, not just be encrypted differently.
    let response = send(&app, session_request(&new_cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(idp.token_hits(), 3);
}

#[tokio::test]
async fn invalid_refresh_is_401_and_clears() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let cookie = SessionValue::new("unknown".to_owned()).to_cookie_value(&test_key());
    let response = send(&bff(&idp), session_request(&cookie)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(cookies(&response), vec![CLEAR_SESSION]);
    assert_eq!(idp.token_hits(), 1);
}

#[tokio::test]
async fn logout_revocation_failure_still_clears_cookie() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let _ = idp.seed_refresh_token("refresh", "Test User");
    idp.fail_revocation_requests();
    let cookie = SessionValue::new("refresh".to_owned()).to_cookie_value(&test_key());
    let response = send(
        &bff(&idp),
        Request::post("/auth/logout")
            .header("sec-fetch-site", "same-origin")
            .header("cookie", format!("__Host-session={cookie}")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(cookies(&response), vec![CLEAR_SESSION]);
    assert_eq!(idp.revoke_hits(), 1);
}

#[tokio::test]
async fn configured_public_origin_is_exactly_required() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let app = bff(&idp);
    for origin in [
        "http://app.example.com:8443",
        "https://app.example.com:9443",
        "http://localhost",
        "https://evil.com",
        "null",
    ] {
        for path in ["/auth/session", "/auth/logout"] {
            let response = send(&app, Request::post(path).header("origin", origin)).await;
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{origin} at {path}"
            );
        }
    }
    let response = send(
        &app,
        Request::post("/auth/session").header("origin", PUBLIC),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(cookies(&response), vec![CLEAR_SESSION]);
    let response = send(&app, Request::post("/auth/logout").header("origin", PUBLIC)).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn fetch_metadata_and_missing_headers_enforce_csrf() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let app = bff(&idp);
    for path in ["/auth/session", "/auth/logout"] {
        assert_eq!(
            send(&app, Request::post(path)).await.status(),
            StatusCode::FORBIDDEN
        );
        for site in ["cross-site", "same-site", "none"] {
            assert_eq!(
                send(
                    &app,
                    Request::post(path)
                        .header("sec-fetch-site", site)
                        .header("origin", PUBLIC)
                )
                .await
                .status(),
                StatusCode::FORBIDDEN
            );
        }
    }
    assert_eq!(
        send(
            &app,
            Request::post("/auth/session").header("sec-fetch-site", "same-origin")
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn tampered_cookie_is_401_without_provider_calls() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let app = bff(&idp);
    let _ = idp.seed_refresh_token("some-token", "Test User");
    let cookie = SessionValue::new("some-token".to_owned()).to_cookie_value(&test_key());
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cookie)
        .unwrap();
    for index in [0, 12, bytes.len() - 1] {
        let mut tampered = bytes.clone();
        tampered[index] ^= 1;
        let cookie = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tampered);
        let response = send(&app, session_request(&cookie)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "byte {index}");
        assert_eq!(cookies(&response), vec![CLEAR_SESSION]);
        assert_eq!(idp.token_hits(), 0);
        assert_eq!(idp.discovery_hits(), 0);
    }
}

#[tokio::test]
async fn bff_trusts_explicit_test_ca() {
    let idp = crate::testing::TestTlsIdp::start(&[("k1", SigningKey::A)]).await;
    let untrusted = bff_router(setup(idp.issuer()));
    assert_eq!(
        send(&untrusted, Request::get("/auth/login")).await.status(),
        StatusCode::BAD_GATEWAY
    );
    let trusted = crate::bff::bff_router_with_extra_roots(
        setup(idp.issuer()),
        vec![crate::testing::tls_ca()],
    )
    .unwrap();
    assert_eq!(
        send(&trusted, Request::get("/auth/login")).await.status(),
        StatusCode::FOUND
    );
}

#[test]
fn public_url_accepts_only_safe_origins() {
    for value in [
        PUBLIC,
        "https://example.com/",
        "http://localhost:8080",
        "http://127.0.0.1:3000/",
    ] {
        assert!(parse_public_url(value).is_ok(), "{value}");
    }
    for value in [
        "",
        "/",
        "example.com",
        "http://example.com",
        "http://[::1]",
        "ftp://localhost",
        "https://user:password@example.com",
        "https://example.com/path",
        "https://example.com/?q=x",
        "https://example.com/#fragment",
        "https://app.example.com./",
        " https://app.example.com/",
        "https://app.example.com/ ",
        "\thttps://app.example.com/",
        "https://app.example.com/\n",
    ] {
        assert!(parse_public_url(value).is_err(), "{value}");
    }
}
