# 0012. The template ships an identity seam, not an authentication system

- **Status:** Accepted
- **Date:** 2026-09-26
- **Amended:** 2026-10-16 — `VerifyCredential` answers in two arms, and the arm that is not a
  statement about the caller gets its own Connect code. Amended rather than superseded: the
  three-cause rule below stands unchanged, and so do the tests that assert it. What changed is that
  a fourth way a verifier fails was never inside that rule's scope, and the template shipped the
  one verifier that could only ever demonstrate the three that were.
- **Amended:** 2026-11-10 — [0027](0027-oidc-verification-is-a-generator-not-a-default.md). The
  `[auth]` config section this record says does not exist now exists, but only in a service that
  has run `cargo xtask add-auth-oidc`, because that is the moment there is code that reads it. The
  template still ships none, and `NoIdentities` is still the default. The reason it gave — a
  section no code reads is furniture — is the reason the section waits for the command.

## Context

Every service in this repository answers requests from anonymous callers, and the only thing
that says so is the absence of code. A product built from this template has to decide where
"an authenticated request" ends and "a handler" begins, and it has to decide that in every
service it scaffolds, with nothing here to copy — so each one invents it, differently, and
the shells inherit whatever each one invented.

Authentication is also the most product-specific thing a template can contain. The identity
provider, the session model, tenancy, token lifetimes and what the compliance regime demands
are all decisions a product makes, and none of them is a decision this repository can make
for it. A prebuilt auth is the thing people rip out — and ripping it out of a *template* is
worse than out of an application, because by then it is in every product, wired into files
those products have since edited.

The tree already showed what an unmade decision looks like from the inside. Three artefacts of
an authentication that was never built had accumulated: a `Trunk.toml` comment in two places
explaining that the dev proxy is same-origin because a refresh cookie is `SameSite=Lax` (no
cookie existed), and an `ApplicationSettings::secure_cookies()` in two places, uncalled, whose
doc comment promised auth cookies. All four were deleted or rewritten by this change. A
template that cannot decide should at least not leave the furniture.

## Decision

Ship the seam — the shape of the boundary — and nothing that crosses it.

`crates/identity` holds it, and the crate's admission rule is the decision: **in** go the
identity type, the credential parse, the verifier trait, the layer that attaches one, the
accessor a handler uses, and the Connect mapping for "no identity"; **out** goes anything that
knows a provider's name, a token's format, a role's meaning or a session's lifetime.

- **`Identity` in request extensions, attached by a tower layer.** `IdentityLayer` parses
  `Authorization: Bearer <token>`, asks a verifier whose it is, and inserts the answer into the
  request's extensions. connectrpc moves those into `RequestContext`, which is the framework's
  own convention for peer identity (`connectrpc::axum::serve_tls` does it for `PeerAddr` and
  `PeerCerts`).
- **The layer parses and attaches; it never rejects.** A layer that rejects has to know which
  methods are public, and that knowledge belongs with the method. So the handler decides, with
  one call: `ctx.principal()?`.
- **`VerifyCredential` is the product's one insertion point**, and the shipped implementation is
  `NoIdentities`, which authenticates nobody. A protected method is therefore unreachable until
  a product writes a verifier — one trait impl and one argument to `build_router`.
- **A credential is a header, opaque.** Bearer, in `Authorization`, and the crate does not care
  whether the token is a JWT, a session id or something else: parsing it is the verifier's job.
  Every generated client can attach one per request — Rust through `ClientConfig`'s default
  headers or `CallOptions`, Kotlin and Swift through the `headers` parameter on every method —
  so the seam reaches the native shells and not only the Rust ones.
- **Rejection is a Connect code, not an HTTP status.** `unauthenticated` (401), with one
  constant message for all three causes — no credential, an unusable one, a rejected one. The
  distinction is an oracle on the wire and is already in the log, where the cause belongs. It
  is also mechanical: `telemetry::http_layer` reads the Connect message out of the error body
  and exports it as `exception.message`, so anything a handler puts in that message lands in
  whatever trace backend a product runs. `permission_denied` (403) is left unused — it means
  "identity known, this action is not yours", and there is no role model here to know one.
  **All three of those causes are statements about the caller**, which is what that rule is
  about. A verifier that reached no verdict at all is not a statement about the caller, and was
  outside this bullet's scope when it was written; the rule above is unchanged by the 2026-10-16
  amendment, and the bullet below is what the amendment added.
- **A verifier answers in two arms** *(added by the 2026-10-16 amendment)*. `verify` returns
  `Result<Principal, VerifyFailure>`, and naming the arm is the implementation's job: there is
  deliberately no `From<VerifyError>` conversion, because a default arm would be the same collapse
  one step earlier. `Rejected` — a verdict, and the verdict was no — keeps everything above byte
  for byte: `Identity::Anonymous`, `unauthenticated`, and a `debug` log, because a token-guessing
  scan must not be able to fill a production log. `Unavailable` — the JWKS fetch that failed, the
  introspection call that timed out, the session lookup against a store that is not answering — is
  `Identity::Indeterminate`, a `warn`, and `internal` (500) with its own constant message, "the
  service could not check that credential". The redaction rule is not arm-specific: neither message
  carries the credential or the cause, and what the split leaks about the caller is nothing, since
  "we did not find out" describes this service and not what was presented.
- **The layer sits inside the nest, inside the RPC span, inside the request timeout.** It reads
  a header rather than a path, so the reasoning that keeps `RpcLayer` outside the nest does not
  apply to it; being inside means a verifier that does I/O is bounded by `timeouts::REQUEST`.
  The rule it generalises to is the one worth remembering: **a layer that can reject must sit
  inside `RpcLayer`**, or its rejections are responses the RPC layer never sees — no span, no
  duration measurement, invisible in exactly the traces and error-rate dashboard you would look
  at during an authentication failure.
- **Both templates.** `templates/service` carries the same seam, so a scaffolded service has it
  the day it is scaffolded, and `services/api` plus `apps/counter` demonstrate it: one protected
  RPC (`counter.v1.CounterService/Whoami`) beside four public ones, and the same pair in the
  template's `Ping`/`Whoami`.
- **Nothing requires a secret to build or test.** The tests supply a verifier that accepts one
  literal token. There is no `[auth]` config section, because there is nothing to configure —
  a verifier is code, not a key, and a config section no code reads is the furniture described
  above.

## Consequences

Choosing an identity provider is additive. A product implements `VerifyCredential`, passes it
to `build_router`, and every protected method starts working; nothing in the template is
deleted to make room, because nothing was there.

The core stays domain-level (ADR 0002). No credential enters the core's API enum or any other
core type, and
`cargo xtask check` now fails if a core grows a dependency on `identity`, `proto`, `connectrpc`
or any other wire crate — the enforceable half of a rule that was prose until there was a crate
worth reaching for. The client half sits in `apps/counter/client`, where ADR 0011 put the
`traceparent`.

Forgetting to protect a method is a test failure, not a silently public endpoint. A test
enumerates `Router::methods()` — the real registration — and requires every method to appear in
an explicit `PUBLIC` or `PROTECTED` list, then proves each list behaves as classified. A new RPC
fails that test until somebody decides which it is, and the assertion that `PROTECTED` is
non-empty is what stops the whole thing being deleted rather than maintained.

The honest cost: **there is no runtime demo of the happy path.** With `NoIdentities` in the
router, `Whoami` refuses every caller — shells, `curl`, `devenv up` — and the authenticated path
is exercised only by tests, which supply a verifier. That is deliberate, and it is the first
thing a future reader will want to "fix": the only way to make the demo work is to ship a
credential, and a credential in a template is a credential in every product.

Two smaller costs. The classification lists live in a test file, so a reader of a handler does
not see them — the handler's own `ctx.principal()?` is what it sees, and the test is what makes
that sufficient. And `Whoami` is scaffolding: replacing it means moving the classification pair
and the authenticated-success test onto the replacement in the same commit, since deleting all
three leaves the seam with no positive coverage at all.

**The fourth case, which this record did not cover** *(2026-10-16)*. The three-cause rule stands,
and the tests that pin it — `a_rejected_credential_is_indistinguishable_from_no_credential`,
`every_protected_method_refuses_an_anonymous_caller`,
`an_anonymous_identity_is_indistinguishable_from_a_rejected_one` — passed unmodified before the
split and pass unmodified after it. What is new is that a product's verifier has to say **which
kind of no** it means.

Why it was invisible from here: `NoIdentities` can only ever produce a verdict of no. It is absent
by policy, not unreachable, and answering `Unavailable` would tell a caller that a service with no
authentication configured had an outage — the wrong answer for the wrong reason. So nothing in this
repository exercised the second arm, neither the state nor the failure it comes from, and the first
product to write a verifier that does I/O was the one that would. A failed JWKS fetch, a
timed-out introspection call and a session lookup against a Postgres that is not answering all left
the building as 401 "authentication required": a claim about a credential no code ever evaluated,
filed in the client-error series on the very dashboard this record is written around, and — for a
client that reacts to 401 by discarding its token — an instruction to re-authenticate against the
same dead dependency.

`Identity::Indeterminate` is where that state now travels, and it has to travel: the code is chosen
by `principal()` in a handler, where the verifier that failed is long gone, and a state that only
existed inside the layer could only have been reported as something else. `identity()` with no
extension in the request still answers `Anonymous`, which is the same decision in the other
direction — a router that forgot to mount the layer is a defect in this service that a test can
catch, and it must read as "refuse", not as "we are broken". `Indeterminate` is a state only a
running verifier can produce.

What a product has somewhere to put, and does not get here: `permission_denied` and roles,
"your token expired" as a distinct answer, a `WWW-Authenticate` header, refresh and rotation,
cookie sessions, tenancy, rate limiting on failed authentication, and an audit trail of who did
what. The last is worth naming because `Whoami` is not one: a product that wants an audit trail
threads the `Principal` into the operation it protects, which also makes forgetting to require
an identity a compile error rather than a test failure.

## What was rejected, and why

**A prebuilt authentication.** OIDC or OAuth flows, password storage and hashing, session
persistence, refresh rotation. Every line of it is a product decision, and a wrong default that
is already wired in costs more to remove than an absent one costs to add. This is not
squeamishness about security: the seam is the part that cannot be retrofitted, so the seam is
the part that ships.

**A static-token verifier in config.** `auth.tokens = ["dev"]`, read by the shells from an
environment variable. It makes the demo work end to end, which is exactly the trade being
refused: it is the smallest possible authentication system, and therefore the one most likely
to survive into production — a committed credential outlives the commit that added it, and no
product's checklist contains its removal.

**Protecting an existing counter RPC.** `Reset` is the method that reads like it should need an
identity, and protecting it breaks the example app in all five shells the day it merges: all
four counter RPCs are core operations, and the shipped verifier authenticates nobody. The
protected method has to be one no shell emits, and DB-free besides, or CI — which has no
Postgres — cannot assert that an authenticated call ever succeeds.

**A per-method interceptor with a registry of protected methods.** It would work, and it would
put the declaration somewhere other than the method: two places to disagree, and the second one
is a list nobody reads. The brief for this change ruled it out and the ruling survives contact
with the code.

**Declaring it in the contract, with a custom proto option.** The strongest shape available —
the declaration beside the method, enforcement generic, an unset option distinguishable from
`false` and therefore fail-closed — and not reachable: connectrpc's `Spec` carries procedure,
stream type, idempotency and side, but no method options, so reading one would mean writing a
protoc plugin or a descriptor-set build step. That is a generator, not a seam. Worth revisiting
if `Spec` grows options.

**A layer that rejects.** Then the layer must know which methods are public, and any rejection
it produces outside `RpcLayer` is invisible in traces and metrics. Both halves are argued above;
the second is why the placement is a rule rather than a preference.

**`unavailable` (503) for a verifier that could not answer.** *(Rejected by the 2026-10-16
amendment.)* The obvious code for "our dependency is down", and wrong on this wire.
`apps/counter/client/src/lib.rs` collapses `ErrorCode::Unavailable` into
`Failure::Unreachable { endpoint }` unconditionally, which renders as "cannot reach the API at
<endpoint>" in all five shells, because connectrpc-rust reports a **refused connection** as
`Unavailable` — the only condition that path tests — and connect-kotlin and connect-swift have
parallel rules for the same fact. A verifier outage is not a connection failure: the request
arrived, was routed, and reached a handler that asked a question and got no answer. `unavailable`
would have every client in this repository describe this service's dependency as the caller's
network, and making that correct would mean changing the client's classification, the five shells'
rendering and the i18n strings — for a code that would then mean two different things on one wire.
`internal` already maps to `Code::ServerFault` and "the API had a problem with that request", which
is true, and needs no change outside `crates/identity`.

**Keeping one failure channel.** *(Rejected by the same amendment.)* `verify` returning
`Result<Principal, VerifyError>` and this crate deciding which arm a cause means. It cannot:
`VerifyError` is a boxed foreign error — a JWT library's variant, a driver's error, an HTTP status —
and classifying it here would have this crate know a provider's failure modes, which is the
admission rule's "out" column. A default arm is the same collapse moved one step earlier, and a
second trait method (`is_available`, or a flag beside the error) puts the decision somewhere a
reviewer of the failing line does not look. Naming the variant at the `Err` site is the whole cost
of the split, and it is also the point of it.

**A cookie credential.** A session model in disguise — who sets it, how it refreshes, `SameSite`,
`Secure`, CSRF — and unreachable from the native shells, which have no cookie jar. It is also
what the deleted `Trunk.toml` comment assumed, and the header is why that comment's reasoning
had to change rather than its conclusion.

**Attaching the parsed credential and letting a handler treat it as an identity.** One trait
impl short of the seam, and one trait impl away from an endpoint that accepts any string, in
every product that stops where the template stops. A layer that reports "a token was presented"
is not reporting an identity, and calling the type `Identity` while it means that would be the
lie that makes it ship.

**Putting the types in `services/api` or in `crates/telemetry`.** The first means a copy in
every scaffolded service; the second folds a concern into a crate whose name argues against it,
and makes a worker that wants logs pull in an identity surface.
