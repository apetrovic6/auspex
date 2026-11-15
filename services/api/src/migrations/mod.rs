//! Schema migrations for this service's database.
//!
//! Migrations live with the service that owns the schema rather than in a repository-wide
//! crate, because "one repository, one schema" is an assumption a monorepo should not
//! make: each service owns its own database, its own migrations and its own advisory-lock
//! key, and a stateless service has none at all.
//!
//! They are a module rather than a separate crate, so there is nothing to register and
//! ownership is unambiguous. `cargo xtask new-migration api <name>` scaffolds a
//! timestamped file and adds it below, which is what `sea-orm-cli migrate generate` would
//! otherwise do.

use sea_orm_migration::prelude::*;

pub mod adopt;
mod m20260923_000001_create_counters;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m20260923_000001_create_counters::Migration)]
    }
}
