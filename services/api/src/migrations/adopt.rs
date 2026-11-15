//! Adopting a schema that already exists.
//!
//! A service's first migration has two lives. On an empty database it *is* the schema: the
//! full DDL, applied in order. On a database that already has the tables — a previous
//! generation of the service, a dump restored ahead of the rollout, a `gorm.AutoMigrate` that
//! created them — the same migration has to record itself as applied without touching a table
//! it did not make. Running the DDL there either fails (`relation already exists`) or, with
//! `IF NOT EXISTS`, silently succeeds against a table whose columns are not the ones this
//! migration describes.
//!
//! [`detect`] is the whole mechanism. A migration calls it before its DDL and returns early
//! when the answer is [`Adoption::Adopt`]; `SeaORM` records a migration as applied when `up`
//! returns `Ok(())`, so returning early is all "adopt" means. It is deliberately not a
//! migrator wrapper: the migration keeps its DDL where a reader looks for it, and the
//! decision is one call they can see.
//!
//! It is proved against Postgres, not against a fixture: `checks.<svc>-module-vm` runs this
//! service's real migrations in the three states — empty (the DDL runs), fully present
//! (adopted, columns untouched) and half present (refused, both sets named). An in-process
//! SQLite test was tried first and removed: it pulled the SQLite stack into the dependency
//! graph for one test, and the SQLite CVE in it fails `cargo deny` (`RUSTSEC-2026-0459`).

use sea_orm_migration::prelude::*;

/// What a first migration should do, decided from the live schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adoption {
    /// None of the tables exist: run the DDL.
    Create,
    /// Every table exists: this schema was created elsewhere, so leave it alone.
    Adopt,
}

impl Adoption {
    /// Whether the migration should return without running its DDL.
    #[must_use]
    pub const fn adopts(self) -> bool {
        matches!(self, Self::Adopt)
    }
}

/// Decide whether `tables` already exist.
///
/// # Errors
///
/// Returns [`DbErr::Migration`] when some of the tables exist and some do not. That state is
/// not a decision this function can make: adopting would leave the schema missing the tables
/// the migration was going to add, and creating would fail on the ones already there. A human
/// has to look, and the error names both sets so they can.
pub async fn detect(manager: &SchemaManager<'_>, tables: &[&str]) -> Result<Adoption, DbErr> {
    let mut present = Vec::new();
    let mut absent = Vec::new();
    for table in tables {
        if manager.has_table(*table).await? {
            present.push(*table);
        } else {
            absent.push(*table);
        }
    }

    if present.is_empty() {
        return Ok(Adoption::Create);
    }
    if absent.is_empty() {
        return Ok(Adoption::Adopt);
    }
    Err(DbErr::Migration(format!(
        "the schema is half there, so this migration cannot decide whether to create or \
         adopt: present [{}], absent [{}]. Drop the present tables to recreate them, or \
         create the absent ones by hand, then run the migration again",
        present.join(", "),
        absent.join(", ")
    )))
}
