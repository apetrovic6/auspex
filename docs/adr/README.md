# Architecture decision records

One file per decision that would otherwise be re-argued. Each records what was chosen,
what was rejected, and why — because the rejected option is the part that gets forgotten,
and without it a decision reads like an arbitrary preference.

They are short on purpose. A record nobody reads is worth nothing, and the code is the
authority on *what* the repository does; these are the authority on *why*.

| # | Decision | Status |
|---|---|---|
| [0001](0001-one-workspace-glob-driven-membership.md) | One Cargo workspace, membership by glob | Accepted |
| [0002](0002-shells-speak-connect.md) | Shells speak Connect; the core stays domain-level | Accepted |
| [0003](0003-a-service-owns-its-schema.md) | A service owns its entities and its migrations | Accepted |
| [0004](0004-generated-code-is-committed.md) | Generated protobuf code is committed | Accepted |
| [0005](0005-versions-toml-is-the-source-of-truth.md) | One file pins every cross-tool version | Accepted |
| [0009](0009-platform-projects-derive-first.md) | Platform projects derive from an app, with a skeleton as the fallback | Accepted |
| [0011](0011-opentelemetry-traces-and-metrics.md) | OpenTelemetry traces and metrics, off unless an endpoint is configured | Accepted |
| [0012](0012-the-template-ships-an-identity-seam.md) | The template ships an identity seam, not an authentication system | Accepted |
| [0013](0013-the-configuration-engine-is-config.md) | The configuration engine is `config`, behind `crates/settings` | Accepted |
| [0014](0014-translations-are-a-crate-and-the-language-is-the-shells-business.md) | Translations are a crate, and the language is the shell's business | Accepted |
| [0015](0015-the-shell-detects-the-locale.md) | The shell detects the locale, and matching it is a lookup | Accepted |
| [0016](0016-the-core-emits-meaning-and-the-shell-decides-how-it-reads.md) | The core emits meaning, and the shell decides how it reads | Accepted |
| [0017](0017-numbers-are-formatted-by-the-shell.md) | Numbers are formatted by the shell, not stringified by it | Accepted |
| [0018](0018-a-plural-family-is-one-key-and-the-locale-picks-its-member.md) | A plural family is one key, and the locale picks which of its strings to read | Accepted |
| [0019](0019-the-schema-is-served-from-descriptors-embedded-at-build-time.md) | The schema is served from descriptors embedded at build time, at two paths | Accepted |
| [0020](0020-instants-cross-the-wire-as-milliseconds-and-the-shell-tells-the-date.md) | An instant crosses the wire as milliseconds, and the shell tells you what day it is | Accepted |
| [0021](0021-the-icu-data-is-sliced-to-the-locales-the-catalog-ships.md) | ICU data is sliced to the locales the catalog ships | Accepted |
| [0022](0022-a-gate-runs-where-a-checked-manifest-says-it-runs.md) | A gate runs where a checked manifest says it runs | Accepted |
| [0023](0023-the-nightly-s-pull-request-is-opened-by-a-scoped-token-and-never-merged.md) | The nightly's pull request is opened by a scoped token and never merged | Accepted |
| [0027](0027-oidc-verification-is-a-generator-not-a-default.md) | OIDC verification is a generator, not a default | Accepted |
| [0028](0028-native-shells-log-in-with-oauth2-not-openidconnect.md) | Native shells log in with oauth2, not openidconnect | Accepted |
| [0029](0029-web-login-bff-with-stateless-encrypted-cookie.md) | Web login through the service (BFF) with stateless encrypted cookie + Web Locks | Accepted |
| [0031](0031-the-product-is-its-own-token-issuer.md) | The product is its own token issuer: email/password and Sign in with Google | Accepted |

## Writing one

`cargo xtask new-adr "<title>"` writes the next one: the number is the next free one, the
filename is the title kebab-cased, the sections are the shape the records above use, and the row
in the table above lands in the same breath as the file. `--date YYYY-MM-DD` backdates it,
because a record is usually written after the decision it records.

Or write it by hand, which is the same two edits: a file named with the next number, and a row in
the table above. `cargo xtask check` fails if either is missing, if two records share a number, if
the sequence has a hole in it, or if a file's heading and its own name disagree about which number
it is — because every citation in this repository cites the number, and two branches opened in the
same week both take "the next free one". The command removes the mistake; the check is what catches
the collision.

In the template checkout, classify each new record exactly once: `DROPPED_ADRS` in
`tools/cli/src/new.rs` for template-only decisions, or `INHERITED_ADRS` in
`tools/xtask/src/invariants.rs` for decisions products retain. `cargo cli check` (also run by
`check-repo` and CI) rejects unclassified, overlapping, duplicate and nonexistent records.

A decision that later turns out wrong is **superseded**, not deleted. Change its status,
say which record replaces it, and leave the reasoning where someone can find it: knowing
that an option was tried and abandoned is more useful than never having heard of it.
