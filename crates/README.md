# `crates/` — shared libraries

Reusable library crates with no `main`: domain types, configuration, identity, telemetry,
generated protocol code, test helpers. Anything imported by more than one app or service belongs
here; anything imported by exactly one belongs next to its consumer.

Every direct child of this directory is a Cargo workspace member, picked up by the
`crates/*` glob in the root `Cargo.toml`. Create a directory with a `Cargo.toml` and it is
a member — there is nothing to register.

    just add lib-crate <name>

> This README is not decoration. A `members` glob that matches **nothing** is a hard error
> ("failed to load manifest for workspace member `crates/*`"), and this file is what keeps
> `crates/*` valid if the last crate here is ever removed — which is easy to do without
> thinking, and takes the whole workspace down with it, `xtask` included, so no check in this
> repository can warn you. Cargo silently skips matched *files* and only requires a
> `Cargo.toml` from matched *directories*. Do not delete it.
