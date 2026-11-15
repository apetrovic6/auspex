# 0031. The product is its own token issuer: email/password and Sign in with Google

- **Status:** Accepted
- **Date:** 2026-11-14

## Context

ADRs 0027, 0028 and 0029 made authentication something a product adds with a generator, and
assumed an outside OpenID Connect provider. The product verifies that provider's access tokens
(0027). Native shells log in to it with `oauth2` (0028). The web shell logs in through the
service, which keeps the provider's refresh token in an encrypted `__Host-session` cookie (0029).
That stays the homelab and enterprise option: Pocket ID or Keycloak, run by whoever runs the
product.

The owner's direction (2026-11-14) adds a second option for consumer products. A consumer can't be
asked to run an identity provider, or to sign in through one they've never heard of. Consumer
products get **email/password**, ported from the auth in the owner's `fishing` project, and
**Sign in with Google**. This has to land before the roles add-on (RT-62), which needs the
product's own user accounts to hang roles on.

That makes the service the **issuer**: it stores credentials, mints the access tokens its handlers
accept, and keeps the refresh state. ADR 0012 refused all of that as a *default*: "password
storage and hashing, session persistence, refresh rotation" are product decisions. This record
keeps that ruling the way 0027 did. Nothing changes until a product runs a generator, the template
still ships `NoIdentities`, and `crates/identity` gains nothing.

### What fishing has, checked against its code

The owner described fishing's auth from memory. Checked against the module (`auth/`,
`refresh_tokens/`, the entities and `config`), the description is mostly right. Where it differs,
the code below is what the port starts from, not the description.

- **argon2id with dummy-hash timing.** The dummy-hash timing is real: `login` verifies against
  `password::dummy_hash()` when the email is unknown, and only then checks whether the user
  exists, so a miss costs the same as a wrong password. The hashing itself lives in fishing's
  `common::password`, which is not in the copy this record was written from, so **the argon2
  variant and its parameters are unverified**. The port checks them, and doesn't inherit them on
  trust.
- **HS256 15-minute JWTs.** Right. `Header::default()` and `Validation::new(HS256)` are used.
  Fifteen minutes is the shipped default (`access_token_ttl_seconds = 900`), and it's
  configurable. `iss` and `aud` are both the constant `fishing-api`, and `sub` is the user's
  `i32` id serialized as a string. `jti` is a UUID that nothing reads back.
- **Hashed, rotating refresh-token families with reuse detection and a grace window.** Right, and
  well commented. The token is 32 random bytes, base64url-encoded. The row stores its SHA-256.
  Rotation revokes the presented row and mints a successor in the same `family_id`. If a revoked
  token comes back, the whole family is revoked, unless the token was revoked within
  `refresh_grace_seconds` (default 10) **and** the family still has a live member. Three things
  the description leaves out, which the port fixes:
  1. **Rotation is a read followed by an unconditional update**, so it is not atomic. Two
     refreshes that both read the row before either one revokes it both pass the "revoked?"
     check, and both mint a live successor. Neither sees reuse. The grace window was built for
     concurrent refreshes, but this race skips it entirely.
  2. **`refresh` never checks the user.** `login` filters on `.active()` (`deactivated_at_utc`),
     but `refresh` doesn't look at the user row. A deactivated account keeps refreshing until
     its refresh token expires (30 days by default).
  3. **The refresh token is in the response body too.** `AuthResponse` carries `refresh_token`
     next to `access_token`. So on the web it isn't only in the HttpOnly cookie: page script can
     read it from the body. That serves native clients, which have no cookie jar, but it undoes
     the cookie's main point in a browser.
- **An HttpOnly SameSite=Lax cookie scoped to the AuthService path.** Right: the cookie is
  `refresh_token`, `Path={API_PREFIX}/auth.v1.AuthService`. Two details matter for the port.
  `Secure` is set only when `base_url` is https. And the cookie is persistent:
  `Max-Age` equals the refresh TTL (30 days). ADR 0029's cookie is the reverse: `__Host-`,
  always `Secure`, `SameSite=Strict`, `Path=/`, and no `Max-Age`.
- **What it lacks.** The four gaps the owner named are confirmed as far as the copy shows:
  - **No email verification.** `register_user` says so, and deliberately leaks "already
    registered" until verification exists.
  - **No password reset.** `AuthService` has only `Register`, `Login`, `Logout` and `Refresh`.
  - **No rate limiting** in any file in the copy. The middleware layer wasn't in the copy, so
    this is unverified rather than disproved.
  - **No access-token revocation.** `logout` says so: the JWT stays valid until `exp`.

  Logout also revokes only the presented token, not its family. A comment in `logout` says "a
  password change" ends a session, but no password-change code is in the copy.
- **Schema facts that don't carry over.** `users.id` is a serial `i32`, so `sub` reveals how
  many accounts exist and can be counted. `users.email` is a plain unique column. Login trims
  the address, but nothing in the copy lowercases it, so whether `A@x` and `a@x` are one account
  is unverified. fishing is on sea-orm 2.0 (`#[sea_orm::model]`, `BelongsTo`, builders), and the
  template is on 1.1. The port is a re-expression of the logic, not a file copy.

The fishing findings above (the rotation race, `refresh` ignoring deactivation, the refresh token
in the body) are defects in fishing as well. Fixing fishing is not part of this record.

### Google, as its documentation states it (fetched 2026-11-14)

- **The ID token.** Verify it against the JWKS at `https://www.googleapis.com/oauth2/v3/certs`,
  cached as `Cache-Control` says. Check that `aud` is one of our client IDs, that `iss` is
  `accounts.google.com` or `https://accounts.google.com`, and `exp`. Key users by `sub`, never
  by email.
- **Email authority.** If `email_verified` is true and the token has an `hd` (a Workspace
  domain), or the address ends in `@gmail.com`, Google is authoritative for that address.
  Otherwise, linking to an existing account needs a separate challenge.
- **Web.** Google Identity Services (GIS) returns an ID token. Its authenticity is checked with
  a `g_csrf_token` cookie that must equal the body field, or with a nonce. No client secret is
  needed.
- **Android.** Credential Manager returns an ID token.
- **Desktop.** Uses the Desktop client type, PKCE and a loopback redirect
  `http://127.0.0.1:<port>`. The client secret is optional. Custom URI schemes and OOB are not
  supported.
- **TUI.** Whether Google's device flow allows the `openid email profile` scopes is
  **unverified**. No part of this record depends on the answer.
- **Access tokens.** Google's access tokens are not JWTs meant for our API, and we never verify
  one.
- **iOS.** The App Store requires Sign in with Apple in any app that offers Sign in with Google.

## Decision

**`cargo xtask add-auth-accounts <service> [--web] [--google]` makes one service the product's
token issuer.** That service owns user accounts and password and Google identities, signs its own
ES256 access JWTs, and keeps refresh families in its database. Handlers stay unchanged: they call
`ctx.principal()?` and get the user's id as the subject, whichever login produced the token. Like
`add-auth-oidc`, nothing exists in a product before the generator runs.

### The account model

One user, many ways in. A login method is an **identity**, keyed by `(provider, subject)`, and a
password is just one provider.

```sql
CREATE TABLE users (
    id                 uuid PRIMARY KEY,           -- UUIDv7, minted by the service; the JWT `sub`
    email              text NOT NULL,              -- as entered, for display and mail
    email_verified_at  timestamptz,                -- NULL until proven (mail link, or Google authority)
    display_name       text NOT NULL DEFAULT '',   -- `name` in /auth/session
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz,
    deactivated_at     timestamptz                 -- checked at login AND at refresh
);
CREATE UNIQUE INDEX users_email_key ON users (lower(email));

CREATE TABLE identities (
    id             uuid PRIMARY KEY,
    user_id        uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    provider       text NOT NULL,                  -- 'password' | 'google'
    subject        text NOT NULL,                  -- Google `sub`; for 'password', the user id
    credential     text,                           -- argon2id PHC string, password only
    email_at_link  text,                           -- what the provider said when linked; never a key
    created_at     timestamptz NOT NULL DEFAULT now(),
    last_used_at   timestamptz,
    UNIQUE (provider, subject),
    CHECK ((provider = 'password') = (credential IS NOT NULL))
);
CREATE UNIQUE INDEX identities_one_per_provider ON identities (user_id, provider);

CREATE TABLE refresh_tokens (
    id              uuid PRIMARY KEY,
    family_id       uuid NOT NULL,
    user_id         uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    token_hash      bytea NOT NULL UNIQUE,         -- SHA-256 of the opaque token
    created_at      timestamptz NOT NULL DEFAULT now(),
    expires_at      timestamptz NOT NULL,
    revoked_at      timestamptz,
    revoked_reason  text                           -- rotated | logout | reuse | reset | signout_all | deactivated
);
CREATE INDEX refresh_tokens_family ON refresh_tokens (family_id) WHERE revoked_at IS NULL;
CREATE INDEX refresh_tokens_user   ON refresh_tokens (user_id)   WHERE revoked_at IS NULL;

CREATE TABLE one_time_tokens (                     -- email verification and password reset
    id          uuid PRIMARY KEY,
    user_id     uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    purpose     text NOT NULL,                     -- 'verify_email' | 'reset_password'
    token_hash  bytea NOT NULL UNIQUE,
    email       text NOT NULL,                     -- the address it was sent to
    expires_at  timestamptz NOT NULL,
    used_at     timestamptz
);

CREATE TABLE outbound_email (                      -- the outbox; see "Outbound email"
    id          uuid PRIMARY KEY,
    recipient   text NOT NULL,
    template    text NOT NULL,
    payload     jsonb NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    sent_at     timestamptz,
    attempts    integer NOT NULL DEFAULT 0,
    last_error  text
);
```

These choices differ from fishing:

- **`users.id` is a UUID, not a serial.** `sub` then reveals nothing about how many accounts
  exist. It's also already a string, which removes fishing's whole
  `serialize_i32_as_string` workaround.
- **Email uniqueness is case-insensitive** (`lower(email)`), and the email is not the user's
  key. Google identities are found by `sub` alone. `email_at_link` is kept for display and
  support. A product that really wants several accounts per address can drop the index. That's
  its decision, and this record notes it.
- **At most one identity per provider per user.** Linking a second Google account to the same
  user is rejected in v1, and a later record can lift it.
- **A user always keeps at least one identity.** Unlinking the last one is refused in code, not
  in the schema.

**Where it lives.** ADR 0003: entities and migrations live in the owning service. The generator
writes the migrations into the service's migrations module, which is applied at boot under the
advisory lock. It also writes the sea-orm 1.1 entities and the store implementation. The
generator refuses a service without a database: run `add-database` first, as `new-migration`
already requires. Everything with no entity in it goes in a new crate, **`crates/accounts`**,
embedded in `tools/xtask` and checked for parity the way `crates/oidc` is (ADR 0027):

- the token signer;
- the refresh-family state machine, written against a store trait the service implements;
- password hashing;
- Google ID-token verification;
- the rate limiter;
- the mailer trait.

Expired and long-revoked rows (refresh tokens, one-time tokens, sent mail) are swept by a periodic
in-process task. fishing never deletes them.

### The token issuer

**The service signs ES256 access JWTs.** HS256, as in fishing, was rejected for three reasons:

- **The existing verifier refuses HMAC on purpose.** `crates/oidc` refuses HS\* outright,
  because with a public JWK as the "secret" HMAC is the classic key-confusion attack (ADR 0027).
  Accepting HS256 would mean a second verification path, with a shared secret, in the one crate
  whose job is to keep that out.
- **Every other service would hold the signing key.** With a symmetric key, any service in the
  product that verifies a token can also mint one. With a key pair, other services and workers
  verify from the published JWKS and can't sign.
- **Rotation is already built for key pairs.** The JWKS cache with refetch on an unknown `kid`
  is what `crates/oidc` already does for Pocket ID, and it is the rotation story for free.

ES256 rather than EdDSA or RS256. All three are in `ALLOWED_ALGORITHMS`. RSA keys and signatures
are large and add nothing here. ES256 is the most widely supported JOSE public-key algorithm if
anything outside Rust ever has to verify one. jsonwebtoken 11 on `aws_lc_rs`, already in the
workspace, signs it with no new crypto dependency.

**Claims:**

- `iss` = `[auth] public_url`.
- `aud` = `[auth] audience`, which defaults to the public URL.
- `sub` = `users.id`.
- `iat`, and `exp` = `iat` + `access_token_ttl_seconds` (default 900).
- `jti`.
- The `kid` header is the key's RFC 7638 JWK thumbprint.

There is no email, name or role claim. Roles arrive with RT-62, and that record decides whether
they ride in the token or are looked up per request.

**Key management.**

- `[auth.issuer] signing_key` is a secret: a PKCS#8 PEM P-256 private key, supplied through the
  settings loader's existing `APP__AUTH__ISSUER__SIGNING_KEY_FILE` indirection, the same as
  0029's `cookie_key`. It's generated with
  `openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256`.
- `[auth.issuer] published_keys` is a list of public PEMs that are published and accepted but
  never used to sign.
- `kid` is derived from the key, never configured, so a `kid` can't name the wrong key.
- A missing, unparsable or non-P-256 key → `settings::Error::Invalid` naming the field, at load.
  The generator never generates a key: a key is a deployment secret, and a generator that wrote
  one would break [ADR 0030](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0030-products-update-by-regenerating-and-merging.md)'s deterministic replay.

**Rotation is three deploys:**

1. Add the new public key to `published_keys`, and wait at least one verifier refresh interval
   (15 minutes, `Timing::refresh_interval`).
2. Make the new key the `signing_key`, and move the old public key into `published_keys`.
3. After `access_token_ttl` plus the verifier's 30-second leeway, remove the old key.

Step 1 isn't strictly needed: verifiers refetch on an unknown `kid`. But that refetch is
rate-limited to one per 30 seconds, and the rotation shouldn't depend on winning that window.
Replicas mid-rollout sign with different keys, and both are published, so a rolling deploy is
safe at every step. Automatic rotation and keys in the database are not in v1.

**How the existing verifier accepts the product's own tokens.** There is still one
`VerifyCredential`, and handlers don't see a difference. `crates/oidc` gains two small things:

- **`Verifier::local(config, keys)`**, which gets its keys from an in-process key set instead of
  fetching them. The issuing service uses it with its own public keys. It does no network I/O,
  and can never answer `Unavailable`. Every other check is the existing code, unchanged: the
  algorithm allow-list, `crit`, `kid`, `iss`, `aud`, `exp`/`nbf` with 30 s leeway, and a non-empty
  `sub`.
- **An optional `jwks_uri` on `oidc::Config`**, which skips discovery. Other services in the
  product verify the issuer's tokens with the ordinary fetching verifier, pointed at the
  issuer's **`GET /.well-known/jwks.json`**. The issuer does not publish an
  `openid-configuration`. That document declares an OpenID provider (authorization endpoint,
  response types, ID tokens), and claiming to be one we aren't is a lie that third-party tooling
  would act on.

### Refresh families, and the 0029 contract kept

The family logic is fishing's, moved into `crates/accounts` against a store trait, with the three
fixes from the context section:

- **Rotation is a conditional update.** The presented token is spent with
  `UPDATE refresh_tokens SET revoked_at = now(), revoked_reason = 'rotated' WHERE id = $1 AND
  revoked_at IS NULL`. If that touches zero rows, the request lost a race, and it goes through
  the grace-window check exactly as a token found already revoked would. Two concurrent
  refreshes can no longer both rotate silently.
- **Reuse detection and the grace window are fishing's rule.** A revoked token, presented
  again, is accepted only if it was revoked within `refresh_grace_seconds` **and** its family
  still has a live member. Then it gets another successor in the same family. Otherwise the
  whole family is revoked (`reuse`), and the caller gets the same 401 as for an unknown token.
- **Refresh checks the user.** A deactivated user is refused, and their live families are
  revoked (`deactivated`).
- **fishing's settings validation is ported with its rules:**
  - `access_token_ttl_seconds` must be greater than the verifier's leeway, which is 30 s here,
    not jsonwebtoken's 60;
  - `refresh_token_ttl_seconds` must be greater than the access TTL;
  - `0 ≤ refresh_grace_seconds < access_token_ttl_seconds`.

**The web contract stays 0029's, byte for byte where the SPA can see it.** `__Host-session` is
still `HttpOnly; Secure; SameSite=Strict; Path=/`, with no `Max-Age`, sealed with AES-256-GCM
under `[auth] cookie_key`. What it seals changes: it was the IdP's refresh token, and now it is
our own opaque refresh token. The four routes behave as follows:

| Route | 0029 (OIDC) | 0031 (own issuer) |
|---|---|---|
| `GET /auth/login` | 302 to the IdP | Serves the sign-in page (below) |
| `POST /auth/session` | Refresh grant at the IdP; `name` from userinfo | Rotate the family in the DB; mint our JWT; `name` from `users.display_name` |
| `POST /auth/logout` | Clear cookie; best-effort RFC 7009; `end_session_endpoint` | Clear cookie; revoke the **family**; `end_session_endpoint: null` |
| CSRF | `Sec-Fetch-Site`/`Origin` check | Same middleware, on every state-changing POST |

**The cookie stays 0029's browser-session cookie in v1** (decided 2026-11-14): no `Max-Age`, so
closing the browser signs the user out, even though the refresh row would live for
`refresh_token_ttl_seconds`. Consumer users expect to stay signed in, and fishing's cookie lasted
30 days. **"Remember me" is an opt-in follow-up:** a checkbox on the sign-in page that sets
`Max-Age` to the refresh TTL for that session only. Changing the default is left to that
follow-up and the product, not this record.

The response bodies are unchanged: `{access_token, expires_in, name}` and
`{end_session_endpoint}`. So `apps/*/shells/web/src/auth.rs` is unchanged, including the Web
Locks serialisation. Two things change in how 0029 was argued:

- **0029's accepted risk is closed.** A non-SPA caller that races the lock now lands in the
  grace window, instead of being signed out.
- **0029's rejected option is taken.** It rejected a server-side session table because it would
  have needed a database. This mode needs one anyway, for the accounts.

0029's code is reused. The cookie codec (`oidc::bff::cookies`) and the CSRF middleware
(`oidc::bff::csrf`) become public API of `crates/oidc`'s `bff` feature, and `crates/accounts`
depends on them. Nothing is copied.

**The sign-in page.** `GET /auth/login` serves one small server-rendered page from the service:
a password form, a registration form and, with `--google`, the Google button. That's how the SPA
stays unchanged: it already links to `/auth/login` and calls `/auth/session` on load. A product
restyles the page by editing the generated template. **The service draws the sign-in UI, not
the SPA** (decided 2026-11-14). GIS then runs on a page the service controls, and a product that
later wants the forms inside its SPA can move them without changing a route.

Each form posts same-origin, so it passes the 0029 CSRF check:

- `POST /auth/password/login`
- `POST /auth/password/register`
- `POST /auth/password/forgot`
- `POST /auth/password/reset`
- `POST /auth/google`

On success the response sets `__Host-session` and sends a 303 to `/`.

**Native shells** have no cookie jar, so they use a Connect service, `auth.v1.AuthService`, with
`Register`, `Login`, `SignInWithGoogle`, `Refresh`, `Logout`, `RequestPasswordReset`,
`ResetPassword` and `VerifyEmail`. These return the refresh token **in the body** and never set a
cookie. The `/auth/*` routes do the reverse: cookie only, never the body. Each path has exactly
one way to carry the credential, so fishing's body-and-cookie overlap can't recur. The service's
public-method test (`services/<svc>/tests/api/auth.rs`, `PUBLIC`) gains every `AuthService`
method except `SignOutEverywhere` and `LinkGoogle`, which require a principal. `Logout` is public
because the refresh token it revokes is the proof, and it has to work after the access token has
expired.

### Sign in with Google

Every platform ends the same way: the client gets a **Google ID token** and hands it to the
service once (`POST /auth/google` on the web, `SignInWithGoogle` from a native shell). The service
verifies it, resolves the user, and starts one of *our* refresh families. From then on the user
holds our tokens, and the Google token is never presented to our API again. Google access tokens
are never requested for our API and never verified.

**Verification** is in `crates/accounts`, and reuses `crates/oidc`'s key cache through the new
`jwks_uri` option, pointed at `https://www.googleapis.com/oauth2/v3/certs`. It checks:

- `alg` RS256;
- a known `kid`;
- `aud` ∈ `[auth.google] client_ids`;
- `iss` ∈ {`accounts.google.com`, `https://accounts.google.com`};
- `exp`;
- a non-empty `sub`;
- on the web, the nonce (below).

The existing `Config` holds one issuer string and one audience, and Google needs two of one and
several of the other. So the Google verifier is a thin wrapper around the shared key cache, not a
second `Verifier` configuration. **The key cache learns `Cache-Control: max-age`.** When the
response has it, the cache uses it as the refresh interval, clamped to
`[min_refetch_interval, 24 h]`. When it doesn't, the cache keeps today's fixed 15 minutes. That
is what Google's documentation asks for, and it is harmless for Pocket ID.

**Per platform:**

- **Web (v1).** GIS in its JavaScript-callback mode, on the sign-in page.
  - `GET /auth/login` mints a nonce, seals it into the existing `__Host-login-flow` cookie
    (`SameSite=Lax`, `Max-Age=600`, as in 0029), and renders it into the GIS initialisation.
  - The page posts the returned credential same-origin to `POST /auth/google`.
  - The service checks the token's `nonce` against the flow cookie, and the 0029 CSRF check
    applies as usual.

  This picks the nonce over GIS's redirect mode with `g_csrf_token`. In redirect mode, Google's
  page POSTs cross-site to our `login_uri`, which fails the `Sec-Fetch-Site` rule by design. That
  route would need its own exemption and its own double-submit check. The callback mode keeps a
  single CSRF rule for every POST. Needs only the **Web client ID**, and no secret.
- **Android (follow-up phase).** Credential Manager returns an ID token, which goes to
  `SignInWithGoogle`. Google's Android documentation has the app pass the *Web* client ID as the
  server client ID, so the token's `aud` is the Web client ID. An **Android client ID**, bound to
  the package name and signing certificate, must also be registered. **This is from Google's
  Android guide as remembered, not from the 2026-11-14 fetch. Verify it before the Android phase
  is built.** If `aud` turns out to be the Android ID, it goes in `client_ids` too.
- **Desktop (follow-up phase).** The 0028 `oauth2` machinery, pointed at Google with a
  **Desktop client ID**: PKCE, and a loopback redirect to `http://127.0.0.1:<ephemeral port>`.
  Custom schemes and OOB are not supported by Google and not attempted. The desktop exchanges the
  code at Google's token endpoint itself, takes the `id_token` from the response, and sends it to
  `SignInWithGoogle`. Google calls the Desktop client secret optional. If a product sets one, it's
  shipped in the binary and treated as public. The service never sees it. The Desktop client ID
  goes in `client_ids`.
- **TUI: password only** (decided 2026-11-14). Whether Google's device flow allows
  `openid email profile` is unverified. Nobody builds TUI Google until that is checked, and the
  TUI never falls back to a loopback browser flow, which a headless terminal can't complete.
- **iOS: password only** (decided 2026-11-14). The App Store requires Sign in with Apple in any
  app that offers Google. **Sign in with Apple gets its own record** when a product needs Google
  on iOS, because it brings its own constraints: private-relay emails, the name sent only on the
  first sign-in, and a client secret that is a JWT expiring every six months.

**Linking rules**, in order:

1. **`(google, sub)` exists** → that user. `email_at_link` is refreshed. The user's email is never
   changed by Google.
2. **No identity, and Google is authoritative for the email** (`email_verified` and either an `hd`
   or an `@gmail.com` address):
   - no user with that email → create the user with the email verified, and the Google identity;
   - a user with that email whose email is **verified** → link Google to that user;
   - a user with that email whose email is **unverified** → link, mark the email verified,
     **delete that user's password identity, and revoke all its refresh families**
     (decided 2026-11-14). All of this happens in one transaction with the link.

     This last rule defeats the pre-account-takeover attack. An attacker registers
     `victim@gmail.com` with a password they know and never verifies it. When the real owner
     arrives through Google, linking would otherwise hand the attacker a working password into
     the victim's account, and any session the attacker already holds. The cost is that a
     legitimate user who registered and never verified loses that password, and can set a new
     one through password reset. The rule applies whatever `require_email_verification` says.
3. **No identity, and Google is not authoritative:**
   - no user with that email → create the user, with the email **unverified**;
   - a user with that email → **refuse to link here**. The response says to sign in with the
     existing method and link Google from the signed-in account, which is the "separate
     challenge" Google's guidance asks for. `POST /auth/google/link` (and a `LinkGoogle` RPC)
     takes an ID token and requires a principal.
4. With `[auth.google] hosted_domains` set, only tokens whose `hd` is in that list are accepted at
   all.

**What's configurable** (`[auth.google]`, present only with `--google`):

- `client_ids`, required and non-empty: every `aud` the service accepts.
- `web_client_id`: the one the sign-in page renders, which must be in `client_ids`. It's
  required when `--web` is also given.
- `hosted_domains`: default empty, which means any.
- `allow_signup`: default `true`. `false` means Google can only sign in to or link existing
  accounts.

**Client IDs a deployment needs:**

| | Web client | Desktop client | Android client |
|---|---|---|---|
| v1 (web) | ✓ | — | — |
| Desktop phase | ✓ | ✓, in the desktop shell's config | — |
| Android phase | ✓ | — | ✓, registered; probably not in `aud` (verify) |

Each one is a separate OAuth client in the product's own Google Cloud project, created by the
product owner. The template ships none.

### The gaps fishing has: decided

- **Email verification: in v1.** Registration writes the user with `email_verified_at = NULL`
  and queues a `verify_email` one-time token. One-time tokens are 32 random bytes, stored as
  their SHA-256, single use, and valid for 24 h. Following the link sets `email_verified_at`.
  Once verification exists, **registration answers the same way** whether or not the address is
  taken: "check your mailbox". An already-registered address gets a "someone tried to register
  with your address" mail instead. That closes the enumeration fishing left open on purpose
  "until email verification exists".
- **No sign-in before verification, by default** (decided 2026-11-14). The setting is
  `[auth] require_email_verification`, a boolean, **default `true`**.
  - **`true`:**
    - A password account with `email_verified_at = NULL` can't sign in, and gets no session from
      `Register`, `Login` or `/auth/password/login`. The answer is the same as for wrong
      credentials, plus a "check your mailbox" hint, so it doesn't reveal that the password was
      right.
    - Following the verification link starts the first session.
    - Accounts that Google is authoritative for are verified when they're created, so this never
      blocks them.
  - **`false`:**
    - An unverified password account can sign in, and registering a new address returns a
      session straight away, as fishing does.
    - The verification mail is still sent, and `email_verified_at` is still recorded, so linking
      rules and a product's own checks can still tell verified accounts from unverified ones.
    - The cost, stated so nobody switches it off without knowing: registration can no longer
      answer the same way for new and taken addresses (a new one gets a session, a taken one
      can't), so **address enumeration through registration comes back**. A pre-hijack account is
      also usable by its creator until the real owner arrives. The Google rule above still wipes
      it then, but nothing stops it before that.
  - The value is validated at load like every other `[auth]` key. The generator writes it
    explicitly into the service's config, so the choice is visible in the file.
- **Password reset: in v1.** `forgot` always answers the same way and queues a
  `reset_password` token (30 minutes, single use) only if the account exists. The response time
  doesn't depend on that, because the mail is queued, not sent inline. A successful reset does
  three things:
  - replaces the credential;
  - marks the email verified, since the user proved they control the mailbox;
  - **revokes every refresh family** of the user (`reset`).
- **Outbound email: an outbox and a trait, in v1.** Handlers never talk to a mail server. They
  insert an `outbound_email` row **in the same transaction** as the token it carries. An
  in-process sender task drains the outbox with retries and backoff, through a `Mailer` trait in
  `crates/accounts`. This gives three properties:
  - a mail outage never turns into a slow or failed sign-in;
  - "forgot password" timing can't reveal whether an account exists;
  - a crash between commit and send loses nothing.

  Two implementations ship: **`LogMailer`**, which writes the message (link included) to the
  log, for development and the VM tests, and is **refused at load unless the environment is
  development or test**; and **SMTP through `lettre`** (decided 2026-11-14). SMTP was chosen over
  any one provider's HTTP API because every provider speaks it, so the product chooses its
  provider in configuration (`[auth.email]`: host, port, credentials, from-address), not in code.
  The `Mailer` trait leaves room for an HTTP transport later. `lettre` is taken on rustls with
  aws-lc-rs. Its TLS features must not unify into `reqwest`'s, which is ADR 0028's trap, and the
  `check-template` probes compile the telemetry exporter alongside it. A separate worker process
  is not needed in v1. The outbox table makes it a later move that needs no schema change.
- **Rate limiting: in v1, in-process.**
  - **Token buckets** (`governor`) per client IP and per normalized email, on login,
    registration, forgot and reset, and per IP on `/auth/google` and `Refresh`.
  - **The client IP** is the socket peer unless `[auth.rate_limit] trusted_proxy_hops` (default
    0) says how many `X-Forwarded-For` hops to trust. Trusting the header by default would let
    any caller choose its own bucket.
  - **A concurrency bound on argon2.** A semaphore sized to the CPU count, because an
    unauthenticated endpoint that costs ~50 ms of CPU per request is a denial-of-service target
    before it's a guessing target.
  - **Limited requests** get `resource_exhausted` (429), which the client mapping already
    distinguishes.
  - **No account lockout.** It turns guessing into a denial of service against the victim.
  - **The limits are per replica**, so N replicas allow N times the budget. That's accepted for
    v1. A shared (Postgres or Redis) limiter is deferred.
- **Logout and revocation: logout and "sign out everywhere" in v1. Access-token revocation
  deferred.**
  - **Logout** revokes the presented token's **whole family**, not one row as fishing does: that
    family *is* this device's session.
  - **"Sign out everywhere"** (`SignOutEverywhere`, principal required) revokes every family of
    the user.
  - **Deactivation, password reset and password change** revoke every family.
  - **Access tokens** stay valid until `exp` (15 minutes by default). Revoking them would need a
    per-request check against shared state (a `sessions_valid_after` on the user row, or a
    denylist) in *every* verifying service. That is deferred, explicitly. A product that can't
    live with 15 minutes shortens `access_token_ttl_seconds`.
- **Password policy.** At least 8 characters and at most 128, with no composition rules (NIST SP
  800-63B's shape). Checking against breached-password lists is deferred. argon2id parameters at
  or above OWASP's minimum: m = 19 MiB, t = 2, p = 1.

### Coexistence with OIDC

**v1: one service runs one mode.** `add-auth-accounts` refuses a service that has run
`add-auth-oidc`, and the reverse, and each names the other in the message. A product can still
have both kinds of service. But no single service accepts both its own tokens and an outside
IdP's, because `Principal.subject` would then mean two different things, in two namespaces, to the
same handler. RT-62's roles need one meaning.

**When the two meet, enterprise SSO joins our accounts as an upstream identity provider, like
Google, not as a second issuer** (decided 2026-11-14). A user signs in through Pocket ID or
Keycloak via the 0029 authorization-code machinery. The callback verifies the IdP's ID token,
resolves an `identities(provider = 'oidc:<issuer>', subject = sub)` row, and starts *our* session.
Handlers keep one subject namespace, RT-62's roles attach to our users, and 0029's callback code
is reused. Building it is a follow-up with its own record. v1 builds only the room for it: the
`provider` column is free text.

### How it ships

- **The generator: `cargo xtask add-auth-accounts <service> [--web] [--google]`.**
  - **It requires** `add-database` on the service, and refuses `add-auth-oidc`.
  - **It writes:**
    - `crates/accounts`, once, from embedded sources, with a parity test, as for `crates/oidc`;
    - `crates/oidc`, if absent, because the verifier and the key cache are its own;
    - the migrations, entities and store;
    - the `[auth]` settings and their `validate()`;
    - the verifier wiring in place of `NoIdentities`;
    - the `AuthService` handlers, and the `PUBLIC` list entries;
    - with `--web`, the BFF routes and the sign-in page;
    - with `--google`, `[auth.google]` and its routes and RPC.
  - **It appends its recipe entry** ([ADR 0030](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0030-products-update-by-regenerating-and-merging.md)) after a successful run, and generates no secrets,
    so replay is deterministic.
  - **It emits an upgrade note** naming the settings a deployment must set before the first
    deploy (`docs/upgrading.md`, [ADR 0030](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0030-products-update-by-regenerating-and-merging.md)): `public_url`, `cookie_key` and `signing_key`, plus
    the Google client IDs with `--google`.
- **The proto.** `auth.v1.AuthService` is written into the service's proto by the generator
  through the existing `new-rpc` machinery, and generated code is committed (ADR 0004).
- **`cli new`.** No new flag in v1. The generator is the opt-in, as for OIDC, and the recipe
  records it. `cli new --auth accounts` would only be a recipe step run early, and can come
  later.
- **The example app.** The counter's web shell works unchanged against a converted `services/api`.
  **Native password login, and Google on desktop and Android, are a follow-up phase**
  (decided 2026-11-14). v1 is the service, the web, password, Google on the web, and the gaps
  above.
  `AuthService` ships in v1, so the follow-up is client work only. It changes
  `apps/counter/client`'s `login` feature, which today speaks only OIDC (0028), and it starts by
  verifying the Android `aud` fact noted above.
- **Configuration**, all validated at load, before sockets open:
  - `[auth] public_url`, `cookie_key` as in 0029;
  - `audience`;
  - `[auth.issuer] signing_key` and `published_keys`;
  - the three lifetimes;
  - `require_email_verification`;
  - `[auth.email]` (transport and from-address);
  - `[auth.rate_limit]`;
  - `[auth.google]`.
- **Tests and gates.**
  - **Unit, in `crates/accounts`:**
    - the family state machine: rotation, the lost-race path, the grace window, a reuse sweep,
      and a swept family never revived;
    - a sign-then-verify round trip through `oidc::Verifier::local`;
    - Google token checks against an in-test JWKS built from `oidc::testing`'s test-only keys:
      each `aud`, both `iss` spellings, expiry, nonce, the `hd`/gmail authority rule, and every
      linking branch;
    - `Cache-Control` handling;
    - settings validation.
  - **Integration, in the service, against Postgres** (the `add-database` harness): register,
    verify, login, refresh, reuse, logout, sign out everywhere, forgot and reset, rate limiting,
    and deactivation at refresh, all through the real router and the real `LogMailer` outbox.
  - **`just check-template`** gains probes for `add-database` + `add-auth-accounts` and
    `--web --google`, compiled with clippy `-D warnings`.
  - **`cargo deny`** covers the new dependencies (`argon2`, `governor`, and the mail transport).
  - **The pipeline VM tests** supply a public test-only signing key and cookie key, as 0029 did.
- **Keycloak in CI (RT-62) is not needed by this record.** The issuer is tested in-process, and
  Google is replaced by a local JWKS, because it can't run in CI. It becomes relevant to the
  upstream-OIDC follow-up, which would need a real IdP to sign in through. And
  RT-62 depends on this record, not the other way round: roles need the `users` table this
  record creates.

## Consequences

- **A product can have accounts without running an IdP.** The template still ships none: the
  seam (ADR 0012) and `NoIdentities` are unchanged, and the generator is the opt-in, as in 0027.
- **`crates/oidc` grows three small pieces:**
  - a local key source;
  - an optional `jwks_uri`;
  - `Cache-Control` handling.

  Its `bff` cookie and CSRF helpers also become public. It still verifies, and never signs.
  Signing lives in `crates/accounts`, so the "this crate never mints a token" property moves
  with the code rather than disappearing.
- **The issuing service becomes stateful in a way 0029's BFF was not.** Every `/auth/session`
  is a database write, so a Postgres outage signs no one in and refreshes no one. Requests with
  an unexpired access token keep working, since verification is local.
- **0029's multi-tab risk is closed** for this mode, by the grace window, and the SPA keeps its
  Web Locks anyway.
- **The fishing port is a rewrite against sea-orm 1.1 and a store trait**, carrying three fixes:
  - atomic rotation;
  - a deactivation check at refresh;
  - no refresh token in a web response body.

  fishing keeps its defects until someone ports the fixes back. That's not tracked here.
- **This record is inherited by products** (`INHERITED_ADRS`), like 0027–0029. The decision lives
  in the product's `services/` and `crates/`.

## What was rejected, and why

- **HS256, as fishing does it.** The existing verifier refuses HMAC by design. A shared secret
  would give every verifying service the power to mint. And the JWKS rotation path already
  exists for key pairs.
- **Issuing tokens through an embedded or bundled IdP** (Keycloak or Pocket ID in the product's
  deployment). That's the 0027–0029 option. For a consumer product it moves the user database
  into software the product doesn't own, and it still needs custom work for Google linking
  rules and mail.
- **A multi-issuer verifier for enterprise SSO beside local accounts.** It would dispatch on
  `iss` and accept both the IdP's tokens and ours. Handlers would then get subjects from two
  namespaces, and RT-62's roles would need a user record that one of them doesn't have. The
  upstream identity keeps one namespace.
- **Users keyed by email.** Google says never to. Emails change, are recycled, and are only
  sometimes Google's to vouch for. `sub` is the key, and email is an attribute.
- **Verifying Google tokens on every API request.** It would make Google's `sub` our principal,
  tie every request to Google's key availability, and give password users and Google users two
  kinds of subject. Google's ID token is exchanged once, for ours.
- **GIS redirect mode with `g_csrf_token`.** It works, but it needs a cross-site POST that the
  0029 CSRF rule rightly refuses, and an exemption with its own double-submit check. The nonce in
  the existing flow cookie keeps one rule for every POST.
- **A minimal `/.well-known/openid-configuration`, so other services need no verifier change.**
  It would advertise an OpenID provider we are not. An optional `jwks_uri` is one field.
- **Sending mail inline from handlers.** Mail latency and outages would become sign-in latency
  and outages, and the timing of "forgot password" would reveal whether an account exists. The
  outbox costs one table.
- **Account lockout after N failures.** It hands an attacker a way to lock any user out. Rate
  limiting by IP and by email, plus the argon2 concurrency bound, slows guessing without that.
- **Leaving fishing's body-and-cookie refresh token as it is.** On the web, that puts the
  long-lived credential where page script can read it, which is what 0029's whole design exists
  to prevent.
