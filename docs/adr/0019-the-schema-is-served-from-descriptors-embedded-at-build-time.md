# 0019. The schema is served from descriptors embedded at build time, at two paths

- **Status:** Accepted
- **Date:** 2026-10-03
- **Amended:** 2026-10-07 — reflection is served only when configuration says so, and the two
  methods are classified. Amended rather than superseded: the decisions above still hold, and
  the question this record left open (it says so itself, under "Consequences") is answered in
  place rather than in a second record a reader of this one would not find.

## Context

Server reflection was not misconfigured; it was absent. Nothing in the tree mentioned
`ServerReflection`, so a client asking for the schema got a 404 from a path nothing served.
Every discovery-based tool reports that as a connection problem — Bruno's gRPC mode and
grpcurl both say they cannot reach the server — which is indistinguishable from the process
being down. The first thing anyone does with a new Connect endpoint is point a client at it
and ask what it offers, so the first experience of this template's API was that it did not
work.

Three things had to be true at once, and each of them is a decision.

**The schema has to exist at runtime.** Reflection answers with `FileDescriptorProto`s, which
are build-time artefacts. The usual shapes are a `FileDescriptorSet` file shipped beside the
binary, or a build script that bakes one in — both of which mean the binary and its schema can
disagree, and the second of which puts `protoc` back in `cargo build`'s path, which
[ADR 0005](0005-versions-are-single-sourced.md)'s committed-output rule exists to avoid.

**Something has to serve it**, over Connect, beside services written against
`protoc-gen-connect-rust`.

**And it has to be where clients look**, which is not one place.

## Decision

**`protoc-gen-buffa` embeds the descriptors (`reflect_mode=bridge`), and
`connectrpc-reflection` serves them at both the API prefix and the server root — when
configuration asks for it, which only the `local` profile does.** (The last clause is the 2026-10-07
amendment; the rest is as originally accepted.)

| piece | what it does |
|---|---|
| `reflect_mode=bridge` | each generated package embeds its descriptors and exposes `descriptor_pool()` |
| `strategy: all` | required by the above: under buf's per-directory default each invocation sees one package, so the pool embedded in one would not cover the others |
| `buffa-descriptor` | the runtime the generated code references by name; a dependency of `crates/proto`, not an implementation detail of the plugin |
| `connectrpc-reflection` | the service, installed on a `connectrpc::Router` |
| two mounts | `/api/grpc.reflection.v1.ServerReflection/…` and the same at `/` |
| `[reflection] enabled` | whether either mount exists at all — off unless a profile says otherwise |

**The schema travels in the binary.** There is no `FileDescriptorSet` to ship and no build
script to produce one, so the schema cannot drift from the code that serves it — they are the
same artefact. `cargo build` still needs no buf, no protoc and no network, because the
generated output is committed and `just proto::check` is what keeps it honest.

**It is mounted twice, and that is the whole point.** `buf curl` and the generated clients
derive the reflection path from the base URI they were given, so for this server they find the
copy under `/api`. gRPC-native tooling asks at the **server root** whatever URL it was given,
because the gRPC protocol defines a path as `/package.Service/Method` and has no notion of a
base path. Serving one and not the other is, from the client's side, indistinguishable from
serving neither — which is exactly the state this record was written out of, since a reader who
tried both `http://host:5000/api` and `http://host:5000` concluded the server was unreachable.
Both service names are routed, `v1` and the superseded `v1alpha`, because released clients
still send both. They are `route_service` rather than a fallback, so the fallback stays free
for the web bundle this origin serves in release builds.

**And it is served only when something chose to serve it** — amended after the fact, which the
"Consequences" section below records in its original words. `Settings` carries a `[reflection]`
section whose `enabled` defaults to **false**, `App::build` turns that into
`startup::Reflection::{Served, Withheld}`, and `build_router` installs both mounts or neither.
The committed `config/api.toml` states `enabled = false` and `config/api.local.toml` — the
`local` profile, which is the default when `APP_PROFILE` is unset — states `true`. So reflection
is on under `devenv up` and off everywhere else until a deployment asks for it in its own
overlay or in `APP__REFLECTION__ENABLED`.

**A missing section means off, and that is the half that matters.** `#[serde(default)]` on the
section is what makes a configuration file that never heard of this key a configuration that
does not serve the schema — which is the situation the defect below was really about, since a
product generated from this template inherits its config files along with everything else.
`a_configuration_with_no_reflection_section_does_not_serve_it` pins it, because the opposite
default is one attribute away and nothing else in the tree would notice.

**Two named variants rather than a `bool` parameter.** `build_router(db, verifier, true)` reads
identically whichever way round the argument was meant, and the wrong way round publishes the
schema. `Reflection::Served` / `Reflection::Withheld` cannot be read backwards, and the two
states are each a test's subject: `helpers::router` serves reflection (the four routing tests
depend on it) and `helpers::router_without_reflection` withholds it.

**Both crates are pinned in `versions.toml` and checked**, for ADR 0005's reason: the buf
plugins and these runtimes move as a set, and buffa treats a minor bump as breaking.
`connectrpc-reflection` gets its **own key** rather than reusing `connectrpc`'s, because the
companion crates are numbered independently inside one release line — `connectrpc` is 0.9.1
and `connectrpc-reflection` is 0.9.0 — so a single pin would have to be wrong for one of them.
`buffa-descriptor` is 0.9.2 like `buffa`, so it checks against that key. Two more version
checks: **39 → 41**.

Measured in [ADR 0017](0017-numbers-are-formatted-by-the-shell.md)'s currency, because the
expectation going in was that this would cost the browser something: `crates/proto` is linked
by the wasm shell, and embedding descriptors into it looks like a download cost.

| artifact | before | after | delta |
|---|---|---|---|
| `counter-web` `.wasm` — what a browser downloads | 860,384 B | 860,184 B | −200 (−0.02%) |

It does not grow, and the 200 bytes are noise rather than a saving. Nothing in the browser
calls `descriptor_pool()`, so the embedded constants are unreachable and the optimiser drops
them. **Reflection is a server cost only** — worth recording because the opposite was assumed,
and because the assumption would have been a reason not to do it.

## Consequences

**Reflection is unauthenticated, and the check that classifies methods cannot see it.**
`every_registered_method_is_classified` enumerates `register_services(…).methods()`;
reflection is installed in `build_router`, around that call, so its two methods never reach
the set compared against `PUBLIC`/`PROTECTED`. That test's own comment — "an unclassified
method is one nobody thought about, and the default must not be public" — is now true of
every method except these two. Anyone who can reach the port can enumerate every service,
method, message and field name. For this repository the schema is public anyway; for a product
generated from it, the exposure is inherited rather than chosen, and that is the defect. It is
filed rather than fixed here because the fix is a configuration decision — reflection is a
development affordance and should probably say so — and this record is the wrong place to
make it quietly.

> **Resolved** (RT-1), and the paragraph above is left standing because it is the question this
> record failed to answer the first time. Reflection is now configuration, as described under
> "Decision": off by default, on in the `local` profile, and a missing section means off.
>
> **It is also classified**, which the flag alone would not fix: whichever way the flag points,
> `every_registered_method_is_classified` still enumerates `register_services(…).methods()` and
> reflection is still installed around that call. It cannot be classified *in* `PUBLIC`, and
> the reason is worth writing down because the two-line fix looks right. That test's
> `assert_eq!` is an **equality** between `PUBLIC ∪ PROTECTED` and the routing table: two more
> entries in `PUBLIC` make the sides unequal and the test fails, and the obvious repair —
> loosening it to "every registered method appears in one of the lists" — deletes the half that
> catches a *classified* method nobody registered, which is how a renamed RPC leaves a stale
> entry pretending to cover something. The equality is the gate.
>
> So `services/api/tests/api/auth.rs` grew a **third** list,
> `UNAUTHENTICATED_NON_SERVICE`: methods served without a credential that no service
> registers. It is held to the same standard from both sides —
> `every_registered_method_is_classified` asserts no registered method can hide in it (which
> would be an exemption from the equality, the one failure mode a third list introduces), and
> `the_unauthenticated_non_service_methods_are_the_ones_the_router_mounts` asserts it is
> exactly what `build_router` mounts, comparing against `startup::reflection_methods` rather
> than a retyped literal. Both halves of the original equality were re-verified by breaking
> them, one at a time.
>
> **Option 3 was rejected**: installing reflection inside `register_services` so the
> enumeration picks it up by itself. It cannot reach the root mount, which is a separate
> `axum::Router` by necessity, so it would cover one of the two copies while looking complete —
> worse than the blindness it replaced.

**The tests assert routing, not schema content.** `ServerReflectionInfo` is bidirectional
streaming, so a unary JSON POST is not a conversation it can complete; what the tests in
`services/api/tests/api/reflection.rs` separate is "this path is served" from "this path is
not", which is the whole of the failure. One of them is a control — an unserved root path still
answers 404 — without which the other three prove nothing. The schema's *contents* need no
test: the descriptors are embedded at build time, so a package missing from them is a package
missing from `proto/`.

Three of the four original tests pass whether or not the flag is honoured, because
`helpers::router` serves reflection deliberately — so `withheld_reflection_answers_at_neither_path`
posts to the paths that *are* routed when reflection is on and asserts all of them 404 with
reflection off. The control is not a substitute for it: the control posts to a path no
configuration ever routes, which is what makes it a control, and it would not notice a
`Withheld` that mounted everything anyway. Verified by making `Withheld` install reflection and
watching it fail (`501` instead of `404`).

**The service template has the same wiring**, so a scaffolded service is discoverable the day
it is created rather than the day someone notices it is not. Drift between the example and the
skeleton is this repository's most repeated defect, and reflection was not going to be an
exception — which is why the template carries the whole decision and not just the mounts: a
`[reflection]` section defaulting to false, a new `config/__name__.local.toml` overlay turning
it on for the `local` profile, the `Reflection` enum, the third classification list, and a
`tests/__name__/reflection.rs` asserting both states. A scaffolded service is discoverable
under `devenv up` and silent in production, which is what the example does.

**The template's Rust is compiled by `just check-archetype-templates`**, since RT-12: that
recipe scaffolds one probe of each of the four archetypes nothing had ever instantiated —
`service`, `worker`, `lib-crate`, `cli-app` — runs `just proto::generate` over the probe's
contract, and builds all four with `cargo check --all-targets`. It runs in the CI `proto`
job, because a scaffolded service does not compile until codegen has run and that is online;
`gates.toml` carries the entry and says so. Verified by breaking `templates/service/…/startup.rs`
and `templates/worker/…/run.rs` on purpose and watching the gate fail with `E0308` in the
rendered skeleton.

Before that it was compiled by nothing, and the cheap verifications still exist beside it:
`templates/` is a Cargo `exclude`, a scaffolded service needs `just proto::generate` before it
builds, and `every_template_renders_coherently` checks sentinel substitution and `Cargo.toml`
parsing rather than Rust syntax. `rustfmt --edition 2024 --check` proves the files parse — but
only that, and **its exit code is not the pass condition**: a nonzero exit is expected here and
the signal to read is the absence of a `^error` parse diagnostic. That measurement stands
after RT-12 as much as before it — the new gate compiles rendered probes, the template files
themselves are still never a workspace member, and nothing here should trust rustfmt's exit
code for the reasons measured below.

Two independent reasons a template file is not rustfmt-clean, measured rather than assumed,
because knowing which one you are looking at is the difference between ignoring a diff and
fixing it. `src/startup.rs` fails on the sentinel: `use crate::{API_PREFIX, config::Settings,
health, timeouts, __snake__}` sorts by where `__snake__` falls lexically, not by where the
crate name it becomes would. `tests/__name__/health.rs` and `src/health/service/v1.rs` fail for
a reason that has nothing to do with sentinels — they use nested `use proto::r#gen::{..}`
groups that rustfmt would flatten, and no `cargo fmt` ever reaches them because `templates/` is
excluded from the workspace. A template file can also be clean: the observability test added by
RT-2 exits 0, which is why that commit could report a clean check honestly.

Beyond that, the template files are kept structurally identical to the `api` ones that do
compile.

**`buf curl` cannot use reflection against a plaintext URL** without
`--http2-prior-knowledge`, which is buf's own guard rather than anything about this server.
Worth knowing before concluding reflection is broken — the failure message names the flag.

## What was rejected, and why

**A `FileDescriptorSet` file, or a build script that bakes one in.** The conventional answer,
and it reintroduces the thing committed generated output exists to prevent: a schema artefact
that is produced separately from the code and can disagree with it. A build script also puts
`protoc` back on `cargo build`'s path for everyone, including contributors who only touch Rust
— and `proto/buf.gen.yaml` is explicit that keeping them out is the property being protected.
`reflect_mode=bridge` makes the schema and the code one artefact instead of two that must
match.

**Mounting only under `/api`.** What a reader would do first, since that is where the services
are, and it leaves grpcurl and Bruno exactly as broken as before — because neither asks there.
The symptom would have been "reflection still does not work", with a diff that looks like it
should.

**Mounting only at the root.** The mirror of the same mistake: `buf curl` and the generated
clients would stop finding it.

**Leaving reflection on and only classifying it** (the cheap half of the fix). It records the
decision in the file where every other method's decision lives, which is why it is half of what
shipped — but on its own it answers "was this chosen?" with "yes, we chose to publish the
schema of every product generated from this template", which is not a choice a template gets to
make for its products. The flag is what moves the decision to the deployment.

**Gating it on `debug_assertions` or on a `cfg` instead of configuration.** Tempting, because
reflection really is a development affordance, and wrong twice: a staging deployment that wants
discovery would have to ship a debug binary, and a release build that wants it would have no way
to ask. It also makes the answer depend on how the binary was compiled rather than on something
an operator can read in a file, which is the opposite of the property this record exists for.

**Defaulting to on with an opt-out.** The smaller diff — no config file changes, nothing to
remember on first run — and it leaves exactly the defect being fixed: a product inherits the
exposure and the only sign is a key it never read about. Opt-in costs one line in one overlay
and makes silence mean "not served".

**One key per mount.** Rejected on the reasoning already in this record: serving one mount and
not the other is, from a client's side, indistinguishable from serving neither, so two keys
offer a configuration whose only distinct state is a confusing one.

**Loosening `every_registered_method_is_classified` to a subset check** so the reflection names
could live in `PUBLIC`. Covered under "Consequences": it trades the gate for a list.

**Reusing the `connectrpc` pin for `connectrpc-reflection`.** Tidier, and wrong: 0.9.1 and
0.9.0 are both current, so one of the two crates would fail a check that exists to catch real
disagreement. A pin that has to be wrong teaches people to ignore the checker.

**Leaving the version pins out of `versions.toml` entirely**, on the grounds that nothing
outside Cargo reads them — the argument `crates/i18n` makes for ICU. It does not transfer:
these two are the runtime half of buf *plugins* pinned by a flake input, which is precisely
the two-tools-must-agree case `[proto]` exists for.

**Excluding `buf.validate`-style types from the embedded image.** Not rejected so much as not
needed yet, and recorded because the neighbouring project had to: embedding a transitive
descriptor set that includes `validate.proto` took its generated `<pkg>.mod.rs` files from
11 KB to 2.18 MB of `u8` literals, rewritten into a new git blob on every `.proto` edit.
Nothing here imports it, so the whole addition is +1,234 lines across four files. The day this
template grows protovalidate, `exclude_types` becomes load-bearing on both the codegen and the
packaging plugin.
