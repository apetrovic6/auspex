# `tools/` — repository tooling

Crates that exist to operate this repository rather than to ship: the `xtask` scaffolder,
code generators, migration helpers. They are workspace members so they are built, linted
and tested like everything else.

Picked up by the `tools/*` glob in the root `Cargo.toml`.

> This README is not decoration. A `members` glob that matches **nothing** is a hard error,
> so while this directory holds no crates yet, this file is what the glob matches. Cargo
> silently skips matched *files* and only requires a `Cargo.toml` from matched
> *directories*. Do not delete it.
