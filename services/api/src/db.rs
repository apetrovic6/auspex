//! Connecting to the database, and running migrations safely with replicas.

use std::time::Duration;

use sea_orm::sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sea_orm::{
    ConnectOptions, ConnectionTrait as _, Database, DatabaseConnection, DbErr, RuntimeErr,
    SqlxPostgresConnector,
};
use sea_orm_migration::MigratorTrait;

use crate::config::DatabaseSettings;
use crate::timeouts;

/// Advisory-lock key for this service's migrations.
///
/// Derived from the service name so that two services migrating their own databases on the
/// same Postgres instance never block each other, while every replica of *this* service
/// contends for the same key.
pub const MIGRATION_LOCK_KEY: i64 = advisory_key("api");

/// FNV-1a over the service name, as a `const fn`.
///
/// `DefaultHasher` is explicitly not stable across Rust releases, and this value must mean
/// the same thing to every replica — including ones built from a different toolchain
/// during a rolling deploy. FNV-1a is fixed by its specification.
const fn advisory_key(name: &str) -> i64 {
    let bytes = name.as_bytes();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
        i += 1;
    }
    // pg_advisory_lock takes a signed bigint; the sign is irrelevant as long as it is
    // deterministic, so the wrap is the intent rather than an accident.
    hash.cast_signed()
}

/// Open the connection pool.
///
/// Lazily: no connection is established here, so an unreachable database does not stop
/// the process from starting. That is what lets a replica boot during an outage and report
/// *not ready* — visible and self-healing — rather than crash-looping on startup. It is
/// also what lets tests build the real router without a database behind it.
///
/// `Database::connect` would connect eagerly, which is why the pool is built through sqlx
/// and adopted, rather than through sea-orm's own constructor.
///
/// # Errors
///
/// Returns an error only if the connection URL itself is malformed.
pub fn connect(settings: &DatabaseSettings) -> Result<DatabaseConnection, DbErr> {
    let options: PgConnectOptions =
        settings
            .connection_url()
            .parse()
            .map_err(|error: sea_orm::sqlx::Error| {
                DbErr::Conn(RuntimeErr::Internal(error.to_string()))
            })?;

    let pool = PgPoolOptions::new()
        .max_connections(settings.max_connections.get())
        .acquire_timeout(timeouts::DB_ACQUIRE)
        .connect_lazy_with(options);

    Ok(SqlxPostgresConnector::from_sqlx_postgres_pool(pool))
}

/// Run pending migrations, serialised across replicas by a Postgres advisory lock.
///
/// Every replica calls this on startup, so without a lock a rollout has N processes
/// running the same migrations concurrently. The lock makes exactly one do the work while
/// the others wait and then find nothing to do.
///
/// The lock is taken on a **dedicated single connection**, not on the pool. Session-level
/// advisory locks belong to a connection, and a pool is free to hand the unlock statement
/// a different connection than the lock — which would silently fail to release it. A pool
/// of exactly one connection makes "same connection" a property of the type rather than a
/// hope. Closing it would release the lock anyway; the explicit unlock is for clarity and
/// for the case where the connection is somehow retained.
///
/// # Errors
///
/// Returns an error if the lock cannot be taken or a migration fails. The error is
/// propagated rather than panicked on: a failed migration should fail this process
/// visibly, not abort it.
pub async fn migrate_with_lock<M: MigratorTrait>(
    db: &DatabaseConnection,
    settings: &DatabaseSettings,
    key: i64,
) -> Result<(), DbErr> {
    let mut options = ConnectOptions::new(settings.connection_url());
    options
        .max_connections(1)
        .min_connections(1)
        .connect_timeout(Duration::from_secs(5))
        .sqlx_logging(false);
    let lock_conn = Database::connect(options).await?;

    tracing::info!(key, "waiting for the migration lock");
    lock_conn
        .execute_unprepared(&format!("SELECT pg_advisory_lock({key})"))
        .await?;
    tracing::info!(key, "migration lock acquired");

    let result = M::up(db, None).await;

    // Released even when the migration failed, so a crashed rollout does not leave the
    // next attempt blocked behind a lock nobody holds a use for.
    let unlocked = lock_conn
        .execute_unprepared(&format!("SELECT pg_advisory_unlock({key})"))
        .await;
    let closed = lock_conn.close().await;

    result?;
    unlocked?;
    closed?;
    tracing::info!("migrations up to date");
    Ok(())
}
