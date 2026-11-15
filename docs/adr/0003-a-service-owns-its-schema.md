# 0003. A service owns its entities and its migrations

- **Status:** Accepted
- **Date:** 2026-09-23

## Context

SeaORM entities and migrations had to live somewhere. The obvious-looking answer was a
shared crate — `crates/domain` — so more than one service could read the same tables and so
the types had one home.

## Decision

Both live in the service that owns the schema. Entities in `services/<name>/src/`,
migrations as a module at `services/<name>/src/migrations/`, applied at boot.

Concurrent replicas are handled by a Postgres **advisory lock** keyed on the service name,
taken on a dedicated single connection, so a rollout with N replicas has one migrating and
the rest waiting.

## Consequences

Each service owns its database, and a stateless one has none — there is no repository-wide
migration step to run, and no shared crate that must be compiled by services that do not use
it. Adding a stateful service means adding a database to `nix/services.nix` and nothing else.

Two services needing the same data is now visibly a design question rather than an
accidental shared dependency: they talk through a contract.

The lock must be taken on its own connection. A pool may hand the unlock to a different
connection than the lock, and a session-scoped advisory lock belongs to a connection.

## What was rejected, and why

**A shared `crates/domain` for entities.** Decisive rather than merely preferable: SeaORM's
`DeriveEntityModel` requires `impl ActiveModelBehavior for ActiveModel`, and from any crate
other than the one defining the entity that is two foreign types. The orphan rule rejects
it. The recommendation for a shared crate was withdrawn on that ground.

**A separate migration crate per service**, as `sea-orm-cli` scaffolds. It works, and it
adds a crate whose only job is to be depended on by exactly one other. A module is the same
code with one less manifest.

**Migrating outside the service**, as a deploy step. It removes the lock problem by removing
concurrency, and replaces it with a schema and a binary that can be deployed out of order.
