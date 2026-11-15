# 0027. OIDC verification is a generator, not a default

- **Status:** Accepted
- **Date:** 2026-11-10

## Context

ADR 0012 ships the identity seam and nothing that crosses it. The shipped verifier,
`NoIdentities`, authenticates nobody, so a protected method refuses every caller until a product
supplies a real one — and 0012 says why that is the point: which credential a service accepts is
the most product-specific decision in the tree. It also says there is no `[auth]` section,
"because there is nothing to configure — a verifier is code, not a key".

The first product that wants a real verifier has an identity provider, not a requirement for a
design. The provider is an OpenID Connect issuer, its access tokens are RS256 JWTs, and the
question is the one 0012 left to the product: what goes on the line that says
`Arc::new(identity::NoIdentities)`. Answering it by hand means every product writes the same
signature check, and the same few mistakes with it — an algorithm taken from the token's own
header, a key set fetched on every request or never refreshed, a provider outage reported as a bad
credential, a root store that works on the developer's machine and not in the image.

The three obvious ways to give them the answer all break something 0012 settled. Shipping it as a
default puts a provider's name, a token's format and a network dependency into every service the
template scaffolds. Shipping it as a feature of `crates/identity` breaks the crate's admission
rule (nothing that knows a provider's name or a token's format). Shipping nothing leaves every
product to write it.

## Decision

**`cargo xtask add-auth-oidc <service>` installs it, for one service, when a product asks.**
Nothing changes in a product until it is run: there is no `[auth]` section, no dependency and no
`crates/oidc` before it. That is 0012's "no section nothing reads" kept intact — the section
exists exactly when the code that reads it does. It is the same shape as `add-database` (a
generator that edits a service in code, anchored, refusing before it writes), and for the same
reason `just add` cannot do it: a template overlay copies files, and this edits `config.rs`,
`main.rs` and a manifest that already exist.

**What it writes.**

- **`crates/oidc`**, once, from a copy embedded in `xtask` (a product has no `templates/`). A
  second service finds it and is only wired; the crate is left exactly as it is, because it is the
  product's after the first run. The template keeps the same crate for real, so `ci-test` compiles,
  lints and runs its tests where they are written, and a test keeps the embedded copy identical to
  it. `cli new` removes it from a product, and with it the workspace dependency entries that
  only it uses, which the command writes back. Inserted workspace dependencies are enclosed
  by `# --- added by cargo xtask add-auth-oidc ---` and
  `# --- end added by cargo xtask add-auth-oidc ---`: product-side ownership markers, not
  template-only markers, so future updates can recognize them without stripping them.
  Dependencies a product already supplied retain their versions and remain outside the markers.
- **In the service:** an `[auth]` section (`issuer`, `audience`) and its `validate()`; the `oidc`
  dependency; `src/auth.rs`, one function (`verifier(&AuthSettings)`) that builds the verifier;
  `main.rs` calling it where `NoIdentities` was, with a comment that is the history of the line; the
  audience in `config/<name>.toml`, defaulting to the product's `[publish].project`; and
  `tests/<name>/oidc.rs`, which drives `Whoami` through the real router and the real verifier
  against a provider in the test process. The function exists because `main.rs` is a binary that no
  test can run: with the construction written in `main.rs`, replacing it with `NoIdentities` left
  every generated test green. The test builds its router through the same function, so breaking it
  fails a test.
- **Optionally `--issuer`**, into `config/<name>.local.toml` for `devenv up`. Never into the
  committed base file, which is baked into the image: an issuer names a deployment.

**What the verifier checks.** A token must be RS256 — the allow-list is exactly that, checked
from the header before the network is touched, so `alg: none` and an HMAC signed with the public
key (the key-confusion attack) cost nothing to refuse. It must name a `kid` the provider
publishes, and carry `exp` (required, 30 s leeway), `nbf` when present, `iss` equal to the
configured issuer, `aud` containing the configured audience, and a `sub` that is not blank. A
token with a `crit` header is refused (RFC 7515 §4.1.11: it lists extensions the recipient must
understand, and this verifier understands none). `iss`, `aud`,
`exp` and `sub` are required to be *present*, because the library validates a claim only if the
token has it. The subject becomes the `Principal`. Nothing else is read: Pocket ID's access tokens
carry no groups and no e-mail, and a verifier that required them would accept nobody.

**Two kinds of "no", as 0012 requires.** A malformed, foreign, expired or wrong-audience token is
`Rejected` (`unauthenticated`). A verifier that could not find out is `Unavailable` (`internal`):
the provider is unreachable and the cache holds nothing that settles it, including a `kid` it has
never seen while the provider cannot be asked. Neither carries the token. Reasons are fixed
strings, never text from the token or the network — `jsonwebtoken`'s own messages are not used,
because some quote the claim they choked on.

**Keys are fetched lazily and cached.** Nothing is fetched at startup, so a replica that boots
during a provider outage comes up and serves its public methods, for the reason `db::connect` is
lazy. Discovery is `<issuer>/.well-known/openid-configuration`; its `issuer` must equal the
configured one, and its `jwks_uri` is held to the same rule as the issuer. A cached key outlives
the provider: a failed refresh keeps the last good set, and a token signed by a key we hold
verifies through an outage. An unknown `kid` may refetch, rate-limited to one fetch per 30 s
whatever asked for it, so a flood of random `kid`s costs the provider one request per interval. A
bad signature on a `kid` we hold never refetches: that would hand anyone a way to make us ask.
The set is refreshed after 15 minutes. A caller that already holds a usable key does not queue
behind a refresh in flight.

Three rules about what a fetch may do to the cache, each from a defect found in review. **A set
with no usable key is a failed fetch**, not an answer: a provider that returns 200 with
`"keys": []` mid-deploy has not revoked everything, and replacing the cache with nothing turned
that into a 401 for every caller. It keeps the old set (or is `Unavailable` when there is none);
a non-empty set still replaces it, so a rotation that drops a key drops it. **A refresh runs on
its own task**, which records its own outcome: when it belonged to the request that started it, a
client disconnect during the first, cold fetch left the rate limit spent with nothing recorded,
and a healthy provider looked down for the next 30 s. **Two keys under one `kid` are trusted by
neither**: which of them signed is not a question to guess at, and "last one wins" would let the
order of a list the provider controls decide it.

**The HTTPS client carries its own trust.** The service image is `FROM scratch`, with no CA
bundle. A verifier against the *platform* store would work in the dev shell and a NixOS host and
fail only in the container, as a handshake error that reads like a provider outage. So the client
(`hyper-util` over `hyper-rustls`, not `reqwest`: see below) is built from a `rustls::ClientConfig`
whose root store is `webpki-roots`, compiled in, and handed over whole; nothing consults the
platform. Redirects are not followed (a 3xx is a failure), there is no idle pool, one timeout holds
the whole exchange to a figure below the service's request budget, a 1 MiB cap on what it reads. A plain `http://` issuer is accepted only
for loopback, for tests and local development, and refused at configuration validation otherwise.
`localhost` is trusted **by name** and not resolved: a hosts file or resolver that maps it elsewhere
defeats the rule. That is accepted — it is the developer's own machine, and the alternative is a
resolver call in configuration validation — and said here so it is a decision and not an oversight.

**Cryptography is `jsonwebtoken` 11 on `aws-lc-rs`**, the same provider the TLS stack uses.

**Startup fails closed.** `AuthSettings::validate()` runs from `config::load`, so a service with no
issuer stops with a message naming `APP__AUTH__ISSUER`. The alternative, a service that starts and
denies everyone, is indistinguishable from a provider outage to whoever is on call.

**Readiness is deliberately not touched.** `/readyz` does not consult the provider. An outage
there must not take every replica out of rotation, public methods included — the reasoning that
keeps `/healthz` off the database — and a request that needs a key it cannot fetch already answers
`internal`, which is visible where it should be. The closing output of the command says so, the way
`add-database` names its probe.

## Consequences

Choosing OIDC is one command and a review of one diff. A product that never runs it carries none
of it: no crate, no dependencies, no `aws-lc-sys` build.

A product that does run it takes a C library build (`aws-lc-sys`) and a second TLS crypto provider
into its graph. `rustls` then has `ring` (from `sqlx`) and `aws-lc-rs` both enabled, so a process
cannot ask rustls for "the default provider" — it panics when there are two. Everything here, and
`sqlx`, names its provider explicitly. Anything a product adds that calls `ClientConfig::builder()`
will hit it, and the failure names the cause.

**The implicit-provider hazard reached further than `crates/oidc`, and the first version of this
record missed it — twice.** `reqwest`'s TLS features are unified across everything one cargo
invocation builds, and `opentelemetry-otlp` (in `crates/telemetry`) builds a blocking `reqwest`
client inside `init`. The first version of this crate used `reqwest` with the provider-less TLS
feature, so that exporter got TLS too, and with no default provider it panicked at construction:
the service died at startup whenever an OTLP endpoint was set (`devenv up` sets one), in any
`--workspace` build, the template's own `api` included, and in no per-package one — which is why
every test passed. The first fix, `reqwest`'s `rustls` feature (an aws-lc-rs default provider),
passed the same test on a developer machine and **failed in the sandboxed `ci-test`**: that
feature's default verifier is the platform trust store, which does not exist in a sandbox and does
not exist in the `FROM scratch` image, so the exporter's client fails to *build* there. The same
failure waiting in the container, caught only because the guard test runs where there is no CA
bundle.

So the fix is to stop touching `reqwest`'s features at all. `crates/oidc` does its HTTPS with
`hyper-util` and `hyper-rustls` over an explicit `ClientConfig`, and the exporter's client is
exactly what it was before this work: no TLS, no provider question. A regression test in
`crates/telemetry` builds the exporter with an endpoint set and runs in the workspace-unified
`ci-test`; it fails on both spellings of the `reqwest` feature there. Re-audited, the other clients
in the graph are fine: `sqlx` calls `builder_with_provider`; `gpui-pre-reqwest` is a separate crate
with its own ring fallback; `hyper-rustls`'s `ClientConfig::builder()` paths are reached only by
features nothing enables. The cost is that this crate owns about a hundred lines of HTTP client
(timeout, body cap, status handling) that `reqwest` would have given it, each with a test.

Rejected for it: a process-wide default provider installed in `telemetry::init` (every product would
then take `rustls` into `crates/telemetry`, including those that never ran the generator, and it
would not fix the platform verifier); building the exporter's client explicitly (a second TLS
configuration to keep in step with the first, and `telemetry` depending on the generator's crates);
and keeping the feature from unifying (cargo does not offer that).

A verifier is only as current as its last fetch. A key the provider revokes keeps verifying until
the set is refreshed (15 minutes) or the process restarts; a provider outage extends that, because
a stale set is kept rather than discarded. That is the price of surviving the outage, and a
product with a stricter revocation requirement needs introspection, which this is not.

**There is no maximum key age** (decided for M2.4). The verifier serves the last good keys
through any outage or no-usable-key answer, and warns on each refresh that did not replace them.
Pocket ID rotates by publishing a new non-empty set, which replaces the old one immediately, so
the cap would buy nothing against the provider this was written for. What it would cost is
real: the identity provider lives in the cluster, cerberus deployments depend on it, and an
age cap would turn a long outage of it into failed logins everywhere, including for callers
holding a token signed by a key we have. Revisit if a provider is found that revokes by
emptying its set, which is the one revocation this design cannot see.

**The pipeline's VM tests give a service with `[auth]` the issuer `http://127.0.0.1:1`**, through
the module's `environment` option and the container's environment. The service refuses to start
without one and the committed file cannot carry it, so without this both `<svc>-module-vm` and
`<svc>-image-vm` failed at config load in any product that had run the generator; the template
never saw it, because its own `api` has no `[auth]`. Nothing listens on that port, which is
fine: keys are fetched lazily and the tests present no token, and loopback `http` is the one
plain-text issuer the verifier accepts. A stub provider on the VM was rejected as a second server
to keep honest for nothing these tests are about. `auth-issuer-vm-environment` (in `ci-pure`)
holds it by evaluating the nodes' environments, since the template's own tests cannot.

`oidc::testing` carries a committed RSA key and a test CA, behind a feature a service enables only
from `[dev-dependencies]`. They are public and protect nothing; a token signed with them is
accepted by a verifier pointed at the test provider and by nothing else.

**Still out**, and named so it is not mistaken for forgotten: groups, roles and any use of claims
beyond `sub` (`permission_denied` stays unused); refresh and rotation of the caller's own token;
sessions and cookies; and the login flows — authorization code with PKCE, device flow — that
produce a token in the first place. Those are the next milestones, and they decide what
`apps/counter`'s clients present. This record is the verifying half only.

## What was rejected, and why

**Shipping it as the default verifier.** It would put a provider, a token format and a network
dependency into every scaffolded service, and make `NoIdentities` a thing to be removed. 0012's
reasoning about a prebuilt authentication applies in full; what changed is that a product can now
ask for this one without writing it.

**A feature of `crates/identity`.** The crate's admission rule is the decision: nothing that knows
a provider's name, a token's format or a role's meaning. This is all three.

**`jsonwebtoken` on `rust_crypto`.** It pulls `rsa` 0.9, which carries RUSTSEC-2023-0071 (the
Marvin timing attack) with no fix below 0.10. `deny.toml` never ignores a vulnerability advisory,
so the choice was between an exception it forbids and a different backend. `aws-lc-rs` costs a C
build and no exception: the licence expressions of `aws-lc-rs` and `aws-lc-sys` are made of `ISC`,
`Apache-2.0`, `MIT`, `MIT-0` and `BSD-3-Clause`, all already on the allow list.

**Hand-rolled RS256 on `ring`.** About a hundred and fifty lines, no C, no new crate — and this
repository would own the JOSE parsing and the algorithm-confusion defences, which is the part of
this that has gone wrong in other people's code. The library has the defences; what we own is the
choice of what to ask it.

**The platform verifier for the HTTPS client.** Right on a host, wrong in the image this repository
builds, and wrong in a way a test suite on a host cannot see.

**`reqwest` for the HTTPS client.** Its TLS features are shared with every other `reqwest` user in
the build, including the telemetry exporter, which turned out to be the dangerous part (above).

**Shipping a CA bundle in the image instead.** A second thing every product must remember, and one
that ages.

**Fetching keys at startup.** A replica would then fail to boot, rather than boot and serve, during
exactly the outage that is hardest to diagnose.

**Failing a request on any refresh failure.** It turns a provider's outage into every caller's,
including those holding a token signed by a key we already have.

**A `/readyz` component for the provider.** It would take every replica out of rotation on a
provider outage, which the public methods did not need, and a service whose job is partly to be
reachable would become unreachable for a reason that is not its own.

**Starting without an issuer and denying everyone.** Fail-closed, but invisible: from outside it
looks like an outage. Refusing to start is the version that says what is wrong.

**Reading groups, roles or e-mail.** The first provider issues none in an access token, and what a
role means is a product's decision (0012). A verifier that demanded them would accept nobody.

## Amendment: configurable signing algorithms (RT-67)

- **Date:** 2026-12-12

**The verifier accepts a configured set of signing algorithms, not only RS256.**

**Why.** Keycloak and others may sign access tokens with PS256 or ES256. Pocket ID uses RS256,
and that stays the default.

**The allow-list.** The algorithms a product may configure are: RS256, RS384, RS512, PS256, PS384,
PS512, ES256, ES384, EdDSA. `none` and every HS* (HMAC) variant are **never** accepted - an HMAC
key would be the public JWK, which is a classic confusion attack. The default is `["RS256"]`.

**Config.** The `AuthSettings` struct gains `algorithms: Vec<String>`, defaulting to
`["RS256"]`. Unknown names, HS*, `none`, an empty list and duplicates are all startup errors
with clear messages. The list maps to `jsonwebtoken::Algorithm` in `oidc::AlgorithmSet::from_strs`.

**Header pre-check.** A token whose `alg` is not in the configured set is refused **before any
network call**, exactly as before for non-RS256.

**JWK matching.** A key is used only if its `kty` (RSA, EC, OKP) fits the token's algorithm
family and, when the JWK carries an `alg`, that `alg` equals the token's. A curve mismatch
is refused (ES256 needs P-256, ES384 P-384, EdDSA Ed25519). Cache semantics are unchanged:
an unknown kid triggers a refresh, no cap on stale-key age.

**`Validation` is built per-request.** `jsonwebtoken::Validation` requires all algorithms
in its list to share a family (RSA, EC, or Ed), but a configured set may span families.
So the `Validation` is built for the token's specific algorithm in `verify_token`, with
the issuer, audience and required claims from the verifier's configuration. The header
pre-check already refused anything outside the configured set before any network call.

**No new crypto dependency.** `jsonwebtoken` 11 on `aws_lc_rs` handles RS*, PS*, ES* and EdDSA.

## Browser login (`add-auth-oidc --web`, RT-59)

```sh
cargo xtask add-auth-oidc api --web --issuer https://id.example.com
```

This additionally mounts `/auth/login`, `/auth/callback`, `/auth/session` and `/auth/logout`
outside the Connect nest, before the SPA fallback (ADR 0029). Configure `[auth] public_url`
(e.g. `https://pilot.noosphere.uk`) or set `APP__AUTH__PUBLIC_URL`. Supply secret
`auth.cookie_key` through the settings loader's existing `APP__AUTH__COOKIE_KEY_FILE`
indirection: the named file contains the standard, padded base64 encoding of 32 random
bytes, generated with `openssl rand -base64 32`. Never commit a deployment key. Missing,
invalid base64 or wrong decoded length is `settings::Error::Invalid` naming `auth.cookie_key`
at load, and Debug/Display redact it. A missing public URL is also refused by name.

`public_url` must be an absolute HTTPS origin, with no path except `/`, query, fragment or
userinfo; HTTP is allowed only for `localhost` or `127.0.0.1`. Register
`<public_url>/auth/callback` with the provider: both authorization and code exchange use it.
The CSRF Origin fallback compares scheme, host and port to that public origin, not proxy
request headers. Scopes default to `openid profile email`, must include `openid`, and contain
no empty/whitespace entries; optional `login_audience` must be nonempty after trimming.

Cookies use aws-lc-rs AES-256-GCM with random 96-bit nonces, reusing the TLS crypto provider.
The SPA never receives a refresh token; it sends its in-memory access token as Bearer.
Without `--web`, generated services remain byte-identical to verifier-only output; the
embedded OIDC crate intentionally also includes disabled, feature-gated BFF sources.
