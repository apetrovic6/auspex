//! The verifier against an identity provider in this process.
//!
//! Unit tests rather than `tests/`, for one reason: the thing worth asserting is *which kind of
//! no* came back (`Rejected` or `Unavailable`), and `Credential` has no public constructor — the
//! layer is the only thing that makes one. `verify_token` takes the token itself, so these see the
//! verdict the layer would map; `services/<name>/tests/<name>/oidc.rs` drives the same verifier
//! through the layer and the router.
//!
//! The provider is real HTTP on a loopback port (`testing::TestIdp`), so what is exercised is the
//! actual fetch, the actual cache and the actual signature check. Nothing is mocked but the
//! provider's identity.

use std::time::Duration;

use identity::VerifyFailure;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::json;

use crate::testing::{self, AUDIENCE, SUBJECT, SigningKey, TestIdp, TestTlsIdp};
use crate::{Config, ConfigError, Timing, Verifier};

/// Timing for a test that does not care about it: the real intervals, a short request timeout so a
/// test against a dead provider does not wait out the production one.
fn timing() -> Timing {
    Timing {
        request_timeout: Duration::from_millis(500),
        ..Timing::default()
    }
}

fn config(idp: &TestIdp) -> Config {
    Config {
        issuer: idp.issuer().to_owned(),
        audience: AUDIENCE.to_owned(),
        ..Default::default()
    }
}

fn config_with_algorithms(idp: &TestIdp, algorithms: &[Algorithm]) -> Config {
    Config {
        issuer: idp.issuer().to_owned(),
        audience: AUDIENCE.to_owned(),
        algorithms: crate::AlgorithmSet::from_strs(
            &algorithms
                .iter()
                .map(|a| format!("{a:?}"))
                .collect::<Vec<_>>(),
        )
        .expect("a valid algorithm set"),
    }
}

fn verifier(idp: &TestIdp) -> Verifier {
    Verifier::with_timing(&config(idp), timing()).expect("a loopback issuer is a valid config")
}

fn verifier_with(idp: &TestIdp, timing: Timing) -> Verifier {
    Verifier::with_timing(&config(idp), timing).expect("a loopback issuer is a valid config")
}

#[track_caller]
fn assert_rejected<T: std::fmt::Debug>(result: Result<T, VerifyFailure>, what: &str) {
    match result {
        Err(VerifyFailure::Rejected(_)) => {}
        other => panic!("{what}: expected Rejected, got {other:?}"),
    }
}

#[track_caller]
fn assert_unavailable<T: std::fmt::Debug>(result: Result<T, VerifyFailure>, what: &str) {
    match result {
        Err(VerifyFailure::Unavailable(_)) => {}
        other => panic!("{what}: expected Unavailable, got {other:?}"),
    }
}

/// base64url without padding — the encoding of a JWT's parts. Written out because the tokens
/// below are deliberately ones no JWT library will build.
fn b64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

fn good(idp: &TestIdp) -> String {
    testing::sign(SigningKey::A, "k1", &testing::claims(idp.issuer()))
}

// ---------------------------------------------------------------------------------------------
// Accepting
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_token_the_provider_signed_names_its_subject() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let verifier = verifier(&idp);

    let principal = verifier
        .verify_token(&good(&idp))
        .await
        .expect("a token signed by a published key, for this issuer and audience");

    assert_eq!(principal.subject, SUBJECT);
}

#[tokio::test]
async fn building_a_verifier_fetches_nothing() {
    // Lazy, or a replica that boots during an IdP outage does not boot.
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let _verifier = verifier(&idp);

    assert_eq!(idp.discovery_hits(), 0);
    assert_eq!(idp.jwks_hits(), 0);
}

#[tokio::test]
async fn an_audience_list_containing_ours_is_accepted() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let mut claims = testing::claims(idp.issuer());
    claims["aud"] = json!(["somebody-else", AUDIENCE]);

    verifier(&idp)
        .verify_token(&testing::sign(SigningKey::A, "k1", &claims))
        .await
        .expect("`aud` may be an array, and ours is in it");
}

#[tokio::test]
async fn the_keys_are_fetched_once_for_many_tokens() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let verifier = verifier(&idp);

    for _ in 0..5 {
        verifier.verify_token(&good(&idp)).await.expect("accepted");
    }

    assert_eq!(idp.discovery_hits(), 1);
    assert_eq!(idp.jwks_hits(), 1);
}

// ---------------------------------------------------------------------------------------------
// Refusing a token
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn alg_none_is_rejected_without_asking_the_provider() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let header = b64(br#"{"alg":"none","typ":"JWT","kid":"k1"}"#);
    let payload = b64(testing::claims(idp.issuer()).to_string().as_bytes());
    let token = format!("{header}.{payload}.");

    assert_rejected(verifier(&idp).verify_token(&token).await, "alg none");
    assert_eq!(
        idp.jwks_hits(),
        0,
        "a wrong algorithm should not wake the provider"
    );
}

#[tokio::test]
async fn a_token_hmac_signed_with_the_public_key_is_rejected() {
    // The key-confusion attack: the verifier holds an RSA public key, the attacker signs HS256
    // with that key's bytes as the shared secret, and a verifier that lets the token choose its
    // algorithm checks the "HMAC" against the key it was handed and accepts.
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some("k1".to_owned());
    let token = jsonwebtoken::encode(
        &header,
        &testing::claims(idp.issuer()),
        &EncodingKey::from_secret(SigningKey::a_public_pem().as_bytes()),
    )
    .expect("HS256 signs with any bytes");

    assert_rejected(
        verifier(&idp).verify_token(&token).await,
        "HS256 with the public key",
    );
}

#[tokio::test]
async fn an_rs256_header_over_a_foreign_signature_is_rejected() {
    // Right algorithm, right `kid`, signed by the key the provider does NOT publish under it.
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let token = testing::sign(SigningKey::B, "k1", &testing::claims(idp.issuer()));

    assert_rejected(
        verifier(&idp).verify_token(&token).await,
        "signed by the wrong key",
    );
}

#[tokio::test]
async fn a_tampered_payload_is_rejected() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let token = good(&idp);
    let mut parts: Vec<&str> = token.split('.').collect();
    let forged = b64(testing::claims(idp.issuer())
        .to_string()
        .replace(SUBJECT, "admin")
        .as_bytes());
    parts[1] = &forged;

    assert_rejected(
        verifier(&idp).verify_token(&parts.join(".")).await,
        "a payload the signature does not cover",
    );
}

#[tokio::test]
async fn a_token_for_another_issuer_is_rejected() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let mut claims = testing::claims(idp.issuer());
    claims["iss"] = json!("https://not-the-issuer.example");

    assert_rejected(
        verifier(&idp)
            .verify_token(&testing::sign(SigningKey::A, "k1", &claims))
            .await,
        "wrong iss",
    );
}

#[tokio::test]
async fn a_token_for_another_audience_is_rejected() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let mut claims = testing::claims(idp.issuer());
    claims["aud"] = json!("a-different-client");

    assert_rejected(
        verifier(&idp)
            .verify_token(&testing::sign(SigningKey::A, "k1", &claims))
            .await,
        "wrong aud",
    );
}

#[tokio::test]
async fn a_token_with_no_audience_is_rejected() {
    // `jsonwebtoken` checks a claim only if it is present; the required-claims list is what makes
    // an absent `aud` a refusal rather than a pass.
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let mut claims = testing::claims(idp.issuer());
    claims.as_object_mut().expect("an object").remove("aud");

    assert_rejected(
        verifier(&idp)
            .verify_token(&testing::sign(SigningKey::A, "k1", &claims))
            .await,
        "no aud",
    );
}

#[tokio::test]
async fn an_expired_token_is_rejected() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let mut claims = testing::claims(idp.issuer());
    // An hour ago: well outside the leeway, which is seconds.
    claims["exp"] = json!(testing::now() - 3600);

    assert_rejected(
        verifier(&idp)
            .verify_token(&testing::sign(SigningKey::A, "k1", &claims))
            .await,
        "expired",
    );
}

#[tokio::test]
async fn a_token_with_no_expiry_is_rejected() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let mut claims = testing::claims(idp.issuer());
    claims.as_object_mut().expect("an object").remove("exp");

    assert_rejected(
        verifier(&idp)
            .verify_token(&testing::sign(SigningKey::A, "k1", &claims))
            .await,
        "a token that never expires",
    );
}

#[tokio::test]
async fn a_token_that_is_not_valid_yet_is_rejected() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let mut claims = testing::claims(idp.issuer());
    claims["nbf"] = json!(testing::now() + 3600);

    assert_rejected(
        verifier(&idp)
            .verify_token(&testing::sign(SigningKey::A, "k1", &claims))
            .await,
        "nbf in the future",
    );
}

#[tokio::test]
async fn a_token_with_no_subject_is_rejected() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let mut claims = testing::claims(idp.issuer());
    claims.as_object_mut().expect("an object").remove("sub");

    assert_rejected(
        verifier(&idp)
            .verify_token(&testing::sign(SigningKey::A, "k1", &claims))
            .await,
        "no sub",
    );
}

#[tokio::test]
async fn a_token_with_an_empty_subject_is_rejected() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let mut claims = testing::claims(idp.issuer());
    claims["sub"] = json!("");

    assert_rejected(
        verifier(&idp)
            .verify_token(&testing::sign(SigningKey::A, "k1", &claims))
            .await,
        "an empty subject names nobody",
    );
}

#[tokio::test]
async fn a_token_that_names_no_key_is_rejected() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let header = Header::new(Algorithm::RS256); // no `kid`
    let token = jsonwebtoken::encode(
        &header,
        &testing::claims(idp.issuer()),
        &EncodingKey::from_rsa_pem(include_bytes!("testing/test-only-signing-key-a.pem"))
            .expect("the committed key parses"),
    )
    .expect("signs");

    assert_rejected(verifier(&idp).verify_token(&token).await, "no kid");
}

#[tokio::test]
async fn things_that_are_not_jwts_are_rejected() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let verifier = verifier(&idp);

    for token in [
        "",
        "not-a-jwt",
        "a.b.c",
        "opaque_access_token_0123456789",
        "....",
        "ey.ey.ey",
    ] {
        assert_rejected(verifier.verify_token(token).await, token);
    }
    assert_eq!(idp.jwks_hits(), 0, "none of these should wake the provider");
}

#[tokio::test]
async fn a_refusal_never_contains_the_token() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let verifier = verifier(&idp);
    let mut expired = testing::claims(idp.issuer());
    expired["exp"] = json!(testing::now() - 3600);
    expired["sub"] = json!("a-subject-that-must-not-leak");

    let tokens = [
        testing::sign(SigningKey::A, "k1", &expired),
        testing::sign(SigningKey::B, "k1", &testing::claims(idp.issuer())),
        "opaque_access_token_0123456789".to_owned(),
        "ey.ey.ey".to_owned(),
    ];
    for token in &tokens {
        let failure = verifier
            .verify_token(token)
            .await
            .expect_err("every one of these is refused");
        let rendered = format!("{failure:?}");
        for part in token.split('.').filter(|part| part.len() > 3) {
            assert!(
                !rendered.contains(part),
                "{rendered} contains a part of the token"
            );
        }
        assert!(!rendered.contains("must-not-leak"), "{rendered}");
    }
}

// ---------------------------------------------------------------------------------------------
// The key cache
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_kid_refetches_and_is_accepted_after_a_rotation() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    // No gap required between fetches, so the one refetch this test wants is allowed.
    let verifier = verifier_with(
        &idp,
        Timing {
            min_refetch_interval: Duration::ZERO,
            ..timing()
        },
    );
    verifier
        .verify_token(&good(&idp))
        .await
        .expect("warm the cache");
    assert_eq!(idp.jwks_hits(), 1);

    // The provider rotates: a new key under a new `kid`, signed with by the next token.
    idp.publish(&[("k1", SigningKey::A), ("k2", SigningKey::B)]);
    let rotated = testing::sign(SigningKey::B, "k2", &testing::claims(idp.issuer()));

    let principal = verifier
        .verify_token(&rotated)
        .await
        .expect("an unseen kid is a reason to look again");
    assert_eq!(principal.subject, SUBJECT);
    assert_eq!(idp.jwks_hits(), 2);

    // And the key it already held is still good.
    verifier
        .verify_token(&good(&idp))
        .await
        .expect("k1 survived the rotation");
}

#[tokio::test]
async fn the_same_kid_with_different_key_material_is_rejected() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let verifier = verifier_with(
        &idp,
        Timing {
            min_refetch_interval: Duration::ZERO,
            ..timing()
        },
    );
    verifier
        .verify_token(&good(&idp))
        .await
        .expect("warm the cache");

    // The provider now publishes B under k1; a token signed by B names k1. We hold A under k1.
    idp.publish(&[("k1", SigningKey::B)]);
    let token = testing::sign(SigningKey::B, "k1", &testing::claims(idp.issuer()));

    assert_rejected(
        verifier.verify_token(&token).await,
        "a kid we hold, a signature from a key we do not",
    );
    assert_eq!(
        idp.jwks_hits(),
        1,
        "a bad signature on a known kid must not be a way to make the provider be asked again"
    );
}

#[tokio::test]
async fn unknown_kids_cost_one_refetch_per_interval() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let interval = Duration::from_millis(1000);
    let verifier = verifier_with(
        &idp,
        Timing {
            min_refetch_interval: interval,
            ..timing()
        },
    );
    verifier
        .verify_token(&good(&idp))
        .await
        .expect("warm the cache");
    assert_eq!(idp.jwks_hits(), 1);

    // Wait out the interval, so that exactly one refetch is permitted, then flood.
    tokio::time::sleep(interval + Duration::from_millis(100)).await;
    for n in 0..20 {
        let token = testing::sign(
            SigningKey::A,
            &format!("random-kid-{n}"),
            &testing::claims(idp.issuer()),
        );
        assert_rejected(
            verifier.verify_token(&token).await,
            "a kid nobody publishes",
        );
    }

    assert_eq!(
        idp.jwks_hits(),
        2,
        "twenty fabricated kids should have cost the provider one request, not twenty"
    );
}

#[tokio::test]
async fn a_warm_cache_survives_the_provider_going_away() {
    let mut idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let verifier = verifier(&idp);
    verifier
        .verify_token(&good(&idp))
        .await
        .expect("warm the cache");

    idp.stop().await;

    verifier
        .verify_token(&good(&idp))
        .await
        .expect("the key is cached; the provider is not needed to use it");
}

#[tokio::test]
async fn a_stale_cache_survives_a_failed_refresh() {
    // `refresh_interval` of zero makes every set stale the moment it lands, so every verification
    // tries to refresh — and with the provider gone, that attempt fails. The old set must be what
    // answers, not an error.
    let mut idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let verifier = verifier_with(
        &idp,
        Timing {
            refresh_interval: Duration::ZERO,
            min_refetch_interval: Duration::ZERO,
            ..timing()
        },
    );
    verifier
        .verify_token(&good(&idp))
        .await
        .expect("warm the cache");

    idp.stop().await;

    verifier
        .verify_token(&good(&idp))
        .await
        .expect("a refresh that fails keeps the last good key set");
}

#[tokio::test]
async fn a_provider_that_is_down_with_a_cold_cache_is_unavailable() {
    let mut idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let token = good(&idp);
    let verifier = verifier(&idp);
    idp.stop().await;

    // Not `Rejected`: the token is fine, and nothing here can say so. A 401 would tell a caller
    // with a good credential that it is bad.
    assert_unavailable(
        verifier.verify_token(&token).await,
        "no cache and no provider",
    );
}

#[tokio::test]
async fn an_unknown_kid_during_an_outage_is_unavailable_not_rejected() {
    // We hold k1 and cannot ask who else exists. A token naming k9 may be a key published an hour
    // ago; refusing it as bad is a claim we cannot back.
    let mut idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let verifier = verifier_with(
        &idp,
        Timing {
            min_refetch_interval: Duration::ZERO,
            ..timing()
        },
    );
    verifier
        .verify_token(&good(&idp))
        .await
        .expect("warm the cache");
    idp.stop().await;

    let token = testing::sign(SigningKey::B, "k9", &testing::claims("http://127.0.0.1:1"));
    assert_unavailable(
        verifier.verify_token(&token).await,
        "an unknown kid and no provider",
    );
}

#[tokio::test]
async fn a_discovery_document_naming_another_issuer_is_refused() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    idp.claim_issuer(Some("https://somebody-else.example"));

    // Unavailable, and not Accepted: the keys of a document that is not ours are not keys to
    // trust, and the operator needs to see a fault rather than a wall of 401s.
    assert_unavailable(
        verifier(&idp).verify_token(&good(&idp)).await,
        "a discovery issuer mismatch",
    );
    assert_eq!(
        idp.jwks_hits(),
        0,
        "the key set of a document that failed the issuer check must never be fetched"
    );
}

#[tokio::test]
async fn a_provider_that_never_answers_is_unavailable_within_the_budget() {
    // Accepts the connection, says nothing. The request timeout is what bounds this.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let issuer = format!("http://{}", listener.local_addr().expect("addr"));
    let silent = tokio::spawn(async move {
        // Each connection is held open and never read from or written to.
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _held = stream;
                std::future::pending::<()>().await;
            });
        }
    });
    let verifier = Verifier::with_timing(
        &Config {
            issuer: issuer.clone(),
            audience: AUDIENCE.to_owned(),
            ..Default::default()
        },
        Timing {
            request_timeout: Duration::from_millis(300),
            ..Timing::default()
        },
    )
    .expect("config");

    let started = std::time::Instant::now();
    let token = testing::sign(SigningKey::A, "k1", &testing::claims(&issuer));
    assert_unavailable(verifier.verify_token(&token).await, "a silent provider");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "took {:?}: the timeout did not bound the fetch",
        started.elapsed()
    );
    silent.abort();
}

// ---------------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------------

fn issuer(url: &str) -> Config {
    Config {
        issuer: url.to_owned(),
        audience: AUDIENCE.to_owned(),
        ..Default::default()
    }
}

#[test]
fn a_plain_http_issuer_is_refused_unless_it_is_loopback() {
    for url in [
        "http://id.example.com",
        "http://10.0.0.5:8080",
        "http://192.168.1.1",
        "http://localhost.example.com",
        "http://127.0.0.1.example.com",
    ] {
        assert!(
            matches!(issuer(url).validate(), Err(ConfigError::InsecureIssuer)),
            "{url} must be refused"
        );
        assert!(
            Verifier::new(&issuer(url)).is_err(),
            "{url}: the constructor refuses too"
        );
    }
    for url in [
        "http://127.0.0.1:8080",
        "http://127.1.2.3",
        "http://localhost:9000",
        "http://[::1]:9000",
        "https://id.example.com",
        "https://id.example.com/realms/main",
    ] {
        issuer(url)
            .validate()
            .unwrap_or_else(|error| panic!("{url}: {error}"));
    }
}

#[test]
fn an_issuer_that_is_not_an_issuer_is_refused() {
    for url in [
        "",
        "   ",
        "id.example.com",
        "ftp://id.example.com",
        "https://user:secret@id.example.com",
        "https://id.example.com/?x=1",
        "https://id.example.com/#frag",
    ] {
        assert!(issuer(url).validate().is_err(), "{url:?} must be refused");
    }
    assert!(
        matches!(
            Config {
                issuer: "https://id.example.com".to_owned(),
                audience: String::new(),
                ..Default::default()
            }
            .validate(),
            Err(ConfigError::EmptyAudience)
        ),
        "an empty audience would accept a token for nobody in particular"
    );
}

// ---------------------------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_client_trusts_the_roots_it_was_given_and_no_others() {
    // The service image has no CA bundle, so the client must carry its trust with it. This is the
    // only observable difference between "the root store we built" and "whatever the platform
    // verifier finds": a provider whose CA exists only in a test is trusted exactly when that CA
    // was added to OUR store. A client on the platform verifier would trust neither case.
    let idp = TestTlsIdp::start(&[("k1", SigningKey::A)]).await;
    let token = testing::sign(SigningKey::A, "k1", &testing::claims(idp.issuer()));
    let config = Config {
        issuer: idp.issuer().to_owned(),
        audience: AUDIENCE.to_owned(),
        ..Default::default()
    };

    let trusting = Verifier::with_extra_roots(&config, timing(), vec![testing::tls_ca()])
        .expect("the test CA is a certificate");
    trusting
        .verify_token(&token)
        .await
        .expect("a provider signed by a root in our store is trusted");
    assert_eq!(idp.jwks_hits(), 1, "over TLS");

    // Without the extra root the same provider is an unknown CA, and the fetch fails: Unavailable,
    // not Rejected — the token never got as far as being judged.
    let stranger = Verifier::with_timing(&config, timing()).expect("config");
    assert_unavailable(
        stranger.verify_token(&token).await,
        "a certificate no compiled-in root vouches for",
    );
}

// ---------------------------------------------------------------------------------------------
// Algorithms other than the one allowed, and a provider that misbehaves
// ---------------------------------------------------------------------------------------------

fn key_a() -> EncodingKey {
    EncodingKey::from_rsa_pem(include_bytes!("testing/test-only-signing-key-a.pem"))
        .expect("the committed key parses")
}

#[tokio::test]
async fn another_rsa_algorithm_is_rejected_even_with_the_right_key() {
    // RS384 under the published key: a perfectly valid signature of a kind this verifier does not
    // accept. The allow-list is exactly [RS256], not "the RSA family".
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let mut header = Header::new(Algorithm::RS384);
    header.kid = Some("k1".to_owned());
    let token = jsonwebtoken::encode(&header, &testing::claims(idp.issuer()), &key_a())
        .expect("RS384 signs with an RSA key");

    assert_rejected(verifier(&idp).verify_token(&token).await, "RS384");
    assert_eq!(
        idp.jwks_hits(),
        0,
        "a wrong algorithm should not wake the provider"
    );
}

/// A server that answers every connection with whatever `response(issuer, path)` returns,
/// verbatim, and returns its issuer.
async fn canned_provider(
    response: impl Fn(&str, &str) -> String + Clone + Send + 'static,
) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let issuer = format!("http://{}", listener.local_addr().expect("addr"));
    let origin = issuer.clone();
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let response = response.clone();
            let issuer = origin.clone();
            tokio::spawn(async move {
                let mut request = [0_u8; 1024];
                let read = stream.read(&mut request).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&request[..read]).into_owned();
                let path = text
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let response = response(&issuer, &path);
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (issuer, server)
}

#[tokio::test]
async fn an_oversized_discovery_document_is_refused() {
    // Valid JSON that would be accepted if it were small enough: discovery names this very server
    // as the issuer and as the key host, and the same document answers the key request with an
    // empty key set. Without the cap the fetch succeeds and the token is merely unknown; with it,
    // nothing is trusted. (A body that was not JSON would be refused for that reason alone, and
    // this test would not be testing the cap.)
    let (issuer, server) = canned_provider(|issuer, _path| {
        let body = format!(
            r#"{{"issuer":"{issuer}","jwks_uri":"{issuer}/jwks","keys":[],"pad":"{}"}}"#,
            "x".repeat(crate::http::MAX_BODY_BYTES)
        );
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    })
    .await;
    let verifier = Verifier::with_timing(
        &Config {
            issuer: issuer.clone(),
            audience: AUDIENCE.to_owned(),
            ..Default::default()
        },
        timing(),
    )
    .expect("config");

    let token = testing::sign(SigningKey::A, "k1", &testing::claims(&issuer));
    assert_unavailable(
        verifier.verify_token(&token).await,
        "a document over the cap",
    );
    server.abort();
}

#[tokio::test]
async fn a_redirecting_provider_is_not_followed() {
    // A discovery URL that redirects is a provider that is misconfigured or being steered, and the
    // one redirect that must never be followed silently is https to http.
    let target = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let location = target.issuer().to_owned();
    let (issuer, server) = canned_provider(move |_issuer, _path| {
        format!(
            "HTTP/1.1 302 Found\r\nlocation: {location}/.well-known/openid-configuration\r\n\
             content-length: 0\r\n\r\n"
        )
    })
    .await;
    let verifier = Verifier::with_timing(
        &Config {
            issuer: issuer.clone(),
            audience: AUDIENCE.to_owned(),
            ..Default::default()
        },
        timing(),
    )
    .expect("config");

    let token = testing::sign(SigningKey::A, "k1", &testing::claims(&issuer));
    assert_unavailable(verifier.verify_token(&token).await, "a redirect");
    assert_eq!(target.discovery_hits(), 0, "the redirect was followed");
    server.abort();
}

#[tokio::test]
async fn a_discovery_document_sending_keys_to_cleartext_is_refused() {
    // The issuer passed the https/loopback rule; the `jwks_uri` it names has to pass it too, or a
    // document served over TLS can point the key fetch at a cleartext address anyone can answer.
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    idp.claim_jwks_uri(Some("http://10.255.255.1/jwks"));

    assert_unavailable(
        verifier(&idp).verify_token(&good(&idp)).await,
        "a cleartext jwks_uri",
    );
    assert_eq!(idp.jwks_hits(), 0);
}

#[tokio::test]
async fn a_jwks_uri_that_breaks_the_rule_is_refused_even_when_it_is_reachable() {
    // The test above cannot tell "refused by the rule" from "nobody answered at 10.255.255.1": both
    // end as Unavailable with no key fetch. This one can. Credentials in the URL break the rule and
    // the address is the provider itself, so without the check the fetch would succeed and the
    // token would be accepted.
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let authority = idp.issuer().trim_start_matches("http://").to_owned();
    idp.claim_jwks_uri(Some(&format!("http://user:secret@{authority}/jwks")));

    assert_unavailable(
        verifier(&idp).verify_token(&good(&idp)).await,
        "a jwks_uri with credentials in it",
    );
    assert_eq!(
        idp.jwks_hits(),
        0,
        "the key set was fetched from a URL the rule forbids"
    );
}

// ---------------------------------------------------------------------------------------------
// After the adversarial review of PR #87
// ---------------------------------------------------------------------------------------------

/// A timing under which a key set is stale the moment it lands and any request may refetch.
fn always_refreshing() -> Timing {
    Timing {
        refresh_interval: Duration::ZERO,
        min_refetch_interval: Duration::ZERO,
        ..timing()
    }
}

#[tokio::test]
async fn an_empty_key_set_does_not_evict_the_keys_we_hold() {
    // A provider that answers 200 with `"keys": []` — mid-deploy, or misconfigured — has not said
    // that every key is revoked. Replacing the cache with nothing turned that glitch into a 401
    // for every caller, which is the failure the Rejected/Unavailable split exists to prevent.
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let verifier = verifier_with(&idp, always_refreshing());
    verifier
        .verify_token(&good(&idp))
        .await
        .expect("warm the cache");

    idp.publish(&[]);

    verifier
        .verify_token(&good(&idp))
        .await
        .expect("an empty set is a failed refresh, and a failed refresh keeps the last good set");
}

#[tokio::test]
async fn an_empty_key_set_on_a_cold_cache_is_unavailable() {
    // Nothing usable and nothing cached: we could not find out, which is not "the token is bad".
    let idp = TestIdp::start(&[]).await;

    assert_unavailable(
        verifier(&idp).verify_token(&good(&idp)).await,
        "a provider that publishes no keys, asked cold",
    );
}

#[tokio::test]
async fn a_key_set_with_no_usable_key_counts_as_empty() {
    // Only an encryption key: after the `use` filter nothing remains, which is the same fact.
    let idp = TestIdp::start(&[]).await;
    let mut encryption = SigningKey::A.jwk("k1");
    encryption["use"] = json!("enc");
    idp.publish_raw(vec![encryption]);

    assert_unavailable(
        verifier(&idp).verify_token(&good(&idp)).await,
        "a key set whose only key is not a signing key",
    );
}

#[tokio::test]
async fn a_rotation_that_drops_a_key_still_drops_it() {
    // The other half of the rule above: a NON-empty set replaces the old one, so a key the
    // provider stopped publishing stops verifying. Only emptiness is treated as a failure.
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let verifier = verifier_with(&idp, always_refreshing());
    verifier
        .verify_token(&good(&idp))
        .await
        .expect("warm the cache");

    idp.publish(&[("k2", SigningKey::B)]);

    assert_rejected(
        verifier.verify_token(&good(&idp)).await,
        "a key the provider has withdrawn",
    );
    let rotated = testing::sign(SigningKey::B, "k2", &testing::claims(idp.issuer()));
    verifier
        .verify_token(&rotated)
        .await
        .expect("the new key is trusted");
}

#[tokio::test]
async fn a_cancelled_cold_fetch_does_not_lock_out_a_healthy_provider() {
    // The first request is dropped mid-fetch (the client went away, or an outer timeout fired).
    // The fetch it started is not its own to abandon: if it dies with the caller, the rate limit
    // is spent with no outcome recorded, and every request for the next interval is told the
    // provider is down when nothing was tried.
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    idp.slow_next_discovery(Duration::from_millis(400));
    let verifier = verifier_with(
        &idp,
        Timing {
            request_timeout: Duration::from_millis(1500),
            ..Timing::default() // the 30 s interval: the lock-out, were there one, would last it
        },
    );
    let token = good(&idp);

    let first =
        tokio::time::timeout(Duration::from_millis(100), verifier.verify_token(&token)).await;
    assert!(
        first.is_err(),
        "the first request should have been dropped mid-fetch"
    );
    tokio::time::sleep(Duration::from_millis(600)).await; // the provider is healthy and quick now

    verifier
        .verify_token(&token)
        .await
        .expect("a cancelled caller must not leave the provider looking down");
}

#[tokio::test]
async fn a_known_key_is_not_made_to_wait_for_a_refresh_in_flight() {
    // The stale set is refreshing slowly. A caller that already holds the key it needs gets it
    // now; queuing behind the refresh would make every request pay the provider's slowness.
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let verifier = std::sync::Arc::new(verifier_with(
        &idp,
        Timing {
            refresh_interval: Duration::from_millis(50),
            min_refetch_interval: Duration::ZERO,
            request_timeout: Duration::from_millis(1500),
        },
    ));
    verifier
        .verify_token(&good(&idp))
        .await
        .expect("warm the cache");
    tokio::time::sleep(Duration::from_millis(100)).await; // now stale
    idp.slow_next_discovery(Duration::from_millis(800));

    let refresher = {
        let verifier = std::sync::Arc::clone(&verifier);
        let token = good(&idp);
        tokio::spawn(async move { verifier.verify_token(&token).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await; // the refresh is under way

    let started = std::time::Instant::now();
    verifier
        .verify_token(&good(&idp))
        .await
        .expect("the key is in the stale set");
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "a caller holding a usable key waited {:?} behind a refresh",
        started.elapsed()
    );
    refresher
        .await
        .expect("joined")
        .expect("the refresher itself is accepted");
}

#[tokio::test]
async fn an_encryption_key_is_not_a_signing_key() {
    // `k1` is published as `use: enc` next to a usable `k2`. A token signed by k1's key material
    // must not verify: the provider said the key is not for signatures.
    let idp = TestIdp::start(&[]).await;
    let mut encryption = SigningKey::A.jwk("k1");
    encryption["use"] = json!("enc");
    idp.publish_raw(vec![encryption, SigningKey::B.jwk("k2")]);

    assert_rejected(
        verifier(&idp).verify_token(&good(&idp)).await,
        "a token signed with a key published for encryption",
    );
}

#[tokio::test]
async fn a_key_published_for_another_algorithm_is_not_used_for_rs256() {
    let idp = TestIdp::start(&[]).await;
    let mut other = SigningKey::A.jwk("k1");
    other["alg"] = json!("RS512");
    idp.publish_raw(vec![other, SigningKey::B.jwk("k2")]);

    assert_rejected(
        verifier(&idp).verify_token(&good(&idp)).await,
        "a token verified with a key the provider restricted to RS512",
    );
}

#[tokio::test]
async fn the_leeway_is_seconds_not_minutes() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let verifier = verifier(&idp);
    let sign = |claims: &serde_json::Value| testing::sign(SigningKey::A, "k1", claims);

    // Inside the skew we tolerate.
    let mut barely_expired = testing::claims(idp.issuer());
    barely_expired["exp"] = json!(testing::now() - 10);
    verifier
        .verify_token(&sign(&barely_expired))
        .await
        .expect("ten seconds of clock skew is tolerated");

    // Ten minutes is not skew. Far inside the hours the old assertions used, far outside 30 s.
    let mut expired = testing::claims(idp.issuer());
    expired["exp"] = json!(testing::now() - 600);
    assert_rejected(
        verifier.verify_token(&sign(&expired)).await,
        "expired ten minutes ago",
    );
    let mut early = testing::claims(idp.issuer());
    early["nbf"] = json!(testing::now() + 600);
    assert_rejected(
        verifier.verify_token(&sign(&early)).await,
        "not valid for ten minutes",
    );
}

#[tokio::test]
async fn a_chunked_response_is_capped_without_a_content_length() {
    use std::fmt::Write as _;
    // The cap on what is read is the loop, not the header: a sender that never announces a length
    // is exactly the one that needs it. The key set here is VALID and carries a usable key, so a
    // verifier without the bound would accept the token; with it, nothing is trusted.
    let jwk = SigningKey::A.jwk("k1");
    let (issuer, server) = canned_provider(move |issuer, path| {
        let (body, chunked) = if path == "/jwks" {
            let body = format!(
                r#"{{"keys":[{jwk}],"pad":"{}"}}"#,
                "x".repeat(crate::http::MAX_BODY_BYTES)
            );
            (body, true)
        } else {
            (
                format!(r#"{{"issuer":"{issuer}","jwks_uri":"{issuer}/jwks"}}"#),
                false,
            )
        };
        if chunked {
            // Several chunks, no Content-Length.
            let mut out = String::from(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n",
            );
            for piece in body.as_bytes().chunks(16 * 1024) {
                let _ = write!(out, "{:x}\r\n", piece.len());
                out.push_str(&String::from_utf8_lossy(piece));
                out.push_str("\r\n");
            }
            out.push_str("0\r\n\r\n");
            out
        } else {
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            )
        }
    })
    .await;
    let verifier = Verifier::with_timing(
        &Config {
            issuer: issuer.clone(),
            audience: AUDIENCE.to_owned(),
            ..Default::default()
        },
        timing(),
    )
    .expect("config");

    let token = testing::sign(SigningKey::A, "k1", &testing::claims(&issuer));
    assert_unavailable(
        verifier.verify_token(&token).await,
        "a chunked body over the cap",
    );
    server.abort();
}

#[tokio::test]
async fn a_hung_request_is_cut_off_by_its_own_timeout_not_only_the_outer_budget() {
    // `request_timeout` is per request; `fetch_budget` (twice it) is the backstop over both. A
    // provider that accepts and says nothing must be given up on at the FIRST, not left for the
    // second to find.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let issuer = format!("http://{}", listener.local_addr().expect("addr"));
    let silent = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _held = stream;
                std::future::pending::<()>().await;
            });
        }
    });
    let verifier = Verifier::with_timing(
        &Config {
            issuer: issuer.clone(),
            audience: AUDIENCE.to_owned(),
            ..Default::default()
        },
        Timing {
            request_timeout: Duration::from_millis(500),
            ..Timing::default()
        },
    )
    .expect("config");

    let started = std::time::Instant::now();
    let token = testing::sign(SigningKey::A, "k1", &testing::claims(&issuer));
    assert_unavailable(verifier.verify_token(&token).await, "a silent provider");
    assert!(
        started.elapsed() < Duration::from_millis(800),
        "took {:?}: the per-request timeout (500 ms) did not fire before the backstop (1000 ms)",
        started.elapsed()
    );
    silent.abort();
}

#[tokio::test]
async fn a_whitespace_subject_names_nobody() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let mut claims = testing::claims(idp.issuer());
    claims["sub"] = json!("   ");

    assert_rejected(
        verifier(&idp)
            .verify_token(&testing::sign(SigningKey::A, "k1", &claims))
            .await,
        "a subject of spaces",
    );
}

#[tokio::test]
async fn a_token_with_a_crit_header_is_rejected() {
    // RFC 7515 §4.1.11: a `crit` header lists extensions the recipient MUST understand, and this
    // verifier understands none. Right algorithm, right key, valid signature: refused anyway.
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("k1".to_owned());
    header.crit = Some(vec!["exp-ext".to_owned()]);
    let token =
        jsonwebtoken::encode(&header, &testing::claims(idp.issuer()), &key_a()).expect("signs");

    assert_rejected(verifier(&idp).verify_token(&token).await, "a crit header");
}

#[tokio::test]
async fn two_keys_under_one_kid_are_ambiguous_and_trusted_by_neither_order() {
    // Which of them signed is not a question this verifier will guess at. Both orders, both
    // signers: nothing verifies under the shared kid. A third, unambiguous key still works.
    for order in [
        [SigningKey::A, SigningKey::B],
        [SigningKey::B, SigningKey::A],
    ] {
        let idp = TestIdp::start(&[]).await;
        idp.publish_raw(vec![
            order[0].jwk("shared"),
            order[1].jwk("shared"),
            SigningKey::A.jwk("solo"),
        ]);
        let verifier = verifier(&idp);

        for signer in [SigningKey::A, SigningKey::B] {
            let token = testing::sign(signer, "shared", &testing::claims(idp.issuer()));
            assert_rejected(verifier.verify_token(&token).await, "an ambiguous kid");
        }
        let solo = testing::sign(SigningKey::A, "solo", &testing::claims(idp.issuer()));
        verifier
            .verify_token(&solo)
            .await
            .expect("the unambiguous key is unaffected");
    }
}

#[tokio::test]
async fn an_error_status_is_not_trusted_for_what_its_body_says() {
    // A 500 (or a 3xx) whose body is a perfectly good discovery document and key set. Whatever sits
    // in front of the provider and answers with an error page is not the provider speaking, and the
    // status is the only thing that says so.
    let jwk = SigningKey::A.jwk("k1");
    let (issuer, server) = canned_provider(move |issuer, path| {
        let body = if path == "/jwks" {
            format!(r#"{{"keys":[{jwk}]}}"#)
        } else {
            format!(r#"{{"issuer":"{issuer}","jwks_uri":"{issuer}/jwks"}}"#)
        };
        format!(
            "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\n\
             content-length: {}\r\n\r\n{body}",
            body.len()
        )
    })
    .await;
    let verifier = Verifier::with_timing(
        &Config {
            issuer: issuer.clone(),
            audience: AUDIENCE.to_owned(),
            ..Default::default()
        },
        timing(),
    )
    .expect("config");

    let token = testing::sign(SigningKey::A, "k1", &testing::claims(&issuer));
    assert_unavailable(
        verifier.verify_token(&token).await,
        "a 500 carrying a valid key set",
    );
    server.abort();
}

// ---------------------------------------------------------------------------------------------
// Algorithm families (RT-67)
// ---------------------------------------------------------------------------------------------

/// A verifier configured with the given algorithms.
fn verifier_with_algorithms(idp: &TestIdp, algorithms: &[Algorithm]) -> Verifier {
    Verifier::with_timing(&config_with_algorithms(idp, algorithms), timing())
        .expect("a loopback issuer is a valid config")
}

#[tokio::test]
async fn rs256_works_with_rs256_configured() {
    // The default: exactly as before.
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let verifier = verifier(&idp);

    verifier
        .verify_token(&testing::sign(
            SigningKey::A,
            "k1",
            &testing::claims(idp.issuer()),
        ))
        .await
        .expect("RS256 with RS256 configured works");
}

#[tokio::test]
async fn ps256_verifies_with_ps256_configured() {
    let idp = TestIdp::start(&[]).await;
    // Publish the RSA key with alg: PS256 so the JWK and token algorithms agree.
    idp.publish_raw(vec![SigningKey::A.jwk_for("k1", Algorithm::PS256)]);
    let verifier = verifier_with_algorithms(&idp, &[Algorithm::PS256]);

    let token = testing::sign_with(
        SigningKey::A,
        Algorithm::PS256,
        "k1",
        &testing::claims(idp.issuer()),
    );
    verifier
        .verify_token(&token)
        .await
        .expect("PS256 with PS256 configured works");
}

#[tokio::test]
async fn es256_verifies_with_es256_configured() {
    let idp = TestIdp::start(&[("k1", SigningKey::Ec)]).await;
    let verifier = verifier_with_algorithms(&idp, &[Algorithm::ES256]);

    let token = testing::sign_with(
        SigningKey::Ec,
        Algorithm::ES256,
        "k1",
        &testing::claims(idp.issuer()),
    );
    verifier
        .verify_token(&token)
        .await
        .expect("ES256 with ES256 configured works");
}

#[tokio::test]
async fn eddsa_verifies_with_eddsa_configured() {
    let idp = TestIdp::start(&[("k1", SigningKey::Ed25519)]).await;
    let verifier = verifier_with_algorithms(&idp, &[Algorithm::EdDSA]);

    let token = testing::sign_with(
        SigningKey::Ed25519,
        Algorithm::EdDSA,
        "k1",
        &testing::claims(idp.issuer()),
    );
    verifier
        .verify_token(&token)
        .await
        .expect("EdDSA with EdDSA configured works");
}

#[tokio::test]
async fn multiple_algorithms_all_work() {
    // RS256 + ES256 and two keys: both verify.
    let idp = TestIdp::start(&[("k1", SigningKey::A), ("k2", SigningKey::Ec)]).await;
    let verifier = verifier_with_algorithms(&idp, &[Algorithm::RS256, Algorithm::ES256]);

    let rs256_token = testing::sign(SigningKey::A, "k1", &testing::claims(idp.issuer()));
    verifier
        .verify_token(&rs256_token)
        .await
        .expect("RS256 with [RS256, ES256] configured");

    let es256_token = testing::sign_with(
        SigningKey::Ec,
        Algorithm::ES256,
        "k2",
        &testing::claims(idp.issuer()),
    );
    verifier
        .verify_token(&es256_token)
        .await
        .expect("ES256 with [RS256, ES256] configured");

    // Both keys were fetched once.
    assert_eq!(idp.jwks_hits(), 1);
}

// ---------------------------------------------------------------------------------------------
// Algorithm filtering — rejected before any network request
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_algorithm_outside_the_configured_set_is_rejected_with_no_network_request() {
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    // Only RS256 configured, not PS256.
    let verifier = verifier_with_algorithms(&idp, &[Algorithm::RS256]);

    let token = testing::sign_with(
        SigningKey::A,
        Algorithm::PS256,
        "k1",
        &testing::claims(idp.issuer()),
    );
    assert_rejected(
        verifier.verify_token(&token).await,
        "PS256 when only RS256 is configured",
    );
    assert_eq!(
        idp.jwks_hits(),
        0,
        "a wrong algorithm should not wake the provider"
    );
}

#[tokio::test]
async fn es256_with_rsa_key_is_refused() {
    // ES256 configured, but the JWKS only has an RSA key under k1. The fetch finds
    // no key usable for ES256, so the cache stays cold and the verdict is Unavailable
    // (we could not find out), not Rejected: the key may exist but we cannot use it.
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let verifier = verifier_with_algorithms(&idp, &[Algorithm::ES256]);

    let token = testing::sign_with(
        SigningKey::Ec,
        Algorithm::ES256,
        "k1",
        &testing::claims(idp.issuer()),
    );
    assert_unavailable(
        verifier.verify_token(&token).await,
        "ES256 token with an RSA key under the same kid",
    );
}

#[tokio::test]
async fn rs256_with_ec_key_is_refused() {
    // RS256 configured, but the JWKS only has an EC key under k1. The fetch finds
    // no key usable for RS256, so the verdict is Unavailable (we could not check).
    let idp = TestIdp::start(&[("k1", SigningKey::Ec)]).await;
    let verifier = verifier_with_algorithms(&idp, &[Algorithm::RS256]);

    let token = testing::sign(SigningKey::A, "k1", &testing::claims(idp.issuer()));
    assert_unavailable(
        verifier.verify_token(&token).await,
        "RS256 token with an EC key under the same kid",
    );
}

#[tokio::test]
async fn a_jwk_whose_alg_disagrees_with_the_token_is_refused() {
    // The JWK carries `alg: RS256` but the token is PS256.
    let idp = TestIdp::start(&[]).await;
    let rsa_key = SigningKey::A.jwk_for("k1", Algorithm::RS256);
    // The `jwk_for` sets `alg: RS256` in the JWK. The verifier allows both RS256 and PS256.
    idp.publish_raw(vec![rsa_key.clone(), SigningKey::Ec.jwk("k2")]);
    let verifier = verifier_with_algorithms(&idp, &[Algorithm::RS256, Algorithm::PS256]);

    let token = testing::sign_with(
        SigningKey::A,
        Algorithm::PS256,
        "k1",
        &testing::claims(idp.issuer()),
    );
    assert_rejected(
        verifier.verify_token(&token).await,
        "PS256 token with a key restricted to RS256 by its JWK alg",
    );
}

#[tokio::test]
async fn a_key_without_an_alg_field_works_for_any_matching_family() {
    // The JWK has no `alg` field, so both RS256 and PS256 should work.
    let idp = TestIdp::start(&[]).await;
    // Build a JWK without an `alg` field.
    let mut jwk = SigningKey::A.jwk("k1");
    jwk.as_object_mut().expect("an object").remove("alg");
    idp.publish_raw(vec![jwk]);
    let verifier = verifier_with_algorithms(&idp, &[Algorithm::RS256, Algorithm::PS256]);

    let rs256_token = testing::sign(SigningKey::A, "k1", &testing::claims(idp.issuer()));
    verifier
        .verify_token(&rs256_token)
        .await
        .expect("RS256 with an unconstrained RSA key");

    let ps256_token = testing::sign_with(
        SigningKey::A,
        Algorithm::PS256,
        "k1",
        &testing::claims(idp.issuer()),
    );
    verifier
        .verify_token(&ps256_token)
        .await
        .expect("PS256 with an unconstrained RSA key");
}

#[tokio::test]
async fn an_ec_p384_key_for_es256_is_refused_curve_mismatch() {
    // The JWK is built from a real P-256 key but with `crv` set to `P-384`. The key material
    // is valid, so the fetch succeeds, but the curve check (ES256 needs P-256) rejects it.
    let idp = TestIdp::start(&[]).await;
    let jwk = SigningKey::Ec.jwk_with_fake_curve("p384", Algorithm::ES256, "P-384");
    idp.publish_raw(vec![jwk, SigningKey::A.jwk("rsa")]);
    let verifier = verifier_with_algorithms(&idp, &[Algorithm::ES256, Algorithm::ES384]);

    // ES256 token naming a key that the JWK claims is P-384 — the curve check is what refuses it.
    let es256_token = testing::sign_with(
        SigningKey::Ec,
        Algorithm::ES256,
        "p384",
        &testing::claims(idp.issuer()),
    );
    assert_rejected(
        verifier.verify_token(&es256_token).await,
        "ES256 token naming a P-384 key",
    );
}

// ---------------------------------------------------------------------------------------------
// Algorithm set validation
// ---------------------------------------------------------------------------------------------

#[test]
fn an_empty_algorithm_list_is_refused() {
    let error = crate::AlgorithmSet::from_strs(&[]).expect_err("empty list");
    assert!(
        matches!(error, crate::ConfigError::EmptyAlgorithms),
        "{error}"
    );
}

#[test]
fn hs256_is_not_allowed() {
    let error = crate::AlgorithmSet::from_strs(&["HS256".to_owned()]).expect_err("HS256");
    assert!(
        matches!(error, crate::ConfigError::UnknownAlgorithm(ref name) if name == "HS256"),
        "{error}"
    );
}

#[test]
fn none_is_not_allowed() {
    let error = crate::AlgorithmSet::from_strs(&["none".to_owned()]).expect_err("none");
    // "none" won't parse as Algorithm at all — that's fine, it's refused.
    assert!(error.to_string().contains("none"), "{error}");
}

#[test]
fn an_unknown_algorithm_is_refused() {
    let error = crate::AlgorithmSet::from_strs(&["BOGUS".to_owned()]).expect_err("bogus");
    assert!(
        matches!(&error, crate::ConfigError::UnknownAlgorithm(name) if name == "BOGUS"),
        "{error}"
    );
}

#[test]
fn a_duplicate_algorithm_is_refused() {
    let error = crate::AlgorithmSet::from_strs(&[
        "RS256".to_owned(),
        "ES256".to_owned(),
        "RS256".to_owned(),
    ])
    .expect_err("duplicate");
    assert!(
        matches!(&error, crate::ConfigError::DuplicateAlgorithm(name) if name == "RS256"),
        "{error}"
    );
}

#[test]
fn every_allowed_algorithm_parses_and_is_accepted() {
    for algorithm in crate::ALLOWED_ALGORITHMS {
        let name = format!("{algorithm:?}");
        crate::AlgorithmSet::from_strs(&[name])
            .unwrap_or_else(|error| panic!("allowed algorithm {algorithm:?} was refused: {error}"));
    }
}

#[test]
fn the_default_is_rs256_only() {
    let set = crate::AlgorithmSet::default_set();
    assert!(set.contains(Algorithm::RS256));
    assert!(!set.contains(Algorithm::ES256));
    assert!(!set.contains(Algorithm::PS256));
    assert!(!set.contains(Algorithm::EdDSA));
}

// ---------------------------------------------------------------------------------------------
// Cache regression (PR #94 review): a refresh with no configured-usable key preserves the cache
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_refresh_with_no_configured_usable_key_keeps_the_old_cache() {
    // P2b: Warm k1 (RS256); refresh publishes only a JWK with alg=RS384. The new set
    // has no key usable for the configured RS256, so the refresh is a failed fetch
    // and the old cache survives.
    let idp = TestIdp::start(&[("k1", SigningKey::A)]).await;
    let verifier = verifier_with(
        &idp,
        Timing {
            refresh_interval: Duration::ZERO,
            min_refetch_interval: Duration::ZERO,
            ..timing()
        },
    );
    let token = testing::sign(SigningKey::A, "k1", &testing::claims(idp.issuer()));
    verifier.verify_token(&token).await.expect("warm the cache");
    assert_eq!(idp.jwks_hits(), 1);

    // Republish: only an RSA key whose JWK `alg` is RS384. The verifier is only RS256.
    idp.publish_raw(vec![SigningKey::B.jwk_for("other", Algorithm::RS384)]);

    // The refresh fires (refresh_interval = 0), finds no RS256-usable key, and keeps
    // the old cache. The RS256 token signed by k1 still verifies.
    verifier
        .verify_token(&token)
        .await
        .expect("cached k1 survives a refresh with no configured-usable key");
    // Two fetches: the initial one and the (failed) refresh.
    assert_eq!(idp.jwks_hits(), 2);
}
