//! `SeaORM` entities for this service's tables.
//!
//! **A fixture, not generated output** — see `probe_lakes.rs` for what stands in for what,
//! why, and the 2026-11-03 check against sea-orm-cli 1.1.20.
//! sea-orm-cli rewrites this file as well as the modules it declares, so the probe replaces
//! all three: the one `add-database` installs declares nothing, and a feature slice naming
//! `crate::entities::probe_lakes` needs the declaration as much as the module. The real
//! generator declares `prelude` as its own file rather than inlining it; this fixture follows
//! it, because that was one of the differences the check found.
//!
//! Regenerate with `just services/api-generator-probe/entities` (sea-orm-cli reads the live
//! schema, so migrate first). Nothing hand-written belongs in here — it is rewritten
//! wholesale. Behaviour hung off an entity goes in the owning feature's `models.rs`.

pub mod prelude;

pub mod probe_lakes;
