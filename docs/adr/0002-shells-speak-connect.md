# 0002. Shells speak Connect; the core stays domain-level

- **Status:** Accepted
- **Date:** 2026-09-24

## Context

A Crux core is pure: it asks for effects and a shell fulfils them. When the counter's value
moved from the app to a server, something had to decide *where* the knowledge of HTTP lives.

Two coherent answers. The core could own the wire — emit an operation that already is a
Connect request, so the protocol is described once. Or the core could stay in its own
vocabulary and let each shell turn that into a call.

A prototype of the first was built and is recorded in `05834ea`.

## Decision

The core emits **domain** operations — an `Increment { by: 1 }` variant on the app's own API
enum — never `POST /counter.v1.CounterService/Increment`. Each shell turns that into an RPC using a
client generated from the same `.proto` the server is built from — `crates/proto` for Rust,
`proto/gen/kotlin` and `proto/gen/swift` for the native shells.

The Rust shells share one implementation at `apps/<app>/client`, generic over the transport.
Kotlin and Swift have one each.

## Consequences

The core carries no protocol code into a mobile binary, and no two shells can disagree about
the contract, because none of them writes it.

The cost is that the mapping is written four times across three languages. That cost is
real, and it is paid deliberately: each mapping is about forty lines, and each has tests
asserting on *effect* against a fake service — because routing `Increment` at the
`Decrement` RPC compiles cleanly in every one of these languages and simply moves the
counter the wrong way.

Adding an effect means fulfilling it in every shell. Rust and Swift make that a compile
error. Kotlin's `when` does **not**, unless used as an expression — which is why
`Core.kt` assigns its result.

## What was rejected, and why

**The core owns the wire.** The argument was drift: four hand-written clients will
eventually disagree. It does not survive generated ones — nothing is hand-written, so there
is nothing to drift. Withdrawn once that was pointed out.

Two findings from the prototype are worth keeping. `CommandContext` cannot be `Sync`, since
a `Request` holds a `Box<dyn FnOnce(Output) + Send>`, while `ClientTransport` requires
`Sync` — reconcilable only through an `Arc<Mutex<…>>` that buys nothing. And `crux_http`'s
ergonomic layer folds 4xx into an error, which is wrong for Connect, where a failure is a
typed response rather than a transport fault.
