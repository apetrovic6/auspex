# 0028. Native shells log in with oauth2, not openidconnect

- **Status:** Accepted
- **Date:** 2026-11-11

## Context

ADR 0027 delivered the *verifying* half of authentication: a service that has run
`add-auth-oidc` checks access tokens. The *producing* half was explicitly left out — the login
flows that mint a token in the first place, and the refresh that keeps one alive. The example
app's TUI and desktop shells still present whatever `COUNTER_API_TOKEN` says, or nothing; the web
shell has no login at all.

The first product to need it has an OpenID Connect provider (Pocket ID), and the obvious library
for an OIDC client is `openidconnect`. It is the wrong one here. `openidconnect` hard-depends on
`rsa` 0.9, which carries RUSTSEC-2023-0071 (the Marvin timing attack) with no fix below 0.10, and
`deny.toml` never ignores a vulnerability advisory — so the choice was between an exception the
policy forbids and a different library. `oauth2` 5 is the layer `openidconnect` sits on, from the
same author, and has no `rsa`: it provides PKCE (S256), the authorization-code exchange, the
device grant and refresh. ID-token validation is not needed, because the API only trusts the
access token (`crates/oidc`), and the "signed in as" name comes from the provider's userinfo
endpoint, called with that access token.

The second constraint is the one ADR 0027 recorded twice. `reqwest`'s TLS features are unified
across everything one cargo invocation builds, and `opentelemetry-otlp` builds a blocking `reqwest`
client inside `crates/telemetry`; any TLS feature a login crate asked of `reqwest` would reach that
exporter and break it. `oauth2`'s own default features pull a `reqwest` client, so it is taken with
`default-features = false` and given a client of our own, on the same `hyper` + `hyper-rustls`
stack `crates/oidc` already owns.

The native client had no TLS at all: it built `HttpClient::plaintext()`, so `COUNTER_API_URL=
https://…` could not work. Login needs HTTPS, and so does any product pointed at a real API.

## Decision

**The example app's native shells log in with `oauth2` 5, off unless configured.** The code lives
in `apps/counter/client`'s `login` feature (native-only; `native` first gains `client-tls`), so the
core never sees a credential (ADR 0012, enforced by `CORE_FORBIDDEN_DEPENDENCIES`) and the web
shell never compiles it. `COUNTER_OIDC_ISSUER` unset means login is off and everything behaves
exactly as before — `COUNTER_API_TOKEN` still works, and no discovery, keyring or button exists.
A product inherits it through `cli new`'s example app and `--drop-example` drops it.

**The HTTPS client is the shared one, extended.** `crates/oidc/src/http.rs`'s rules carry over
verbatim — no redirects, a 1 MiB body cap, one timeout over the whole exchange, no idle pool — and
the pieces that were oidc-only (`rustls`, `webpki-roots`, `hyper`, `hyper-rustls`, `hyper-util`,
`http-body-util`, `url`) move out of the template-only section of `[workspace.dependencies]` into
the shared one, because the client crate now needs them in a product too. Only `jsonwebtoken` stays
template-only. The `oauth2::AsyncHttpClient` impl is a thin adapter over this client: the login
module supplies its own HTTP, so no reqwest and no TLS feature on it. The `rustls::ClientConfig` is
built with `builder_with_provider(aws_lc_rs::default_provider())`, never `ClientConfig::builder()`,
which panics when `sqlx`'s `ring` is compiled in beside aws-lc-rs; the roots are `webpki-roots`,
compiled in, so the native shells need no platform CA bundle either.

**Two flows, one per shell.** The TUI uses the device flow (RFC 8628): show the user code and the
verification link, and poll, honouring `authorization_pending`, `slow_down`, expiry and denial.
While a device code is displayed, `y` copies the verification link, `c` copies the user code,
and `o` explicitly opens the link using the same injectable opener as desktop login. Nothing
opens automatically: headless/SSH terminals remain supported. Copy uses OSC 52 through the
terminal backend, not an X11/Wayland clipboard; terminal clipboard permission must allow it,
and tmux needs `set -g set-clipboard on`. The screen keeps the plain link and code, while
copy/open prefer RFC 8628's optional `verification_uri_complete` when supplied (Pocket ID
supplied it in a live device-authorization probe on 2026-11-12). Both locales show key hints
and action confirmations; opener failure is a message rather than a panic.

The desktop uses the authorization-code flow with S256 PKCE and a loopback redirect: bind
`127.0.0.1:0`, `redirect_uri = http://127.0.0.1:<port>/callback`, a random `state`, serve exactly
one valid callback (state compared in constant time, other paths 404'd, wrong state 400'd without
consuming the shot), answer with a small "close this tab" page, and shut the listener down after
use. The browser is opened with the `open` crate (`that_detached`), not a shell: it is one
well-scoped call, cross-platform for the Apple build, and the URL carries only `state` and the PKCE
challenge — never a secret, so handing it to a command would be safe too, but `open` is the one
that stays correct on macOS. The client id defaults to `counter-native` — a public client's id is a
registration, not a credential — overridable with `COUNTER_OIDC_CLIENT_ID`; `pilot` is pilot's.

**Sessions are per shell, and refresh is proactive.** `AuthTransport`, a `ClientTransport` wrapper
beside `TracingTransport`, attaches the current access token and refreshes it before expiry (a 60 s
skew), never as a retry-on-401 — a mutation retried after a 401 would be sent twice. The exchange
and its persistence run in a **spawned task** that owns the work; callers await a shared handle, so
an RPC that hits its own deadline drops only its *wait* and the rotated pair is still persisted —
the next call presents the rotated token, never the spent one. Refresh is serialised, and after a
transient failure a short **cooldown** (30 s) makes a burst of calls share one attempt instead of
hammering a struggling provider. Only a refresh the provider *refuses* — `invalid_grant`,
`invalid_client` or `unauthorized_client`, judged by the OAuth error **code** — clears the session
and reports signed out; `temporarily_unavailable`, `server_error`, any 5xx, a timeout or a
malformed body all keep the session. A refresh response that omits `refresh_token` (RFC 6749 §6)
keeps the one that was presented, and a sign-in without a refresh token is allowed — the session
works until the access token expires, then signs out. A token response without `expires_in`
defaults to 300 s. A sign-out (or a newer sign-in) wins over an in-flight refresh or sign-in via a
session **generation** counter, so a racing flow discards its stale result. Each shell stores its
own session — keyed per app and per shell — because Pocket ID rotates the refresh token on every
use, and two shells sharing one would revoke each other.

**Storage is the OS keyring and the file as one source of truth.** `keyring` 4 (Secret Service over
zbus on Linux) plus a JSON file in `$XDG_STATE_HOME/<app>/session-<shell>.json`. A load reads
**both** and returns the newer session (by `saved_at_unix_ms`), so a stale keyring entry from an
earlier fallback write never beats the file's newer session, and an empty keyring does not hide a
good file session. A save writes the keyring when it can and the file otherwise, then best-effort
deletes the other backend's copy. A clear deletes both and fails when a backend that still holds a
session refused — surfaced to the shells as "couldn't forget the saved session". The file is
written atomically (`create_new` temp file with a random suffix, mode 0600 at creation, renamed
over the target, removed on every error path) in a 0700 directory; nothing follows a symlink (a
session path or app directory that is a symlink is refused, and mode changes go through an
`O_NOFOLLOW` descriptor). A corrupt or missing file is signed out; an old-format session without
`saved_at` or `issuer` is also signed out. With neither a keyring nor a state directory the session
lives in memory for the process only — never in the current working directory — and the shells show
an i18n note that it won't be kept after closing. The backends sit behind a trait so CI — which has
no Secret Service — covers the keyring with a fake and the file backend for real.

**Discovery, the API and the issuer are bound and scheme-checked.** A discovered authorization,
token, device-authorization or userinfo endpoint must be `https`, unless the issuer itself is
loopback (the `validate_issuer` exemption); a document that moves the credential exchange to
plaintext is refused with `Malformed` before any request. The document's own `issuer`, when
present, must be **exactly** the configured issuer (RFC 8414 §3.3). A session records its issuer,
and a stored session whose issuer differs from the configured one is signed out and cleared, its
refresh token never sent. When login is configured, the native client also refuses a non-loopback
`http://` `COUNTER_API_URL` at startup, so a refreshed session token never rides cleartext. The
device-poll interval is floored at 5 s (injectable in tests) so a provider's `interval: 0` cannot
turn the TUI into a busy loop. Sign-in is single-flight: a second call while a flow runs is a no-op,
and dropping a flow resets the status and closes the listener. A `callback?error=…` with the right
`state` ends the desktop flow at once as "denied" (as does a device-flow consent denial); callback
connections are served concurrently, capped at 16, so a silent peer cannot starve the real callback.

**Scopes are `openid profile email`**, the ones userinfo needs.

## Consequences

The native shells get a working login, and the native API client gets TLS for any `https://`
`COUNTER_API_URL`, login or not. A shell never holds a token in the core, and nothing logs, prints
or puts one on a command line: the `Session` type's `Debug` redacts its tokens, and every error a
login path can produce is a fixed string chosen in this code, never text from the token or the
network.

The template-only section of `[workspace.dependencies]` shrinks to `jsonwebtoken`; the seven TLS
and HTTP entries move into the shared section. `add-auth-oidc`'s plan-workspace step now writes
back only what a product lacks, and its fixture is updated to match. A product that uses
`--drop-example` keeps the seven shared entries but no longer builds anything that reads them;
that is a harmless unused-dependency warning, not a failure, and it is the price of not
duplicating the versions in two manifests.

The dependency graph gains `oauth2` (with its `sha2`, `chrono`, `rand`, `base64` companions),
`keyring` (unifying with the gpui stack's zbus 5, and adding `sha2` 0.11 beside 0.10, a
`multiple-versions` warning and not a refusal), and `open`. No `rsa`, no reqwest, no ring from
this change.

**Still out**, named so it is not mistaken for forgotten: the web shell's login (M2.3 — a browser
redirect and a cookie/session the page holds), the Android platform's login (M3 — the same flows
through its own clients), end-session revocation, cancelling a sign-in that is already in flight
(N10), and any use of claims beyond a display name.

## What was rejected, and why

**`openidconnect`.** It hard-depends on `rsa` 0.9 (RUSTSEC-2023-0071), and `deny.toml` never
ignores a vulnerability advisory. `oauth2` 5 has no `rsa` and provides everything the flows need;
the one thing it does not provide, ID-token validation, is not needed here.

**`reqwest` for the login HTTP client.** Its TLS features are shared with the telemetry exporter
and break it, exactly as ADR 0027 records. `oauth2`'s default features enable reqwest; turning them
off and supplying our own client removes the shared switch.

**`ClientConfig::builder()`.** With `ring` (sqlx) and aws-lc-rs both compiled in, it panics; the
provider is named explicitly.

**A static token in config.** Rejected in ADR 0012 and unchanged: a committed credential outlives
the commit that added it.

**One shared session for both shells.** Pocket ID rotates the refresh token on every use, so a
shared session would have the TUI and the desktop revoke each other.

**Retry-on-401 refresh.** A failed mutation retried after a 401 is a mutation sent twice; the
transport refreshes before expiry instead, so a request never races its own credential.

**`xdg-open` via `std::process`.** It would work on Linux, but `open` is the same one call and
stays correct on macOS, where the desktop shell also builds.
