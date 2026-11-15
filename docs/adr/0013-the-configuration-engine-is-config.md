# 0013. The configuration engine is `config`, behind `crates/settings`

- **Status:** Accepted
- **Date:** 2026-09-28

## Context

The configuration crate — `crates/config` when this began, `crates/settings` by the end of
it, for a reason recorded below — was 215 lines and the only crate in the workspace that
named `figment`. Everything else reaches configuration through it: `services/api`,
`templates/service` and `templates/worker` each call its `load` and return
`Result<Settings, _>`, and none of them knows what merges the layers.

figment 0.10.19 was published **2024-05-17** and is still the newest version — two years
and four months with no release. It is worth being precise about what that is and is not:

- The repository is **not archived** and carries no deprecation notice. Issues and pull
  requests still accumulate.
- There is **no RUSTSEC advisory** against it. `just deny` on the tree before this change
  reported `advisories ok, bans ok, licenses ok, sources ok`, and figment appears nowhere
  in `deny.toml`'s ignore list.
- Nothing was broken. The crate worked, and it would have gone on working.

So this is a **staleness decision, not a security one**, and a reader in a year should not
conclude from it that something was wrong. The strongest single piece of evidence is that a
fork exists which says the quiet part out loud: `figment2` (0.11.5, `lmmx/figment2`)
describes itself as a "**Remaintained fork of figment**".

The argument that actually decides it is this repository's own policy. `deny.toml`'s ignore
list carries eight unmaintained advisories, every one of them on a *transitive* dependency,
each annotated with who pulls it in — and the block's own comment says a vulnerability
advisory must never be added there, because the answer is to fix or replace the dependency.
figment was a **direct** dependency, chosen here, with nobody upstream to blame. The day
RUSTSEC publishes an unmaintained advisory for it, `just deny` fails, the scheduled
`advisories` workflow goes red, and this migration happens anyway — under time pressure, on
whatever day that is, instead of deliberately now.

## Decision

Replace the engine with the `config` crate (`rust-cli/config-rs`, 0.15.26, published
2026-09-21) and **keep the public API byte-identical**. Every exported item stays as it was,
spelling included: `ENV_PREFIX`, `ENV_SEPARATOR`, `PROFILE_VAR`, `DEFAULT_PROFILE`,
`DEFAULT_DIR`, `DIR_VAR`, `Error`, `profile()`, `dir()`, `load()`, `load_from()`, and the
module doc comment's description of the layering — which is the contract the crate actually
publishes. No config file changed, no environment variable, no documented key, no
`versions.toml` entry.

Four details of the swap are decisions rather than mechanics:

- **The crate is renamed `crates/config` → `crates/settings`, and the dependency keeps the
  name `config`.** The two can coexist — Cargo identifies a package by name **and source**,
  so a path package and a registry package called `config` are simply two packages, and that
  was the first arrangement, verified by compiling rather than by reasoning. It was wrong for
  a reason that only shows up at the command line: with two packages named `config` in the
  graph, `cargo build -p config` answers ``error: specification `config` is ambiguous`` and
  offers `config@0.1.0` or `config@0.15.26`. Nothing in the Justfiles or the pipeline selects
  a package that way, so every gate stayed green while a one-off command a developer would
  reasonably type was broken. Renaming the crate is the only spelling that removes it —
  aliasing the dependency does not, because the ambiguity is over package names in the
  resolve graph and not over extern names. The consumers that had to change to buy that are
  four lines in `services/api`, `crates/telemetry` and the two service templates.
- **The dependency is declared in `crates/settings/Cargo.toml`, not in
  `[workspace.dependencies]`.** It has exactly one consumer: the same call
  `crates/connect-web` already makes for `bytes`, `http` and `js-sys`.
- **`default-features = false, features = ["toml"]`.** The defaults are
  `toml + json + yaml + ini + ron + json5 + convert-case + async` — five parsers and an
  async-runtime dependency for a crate that reads one TOML file per service.
- **`Error::Load`'s source stays boxed.** `ConfigError` is 96 bytes (measured); inline, the
  variant is 152, and every `Result<T, Error>` in the API then crosses
  `clippy::result_large_err`'s 128-byte threshold, which `-D warnings` turns into a failed
  build. Boxed, `Error` is 56.

**The tests stopped touching the process environment**, and that was forced rather than
chosen. figment's `Jail` gave each test an isolated working directory and environment by
doing the `unsafe` inside its own crate; the obvious replacement is not available, because
`std::env::set_var` became `unsafe` in edition 2024 and `[workspace.lints.rust]` sets
`unsafe_code = "forbid"`, which an inner `allow` cannot relax. What replaced it is better:
`Environment::source(Some(map))` takes the environment **as data**, which upstream added for
exactly this ("when you want to test your own code using this source, without the need to
change the actual system environment variables"). The file layers use `tempfile`, already a
direct dependency of `tools/xtask`. The tests now mutate nothing, race nothing, and no
longer depend on nextest's process-per-test for correctness.

For the same reason, `profile()` and `dir()` **split into a pure half and a one-line env
half** (`profile_from(Option<String>)`, `dir_from(Option<OsString>)`). Reading a variable is
not the part that can be wrong; deciding what to do when it is absent, empty or set is — and
that decision was previously untestable in-process. It is now tested, including the case
that was open: **an empty value counts as unset**. `APP_PROFILE=""` is what a launcher emits
for `APP_PROFILE=$PROFILE` when `PROFILE` was never set, and a profile literally named `""`
sends the loader looking for `api..toml`. That is the one intentional behaviour change in
this record, and it is the only one.

Two literals now have to agree — `ENV_PREFIX` (`"APP__"`, public, deployed, documented) and
the private `PREFIX` (`"APP"`), because figment wanted prefix and separator in one string
while `config` wants them apart. A test asserts `ENV_PREFIX == PREFIX + ENV_SEPARATOR`,
which is the shape this repository keeps closing with a check rather than a comment.

## Consequences

One crate's internals changed and nothing else did. `just ci` is green with no consumer
edited; if `services/api` had needed a change, the API would not have been kept identical and
the swap would have been wrong.

The lockfile **strictly shrinks**, and by more than expected: `config` 0.15.26 was *already*
resolved, because `leptos_config` depends on it with the same `default-features = false` and
`toml`. So the migration adds no package to the graph and removes five — `figment`, `pear`,
`pear_codegen`, `inlinable_string`, `uncased` — figment's whole proc-macro stack. Net −53
lines. `just deny` stays green: `advisories ok, bans ok, licenses ok, sources ok`.

Four behaviours that neither engine documented clearly are now pinned by a test rather than
left to chance: a single underscore inside a key does not nest (which is *why* `__` was
chosen, and nothing proved it before); a present-but-malformed TOML file is an error naming
the file; an empty environment value **overrides** rather than falling through to the file
below it (`ignore_empty` stays off); and a numeric-looking string keeps its spelling
(`try_parsing` stays off). Each was negative-tested — the assertion was broken, the failure
watched, the break reverted.

The crate moved: every `config::load` outside it is now `settings::load`, in `services/api`,
`crates/telemetry` and both service templates — four lines, plus the paths in their manifests.
`cargo … -p settings` is unambiguous, which is the whole point of having paid for it; under
the first arrangement `-p config` was not, and that is recorded in the Decision rather than
here because it is the reason for the name, not a side effect of it.

A correction worth recording, because it was in the plan this came from and is false:
`config` 0.15's `Value::into_string` **does** coerce an integer back to a string, so turning
on `try_parsing(true)` would *not* make a numeric-looking secret fail to deserialize into
`String`. The real cost is subtler and is what the test now pins — every variable makes a
round trip through `bool`/`i64`/`f64`, so a value whose spelling is not the number's comes
back rewritten: `007` as `7`, `1.50` as `1.5`, `True` as `true`. Nothing downstream can tell
it was ever different.

## What was rejected, and why

**`figment2`.** This is the closest call in the record and the one to reach for if a future
reader thinks the migration was overpriced. The "remaintained fork" at 0.11.5 is near-zero
work — change the import path — against an afternoon for this. Rejected because a fork's
maintenance is one person's attention with less history than the original's, and swapping a
stalled upstream for an unproven one buys a smaller version of the same problem. `config` is
maintained under the `rust-cli` organisation with a long release history and a release a week
before this record was written.

**Staying on figment.** Defensible on the day it was considered: nothing was broken and
`just deny` was green. Rejected for the reason in the Context — figment was a *direct*
dependency, and this repository's own supply-chain policy says a direct unmaintained
dependency gets replaced rather than ignored. Doing it now costs an afternoon; doing it the
morning RUSTSEC publishes costs the same afternoon plus a red pipeline and a decision made
under pressure.

**Keeping the crate named `config`.** This was the first decision and it was reversed. The
argument for it was real — the rename touches every consumer, both service templates and the
vocabulary the ADRs and READMEs already use, against a name collision that the compiler
handles cleanly. What it missed is that the collision is not only a compiler question:
`cargo build -p config` becomes ambiguous, which no gate catches, because no gate selects a
package by bare name. Reversed once that was measured. Recorded because "why is the
configuration crate called `settings`?" is exactly the question that gets asked, and the
answer is a command-line error nobody sees until they type it.

**Aliasing the dependency** (`config_rs = { package = "config", … }`). It compiles, and it
was the shape the original plan called for. It would not have helped: the ambiguity is over
package names in the resolve graph, so `-p config` stays ambiguous however the dependent
spells its extern name. An alias would have added a second name for the crate and fixed
nothing.

**`Environment::try_parsing(true)`.** Buys nothing — `Value::into_uint`/`into_int`/
`into_float` already parse a string, which is how `APP__PORT=9999` reaches a `u16` — and
rewrites values on the way through. Pinned by a test so nobody turns it on to fix a bug it
causes.

**`Environment::ignore_empty(true)`.** The opposite choice on the empty-value row, and a
defensible one. Rejected because `APP__PORT=` shadowing the file layer with an unusable value
is a loud failure at boot, whereas silently falling through hides the fact that a deployment
set a variable to nothing.

**Changing the layering, the prefix, the separator or the profile variable** while the file
was open. Every one of them is deployed. This change had to be invisible to a running
service, and it is — with two measured exceptions, both recorded here rather than buried in a
diff because they are the only places a running service could notice.

The first is the empty-means-unset case above: `APP_PROFILE=` used to name a profile whose
name was the empty string, and now falls back to `local`.

The second was found by running figment's exact former merge against the new one, rather than
by reading either: **a numeric-looking value in a string field used to be a boot failure.**
Under figment, `APP__DATABASE__PASSWORD=007` came back as
`Err(InvalidType(Unsigned(7), "a string"))` — figment parsed the value eagerly and then could
not deserialize the integer into a `String`. Under `config` it is `Ok("007")`. The difference
only widens what is accepted: nothing that booted before fails now, and something that failed
before boots. It is recorded because it means figment had a latent bug here that this change
fixes, and because "invisible" would otherwise be a claim nobody had actually tested.
