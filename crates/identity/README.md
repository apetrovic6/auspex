# `identity`

The seam between an authenticated request and a handler. Not an authentication system, and
the difference is the whole point: `docs/adr/0012` records why the template ships one and not
the other.

What is here:

- `Identity` / `Principal` — who a request turned out to be from, carried in the request's
  extensions: `Authenticated`, `Anonymous`, or `Indeterminate` when this service could not find
  out;
- `IdentityLayer` — a tower layer that parses `Authorization: Bearer …`, asks a verifier whose
  it is, and attaches the answer. It never rejects;
- `VerifyCredential` — the one insertion point a product owns. Implement it, pass it to
  `build_router`, and nothing else in the template changes;
- `VerifyFailure` — what that insertion point answers when it has no principal, and **which kind
  of "no" it means**: `Rejected` (a verdict, and the verdict was no) or `Unavailable` (no verdict
  — the JWKS fetch, the introspection call, the session lookup). There is deliberately no
  conversion from a bare error, so an implementation cannot fail without saying which fact it
  means;
- `NoIdentities` — the shipped default, which authenticates nobody, so a protected method is
  unreachable until a product supplies a verifier;
- `IdentityContext::principal` — what a protected handler calls, and the two constant Connect
  errors it returns: `Unauthenticated` (`unauthenticated`, 401) when the request carries no
  identity, and `VerifierUnavailable` (`internal`, 500) when this service could not determine one.

## Two ways a verifier fails

They are not the same fact, and the wire says so.

| arm | what it means | identity | on the wire | log |
| --- | --- | --- | --- | --- |
| `VerifyFailure::Rejected` | a verdict, and the verdict was no | `Identity::Anonymous` | `unauthenticated` (401) | `debug` |
| `VerifyFailure::Unavailable` | no verdict: the dependency did not answer | `Identity::Indeterminate` | `internal` (500) | `warn` |

The first row is the no-oracle rule: "no credential", "an unusable one" and "a rejected one" are
one answer, because the difference is an oracle for whoever is guessing tokens — and `debug` rather
than `warn`, because a scan can present as many rejected credentials as it likes and must not be
able to fill a production log. The second row is not a statement about the caller at all, so it is
not folded into the first: a product whose verifier does I/O has an outage before it has a bad
credential, and a caller holding a good credential must not be told their credential is bad.

`internal`, not `unavailable`: `apps/counter/client` maps `Unavailable` to "cannot reach the API at
<endpoint>" in all five shells — the SDKs report a refused connection as `Unavailable` — and a
verifier's dead dependency is not the caller's network. `docs/adr/0012` records that, and what it
rejected to keep it true.

Neither message carries the credential or the cause. `telemetry::http_layer` exports the Connect
message as `exception.message`, so a cause that leaked into either one would land in whatever trace
backend a product runs; the cause is in the log, which is where it belongs.

## The admission rule

**In:** the identity type, the credential parse, the verifier trait, the layer that attaches
one, the accessor a handler uses, and the Connect mapping for "no identity".

**Out:** anything that knows a provider's name, a token's format, a role's meaning, or a
session's lifetime. No OIDC, no OAuth flow, no password storage or hashing, no session
persistence, no refresh or rotation, no tenancy, no roles. A rule with no outside is not a
rule, and this crate stays small because the outside is large.

Two consequences worth stating where they will be read:

- **The layer parses and attaches; the handler decides.** A layer that rejects has to know
  which methods are public, and that knowledge belongs with the method. So there is no
  registry of protected methods here, and there should not be one.
- **Nothing here is configurable.** A verifier is code, not a key, and a config section no
  code reads is a promise nobody keeps.

## Where the other halves live

The client half is transport, not identity: a shell attaches a credential with
`ClientConfig::with_default_header` (see `apps/counter/client`), and a Crux core never learns
a token exists (ADR 0002). The service half — mounting the layer and requiring an identity in
one handler — is `services/api/src/startup.rs` and `tests/api/auth.rs`, mirrored in
`templates/service` so a scaffolded service has the seam the day it is scaffolded.
