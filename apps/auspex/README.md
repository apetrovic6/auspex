# auspex

A Crux app: one core holding the logic, several shells drawing it.

```
core/           the portable application logic — no I/O, no framework
shells/tui      terminal (ratatui)
```

The core never performs an effect; it asks for one, and a shell fulfils it. That is what
lets every shell share one set of decisions without any of them disagreeing.

```sh
just apps/auspex/dev              # format, lint, build and test every crate here
just apps/auspex/shells/tui/run
```

Only the shells listed above were created. `just add-shell auspex <tui|web|desktop>` adds another; nothing else needs editing.

## Two things this archetype did not create

**Native platform projects.** A Gradle or Xcode project is not a Cargo crate, and is only
wanted sometimes, so it is a separate command:

```sh
just add platform-android auspex    # or apple
```

It derives from an app that already has that platform, renaming every identifier as it
copies, and falls back to the skeleton in `templates/platform-<platform>` when no app does.
The Justfile here discovers `platforms/*` at runtime, so nothing else needs editing.

**A way to reach a server.** The core's only effect is `Render`, so this app's state is
entirely local. When it needs the outside world, add an `Effect` variant whose operation is
in this app's own vocabulary — `Increment { by }`, not `POST /v1/increment` — and let each
shell fulfil it with a client generated from the contract. `apps/counter` is the worked
example: the core stays domain-level, `apps/counter/client` maps those operations onto
Connect calls for the Rust shells, and the Kotlin and Swift shells do the same with their
own generated clients.

Both are additive. Neither changes anything already here.
