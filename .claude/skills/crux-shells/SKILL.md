---
name: crux-shells
description: Work on a Crux core or one of its shells (tui, web, desktop, Android, Apple) — especially wiring a shell to an API. Use when adding an effect, adding a shell, changing a ViewModel, or when an effect is being dropped and the UI hangs.
---

# Cores and shells

A Crux core is pure: it never performs an effect, it **asks** for one, and a shell fulfils
the request and hands the answer back. That is what lets five shells share one set of
decisions without disagreeing.

```
apps/<app>/core          the logic. No I/O, no framework, no protocol.
apps/<app>/client        the Rust shells' shared Connect client (optional)
apps/<app>/shells/*      tui (ratatui), web (Leptos/wasm), desktop (GPUI)
apps/<app>/platforms/*   android (Gradle/Compose), apple (Xcode/SwiftUI)
```

## The rule: the core stays domain-level

The core asks for an `Increment { by: 1 }` variant on its own API enum — a **domain**
operation. It never names HTTP, a path, or an encoding. Turning that into an RPC is each shell's job, using a client
generated from the same `.proto` the server is built from.

The cost is that each shell implements the call — four times, across three languages. The
benefit is that the core carries no protocol code into a mobile binary, and no two shells
can disagree about the contract because none of them writes it.

Worked example: `apps/counter`. Read `core/src/api.rs` and then one shell's mapping.

## Adding an effect

1. Add the operation and its output in `core/src/api.rs`, with
   `impl Operation for X { type Output = Y; }`. Both derive `Facet` + `Serialize` — that is
   what the typegen reads to emit Kotlin and Swift.
2. Add a variant to `Effect` in `core/src/app.rs`.
3. Handle the answer as an `Event` variant the core sends itself.
4. **Fulfil it in every shell.** Adding a variant without this is the failure mode below.

Model a failure as a *value* (`Result::Error { message }`), not as a dropped request. A shell
that can silently drop a request leaves the core pending forever with nothing on screen to
explain why.

## The failure mode to watch for

An unresolved effect hangs the app with no error. Each shell has a different defence:

- **Rust** (`match` on `Effect`) — exhaustive, so a new variant is a compile error.
- **Swift** (`switch`) — exhaustive, same.
- **Kotlin** (`when`) — **only exhaustive when used as an expression.** As a statement it
  compiles and silently drops the new effect. This is why `Core.kt` assigns the result:
  `val handled: Unit = when (...) { ... }`. Keep it that way.

## Wiring a shell to the API

The shape is the same everywhere: start the call, return immediately so the UI keeps
drawing, and carry the `Request` (or its id) *with* the call so it comes back attached to
the answer — which is what makes a table of outstanding calls unnecessary.

| Shell | Runs the call on | Gets the answer back via |
|---|---|---|
| tui | a `tokio::spawn`ed task | nothing — the task resolves where it lands, and the frame timer repaints |
| desktop | gpui's foreground executor | nothing — the task resolves where it lands |
| web | the local executor | nothing — one thread, so the task resolves directly |
| android | `viewModelScope` | the same coroutine, then `core.resolve(id, bytes)` |
| apple | a `Task` on the main actor | the same task, then `core.resolve(id:data:)` |

Native shells use `CoreFfi.resolve(id, bytes)`; Rust shells use `Core::resolve(&mut request, result)`.

The tui is the one that looks surprising, and the reason is its loop. It follows ratatui's
own async example (`examples/apps/async-github`): a `tokio::select!` over a frame timer and
the terminal's events. (auspex reads those on a thread of its own, `handoff::TerminalEvents`,
not `crossterm::EventStream`: `s` must stop reading the keyboard while ssh has it, and the
stream's reader cannot be stopped safely, which froze the loop about one `s` in two.) Because the timer repaints whether or not anything changed, an
answer no longer has to be carried back to the thread that owns the terminal — `Core` is
`Sync`, so the task resolves it, spawns whatever further effects that produces, and the next
frame shows the result. That is what let the channel this shell used to need disappear.

The desktop shell no longer carries answers back either, for a different reason. gpui's
foreground executor runs on the thread that owns `Context`, so a call driven there is
already where its answer has to be resolved. What stops that being obvious is tokio: the
transport is `connectrpc`'s, and its client-side deadline uses `tokio::time::timeout_at`,
which panics with *"there is no reactor running"* unless a runtime is **entered** on the
polling thread. Entered, not driving — that distinction was measured, not assumed, and it
is the whole design. `main` builds a runtime, holds an `EnterGuard` across
`application().run(..)`, and spawns nothing on it; gpui does the polling and tokio is a
reactor and a timer underneath.

If you add a shell that talks to the API, the question to ask is not "which runtime" but
"which thread may touch the UI, and is a runtime entered on it".

**Two type worlds meet in the mapping and nowhere else.** Across the FFI boundary the
operation arrives as bincode generated from the Rust core; the RPC takes protobuf generated
from the contract. Neither knows about the other. Keep that conversion in one file per
language: `apps/<app>/client` for the Rust shells, and one hand-written client in each
platform project (`apps/<app>/platforms/android/shared`, `apps/<app>/platforms/apple`).

**Test the mapping.** Sending `Increment` to the `Decrement` RPC compiles cleanly in every
one of these languages and just moves the counter the wrong way. Assert on *effect* against
a fake service that keeps a real number — see `apps/counter/client/tests/mapping.rs` and the
client test under `apps/counter/platforms/android/shared/src/test`.

## Wasm and hyper

`connectrpc`'s stock transport is hyper, which needs sockets. The browser shell uses
`crates/connect-web` instead — a `fetch` transport. `ClientTransport` demands a `Send`
future and browser futures are not, so the non-`Send` work runs on the local executor and
only its result crosses back through a oneshot. No `unsafe`, and the `JsValue` never leaves
its thread.

Keep `connectrpc`'s `client` feature out of anything the web shell compiles.

## After changing the core

Native bindings are build output and must be regenerated:

```sh
devenv shell -- just apps/<app>/generate-android
```

Both steps must succeed. Typegen without a successful `boltffi pack` leaves the Kotlin types
and the `.so` disagreeing, and the app dies at launch on a bincode error naming nothing.
