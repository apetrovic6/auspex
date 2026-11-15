//! Behaviour hung off the generated `counters` entity.
//!
//! `entities/` is rewritten wholesale, so nothing worth keeping lives there. Coherence is
//! per-crate rather than per-module, so these impls can live here instead.

use sea_orm::entity::prelude::*;

use crate::entities::counters::ActiveModel;

/// Required by `DeriveEntityModel` whether or not it does anything.
///
/// Deliberately empty. `updated_at` has a database default and is set explicitly on write;
/// a hook here would be bypassed by `update_many`, raw SQL, migrations, and anything
/// connecting outside this process, so the database is the right place for that rule.
impl ActiveModelBehavior for ActiveModel {}
