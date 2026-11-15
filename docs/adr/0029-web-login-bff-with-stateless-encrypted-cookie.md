# 0029. Web login through the service (BFF) with stateless encrypted cookie + Web Locks

- **Status:** Accepted
- **Date:** 2026-11-13

## Context

ADR 0027 delivered the *verifying* half of authentication: a service with `add-auth-oidc` checks
OIDC access tokens. ADR 0028 delivered the *producing* half for native shells: the TUI and
desktop log in with oauth2 and store a refresh token in the OS keyring. The web shell had no
login at all — it could only present `COUNTER_API_TOKEN`.

A browser cannot hold a refresh token safely: JavaScript can read localStorage and
sessionStorage, and any dependency or XSS can exfiltrate one. The refresh token must be
inaccessible to the page. The page needs only a short-lived access token to call the API.

## Decision

**The service is the Backend For Frontend.** It handles the OAuth2 authorization-code flow with
PKCE, stores the refresh token in an encrypted HttpOnly cookie, and gives the SPA only a
short-lived access token. The SPA sends it as `Authorization: Bearer` (ADR 0012 unchanged).

### Routes (plain axum, at the root, before the SPA fallback)

- `GET /auth/login` — generates state and PKCE verifier, stores them in an encrypted
  `__Host-login-flow` cookie (SameSite=Lax, Max-Age=600), and redirects to the authorization
  endpoint with 302 Found.
- `GET /auth/callback` — checks state against the flow cookie, exchanges the authorization code
  with the PKCE verifier, stores the refresh token in an encrypted `__Host-session` cookie
  (SameSite=Strict), clears the flow cookie, and redirects to `/` with 302 Found.
- `POST /auth/session` — refreshes the session with the refresh token from the cookie, rotates
  (sets a new cookie), and returns `{access_token, expires_in, name}`. Expired or spent token →
  401 and clears the cookie.
- `POST /auth/logout` — clears the cookie, best-effort RFC 7009 revocation, returns the
  provider's `end_session_endpoint` URL if discovery has one.

### Cookies

- **Encryption:** AES-256-GCM with aws-lc-rs (already used by rustls), random 96-bit nonce
  per seal. `[auth] cookie_key` is a secret containing the standard, padded base64 encoding
  of exactly 32 bytes. Generate with `openssl rand -base64 32`, store the text in a secret
  file, and supply its path through the settings loader's existing `APP__AUTH__COOKIE_KEY_FILE`
  indirection. Missing, invalid base64 or wrong decoded length with `--web` enabled →
  `settings::Error::Invalid` naming `auth.cookie_key` at load. Debug/Display redact the key;
  never commit a deployment key. The BFF is never silently left unmounted.
- **Session cookie:** `__Host-session`; HttpOnly; Secure; SameSite=Strict; Path=/. No max-age,
  so it lasts until the browser closes or logout clears it.
- **Flow cookie:** `__Host-login-flow`; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=600.
  Must survive the IdP's top-level redirect.

### CSRF

POST endpoints require `Sec-Fetch-Site: same-origin`, or when absent an `Origin` equal to
the configured `[auth] public_url` origin (scheme, host and port), never the incoming
request URI or forwarded headers. Either check failing → 403. The refresh token never
appears in any response body or log; token response bodies are never logged.

### Public client + PKCE

No client secret. S256 PKCE uses a random verifier and SHA-256 challenge. Token exchanges
are direct `application/x-www-form-urlencoded` POSTs through the same HTTPS client
`crates/oidc` already owns (hyper-rustls + aws-lc-rs + webpki-roots). The authorization
request and code exchange both use `public_url + "/auth/callback"` as `redirect_uri`.

### The multi-tab problem

Pocket ID refresh tokens are single-use. Two tabs calling `/auth/session` at once present the
same token, and the second gets `invalid_grant`, which means signed out.

**Chosen: encrypted cookie plus a cross-tab lock in the SPA.** The SPA wraps `/auth/session`
in `navigator.locks.request("auth-session", …)` (the Web Locks API). The browser serialises
the tabs, and a waiting tab then sends the already-rotated cookie, because the cookie jar is
shared. The service stays stateless and works with no DB and with several replicas.

**Risk accepted:** a non-SPA caller can still race. This is documented but not fixed — a
service that needs to serve both native apps and browsers should use a server-side session
table or accept the occasional re-login.

### Where the code lives

`cargo xtask add-auth-oidc <svc> --web` enables the `bff` feature and wires the routes into
the service. Without `--web`, generated `services/` remains byte-identical to the
verifier-only generator. The embedded `crates/oidc` intentionally differs: it always carries
the feature-gated BFF sources and their manifest/test support, but production code does not
enable that feature unless requested. This crate-level difference is accepted; it keeps
one embedded crate rather than two implementations to maintain.

### Configuration

- `[auth] public_url`: required with `--web`, e.g. `https://pilot.noosphere.uk`. Absolute HTTPS
  origin only; HTTP is allowed only for `localhost` or `127.0.0.1`. No path beyond `/`,
  query, fragment or userinfo. Also supplies the exact CSRF fallback origin.
- `[auth] scopes`: default `["openid", "profile", "email"]`; nonempty, must contain `openid`,
  and every entry must be nonempty without whitespace.
- `[auth] login_audience`: optional authorization-endpoint audience, nonempty after trimming.
- `[auth] cookie_key`: required secret, standard padded base64 of exactly 32 random bytes.

These are validated before sockets or background work start. Supply public URL as
`APP__AUTH__PUBLIC_URL`; `APP__AUTH__COOKIE_KEY_FILE` names a file containing the base64
text, not a raw binary key and not a separate path-valued setting. No configuration key
uses the reserved `_file` suffix.

### Keycloak note

Keycloak needs an Audience mapper so that `aud` holds the client id. Without one, Keycloak
access tokens omit the `aud` claim and the verifier rejects them.

## Consequences

The web shell gets login. Cookie encryption reuses aws-lc-rs, already in the TLS dependency
tree, rather than adding the RustCrypto aes-gcm stack. SHA-256 and random PKCE generation
remain explicit dependencies.

The SPA holds the access token in memory only and sends it as Bearer. No refresh token ever
reaches the page. The service stays stateless: no DB, no server-side session table. A
deployment with several replicas works without shared state because the cookie key is the same
on every replica (a k8s Secret or clan var).

The `crates/oidc` crate grows a `bff` feature and module. Its own dev-dependency enables
`bff` so normal `cargo test -p oidc` and CI nextest execute the BFF tests. The generator's
parity test keeps embedded sources identical, and `just check-archetypes` builds and tests
both verifier-only and `--web` converted services.

Pipeline VM tests supply the required web settings only when the service's direct `oidc`
dependency enables `bff`: the public URL is the VM's loopback HTTP origin, and the cookie
key is a public, test-only constant, never a deployment secret. Both module nodes, the
image container and the hand-run migration receive the same environment. The eval-only
`auth-issuer-vm-environment` check covers forced web/verifier-only cases and real-service
wiring; verifier-only services retain their issuer-only environment.

The SPA keeps its access token only in memory. Only a session 401 immediately signs it out;
transient failures retry with bounded exponential backoff and retain the token only until
its expiry. Logout invalidates pending restores, clears the token after its POST, and follows
the optional end-session URL. Without an ID-token hint, provider-side logout remains
provider-dependent. A provider `error` callback may show the denied banner without a valid
flow cookie; it clears only the flow cookie and creates no session.

## What was rejected, and why

**A server-side session table.** It would require the service to have a database, add a table
and a migration to the generator, and leave rows to clean up. The cookie-only approach is
simpler and works for the first product (Pocket ID + single SPA). A service that needs to
serve both native and web clients can add a session table as a follow-up.

**Using the `oauth2` crate for the server flow.** The `AsyncHttpClient` trait requires a
lifetime-parameterised adapter that proved fragile. The OAuth2 protocol is straightforward
(three POST endpoints), and implementing it directly with SHA-256 and the existing HTTP client
is about 80 lines of clear code. It also removes the `oauth2` dependency from `crates/oidc`,
which previously only used it through the native login in `apps/counter/client`.

**Storing the refresh token in the SPA (localStorage/sessionStorage).** Any JavaScript on the
page can read it. The HttpOnly cookie is invisible to `document.cookie` and to any dependency
or injected script.

**A permanent session (long max-age).** A refresh token that outlives the browser session is
a credential the user did not explicitly store. The session cookie has no max-age, so it dies
with the browser, matching the user's expectation that closing the browser means signing out.