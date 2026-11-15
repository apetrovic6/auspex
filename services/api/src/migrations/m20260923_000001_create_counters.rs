use sea_orm_migration::prelude::*;

use super::adopt;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// The tables this first migration owns.
///
/// A first migration is usually the whole schema rather than one table, and the adoption
/// decision is about the *set*: with two tables here, "some of it is already there" is a state
/// the service refuses instead of guessing at, and it is what lets the module VM test prove all
/// three outcomes against Postgres — empty, every table present, one of two present.
const TABLES: [&str; 2] = ["counters", "counter_events"];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // A first migration has two lives (see `migrations::adopt`). On a database this
        // service's own history created, the tables are absent and the DDL below runs. On one
        // where something else already created them — a dump restored ahead of the rollout, or
        // the Go service this port replaces and its `gorm.AutoMigrate` — this returns early and
        // SeaORM records the migration as applied, which is what adoption means.
        //
        // The DDL below deliberately does NOT say `if_not_exists`. Adoption is the decision, and
        // `if_not_exists` would make it unobservable: a table this migration did not describe
        // would be silently accepted, and nothing could tell the adopt path from the create path
        // — which is exactly the failure the mechanism exists to prevent.
        if adopt::detect(manager, &TABLES).await?.adopts() {
            tracing::info!(
                tables = ?TABLES,
                "adopting an existing schema: every table is already there"
            );
            return Ok(());
        }

        manager
            .create_table(
                Table::create()
                    .table(Counters::Table)
                    .col(ColumnDef::new(Counters::Id).text().not_null().primary_key())
                    .col(
                        ColumnDef::new(Counters::Count)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(Counters::UpdatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(CounterEvents::Table)
                    .col(
                        ColumnDef::new(CounterEvents::Id)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(CounterEvents::CounterId).text().not_null())
                    .col(
                        ColumnDef::new(CounterEvents::Count)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(CounterEvents::RecordedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await
    }

    /// Dropping an adopted schema is still possible, and still deliberate.
    ///
    /// The migration has no way to remember whether it created the tables, so `down` cannot be
    /// made safe automatically: a rollback of an adopted schema drops tables another system
    /// owns. Rollbacks run only when a human asks for one, which is where that judgement
    /// belongs; the alternative — a `down` that silently does nothing — would leave a rollback
    /// that reports success and changes nothing.
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Reverse creation order, so a future foreign key between them would still drop.
        manager
            .drop_table(Table::drop().table(CounterEvents::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Counters::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum Counters {
    Table,
    Id,
    Count,
    UpdatedAt,
}

/// Append-only history of a counter's values, so a client can catch up without polling.
///
/// Nothing reads it in this example; it exists because a first migration that owns more than one
/// table is the normal shape, and the adoption decision's "some of them are already there" case
/// is only reachable when a migration owns more than one.
#[derive(DeriveIden)]
enum CounterEvents {
    Table,
    Id,
    CounterId,
    Count,
    RecordedAt,
}
