//! `SeaORM` entity prelude, as sea-orm-cli writes it — a fixture, see `probe_lakes.rs`.
//!
//! Real output puts this re-export in its own file and declares it from `mod.rs`; the
//! hand-written fixture used to inline it, which was one of the differences the 2026-11-03
//! check found.

pub use super::probe_lakes::Entity as ProbeLakes;
