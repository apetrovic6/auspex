//! Generated-shape entity for the `probe_lakes` table.
//!
//! **A fixture, not generated output.** `just check-template` copies this over a
//! probe service's `src/entities/` to stand in for `just services/<svc>/entities`, which
//! runs sea-orm-cli against a live schema and so cannot run in an offline gate. It is
//! hand-written to be exactly what that command would write for the table
//! `cargo xtask new-migration <svc> create_probe_lakes --field name:string --field
//! description:string? --field deactivated:bool --field latitude:double` creates — the probe
//! runs that same migration generator, so the two can be read side by side.
//!
//! Checked by hand against sea-orm-cli 1.1.20 on 2026-11-03, against a real Postgres: the
//! command writes exactly the shape below — `#[sea_orm(column_type = "Text")]` on `name`,
//! `#[sea_orm(column_type = "Text", nullable)]` on `description`, and a separate
//! `prelude.rs` declared from `mod.rs`. The fixture did not have those before that run; it
//! does now. The 2.0.3 CLI devenv provided then emitted the same entity code, so the skew
//! was not a defect for this table — but it was a whole major version that no check connected
//! to the runtime.
//!
//! The CLI is now pinned to the runtime in `versions.toml` and built by
//! `nix/sea-orm-cli.nix`, so it cannot drift without `cargo xtask check-versions` failing.
//!
//! What is still unproven: no gate runs sea-orm-cli. Raising the pin means re-running
//! `entities` against a real schema and diffing this fixture by hand; that instruction is in
//! `versions.toml` beside the pin, and this gate continues to compile only what comes after
//! the entity.
//!
//! No `impl ActiveModelBehavior for ActiveModel` here: the `entities` recipe passes
//! `--impl-active-model-behavior=false` to sea-orm-cli, and the feature's `models.rs` is the
//! single definition. A copy here would be a duplicate impl and would not compile.
//!
//! `PartialEq` without `Eq`, unlike `services/api/src/entities/counters.rs`: `latitude` is an
//! `f64`, so sea-orm-cli derives the one trait an `f64` can have.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "probe_lakes")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    #[sea_orm(column_type = "Text")]
    pub name: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub description: Option<String>,
    pub deactivated: bool,
    #[sea_orm(column_type = "Double")]
    pub latitude: f64,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
