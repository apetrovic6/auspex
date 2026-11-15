use connectrpc::{RequestContext, Response, ServiceRequest, ServiceResult};
use identity::IdentityContext as _;
use proto::r#gen::buffa::counter::v1::{
    DecrementRequest, DecrementResponse, GetRequest, GetResponse, IncrementRequest,
    IncrementResponse, ResetRequest, ResetResponse, WhoamiRequest, WhoamiResponse,
};
use proto::r#gen::connect::counter::v1::CounterService;
// `DateTimeWithTimeZone` is sea-orm's alias for `chrono::DateTime<FixedOffset>`, taken
// from its prelude rather than named through `chrono` directly: chrono is in this crate's
// graph via `sea-orm/with-chrono` but is not, and need not become, a direct dependency.
use sea_orm::entity::prelude::DateTimeWithTimeZone;
use sea_orm::{ConnectionTrait as _, DatabaseConnection, DbBackend, Statement, Value};

use crate::counter::domain::{self, Step};
use crate::counter::errors::CounterError;

/// The single counter this service keeps.
///
/// The contract has no counter identifier, so there is exactly one row. Naming it rather
/// than relying on a magic id keeps the table usable if the contract later grows one.
const COUNTER_ID: &str = "global";

pub struct Counter {
    db: DatabaseConnection,
}

impl Counter {
    #[must_use]
    pub const fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Apply a delta and return the new value, atomically.
    ///
    /// One statement rather than read-modify-write: two concurrent increments through a
    /// select-then-update would each read the same value and one update would be lost.
    /// `count = counters.count + EXCLUDED.count` makes the database do the arithmetic
    /// under its own row lock, and the upsert means a missing row is not a special case.
    async fn apply(&self, delta: i64) -> Result<domain::Counter, CounterError> {
        self.write(UPSERT_STEP, delta).await
    }

    /// Run a write and hand back what it stamped.
    ///
    /// `updated_at` travels as its own column because the database owns the rule that
    /// sets it (`counter/models.rs`), and it leaves as epoch milliseconds because that
    /// is what the contract carries (ADR 0020): the conversion happens here, once per
    /// statement, so no caller has to know what a `timestamp with time zone` is.
    async fn write(&self, sql: &str, value: i64) -> Result<domain::Counter, CounterError> {
        let row = self
            .db
            .query_one(Statement::from_sql_and_values(
                DbBackend::Postgres,
                sql,
                [Value::from(COUNTER_ID), Value::from(value)],
            ))
            .await?
            .ok_or_else(|| {
                // RETURNING on an upsert always produces a row; no row means the statement
                // did not do what this code thinks it does.
                CounterError::Store(sea_orm::DbErr::Custom(
                    "counter upsert returned no row".to_owned(),
                ))
            })?;
        Ok(domain::Counter {
            count: row.try_get::<i64>("", "count")?,
            updated_at_unix_ms: row
                .try_get::<DateTimeWithTimeZone>("", "updated_at")?
                .timestamp_millis(),
        })
    }

    async fn read(&self) -> Result<domain::Counter, CounterError> {
        let row = self
            .db
            .query_one(Statement::from_sql_and_values(
                DbBackend::Postgres,
                SELECT_ONE,
                [Value::from(COUNTER_ID)],
            ))
            .await?;
        // No row simply means nobody has counted yet: zero, stamped as `never`. `0` is
        // the contract's shared "never" precisely because the shells blank it rather than
        // rendering it, so a counter that was never read does not display as a moment in
        // 1970 (ADR 0020). The schema never produces this pair — its `updated_at` is
        // `NOT NULL DEFAULT CURRENT_TIMESTAMP` — so it is the only no-moment there is.
        row.map_or(
            Ok(domain::Counter {
                count: 0,
                updated_at_unix_ms: 0,
            }),
            |row| {
                Ok(domain::Counter {
                    count: row.try_get::<i64>("", "count")?,
                    updated_at_unix_ms: row
                        .try_get::<DateTimeWithTimeZone>("", "updated_at")?
                        .timestamp_millis(),
                })
            },
        )
    }
}

/// The statements `Counter` runs, named so `stamp_tests` can assert what they promise to
/// return: CI never executes them, and a `RETURNING` list missing `updated_at` is a
/// production-only failure (ADR 0020).
const UPSERT_STEP: &str = "INSERT INTO counters (id, count, updated_at) VALUES ($1, $2, now()) \
                          ON CONFLICT (id) DO UPDATE SET count = counters.count + EXCLUDED.count, \
                          updated_at = now() RETURNING count, updated_at";

const UPSERT_RESET: &str = "INSERT INTO counters (id, count, updated_at) VALUES ($1, $2, now()) \
                            ON CONFLICT (id) DO UPDATE SET count = EXCLUDED.count, updated_at = now() \
                            RETURNING count, updated_at";

const SELECT_ONE: &str = "SELECT count, updated_at FROM counters WHERE id = $1";

impl CounterService for Counter {
    #[tracing::instrument(name = "counter.get", skip_all)]
    async fn get(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetRequest>,
    ) -> ServiceResult<GetResponse> {
        Response::ok(self.read().await?.into())
    }

    #[tracing::instrument(name = "counter.increment", skip_all)]
    async fn increment(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, IncrementRequest>,
    ) -> ServiceResult<IncrementResponse> {
        let step = Step::from(request.to_owned_message());
        Response::ok(self.apply(step.get()).await?.into())
    }

    #[tracing::instrument(name = "counter.decrement", skip_all)]
    async fn decrement(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, DecrementRequest>,
    ) -> ServiceResult<DecrementResponse> {
        // The same step, the other way: `Step` carries a magnitude, and which direction it
        // applies in is this handler's business rather than the conversion's.
        let step = Step::from(request.to_owned_message()).negated();
        Response::ok(self.apply(step.get()).await?.into())
    }

    #[tracing::instrument(name = "counter.reset", skip_all)]
    async fn reset(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ResetRequest>,
    ) -> ServiceResult<ResetResponse> {
        Response::ok(self.write(UPSERT_RESET, 0).await?.into())
    }

    /// Who the caller is — the one method here that requires an identity.
    ///
    /// It touches no table, on purpose: the authenticated path has to be assertable in CI,
    /// and CI has no Postgres (`tests/api/helpers.rs` points the pool at a port with nothing
    /// behind it). A protected method that needed the store could only ever prove the gate
    /// opened, by failing differently afterwards.
    #[tracing::instrument(name = "counter.whoami", skip_all)]
    async fn whoami(
        &self,
        ctx: RequestContext,
        _request: ServiceRequest<'_, WhoamiRequest>,
    ) -> ServiceResult<WhoamiResponse> {
        // The seam, at the call site. The handler decides it needs an identity; the layer
        // that attached one decides nothing, because a layer that refuses would have to know
        // which methods are public, and that knowledge belongs here.
        //
        // For this method, forgetting the line is a compile error — there would be nothing to
        // put in `subject`. `tests/api/auth.rs` covers the methods where forgetting would
        // compile: it enumerates the routing table and fails on any method nobody classified.
        let principal = ctx.principal()?;
        Response::ok(WhoamiResponse {
            subject: principal.subject,
            ..Default::default()
        })
    }
}

/// The instant on its way out of the process.
///
/// CI has no Postgres behind the pool, and the `sea-orm/mock` feature cannot be switched on
/// for this crate to get one: enabling it removes `Clone` from `DatabaseConnection` (`sea-orm`
/// `cfg_attr(not(feature = "mock"), derive(Clone))`) and `startup.rs` clones the connection
/// into every service, so the union of features fails to compile the crate that wants to
/// test it. These are what remains assertable without a database, and the chain they close
/// is stated link by link in the two tests below: the statements name the column, and the
/// message carries the value. What they cannot cover — Postgres stamping `now()`, and
/// sea-orm extracting a `timestamp with time zone` into `DateTimeWithTimeZone` — is
/// database and library behaviour, on rows this repository does not write.
///
/// An always-zero `updated_at` would pass every other gate in the repository, which is why
/// these two exist at all: the shells format whatever arrives, and the core displays
/// whatever the wire carries.
#[cfg(test)]
mod stamp_tests {
    use super::*;

    /// The instant handed to the message — 2025-10-03T08:00:00.123Z, the same instant the
    /// `crates/i18n` tests render into sentences. A real instant rather than a round one:
    /// a field silently dropped or re-zeroed would still equal a literal `0`.
    const STAMPED_MS: i64 = 1_759_478_400_123;

    #[test]
    fn the_moment_a_response_carries_is_the_moment_it_was_handed() {
        // Through the conversion a handler actually uses, rather than through a helper
        // beside it: what this asserts is that the instant survives the whole way out, and
        // the response is the last place it can be dropped.
        let response = GetResponse::from(domain::Counter {
            count: 7,
            updated_at_unix_ms: STAMPED_MS,
        });
        assert_eq!(response.state.count, 7);
        assert_eq!(
            response.state.updated_at_unix_ms, STAMPED_MS,
            "the contract's milliseconds, unchanged — the shell is owed an instant it can format (ADR 0020)"
        );
    }

    /// `write` and `read` `try_get` an `updated_at`; these statements are the only thing
    /// that promises the row has one. A `RETURNING` edited down to `count` compiles, stays
    /// green in CI (which has no database to run it against), and fails in production on
    /// every write — so the promise is asserted on the statements the code actually binds.
    #[test]
    fn every_statement_names_the_instant_it_asks_back() {
        assert!(
            UPSERT_STEP.contains("RETURNING count, updated_at"),
            "apply() try_gets both columns: {UPSERT_STEP}"
        );
        assert!(
            UPSERT_RESET.contains("RETURNING count, updated_at"),
            "reset() try_gets both columns: {UPSERT_RESET}"
        );
        assert!(
            SELECT_ONE.contains("SELECT count, updated_at"),
            "read() try_gets both columns: {SELECT_ONE}"
        );
    }
}
