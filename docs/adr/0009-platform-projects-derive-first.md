# 0009. A native platform project derives from an app, and falls back to a skeleton

- **Status:** Accepted
- **Date:** 2026-09-25

## Context

`just new crux-app <name>` creates a core, three Rust shells and a Justfile. It cannot create
the Gradle or Xcode project, for reasons that are not the templater's fault: those projects
carry binary assets, and their identifiers reach into a package directory tree, a Gradle
namespace, an `applicationId` and a bundle prefix rather than sitting in one manifest field.

`cargo xtask add-platform <app> <platform>` fills that gap. The question is what it copies.

## Decision

Both, in a fixed order. **An app that already has that platform** is preferred. **The
skeleton under `templates/platform-<platform>`** is used when no app has one, and can be
forced with `--from-template`.

Two things make the copy correct rather than approximate. Identifiers are substituted as the
tree is walked — through `xtask rename`'s spellings for a donor, through the template
sentinels for the skeleton — and non-UTF-8 files are copied byte for byte, because the
Gradle wrapper jar and a donor's launcher icons are not text. After a skeleton copy the
project also adopts the organisation prefix the app's own `boltffi.toml` declares, since
that file is where the generators are told to write and a Gradle namespace that disagrees
with it produces bindings the app module cannot see.

## Consequences

A repository with an example app gets a project known to work today, carrying whatever that
app has grown. A repository that deleted the example app — which a generated product is
expected to do — still gets a working project instead of an error telling it to write one by
hand.

The skeleton is the *floor*: the minimal project for a core whose only effect is `Render`.
That matches what `crux-app` generates. An app that grows an API effect follows
`apps/counter`, and the exhaustiveness check in `Core` names every site that must change.
`add-platform` says which way round the mismatch runs, in both directions.

The real cost is that the skeleton is a second copy of a project CI does not build, free to
stop compiling unobserved. Three things answer that, and all three were needed:

- `cargo xtask check-versions` covers the skeleton's Gradle pins — SDK levels, build-tools,
  the Gradle distribution, the Connect and protobuf runtimes — exactly as it covers the apps.
- A unit test renders both skeletons: a surviving sentinel, a project copied from the wrong
  level, or a `gradlew` without its executable bit all fail there.
- `just check-platform-android` scaffolds a throwaway app, gives it a platform project from
  the skeleton and **builds** it with Gradle. It is the only check that compiles this Kotlin,
  and it runs on every push in `ci-android`; `just check-archetypes` does the same for the
  Rust archetypes and the api generators in `ci-pure` ([ADR 0026](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0026-the-front-end-is-a-binary-and-products-pin-it.md)).

That last one earned its place immediately: the first run failed, because the skeleton's
`MainActivity` said `Event.Reset` while its core's all-unit `Event` is emitted as a Kotlin
`enum class` whose constants are `RESET`. `apps/counter` has a payload-carrying variant, so
its `Event` is a `sealed interface` and PascalCase is right there. Nothing but a compile
would have found that.

## What was rejected, and why

**Derive only, no skeleton.** What this repository did until now. It is honest about rot —
there is nothing to rot — but it fails exactly where a template should not: a product that
deletes the example app has no donor, and the instruction "write the first one by hand" is
the archaeology this monorepo exists to remove.

**Skeleton only, no donor.** One code path instead of two, at the price of the property that
makes the derive path good: a donor is a project CI builds every run. It would also lose
whatever a real project has grown on every copy, which is the interesting part.

**A skeleton with no binaries.** Nearly achieved: the launcher icons are adaptive-icon XML
and vector drawables, so the ten density-specific `.webp` files are gone (`minSdk` is 34, so
`mipmap-anydpi` always wins). The Gradle wrapper jar has to stay — `gradlew` needs it, there
is no `gradle` on `PATH` in this environment to regenerate it with, and Gradle's own guidance
is to commit it. One binary, and `gradle-wrapper.properties` is pinned against
`versions.toml` so it cannot drift from the apps.

## Where this does not reach

`cargo xtask add-shell <app> <tui|web|desktop>` adds a Rust shell to a Crux app. It copies the
skeleton, and it has no donor and no `--from-template` — not an oversight of this ADR, but its
argument applied where the premise is absent.

Derive-first is worth its cost because the platform skeleton is a *stub* of a project that
cannot be stubbed honestly, and a donor is a project CI builds every run. A Rust shell's
skeleton is the opposite: it is complete — `Cargo.toml`, `Justfile`, `src/`, and for web
`Trunk.toml`, `index.html` and `styles/` — while a sibling app's shell is that app's own UI
code, its own answers to effects its own core has grown, and its own translation keys.
Deriving would copy the exact thing a new shell exists not to inherit, and the skeleton would
be the option that is *right* rather than the floor. A flag there would offer a worse copy
under the name of a choice, which is the thing this ADR's rejection of `--from-template` was
already about.
