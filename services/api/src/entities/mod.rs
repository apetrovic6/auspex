//! `SeaORM` entities for this service's tables.
//!
//! Regenerate with `just services/api/entities` (sea-orm-cli reads the live schema, so
//! migrate first). Nothing hand-written belongs in here — it is rewritten wholesale.
//! Behaviour hung off an entity goes in the owning feature's `models.rs`, which works
//! because coherence is per-crate: a foreign trait on a local type can be implemented from
//! anywhere in this crate. That is also why entities live in the service rather than in a
//! shared crate — from another crate, `impl ActiveModelBehavior for ActiveModel` would be
//! two foreign types and the orphan rule would reject it.
//!
//! **Including these lines.** sea-orm-cli replaces this file too, so anything written here
//! survives only until the next schema change. That is why the lint allowance the generated
//! modules need sits on `pub mod entities;` in `lib.rs` instead of here: `lib.rs` is outside
//! the `--output-dir` the recipe passes, so it is the nearest place that is not rewritten.

pub mod counters;

pub mod prelude {
    pub use super::counters::Entity as Counters;
}
