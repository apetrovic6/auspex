//! `cargo xtask add-database <service>`.
//!
//! `just add service` creates a service with no schema, and that is the right default:
//! a service owns its own database or has none at all, so the template cannot ship one
//! without asserting that every service wants Postgres. This is the command that asserts
//! it, for one service, when a product decides so.
//!
//! # What it does not do
//!
//! It does not make the readiness probe check the database. "Ready" means something
//! different per service — a read-mostly service with a cache may well be ready while
//! Postgres is down — and `services/api`'s answer is one choice rather than the choice.
//! The probe is named in the closing output instead.

use anyhow::{Context as _, Result};

use super::{Service, Writes, read_for_edit, report, rustedit, write_planned};

/// Install the schema plumbing into a stateless service.
///
/// # Errors
///
/// Returns an error if the service already has a database, if any file it would write
/// exists, if any of the anchored edits does not match, or if the `Justfile` or the settings
/// file already carries a `migrate:` recipe or a `[database]` section this command did not
/// write — all of which mean the service has been shaped by hand since it was scaffolded, and
/// the wiring has to be done by hand too.
pub fn add_database(name: &str) -> Result<()> {
    let service = Service::open(name)?;
    if service.has_database() {
        anyhow::bail!(
            "service {:?} already has {} — nothing to install",
            service.name,
            service.rel(&service.src("db.rs"))
        );
    }

    let outcome = convert(&service)?;
    let amended: Vec<&std::path::Path> = outcome
        .amended
        .iter()
        .map(std::path::PathBuf::as_path)
        .collect();

    report(&service, &outcome.written, &amended)?;

    println!("\n  The readiness probe still reports ready without consulting the database.");
    println!("  What `ready` means when Postgres is down is a decision this tool should not");
    println!("  make for you; `services/api/src/health/service/v1.rs` is one worked answer.");

    println!("\nNext:");
    println!("  devenv up postgres                    # or the whole stack");
    println!(
        "  cargo xtask new-migration {} create_<table> --field name:string",
        service.name
    );
    println!("  just services/{}/migrate", service.name);
    println!("  just services/{}/entities", service.name);
    Ok(())
}

/// What a conversion produced, so the reporting can stay outside it.
///
/// `Debug` because a test asserts on the failure side of `convert`, and `Result::expect_err`
/// needs the success type to be printable.
#[derive(Debug)]
pub(super) struct Outcome {
    /// New files, in the order they were written.
    written: Vec<std::path::PathBuf>,
    /// Every file the conversion meant to edit, whether its contents actually changed.
    amended: Vec<std::path::PathBuf>,
}

/// The conversion itself, and the half a test can drive.
///
/// `add_database` takes a name and resolves it through `Service::open`, which goes through
/// `repo::root()` — `env!("CARGO_MANIFEST_DIR")`, fixed at compile time — so nothing that
/// takes a name can ever be pointed at a temporary tree. This takes the `Service`, whose
/// `root`, `dir` and `name` are all public, and it is the whole job: plan every file, then
/// write. `report` stays outside it, so driving this from a test never shells out to rustfmt
/// over a directory that is about to be dropped.
///
/// Every file is resolved before any file is written, which is the whole point of the shape.
/// See the comment on the three reads below.
///
/// `pub(super)`, and its `Outcome` with it, so `api::fixtures` can run this generator ahead of the
/// three that need a converted service — the chain `just check-template` walks, in a
/// temporary tree rather than in a probe a test cannot break anchors in.
///
/// # Errors
///
/// Same as `add_database`: an existing file it would overwrite, or an anchored edit that
/// does not match.
pub(super) fn convert(service: &Service) -> Result<Outcome> {
    let snake = service.name.replace('-', "_");

    // Planned first, written second: every refusal below has to arrive before the service
    // is half converted, because the second run would then refuse on the mess the first
    // one left.
    let mut writes = Writes::default();
    writes.add(service.src("db.rs"), db_rs(&service.name));
    writes.add(
        service.src("migrations/mod.rs"),
        migrations_mod(&service.name),
    );
    writes.add(service.src("migrations/adopt.rs"), adopt_rs());
    writes.add(service.src("entities/mod.rs"), entities_mod(&service.name));

    // Every edit is resolved against the file on disk before anything is written, so a
    // service whose `main.rs` has been rewritten is refused with the anchor quoted rather
    // than silently skipped.
    let lib = service.src("lib.rs");
    let config = service.src("config.rs");
    let main = service.src("main.rs");
    let startup = service.src("startup.rs");
    let manifest = service.dir.join("Cargo.toml");
    let justfile = service.dir.join("Justfile");
    let settings_toml = service.dir.join(format!("config/{}.toml", service.name));
    let local_toml = service
        .dir
        .join(format!("config/{}.local.toml", service.name));
    // The integration tests call `build_router` and `register_services`, and this command
    // changes what both of those take. Converting `src/` and leaving `tests/` behind compiles
    // the library and breaks `cargo test` — which `just ci` does not run for a service that
    // does not exist yet, so the first person to see it is whoever converted their service.
    let tests = service.dir.join(format!("tests/{}", service.name));
    let helpers = tests.join("helpers.rs");
    let auth = tests.join("auth.rs");
    let reflection = tests.join("reflection.rs");

    // The manifest, the settings file and the `Justfile` were the three files `amend` reached
    // *after* the writes below, and that is RT-31: a service missing one of them was refused
    // once `src/db.rs` and the seven rewired files had already landed, so what it left behind
    // was a service with a database module nothing declared and a manifest with no sea-orm in
    // it — a tree the next run refuses on for reasons of its own. Read them here, plan them
    // here, and a refusal arrives before this function has touched the service at all.
    //
    // `read_for_edit` rather than a bare `read_to_string`, for the same reason the test suite
    // reads use it: `No such file or directory (os error 2)` on its own does not say which
    // file was wanted.
    let manifest_text = read_for_edit(service, &manifest)?;
    let settings_text = read_for_edit(service, &settings_toml)?;
    let local_text = read_for_edit(service, &local_toml)?;
    let justfile_text = read_for_edit(service, &justfile)?;

    let edited = [
        (lib.clone(), plan_lib(&std::fs::read_to_string(&lib)?)?),
        (
            config.clone(),
            plan_config(&std::fs::read_to_string(&config)?)?,
        ),
        (
            main.clone(),
            plan_main(&std::fs::read_to_string(&main)?, &snake)?,
        ),
        (
            startup.clone(),
            plan_startup(&std::fs::read_to_string(&startup)?)?,
        ),
        // Named in the error, unlike the reads above: a service that has lost its `src/` is a
        // service nobody could have scaffolded, but a suite someone deleted or renamed is an
        // ordinary thing to meet, and `No such file or directory (os error 2)` on its own does
        // not say which file was wanted.
        (
            helpers.clone(),
            plan_test_helpers(&read_for_edit(service, &helpers)?, &snake)?,
        ),
        (
            auth.clone(),
            plan_test_auth(&read_for_edit(service, &auth)?)?,
        ),
        (
            reflection.clone(),
            plan_test_reflection(&read_for_edit(service, &reflection)?)?,
        ),
        (
            manifest.clone(),
            // `amend` named the path in its error; `plan_manifest` on its own does not, and
            // "the manifest has no [dependencies] table" in a command that touches ten files
            // is not an error anyone can act on. The test that caught this is
            // `a_manifest_without_a_dependencies_table_is_refused_without_writing_anything`.
            plan_manifest(&manifest_text)
                .with_context(|| format!("editing {}", service.rel(&manifest)))?,
        ),
        // The two files that used to have no way to say they had done nothing. Both planners
        // returned `String` and both opened with a substring guard, so a `Justfile` with a
        // `migrate:` of its own and a `config/<name>.toml` with a `[database]` of its own were
        // reported as `✓ wired into:` having had neither recipe installed. They are in the
        // planned half already — this is the return type changing, not the ordering — and each
        // gets the same `editing <path>` context `plan_manifest` has, because a refusal in a
        // command that touches ten files has to name the file it is about.
        (
            settings_toml.clone(),
            plan_settings_toml(&settings_text, &service.name)
                .with_context(|| format!("editing {}", service.rel(&settings_toml)))?,
        ),
        // The other half of the split. The base file carries the pool size; the location
        // lives here, where devenv reads it and the image does not bake it, so a deployment's
        // URL and a committed structured field never meet.
        (
            local_toml.clone(),
            plan_local_settings_toml(&local_text, &service.name)
                .with_context(|| format!("editing {}", service.rel(&local_toml)))?,
        ),
        (
            justfile.clone(),
            plan_justfile(&justfile_text, &service.name)
                .with_context(|| format!("editing {}", service.rel(&justfile)))?,
        ),
    ];

    let written = writes.commit(service)?;
    write_planned(service, &edited)?;

    Ok(Outcome {
        written,
        amended: vec![
            lib,
            config,
            main,
            startup,
            helpers,
            auth,
            reflection,
            manifest,
            settings_toml,
            justfile,
        ],
    })
}

/// `src/db.rs`, with the advisory key derived from this service's name.
fn db_rs(service: &str) -> String {
    format!(
        r#"//! Connecting to the database, and running migrations safely with replicas.

use std::time::Duration;

use sea_orm::sqlx::postgres::{{PgConnectOptions, PgPoolOptions}};
use sea_orm::{{
    ConnectOptions, ConnectionTrait as _, Database, DatabaseConnection, DbErr, RuntimeErr,
    SqlxPostgresConnector,
}};
use sea_orm_migration::MigratorTrait;

use crate::config::DatabaseSettings;
use crate::timeouts;

/// Advisory-lock key for this service's migrations.
///
/// Derived from the service name so that two services migrating their own databases on the
/// same Postgres instance never block each other, while every replica of *this* service
/// contends for the same key.
pub const MIGRATION_LOCK_KEY: i64 = advisory_key("{service}");

/// FNV-1a over the service name, as a `const fn`.
///
/// `DefaultHasher` is explicitly not stable across Rust releases, and this value must mean
/// the same thing to every replica — including one built from a different toolchain during
/// a rolling deploy. FNV-1a is fixed by its specification.
const fn advisory_key(name: &str) -> i64 {{
    let bytes = name.as_bytes();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {{
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
        i += 1;
    }}
    // pg_advisory_lock takes a signed bigint; the sign is irrelevant as long as it is
    // deterministic, so the wrap is the intent rather than an accident.
    hash.cast_signed()
}}

/// Open the connection pool.
///
/// Lazily: no connection is established here, so an unreachable database does not stop the
/// process from starting. That is what lets a replica boot during an outage and report
/// *not ready* — visible and self-healing — rather than crash-loop on startup. It is also
/// what lets tests build the real router with no database behind it.
///
/// `Database::connect` would connect eagerly, which is why the pool is built through sqlx
/// and adopted rather than through sea-orm's own constructor.
///
/// # Errors
///
/// Returns an error only if the connection URL itself is malformed.
pub fn connect(settings: &DatabaseSettings) -> Result<DatabaseConnection, DbErr> {{
    let options: PgConnectOptions =
        settings
            .connection_url()
            .parse()
            .map_err(|error: sea_orm::sqlx::Error| {{
                DbErr::Conn(RuntimeErr::Internal(error.to_string()))
            }})?;

    let pool = PgPoolOptions::new()
        .max_connections(settings.max_connections.get())
        .acquire_timeout(timeouts::DB_ACQUIRE)
        .connect_lazy_with(options);

    Ok(SqlxPostgresConnector::from_sqlx_postgres_pool(pool))
}}

/// Run pending migrations, serialised across replicas by a Postgres advisory lock.
///
/// Every replica calls this on startup, so without a lock a rollout has N processes running
/// the same migrations concurrently. The lock makes exactly one do the work while the
/// others wait and then find nothing to do.
///
/// The lock is taken on a **dedicated single connection**, not on the pool. Session-level
/// advisory locks belong to a connection, and a pool is free to hand the unlock statement a
/// different connection than the lock — which would silently fail to release it. A pool of
/// exactly one connection makes "same connection" a property of the type rather than a hope.
///
/// # Errors
///
/// Returns an error if the lock cannot be taken or a migration fails. The error is
/// propagated rather than panicked on: a failed migration should fail this process visibly.
pub async fn migrate_with_lock<M: MigratorTrait>(
    db: &DatabaseConnection,
    settings: &DatabaseSettings,
    key: i64,
) -> Result<(), DbErr> {{
    let mut options = ConnectOptions::new(settings.connection_url());
    options
        .max_connections(1)
        .min_connections(1)
        .connect_timeout(Duration::from_secs(5))
        .sqlx_logging(false);
    let lock_conn = Database::connect(options).await?;

    tracing::info!(key, "waiting for the migration lock");
    lock_conn
        .execute_unprepared(&format!("SELECT pg_advisory_lock({{key}})"))
        .await?;
    tracing::info!(key, "migration lock acquired");

    let result = M::up(db, None).await;

    // Released even when the migration failed, so a crashed rollout does not leave the next
    // attempt blocked behind a lock nobody holds a use for.
    let unlocked = lock_conn
        .execute_unprepared(&format!("SELECT pg_advisory_unlock({{key}})"))
        .await;
    let closed = lock_conn.close().await;

    result?;
    unlocked?;
    closed?;
    tracing::info!("migrations up to date");
    Ok(())
}}
"#
    )
}

/// `src/migrations/mod.rs`, with no migrations in it yet.
fn migrations_mod(service: &str) -> String {
    format!(
        "//! Schema migrations for this service's database.\n\
         //!\n\
         //! Migrations live with the service that owns the schema rather than in a\n\
         //! repository-wide crate, because \"one repository, one schema\" is an assumption a\n\
         //! monorepo should not make: each service owns its own database, its own migrations\n\
         //! and its own advisory-lock key, and a stateless service has none at all.\n\
         //!\n\
         //! They are a module rather than a separate crate, so there is nothing to register\n\
         //! and ownership is unambiguous. `cargo xtask new-migration {service} <name>`\n\
         //! scaffolds a timestamped file and adds it below, which is what\n\
         //! `sea-orm-cli migrate generate` would otherwise do.\n\
         //!\n\
         //! [`adopt`] is the first migration's other life: a schema that already exists (a\n\
         //! dump restored ahead of the rollout, or the system this service replaces) is\n\
         //! adopted rather than recreated. `services/api/src/migrations/adopt.rs` is the\n\
         //! worked example.\n\
         \n\
         use sea_orm_migration::prelude::*;\n\
         \n\
         pub mod adopt;\n\
         \n\
         pub struct Migrator;\n\
         \n\
         #[async_trait::async_trait]\n\
         impl MigratorTrait for Migrator {{\n\
         \x20   fn migrations() -> Vec<Box<dyn MigrationTrait>> {{\n\
         \x20       vec![]\n\
         \x20   }}\n\
         }}\n"
    )
}

/// `src/migrations/adopt.rs`: the first-migration adoption mechanism.
///
/// Emitted rather than shared from a crate, for the reason migrations themselves are not
/// shared: the code that runs against a service's schema lives in that service, and the
/// per-service copy is what lets one service adopt a legacy schema while another never
/// considers it. `services/api/src/migrations/adopt.rs` is the worked example, including the
/// test that covers both decisions.
fn adopt_rs() -> String {
    "//! Adopting a schema that already exists.\n\
     //!\n\
     //! A service's first migration has two lives. On an empty database it *is* the schema:\n\
     //! the full DDL, applied in order. On a database that already has the tables — a\n\
     //! previous generation of the service, a dump restored ahead of the rollout, a\n\
     //! `gorm.AutoMigrate` that created them — the same migration has to record itself as\n\
     //! applied without touching a table it did not make. Running the DDL there either fails\n\
     //! (`relation already exists`) or, with `IF NOT EXISTS`, silently succeeds against a\n\
     //! table whose columns are not the ones this migration describes.\n\
     //!\n\
     //! [`detect`] is the whole mechanism. A migration calls it before its DDL and returns\n\
     //! early when the answer is [`Adoption::Adopt`]; `SeaORM` records a migration as applied\n\
     //! when `up` returns `Ok(())`, so returning early is all \"adopt\" means.\n\
     \n\
     use sea_orm_migration::prelude::*;\n\
     \n\
     /// What a first migration should do, decided from the live schema.\n\
     #[derive(Debug, Clone, Copy, PartialEq, Eq)]\n\
     pub enum Adoption {\n\
     \x20   /// None of the tables exist: run the DDL.\n\
     \x20   Create,\n\
     \x20   /// Every table exists: this schema was created elsewhere, so leave it alone.\n\
     \x20   Adopt,\n\
     }\n\
     \n\
     impl Adoption {\n\
     \x20   /// Whether the migration should return without running its DDL.\n\
     \x20   #[must_use]\n\
     \x20   pub const fn adopts(self) -> bool {\n\
     \x20       matches!(self, Self::Adopt)\n\
     \x20   }\n\
     }\n\
     \n\
     /// Decide whether `tables` already exist.\n\
     ///\n\
     /// # Errors\n\
     ///\n\
     /// Returns [`DbErr::Migration`] when some of the tables exist and some do not. That\n\
     /// state is not a decision this function can make: adopting would leave the schema\n\
     /// missing the tables the migration was going to add, and creating would fail on the\n\
     /// ones already there. A human has to look, and the error names both sets so they can.\n\
     pub async fn detect(manager: &SchemaManager<'_>, tables: &[&str]) -> Result<Adoption, DbErr> {\n\
     \x20   let mut present = Vec::new();\n\
     \x20   let mut absent = Vec::new();\n\
     \x20   for table in tables {\n\
     \x20       if manager.has_table(*table).await? {\n\
     \x20           present.push(*table);\n\
     \x20       } else {\n\
     \x20           absent.push(*table);\n\
     \x20       }\n\
     \x20   }\n\
     \n\
     \x20   if present.is_empty() {\n\
     \x20       return Ok(Adoption::Create);\n\
     \x20   }\n\
     \x20   if absent.is_empty() {\n\
     \x20       return Ok(Adoption::Adopt);\n\
     \x20   }\n\
     \x20   Err(DbErr::Migration(format!(\n\
     \x20       \"the schema is half there, so this migration cannot decide whether to create or adopt: present [{}], absent [{}]. Drop the present tables to recreate them, or create the absent ones by hand, then run the migration again\",\n\
     \x20       present.join(\", \"),\n\
     \x20       absent.join(\", \")\n\
     \x20   )))\n\
     }\n"
        .to_owned()
}

/// `src/entities/mod.rs`, before sea-orm-cli has written anything into it.
fn entities_mod(service: &str) -> String {
    format!(
        "//! `SeaORM` entities for this service's tables.\n\
         //!\n\
         //! Regenerate with `just services/{service}/entities` (sea-orm-cli reads the live\n\
         //! schema, so migrate first). Nothing hand-written belongs in here — it is rewritten\n\
         //! wholesale. Behaviour hung off an entity goes in the owning feature's `models.rs`,\n\
         //! which works because coherence is per-crate: a foreign trait on a local type can be\n\
         //! implemented from anywhere in this crate. That is also why entities live in the\n\
         //! service rather than in a shared crate — from another crate,\n\
         //! `impl ActiveModelBehavior for ActiveModel` would be two foreign types and the\n\
         //! orphan rule would reject it.\n\
         //!\n\
         //! Nothing you write here survives. sea-orm-cli replaces this file on every run,\n\
         //! including these lines — which is why the lint allowance the generated modules\n\
         //! need sits on `pub mod entities;` in `lib.rs` instead of here, where it would be\n\
         //! lost the first time the schema changed.\n\
         \n\
         // Empty until the first `just services/{service}/entities` run, which writes one\n\
         // module per table and rewrites this file to declare them.\n"
    )
}

/// `src/lib.rs`: declare the three new modules.
///
/// The lint allowance for the entities goes **here**, on the module declaration, and not in
/// `entities/mod.rs` where it reads more naturally. `sea-orm-cli` rewrites that file
/// wholesale on every `just services/<svc>/entities` run — it is the one file in the tree
/// whose contents cannot be relied on to survive — so an allowance written there is lost
/// the first time the schema changes, and what comes back is `empty_enums` on every
/// generated `pub enum Relation {}` and a failing `just check` nobody has a fix for. On the
/// declaration it is hand-written code in a hand-written file, and it covers the whole
/// module either way.
fn plan_lib(source: &str) -> Result<String> {
    let entities = "// Everything under `entities/` is regenerated wholesale by sea-orm-cli, so the\n\
        // workspace's pedantic lint set has nothing to offer it: any fix would have to be made\n\
        // in the generator and would be overwritten on the next run. The allowance lives on\n\
        // this declaration rather than inside the module because that file is itself rewritten.\n\
        #[allow(\n\
        \x20   clippy::pedantic,\n\
        \x20   clippy::nursery,\n\
        \x20   reason = \"generated by sea-orm-cli; fixes would be overwritten\"\n\
        )]\n\
        pub mod entities;";

    // `db::connect` hands this to the pool, so installing the module without installing the
    // constant leaves a service that does not compile — which is how it shipped until RT-22
    // put a gate on the generated output. Its value and the assertion are
    // `services/api/src/lib.rs`'s, because the reasoning is the same one: the pool has to
    // give up before the request deadline does, or a database outage reaches the caller as an
    // empty 408 with nothing in it to act on.
    //
    // The anchor carries the skeleton's `5` rather than matching the declaration loosely: an
    // edit that cannot see the value it is asserting against is an edit that would splice
    // `3 < REQUEST` into a service whose REQUEST is 1 and call it done. A product that has
    // already retuned the deadline gets a refusal naming this line instead.
    let acquire = "    /// Outer deadline for any request.\n\
        \x20   pub const REQUEST: Duration = Duration::from_secs(5);\n\
        \n\
        \x20   /// How long a handler waits for a pooled connection.\n\
        \x20   pub const DB_ACQUIRE: Duration = Duration::from_secs(3);\n\
        \n\
        \x20   const _: () = assert!(\n\
        \x20       DB_ACQUIRE.as_secs() < REQUEST.as_secs(),\n\
        \x20       \"the pool must fail before the request deadline, or a database outage is \\\n\
        \x20        reported as an empty 408 instead of an `internal` Connect error\"\n\
        \x20   );";

    let mut out = source.to_owned();
    for declaration in ["pub mod db;", "pub mod migrations;"] {
        out = rustedit::insert_mod(&out, declaration, "pub mod config;")?;
    }
    // Inserted last and as a block: `insert_mod` sorts on the whole declaration, and an
    // attribute above it is not something it knows how to place.
    rustedit::apply(
        &out,
        &[
            rustedit::Edit {
                what: "declare the entities module",
                find: "pub mod db;",
                replace: format!("pub mod db;\n{entities}"),
            },
            rustedit::Edit {
                what: "add the connection-acquisition timeout `db::connect` reads",
                find: "    /// Outer deadline for any request.\n    pub const REQUEST: Duration = Duration::from_secs(5);",
                replace: acquire.to_owned(),
            },
        ],
    )
}

/// `src/config.rs`: the `database` section and the type behind it.
///
/// The emitted text is the shape `services/api/src/config.rs` carries — two alternative
/// connection spellings, `_FILE`-friendly, and a validation that refuses both at once. It is
/// copied rather than shared because a service owns its settings type, and a shared type
/// would have to live in a crate that knows what a database is.
fn plan_config(source: &str) -> Result<String> {
    let settings = r#"/// How this service reaches its database.
#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseSettings {
    /// A complete connection URL, and the alternative to the structured fields below.
    ///
    /// This is the shape a managed database hands its consumer: `CloudNativePG`'s
    /// `<cluster>-app` secret carries the whole URL under `uri`, and a deployment mounts it
    /// or names it from `APP__DATABASE__URL_FILE` rather than reassembling it from parts that
    /// the operator may have changed.
    ///
    /// Set this OR the structured fields, never both — [`DatabaseSettings::validate`]
    /// refuses a configuration that does both, because the two shapes disagree about which
    /// host, role and password is in use and silently picking one would look like success.
    #[serde(default)]
    pub url: String,
    /// Structured connection fields, for a deployment where each part is set separately.
    ///
    /// Defaulted rather than required, so a URL-only configuration deserializes; absence is
    /// caught by [`DatabaseSettings::validate`], which names every field it needed and did
    /// not get.
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    /// Empty in the committed file on purpose — a password belongs in the environment
    /// (`APP__DATABASE__PASSWORD`, or `APP__DATABASE__PASSWORD_FILE` for a mounted secret),
    /// never in a file a pull request can read.
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub database_name: String,
    /// Postgres counts every pooled connection against its own `max_connections`,
    /// so this is a share of a global budget rather than a local tuning knob.
    pub max_connections: NonZeroU32,
}

impl DatabaseSettings {
    /// Refuse a configuration that names two databases.
    ///
    /// The rule is one shape or the other: a `url` excludes `host`, `port`, `username`,
    /// `password` and `database_name`, and a configuration with no `url` requires all of
    /// them. Called from `load`, so a bad configuration fails before a pool exists rather
    /// than at the first connection attempt.
    ///
    /// # Errors
    ///
    /// Returns [`settings::Error::Invalid`] naming the fields that are missing, or the
    /// fields that contradict `url`.
    pub fn validate(&self) -> Result<(), settings::Error> {
        let structured: Vec<&str> = [
            ("host", !self.host.is_empty()),
            ("port", self.port != 0),
            ("username", !self.username.is_empty()),
            ("database_name", !self.database_name.is_empty()),
        ]
        .into_iter()
        .filter_map(|(name, set)| set.then_some(name))
        .collect();

        if self.url.is_empty() {
            let missing: Vec<&str> = ["host", "port", "username", "database_name"]
                .into_iter()
                .filter(|name| !structured.contains(name))
                .collect();
            if !missing.is_empty() {
                return Err(settings::Error::Invalid(format!(
                    "database.url is unset and the structured fields are incomplete — set database.url, or set {}",
                    missing.join(", ")
                )));
            }
            return Ok(());
        }

        let mut contradicting = structured;
        if !self.password.is_empty() {
            contradicting.push("password");
        }
        if contradicting.is_empty() {
            Ok(())
        } else {
            Err(settings::Error::Invalid(format!(
                "database.url is set together with {} — a URL excludes the structured fields",
                contradicting.join(", ")
            )))
        }
    }

    /// Connection URL for this database, after [`Self::validate`].
    #[must_use]
    pub fn connection_url(&self) -> String {
        if !self.url.is_empty() {
            return self.url.clone();
        }
        format!(
            "postgres://{}:{}@{}:{}/{}",
            self.username, self.password, self.host, self.port, self.database_name
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApplicationSettings {"#;

    rustedit::apply(
        source,
        &[
            rustedit::Edit {
                what: "import NonZeroU32 for the pool size",
                find: "use serde::Deserialize;",
                replace: "use std::num::NonZeroU32;\n\nuse serde::Deserialize;".to_owned(),
            },
            rustedit::Edit {
                what: "add `database` to Settings",
                find: "    pub application: ApplicationSettings,",
                replace:
                    "    pub application: ApplicationSettings,\n    pub database: DatabaseSettings,"
                        .to_owned(),
            },
            rustedit::Edit {
                what: "add the DatabaseSettings type",
                find: "#[derive(Debug, Clone, Deserialize)]\npub struct ApplicationSettings {",
                replace: settings.to_owned(),
            },
            rustedit::Edit {
                what: "validate the database section after loading it",
                find: "pub fn load() -> Result<Settings, settings::Error> {\n    settings::load(CONFIG_NAME)\n}",
                replace: "pub fn load() -> Result<Settings, settings::Error> {\n    let settings: Settings = settings::load(CONFIG_NAME)?;\n    settings.database.validate()?;\n    Ok(settings)\n}"
                    .to_owned(),
            },
        ],
    )
}

/// `src/main.rs`: connect, migrate on boot, and a `migrate` subcommand.
fn plan_main(source: &str, snake: &str) -> Result<String> {
    let migrate_on_boot = "            // Applied before the socket is served, under an advisory lock, so a rollout\n\
        \x20           // with N replicas has one migrating and the rest waiting.\n\
        \x20           db::migrate_with_lock::<Migrator>(&db, &settings.database, MIGRATION_LOCK_KEY)\n\
        \x20               .await\n\
        \x20               .context(\"migrations failed\")?;\n\n";

    rustedit::apply(
        source,
        &[
            rustedit::Edit {
                what: "import the database module and the migrator",
                // Two lines, not `use {snake}::{config, startup::App};` — and the difference
                // is the whole reason this edit used to refuse every service it was pointed
                // at. The skeleton is written merged, but nothing ever sees it that way:
                // `cargo xtask new service` rustfmts what it renders, `imports_granularity =
                // "Module"` splits the merge, and the file on disk has one line per module.
                // The anchor is therefore the *rendered* spelling, which the skeleton now
                // carries too so that `every_edit_matches_the_service_skeleton` is reading
                // the real thing. Adjacency is safe whatever the service is called: no crate
                // sorts between two imports from the same one.
                find: &format!("use {snake}::config;\nuse {snake}::startup::App;"),
                replace: format!(
                    "use {snake}::config;\n\
                     use {snake}::db::{{self, MIGRATION_LOCK_KEY}};\n\
                     use {snake}::migrations::Migrator;\n\
                     use {snake}::startup::App;"
                ),
            },
            rustedit::Edit {
                what: "add the `migrate` subcommand",
                find: "    /// Serve requests (the default).\n    Serve,\n}",
                replace: "    /// Serve requests (the default). Applies pending migrations first, under an\n\
                    \x20   /// advisory lock so concurrent replicas do not race.\n\
                    \x20   Serve,\n\
                    \x20   /// Apply pending migrations and exit, without serving.\n\
                    \x20   Migrate,\n}"
                    .to_owned(),
            },
            rustedit::Edit {
                what: "open the connection pool",
                find: "    telemetry::install_panic_hook();",
                replace: "    telemetry::install_panic_hook();\n\n\
                    \x20   let db = db::connect(&settings.database).context(\"could not create the database pool\")?;"
                    .to_owned(),
            },
            rustedit::Edit {
                what: "handle the `migrate` subcommand",
                find: "    match cli.command.unwrap_or(Command::Serve) {",
                replace: "    match cli.command.unwrap_or(Command::Serve) {\n\
                     \x20       Command::Migrate => {\n\
                     \x20           db::migrate_with_lock::<Migrator>(&db, &settings.database, MIGRATION_LOCK_KEY)\n\
                     \x20               .await\n\
                     \x20               .context(\"migrations failed\")?;\n\
                     \x20       }"
                    .to_owned(),
            },
            rustedit::Edit {
                what: "migrate before serving",
                find: "        Command::Serve => {\n",
                replace: format!("        Command::Serve => {{\n{migrate_on_boot}"),
            },
            rustedit::Edit {
                what: "hand the connection to the application",
                find: "App::build(&settings, verifier)",
                replace: "App::build(&settings, db, verifier)".to_owned(),
            },
        ],
    )
}

/// `src/startup.rs`: thread the connection from `build` to `register_services`.
///
/// `register_services` takes it even though nothing reads it yet, and says so in a comment.
/// The alternative — adding the parameter with the first database-backed feature — changes
/// three signatures at the moment someone is thinking about a feature, and the `let _`
/// below is a cheaper and more legible placeholder than that churn.
fn plan_startup(source: &str) -> Result<String> {
    rustedit::apply(
        source,
        &[
            rustedit::Edit {
                what: "import DatabaseConnection",
                find: "use identity::{IdentityLayer, VerifyCredential};",
                replace: "use identity::{IdentityLayer, VerifyCredential};\nuse sea_orm::DatabaseConnection;"
                    .to_owned(),
            },
            rustedit::Edit {
                what: "take a connection in App::build",
                find: "    pub async fn build(\n        settings: &Settings,\n        verifier: Arc<dyn VerifyCredential>,",
                replace: "    pub async fn build(\n        settings: &Settings,\n        db: DatabaseConnection,\n        verifier: Arc<dyn VerifyCredential>,"
                    .to_owned(),
            },
            rustedit::Edit {
                what: "pass the connection to build_router",
                find: "            router: build_router(\n                verifier,",
                replace: "            router: build_router(\n                db,\n                verifier,"
                    .to_owned(),
            },
            rustedit::Edit {
                what: "take a connection in build_router",
                find: "pub fn build_router(\n    verifier: Arc<dyn VerifyCredential>,\n    reflection: Reflection,\n    web: Web,\n) -> axum::Router {",
                replace: "pub fn build_router(\n    db: DatabaseConnection,\n    verifier: Arc<dyn VerifyCredential>,\n    reflection: Reflection,\n    web: Web,\n) -> axum::Router {"
                    .to_owned(),
            },
            rustedit::Edit {
                what: "pass the connection to register_services",
                find: "let registered = register_services(Router::new());",
                replace: "let registered = register_services(Router::new(), db);".to_owned(),
            },
            rustedit::Edit {
                what: "take a connection in register_services",
                find: "pub fn register_services(router: Router) -> Router {\n    router",
                replace: "pub fn register_services(router: Router, db: DatabaseConnection) -> Router {\n\
                    \x20   // Threaded in for the features that need one. Until the first does, nothing\n\
                    \x20   // reads it — the binding keeps the signature stable so adding a\n\
                    \x20   // database-backed feature is a line here rather than a change through three\n\
                    \x20   // functions. `cargo xtask new-feature` removes this when it adds that line.\n\
                    \x20   let _ = &db;\n\n\
                    \x20   router"
                    .to_owned(),
            },
        ],
    )
}

/// `tests/<name>/helpers.rs`: a pool that is never contacted, for the router the suite builds.
///
/// The point of the whole edit is that the integration tests keep running with **no Postgres
/// anywhere**, which is what lets them stay in `just ci` after a service gains a schema. A
/// sea-orm pool built with `connect_lazy_with` dials nothing until a query is issued, so every
/// route that does not touch the database is still testable — and one that does now fails
/// loudly against port 1 instead of quietly finding a real database on 5432.
///
/// `services/api/tests/api/helpers.rs` is the worked version of this, and deliberately so: the
/// shape a converted service gets is the shape the example service already has.
fn plan_test_helpers(source: &str, snake: &str) -> Result<String> {
    let dead_pool = "/// A pool pointed at a database that is never contacted.\n\
         ///\n\
         /// The pool is lazy, so nothing connects until a handler issues a query. That makes\n\
         /// every route which does not touch the database testable with no Postgres anywhere,\n\
         /// which is what keeps this suite in `just ci`. Split out of [`router`] because the\n\
         /// routing-table test in `auth.rs` needs the same pool without a router around it.\n\
         pub fn dead_pool() -> sea_orm::DatabaseConnection {\n\
         \x20   let settings = DatabaseSettings {\n\
         \x20       // The structured shape, which is what a development checkout uses; a pool\n\
         \x20       // whose URL is assembled from parts is this one.\n\
         \x20       url: String::new(),\n\
         \x20       host: \"127.0.0.1\".to_owned(),\n\
         \x20       // Port 1 is never a database. Chosen so a test that accidentally depends\n\
         \x20       // on one fails loudly instead of quietly using a real one.\n\
         \x20       port: 1,\n\
         \x20       username: \"unused\".to_owned(),\n\
         \x20       password: String::new(),\n\
         \x20       database_name: \"unused\".to_owned(),\n\
         \x20       max_connections: NonZeroU32::new(1).expect(\"1 is non-zero\"),\n\
         \x20   };\n\
         \x20   db::connect(&settings).expect(\"the connection URL should be valid\")\n\
         }\n\n";

    rustedit::apply(
        source,
        &[
            rustedit::Edit {
                what: "import NonZeroU32 for the pool size",
                find: "use std::sync::Arc;",
                replace: "use std::num::NonZeroU32;\nuse std::sync::Arc;".to_owned(),
            },
            rustedit::Edit {
                // Anchored on the service's own `startup` import rather than on a line number
                // in the import block: where the service's crate sorts among the externals
                // depends on what it is called, but `config` and `db` always sort before
                // `startup` and nothing can come between two imports from one crate.
                what: "import the settings type and the connector",
                find: &format!("use {snake}::startup::{{Reflection, Web}};"),
                replace: format!(
                    "use {snake}::config::DatabaseSettings;\n\
                     use {snake}::db;\n\
                     use {snake}::startup::{{Reflection, Web}};"
                ),
            },
            rustedit::Edit {
                what: "add the dead pool",
                find: "/// The real router,",
                replace: format!("{dead_pool}/// The real router,"),
            },
            // The two call sites, by their arguments alone. `build_router(..)` is reached
            // through a path whose length depends on the service's name, so rustfmt may have
            // wrapped the call across lines — the argument list has not moved either way.
            // The one call site. The helpers assemble every constructor through `build`, and
            // that function's argument list is short enough that rustfmt keeps it on one line
            // whatever the service is called — three call sites would be three anchors, and the
            // one rustfmt wrapped would keep the stateless signature.
            //
            // This edit is the one no fast gate can catch. `templates/` is outside every
            // workspace glob, so `just rust-test` never compiles a converted service's tests at
            // all; a call left on the stateless signature fails only in a scaffolded product,
            // months later and with an error naming the wrong file. `check-template` runs the
            // converted suite, and this anchor is why it stays green instead of being a note in
            // a README.
            rustedit::Edit {
                what: "give the router a connection",
                find: "build_router(verifier, reflection, Web::Absent)",
                replace: "build_router(dead_pool(), verifier, reflection, Web::Absent)".to_owned(),
            },
        ],
    )
}

/// `tests/<name>/auth.rs`: the routing-table test builds its own router.
///
/// It calls `register_services` directly — that is the point of it, since the classification
/// has to be checked against the real registration rather than against a list someone keeps
/// up to date — so it is the one test outside `helpers.rs` that this command's signature
/// change reaches. Only the call and the import move; the PUBLIC/PROTECTED lists are the
/// product's to fill in and nothing here touches them.
fn plan_test_auth(source: &str) -> Result<String> {
    rustedit::apply(
        source,
        &[
            rustedit::Edit {
                what: "import the dead pool",
                find: "use crate::helpers::{",
                replace: "use crate::helpers::{\n    dead_pool,".to_owned(),
            },
            rustedit::Edit {
                what: "give the routing table a connection",
                find: "register_services(Router::new())",
                replace: "register_services(Router::new(), dead_pool())".to_owned(),
            },
        ],
    )
}

/// `tests/<name>/reflection.rs`: the configuration literal the gating test deserialises.
///
/// `Settings.database` is required after this command — that is its point, and making it an
/// `Option` was rejected precisely so a service with a schema cannot configure itself out of
/// one. The reflection suite builds a `Settings` from a JSON literal that predates the field,
/// so `a_configuration_with_no_reflection_section_does_not_serve_it` fails with
/// "missing field `database`" on every converted service — and it fails only at *runtime*.
/// This is the file RT-22's probe could not catch: the probe compiled the converted suite and
/// did not run it, and
/// a required field spliced into `Settings` compiles against a stale literal whatever the probe
/// does with the binary. Running the suite is what makes the omission visible; editing this
/// file is what makes the suite pass once it runs.
///
/// `services/api/tests/api/reflection.rs` is the worked version of this, and deliberately so —
/// the same literal with the same `"database"` block, updated by hand when the database landed
/// there. Its username and database name are `unused` because nothing ever connects: the test
/// deserialises, it does not dial.
fn plan_test_reflection(source: &str) -> Result<String> {
    let literal = "const WITHOUT_REFLECTION: &str = r#\"{\n        \"application\": { \"host\": \"127.0.0.1\", \"port\": 0 },\n        \"database\": {\n            \"host\": \"127.0.0.1\",\n            \"port\": 5432,\n            \"username\": \"unused\",\n            \"database_name\": \"unused\",\n            \"max_connections\": 1\n        }\n    }\"#;";

    rustedit::apply(
        source,
        &[rustedit::Edit {
            what: "give the reflection test's configuration literal the database section the converted Settings requires",
            find: r##"const WITHOUT_REFLECTION: &str = r#"{ "application": { "host": "127.0.0.1", "port": 0 } }"#;"##,
            replace: literal.to_owned(),
        }],
    )
}

/// `Cargo.toml`: the two sea-orm crates, from the workspace.
fn plan_manifest(source: &str) -> Result<String> {
    super::add_dependencies(source, &["sea-orm", "sea-orm-migration"])
}

/// The keys the BASE `[database]` section carries: the pool size, and nothing else.
///
/// `password` is deliberately absent: `DatabaseSettings` carries it `#[serde(default)]`, so a
/// section without one starts fine — the password belongs in the environment anyway. The
/// LOCATION keys are not here either, and that is the fix this split exists for: the base file
/// is what the image bakes, so a location committed there is a structured field every
/// deployment inherits, and a deployment that supplies a URL (a managed cluster's secret, a
/// peer-auth socket) would be refused for setting both shapes. The location belongs in the
/// `local` overlay, which devenv reads and the image does not bake.
const BASE_DATABASE_KEYS: [&str; 1] = ["max_connections"];

/// The keys the LOCAL overlay's `[database]` section carries: the development location.
const LOCAL_DATABASE_KEYS: [&str; 4] = ["host", "port", "username", "database_name"];

/// The base `[database]` section `plan_settings_toml` appends, spelled exactly as it writes it.
///
/// Its own function because a refusal has to quote it: the point of refusing is that a human
/// installs the same thing by hand, and they cannot do that from a description of it.
fn base_database_section() -> String {
    "[database]\n\
     # A share of the server's own `max_connections`, not a local tuning knob: every\n\
     # replica of this service multiplies it.\n\
     #\n\
     # No location here: the base file is baked into the image, and a deployment supplies the\n\
     # database in one shape — the structured fields or a `url` — through its environment.\n\
     max_connections = 5\n"
        .to_owned()
}

/// The local overlay's `[database]` section: the development location, and only that.
///
/// It lives in `config/<name>.local.toml`, which devenv runs with and the image does not bake —
/// so `devenv up` reaches the database it creates, and a deployment is free to supply a URL
/// without a committed structured field standing in the way.
fn local_database_section(service: &str) -> String {
    let database = service.replace('-', "_");
    format!(
        "[database]\n\
         host = \"127.0.0.1\"\n\
         port = 5432\n\
         username = \"{database}\"\n\
         # No password here on purpose: it belongs in `APP__DATABASE__PASSWORD`, never in a\n\
         # file a pull request can read.\n\
         database_name = \"{database}\"\n"
    )
}

/// `config/<name>.toml`: the committed half of the connection settings.
///
/// Three states, and only the middle one is a refusal:
///
/// 1. the file already holds the section this command writes — left byte for byte, which is the
///    behaviour RT-31 pinned in `a_file_that_already_has_what_the_generator_would_write_is_left_alone`;
/// 2. the file has a `database` table this command did not write — refused, and named;
/// 3. the file has no `database` table — the section is appended, as it always has been.
///
/// The discriminator between 1 and 2 is the **generated section itself**, not the string
/// `[database]`, and it is compared line by line rather than as bytes: a file saved without a
/// final newline is the same section, and a section with one extra key is not.
///
/// Strict on purpose: a section a human reordered, or renamed a key in, or filled with their own
/// values is not this command's section, and appending a second table of the same name beside it
/// would give a file that does not parse. What the strictness costs is a refusal on a section that
/// is merely arranged differently — which is why the message quotes both what the file has and
/// what was wanted.
///
/// State 2 is refused rather than repaired because `plan_config` adds a **required**
/// `pub database: DatabaseSettings`. A section missing `max_connections` leaves a service that
/// compiles, starts, and then dies on a deserialization error pointing at a file this command
/// decided not to touch — and the old guard reported exactly that outcome as `✓ wired into:`.
///
/// # Errors
///
/// Returns an error if the file does not parse as TOML, or if it carries a `database` table this
/// command did not write.
fn plan_settings_toml(source: &str, _service: &str) -> Result<String> {
    plan_database_section(
        source,
        &base_database_section(),
        &BASE_DATABASE_KEYS,
        "settings",
    )
}

/// `config/<name>.local.toml`: the local development location.
///
/// The same three states as [`plan_settings_toml`], for the same reason, against the overlay
/// devenv runs with. That file already carries a table of its own (`[reflection]`, and in the
/// api example telemetry), so the section is appended to a file with content.
///
/// # Errors
///
/// Same as [`plan_settings_toml`].
fn plan_local_settings_toml(source: &str, service: &str) -> Result<String> {
    plan_database_section(
        source,
        &local_database_section(service),
        &LOCAL_DATABASE_KEYS,
        "local",
    )
}

/// The shared body of the two planners above.
///
/// `which` names the half a refusal is about, because there are two files now and "this file
/// already has a `database` table this command did not write" does not say which one the reader
/// is looking at.
fn plan_database_section(
    source: &str,
    section: &str,
    keys: &[&str],
    which: &str,
) -> Result<String> {
    let already_ours = toml_section(source, "database")
        .is_some_and(|text| block_lines(&text) == block_lines(section));
    if already_ours {
        return Ok(source.to_owned());
    }

    // Parsed rather than substring-searched, for two reasons: `[database]` inside a comment is
    // not a section, and the keys a section actually carries are knowable only by reading it.
    let document: toml_edit::DocumentMut = source
        .parse()
        .context("it does not parse as TOML, so there is nothing to append to")?;

    // `get` rather than `as_table`: a `database` written as an inline table — `database = { host =
    // "…" }` — is a `database` too, and appending a `[database]` section beside it would give a
    // file with the same key twice, which does not parse at all.
    let Some(item) = document.get("database") else {
        return Ok(format!("{}\n{}", source.trim_end(), section));
    };

    let missing: Vec<&str> = keys
        .iter()
        .copied()
        .filter(|key| !carries_key(item, key))
        .collect();

    // Named key by key, because "this section is not mine" is not something a reader can act on
    // while "this section is missing `max_connections`" is one line they can write without
    // knowing anything about this generator.
    let diagnosis = if missing.is_empty() {
        "It is not the section this command writes — the keys, the values or the order are \
         someone else's — and this command does not edit a section a person wrote."
            .to_owned()
    } else {
        let names = missing
            .iter()
            .map(|key| format!("`{key}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let required = keys
            .iter()
            .map(|key| format!("`{key}`"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "It is missing {names} of the keys `DatabaseSettings` requires ({required}). A \
             section that does not carry all of them compiles, starts, and then fails on a \
             deserialization error pointing at this file."
        )
    };

    let mut paragraphs: Vec<String> = vec![format!(
        "this file already has a `database` table this command did not write (the {which} half \
         of the split: the base file carries `max_connections`, the local overlay carries the \
         development location)."
    )];
    // Quote what is there when the file is legible enough to have a section to quote; an inline
    // table has no `[database]` line to find, so it is quoted as TOML renders it.
    let quoted = toml_section(source, "database").or_else(|| {
        document
            .get("database")
            .map(|item| item.to_string().trim_end().to_owned())
    });
    if let Some(text) = quoted {
        paragraphs.push(format!("What the file has:\n\n{text}"));
    }
    paragraphs.push(diagnosis);
    paragraphs.push(format!(
        "Add the keys yourself, or delete the section and run this again, which writes:\n\n{}",
        indent_lines(section)
    ));

    anyhow::bail!("{}", paragraphs.join("\n\n"));
}

/// Whether a `database` item carries one of the keys `DatabaseSettings` requires.
///
/// Both spellings a settings file can use for the same key: a `[database]` section, and an inline
/// table written as `database = { … }`.
fn carries_key(item: &toml_edit::Item, key: &str) -> bool {
    item.as_table().is_some_and(|table| table.contains_key(key))
        || item
            .as_inline_table()
            .is_some_and(|table| table.get(key).is_some())
}

/// The `[name]` section as it is written in `source`.
///
/// Two jobs: it is what state one compares against, and it is what a refusal quotes. Line
/// scanning, stopping at the next table header unless it is a sub-table of this one. It is
/// deliberately not authoritative about whether a `database` exists — the parse above answers
/// that, and answers it for the inline spelling too — so when the header cannot be found the
/// refusal omits the quote rather than guessing at what the file looks like.
fn toml_section(source: &str, name: &str) -> Option<String> {
    let header = format!("[{name}]");
    let subtable = format!("[{name}.");
    let lines: Vec<&str> = source.lines().collect();
    let start = lines.iter().position(|line| line.trim() == header)?;
    let rest = &lines[start + 1..];
    let end = rest
        .iter()
        .position(|line| {
            line.trim_start().starts_with('[') && !line.trim_start().starts_with(&subtable)
        })
        .unwrap_or(rest.len());
    Some(
        lines[start..start + 1 + end]
            .join("\n")
            .trim_end()
            .to_owned(),
    )
}

/// Indent a block by four spaces, so that quoting it inside an error message reads as a block.
fn indent_lines(block: &str) -> String {
    block
        .trim_end()
        .lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("    {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One of the two recipes `plan_justfile` appends.
struct JustfileRecipe {
    /// The name, which is what a `Justfile` may already define and what a refusal names.
    name: &'static str,
    /// The comment written above the recipe.
    comment: &'static str,
    /// The recipe itself: its attribute line if it has one, its definition line, and its body.
    ///
    /// No comment, because a comment is not part of a recipe — a `migrate:` someone wrote by hand
    /// without the comment above it is still this recipe, and refusing that would be refusing a
    /// spelling rather than a conflict. An attribute line *is* part of it: `[script('bash')]`
    /// decides which shell the recipe runs in, so an `entities:` without one is a different recipe.
    body: String,
}

/// The two recipes, in the order they are appended.
///
/// Its own function because a refusal has to quote them: what the refusal points at is "rename
/// yours and run this again, or install these two by hand", and nobody can install two recipes
/// from a description of them.
fn justfile_recipes(service: &str) -> Vec<JustfileRecipe> {
    let database = service.replace('-', "_");
    vec![
        JustfileRecipe {
            name: "migrate",
            comment: "# apply pending migrations and exit",
            body: "migrate:\n\x20   @just rust::run {{ pkg }} -- migrate".to_string(),
        },
        JustfileRecipe {
            name: "entities",
            comment: "# regenerate SeaORM entities from the live schema (migrate first — the generator\n\
                      # reads the database, not the migration source)",
            body: format!(
                "[script('bash')]\n\
                 entities:\n\
                 \x20   set -euo pipefail\n\
                 \x20   echo '{{{{ style(\"command\") }}}}entities:{{{{ NORMAL }}}}'\n\
                 \x20   sea-orm-cli generate entity \\\n\
                 \x20       --database-url \"postgres://{database}:${{APP__DATABASE__PASSWORD:-{database}}}@127.0.0.1:5432/{database}\" \\\n\
                 \x20       --output-dir src/entities \\\n\
                 \x20       --impl-active-model-behavior=false \\\n\
                 \x20       --with-serde none"
            ),
        },
    ]
}

/// The text `plan_justfile` appends for these recipes: each one's comment and body, separated by
/// a blank line, which is how a `Justfile` separates recipes.
fn justfile_blocks(recipes: &[&JustfileRecipe]) -> String {
    recipes
        .iter()
        .map(|recipe| format!("{}\n{}\n", recipe.comment, recipe.body))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a `Justfile` does with one of the two recipe names.
enum RecipeState {
    /// Nothing defines the name.
    Absent,
    /// The name is defined, and it is the recipe this command installs.
    Installed,
    /// The name is defined, and it is something else. The text is what the file has.
    Foreign(String),
}

/// `Justfile`: the two recipes a schema needs.
///
/// Three states per recipe — installed, foreign, absent — and only the middle one is a refusal:
///
/// 1. the file already defines both recipes as this command writes them — left byte for byte,
///    which is the behaviour RT-31 pinned in `a_file_that_already_has_what_the_generator_would_write_is_left_alone`;
/// 2. the file defines either name as something else — refused, and both names it defines are
///    named, because the conversion installs nothing and a reader has to know which of the two
///    they have to move;
/// 3. the file defines neither — both appended, as they always have been.
///
/// State 3 is per recipe rather than all-or-nothing, which is what makes the whole thing
/// idempotent in the case a refusal message invites: someone follows the message, installs one
/// recipe by hand, and runs the command again. The recipe they wrote is left alone and the other
/// one is appended. A guard keyed on `migrate` alone — which is what this used to be — would
/// instead skip both.
///
/// The guard here used to be `source.contains("\nmigrate:")`, and it was wrong four ways at once,
/// every one of them silent:
///
/// - one guard covered **both** recipes, so a `Justfile` that already had `entities:` and no
///   `migrate:` skipped the append and got neither recipe;
/// - the leading newline meant a `Justfile` whose **first line** is `migrate:` did not match at
///   all, so both recipes were appended and `just` then refused the file for a duplicate recipe;
/// - a substring match cannot tell a recipe from a variable assignment, so `migrate := "..."` — a
///   `Justfile` that is perfectly fine — was the one spelling a name-based rule would refuse;
/// - and when it did match, it matched a *name* rather than the block, so a `migrate:` that
///   deploys something was accepted in silence and reported as `✓ wired into:` with neither
///   recipe installed.
///
/// # Errors
///
/// Returns an error if the `Justfile` defines `migrate` or `entities` as anything other than the
/// two recipes this command installs.
fn plan_justfile(source: &str, service: &str) -> Result<String> {
    let recipes = justfile_recipes(service);
    let states: Vec<(&JustfileRecipe, RecipeState)> = recipes
        .iter()
        .map(|recipe| (recipe, recipe_state(source, recipe)))
        .collect();

    let conflicts: Vec<(&JustfileRecipe, &str)> = states
        .iter()
        .filter_map(|(recipe, state)| match state {
            RecipeState::Foreign(text) => Some((*recipe, text.as_str())),
            RecipeState::Absent | RecipeState::Installed => None,
        })
        .collect();

    if conflicts.is_empty() {
        let missing: Vec<&JustfileRecipe> = states
            .iter()
            .filter_map(|(recipe, state)| match state {
                RecipeState::Absent => Some(*recipe),
                RecipeState::Foreign(_) | RecipeState::Installed => None,
            })
            .collect();
        return if missing.is_empty() {
            Ok(source.to_owned())
        } else {
            Ok(format!(
                "{}\n{}",
                source.trim_end(),
                justfile_blocks(&missing)
            ))
        };
    }
    let names = conflicts
        .iter()
        .map(|(recipe, _)| format!("`{}:`", recipe.name))
        .collect::<Vec<_>>()
        .join(" and ");
    let first = if conflicts.len() == 1 {
        format!(
            "this file already defines {names}, and that is not the recipe this command installs."
        )
    } else {
        format!("this file already defines {names}, and neither is a recipe this command installs.")
    };
    let offending = conflicts
        .iter()
        .map(|(_, text)| indent_lines(text))
        .collect::<Vec<_>>()
        .join("\n\n");

    let all: Vec<&JustfileRecipe> = recipes.iter().collect();
    let plain = conflicts
        .iter()
        .map(|(recipe, _)| recipe.name)
        .collect::<Vec<_>>()
        .join(" or ");

    let mut paragraphs = vec![first, format!("What the file has:\n\n{offending}")];
    paragraphs.push(format!(
        "`add-database` installs two recipes, `migrate` and `entities`, and it installs them all \
         or not at all. Appending a recipe beside one that already has the name gives a `Justfile` \
         `just` refuses to parse for a duplicate recipe; skipping it leaves the name taken, and a \
         `just services/{service}/{plain}` that means something else is worse than one that does \
         not exist."
    ));
    paragraphs.push(format!(
        "Rename what is yours to what it does — a `migrate:` that deploys is a `deploy:` — and \
         run this again, or install the two recipes by hand:\n\n{}",
        indent_lines(&justfile_blocks(&all))
    ));

    anyhow::bail!("{}", paragraphs.join("\n\n"));
}

/// Which of the three states a `Justfile` is in for one recipe.
fn recipe_state(source: &str, recipe: &JustfileRecipe) -> RecipeState {
    match justfile_recipe(source, recipe.name) {
        None => RecipeState::Absent,
        Some(text) if block_lines(&text) == block_lines(&recipe.body) => RecipeState::Installed,
        Some(text) => RecipeState::Foreign(text),
    }
}

/// The recipe named `name` as the file writes it — its attribute lines, its definition line and
/// its body — if the file defines one at the top level.
fn justfile_recipe(source: &str, name: &str) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    let start = lines.iter().position(|line| defines_recipe(line, name))?;
    // An attribute line belongs to the recipe below it, and `[script('bash')]` is what makes the
    // generated `entities` recipe a bash recipe: leaving it out of the block would call a recipe
    // that runs in `sh` the same recipe.
    let attributes = lines[..start]
        .iter()
        .rev()
        .take_while(|line| is_recipe_attribute(line))
        .count();
    let body = lines[start + 1..]
        .iter()
        .take_while(|line| line.starts_with([' ', '\t']))
        .count();
    Some(lines[start - attributes..start + 1 + body].join("\n"))
}

/// An attribute line, like the `[script('bash')]` the generated `entities` recipe carries.
fn is_recipe_attribute(line: &str) -> bool {
    let text = line.trim();
    line.starts_with('[') && text.ends_with(']') && text.len() > 2
}

/// The lines of a block, with trailing whitespace and trailing blank lines dropped.
///
/// A byte comparison would call these two the same file and they are not the same recipe, and it
/// would call these two different files and they are the same recipe: a `Justfile` saved without
/// a final newline, or with a trailing space on a recipe line, is the file it was before. What a
/// recipe *is* is its lines, so the lines are what gets compared.
fn block_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| line.trim_end().to_owned())
        .collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// Whether `line` defines a just recipe named `name`.
///
/// Line scanning at column 0, which is what a substring search cannot be:
///
/// - an indented line is a recipe **body**, so a `just migrate` call inside another recipe is not
///   a definition;
/// - `migrate := "..."` is a variable **assignment**, and a rule of "starts with `migrate`,
///   contains a colon" refuses a `Justfile` that is fine;
/// - `migrate: build` (dependencies) and `migrate arg:` (parameters) are both definitions, and so
///   are `@migrate:`, where `@` makes the recipe quiet, and `-migrate:`, where `-` is just's
///   ignore-errors modifier;
/// - an attribute line — the `entities` recipe this command installs carries `[script('bash')]` —
///   is not a definition, and does not hide the definition below it.
fn defines_recipe(line: &str, name: &str) -> bool {
    // Column 0: the first character is not whitespace.
    let Some(first) = line.chars().next() else {
        return false;
    };
    if first.is_whitespace() {
        return false;
    }
    // `@` and `-` both lead a recipe name without being part of it: `@migrate:` is a recipe
    // named `migrate` whose every line is quiet, and `-` is the ignore-errors modifier. The `@`
    // spelling was missed once, and missing it is this ticket's own defect in another costume —
    // a second `migrate:` appended beside one `just` already knows, and `just` then refuses the
    // whole file for a duplicate recipe rather than the command refusing anything.
    let rest = line.trim_start_matches(['@', '-']);
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(rest.len());
    if &rest[..end] != name {
        return false;
    }
    let after = &rest[end..];
    // The first colon outside quotes is the one that decides what the line is. A colon inside a
    // quoted value is part of the value: `migrate = "http://127.0.0.1:8080"` has one, and is not
    // a recipe.
    let Some(colon) = find_recipe_colon(after) else {
        return false;
    };
    // `name := value` is an assignment, which is the spelling the ticket names as the false
    // positive a name-based rule has to exclude.
    if after[colon + 1..].starts_with('=') {
        return false;
    }
    // A colon straight after the name is a recipe with dependencies (`migrate: build`); a colon
    // after a whitespace-separated parameter list is a recipe with parameters (`migrate arg:`).
    colon == 0 || after[..colon].starts_with([' ', '\t'])
}

/// The index of the first `:` in `text` that is not inside a quoted string.
fn find_recipe_colon(text: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (index, c) in text.char_indices() {
        if let Some(open) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == open {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            ':' => return Some(index),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo;

    /// The stateless shapes these edits are written against, as `templates/service` renders
    /// them. Kept in the test rather than read from disk so a failure says "the edit no
    /// longer matches the template" at the line that has to change.
    const STATELESS_CONFIG: &str = "use serde::Deserialize;\n\n\
        #[derive(Debug, Clone, Deserialize)]\n\
        pub struct Settings {\n\
        \x20   pub application: ApplicationSettings,\n\
        }\n\n\
        #[derive(Debug, Clone, Deserialize)]\n\
        pub struct ApplicationSettings {\n\
        \x20   pub host: String,\n\
        }\n\n\
        /// Load settings for this service.\n\
        pub fn load() -> Result<Settings, settings::Error> {\n\
        \x20   settings::load(CONFIG_NAME)\n\
        }\n";

    #[test]
    fn the_config_grows_a_database_section() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let out = plan_config(STATELESS_CONFIG).expect("the anchors match");
        assert!(out.contains("pub database: DatabaseSettings,"), "{out}");
        assert!(out.contains("pub struct DatabaseSettings {"), "{out}");
        assert!(out.contains("use std::num::NonZeroU32;"), "{out}");
        assert!(
            out.contains("settings.database.validate()?;"),
            "the generated load must refuse a contradictory section: {out}"
        );
        // The type it was spliced above must still be there, exactly once.
        assert_eq!(
            out.matches("pub struct ApplicationSettings {").count(),
            1,
            "{out}"
        );
    }

    #[test]
    fn a_reshaped_config_is_refused_rather_than_half_edited() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let error = plan_config("// someone rewrote this file\n")
            .expect_err("a missing anchor must be refused");
        assert!(error.to_string().contains("could not"), "{error}");
    }

    #[test]
    fn the_manifest_gains_both_sea_orm_crates_once() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let manifest = "[package]\nname = \"demo\"\n\n[dependencies]\nanyhow.workspace = true\n";
        let out = plan_manifest(manifest).expect("a manifest with dependencies");
        assert!(out.contains("sea-orm.workspace = true"), "{out}");
        assert!(out.contains("sea-orm-migration.workspace = true"), "{out}");

        // Idempotent: running it twice must not duplicate an entry.
        let twice = plan_manifest(&out).expect("still a manifest");
        assert_eq!(twice.matches("sea-orm.workspace").count(), 1, "{twice}");
    }

    #[test]
    fn the_settings_file_gains_a_database_section_once() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let out = plan_settings_toml("[application]\nport = 5000\n", "api")
            .expect("a settings file with no [database] section");
        assert!(out.contains("[database]"), "{out}");
        assert!(out.contains("max_connections = 5"), "{out}");
        assert!(
            !out.contains("password ="),
            "the password stays in the environment: {out}"
        );
        // The split: the base file carries the pool size and NO location. A location committed
        // here is a structured field the image bakes, and a deployment supplying a URL would be
        // refused for setting both shapes — which is the bug this split exists to prevent.
        let parsed: toml::Value = toml::from_str(&out).expect("the written file is TOML");
        let database = parsed
            .get("database")
            .and_then(toml::Value::as_table)
            .expect("a [database] table");
        let mut keys: Vec<&str> = database.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["max_connections"],
            "the base file carries the pool size and no location: {out}"
        );
        assert_eq!(
            plan_settings_toml(&out, "api").expect("still a settings file"),
            out,
            "idempotent"
        );

        // The other half: the local overlay gains the location, and only the location.
        let local = plan_local_settings_toml("[reflection]\nenabled = true\n", "demo")
            .expect("a local overlay with no [database] section");
        let parsed: toml::Value = toml::from_str(&local).expect("the written overlay is TOML");
        let database = parsed
            .get("database")
            .and_then(toml::Value::as_table)
            .expect("a [database] table");
        let mut keys: Vec<&str> = database.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["database_name", "host", "port", "username"],
            "the local overlay carries the location and nothing else: {local}"
        );
        assert_eq!(database["username"].as_str(), Some("demo"));
        assert!(
            parsed.get("reflection").is_some(),
            "the overlay keeps its own tables: {local}"
        );
        assert_eq!(
            plan_local_settings_toml(&local, "demo").expect("still an overlay"),
            local,
            "idempotent"
        );
    }

    #[test]
    fn the_justfile_gains_both_recipes_once() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let justfile = "pkg := \"demo\"\n\nrun:\n    @just rust::run {{ pkg }} -- serve\n";
        let out = plan_justfile(justfile, "demo").expect("a Justfile with neither recipe");
        assert!(out.contains("\nmigrate:"), "{out}");
        assert!(out.contains("sea-orm-cli generate entity"), "{out}");
        assert_eq!(
            plan_justfile(&out, "demo").expect("still a Justfile"),
            out,
            "idempotent"
        );
    }

    #[test]
    fn a_variable_named_migrate_is_not_a_recipe() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // The false positive a name-based rule would have: `migrate := "..."` is a just variable
        // assignment, and a `Justfile` that only has one is a `Justfile` this command should
        // happily convert.
        let justfile = "pkg := \"demo\"\nmigrate := \"deploy the thing\"\n\nrun:\n    @just rust::run {{ pkg }} -- serve\n";
        let out = plan_justfile(justfile, "demo")
            .expect("an assignment is not a recipe, so there is no conflict");
        assert!(out.contains("\nmigrate:\n    @just rust::run"), "{out}");
        assert!(out.contains("\nentities:"), "{out}");
    }

    #[test]
    fn a_quiet_recipe_is_still_a_recipe() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // `@migrate:` is a recipe named `migrate`; the `@` suppresses echoing for all of its
        // lines. Checked against the `just` in this shell rather than assumed: it lists the
        // recipe as `migrate`, and a file carrying both spellings is refused with "recipe
        // `migrate` first defined on line 1 is redefined on line 4". A guard that reads the `@`
        // as part of the name appends the second one and hands `just` that file.
        for justfile in [
            "pkg := \"demo\"\n\n@migrate:\n    @echo not this command's recipe\n",
            "@migrate:\n    @echo first line, and quiet\n",
            "pkg := \"demo\"\n\n@entities:\n    @echo not this command's recipe\n",
        ] {
            let error = plan_justfile(justfile, "demo")
                .expect_err("a quiet recipe of that name is a conflict like any other");
            assert!(error.to_string().contains("not the recipe"), "{error}");
        }
    }

    #[test]
    fn a_recipe_with_dependencies_or_parameters_is_still_a_recipe() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // `migrate: build` is dependencies and `migrate arg:` is a parameter list. Both define
        // `migrate`, and a guard that only recognises the bare `migrate:` spelling would append a
        // second one and hand the file to `just`, which refuses to parse a duplicate recipe.
        for justfile in [
            "pkg := \"demo\"\n\nmigrate: build\n    @echo not this command's recipe\n",
            "pkg := \"demo\"\n\nmigrate target:\n    @echo not this command's recipe\n",
            "pkg := \"demo\"\n\n-entities:\n    @echo not this command's recipe\n",
        ] {
            let error = plan_justfile(justfile, "demo")
                .expect_err("a recipe of that name is a conflict, however it is spelled");
            assert!(error.to_string().contains("not the recipe"), "{error}");
        }
    }

    #[test]
    fn a_recipe_the_file_already_has_is_left_alone_and_the_other_one_is_appended() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // The state a refusal message invites someone into: they install `migrate` by hand from
        // the quoted block, and run the command again. The recipe they wrote stays, the one that
        // is missing is appended. A guard keyed on `migrate` alone — which is what this used to
        // be — would have skipped both and left a service with a `migrate` recipe and no way to
        // regenerate its entities.
        let installed = justfile_recipes("demo")
            .into_iter()
            .find(|recipe| recipe.name == "migrate")
            .expect("the migrate recipe");
        let justfile = format!("pkg := \"demo\"\n\n{}\n", installed.body);

        let out = plan_justfile(&justfile, "demo")
            .expect("only the entities recipe is missing from this file");

        assert_eq!(out.matches("migrate:").count(), 1, "{out}");
        assert!(
            out.contains("@just rust::run {{ pkg }} -- migrate"),
            "{out}"
        );
        assert!(out.contains("[script('bash')]"), "{out}");
        assert!(out.contains("sea-orm-cli generate entity"), "{out}");
        assert_eq!(
            plan_justfile(&out, "demo").expect("both recipes are there now"),
            out,
            "idempotent"
        );
    }

    #[test]
    fn a_file_saved_without_a_final_newline_is_the_same_file() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // What a byte comparison would get wrong: the last line of a file often has no newline,
        // and a recipe with a trailing space on it is the same recipe. Comparing lines is what
        // makes state one state one.
        let justfile =
            plan_justfile("pkg := \"demo\"\n", "demo").expect("a Justfile with neither recipe");
        let saved_without_newline = justfile.trim_end().to_owned();
        assert_eq!(
            plan_justfile(&saved_without_newline, "demo")
                .expect("the same Justfile, saved differently"),
            saved_without_newline
        );

        let settings = plan_settings_toml("[application]\nport = 5000\n", "api")
            .expect("a settings file with no [database] section");
        let saved_without_newline = settings.trim_end().to_owned();
        assert_eq!(
            plan_settings_toml(&saved_without_newline, "api")
                .expect("the same settings file, saved differently"),
            saved_without_newline
        );
    }

    #[test]
    fn a_database_written_as_an_inline_table_is_not_a_section_to_append_beside() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // `database = { host = "…" }` is a `database` too — a root-level one, which means it has
        // to be written before any table header, since a `database = …` under `[application]` is
        // `application.database` and nothing to do with the pool. A guard that only looked for a
        // `[database]` header would append a section beside it, and a TOML file with the same key
        // twice does not parse — so the service would not start, and the command would have
        // written the file that stops it.
        let settings = "database = { host = \"127.0.0.1\", port = 5432, username = \"api\", database_name = \"api\", max_connections = 5 }\n\n[application]\nport = 5000\n";
        let error = plan_settings_toml(settings, "api")
            .expect_err("an inline table is the same key as a section");
        let message = format!("{error:#}");
        assert!(message.contains("did not write"), "{message}");
        assert!(message.contains("host = \"127.0.0.1\""), "{message}");
    }

    #[test]
    fn a_comment_mentioning_a_section_is_not_that_section() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // The old guard was `source.contains("[database]")`, which cannot tell a table from a
        // sentence about one — so this file, which has no `[database]` table at all, used to be
        // left alone and reported as wired in.
        let source = "# A [database] section belongs to the service, not to a shared crate.\n[application]\nport = 5000\n";
        let out = plan_settings_toml(source, "api").expect("a comment is not a table");
        assert!(out.contains("max_connections = 5"), "{out}");
        assert!(out.contains("# A [database] section belongs"), "{out}");
        assert_eq!(
            out.matches("host = \"127.0.0.1\"").count(),
            0,
            "the base file carries no location: {out}"
        );
    }

    #[test]
    fn every_edit_matches_the_service_skeleton() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // The anchors are written against what `just add service` renders, and nothing else
        // ties the two together: an edit to `templates/service` that moves one of them would
        // otherwise surface as `add-database` refusing, months later, on somebody's product.
        // Reading the template here makes that a failure in this repository instead.
        let template = repo::root().join("templates/service/services/__name__");
        let read = |name: &str| {
            std::fs::read_to_string(template.join("src").join(name))
                .unwrap_or_else(|error| panic!("reading the {name} skeleton: {error}"))
        };

        let lib = plan_lib(&read("lib.rs")).expect("lib.rs");
        assert!(lib.contains("pub mod db;"), "{lib}");
        assert!(lib.contains("pub mod entities;"), "{lib}");
        assert!(lib.contains("pub mod migrations;"), "{lib}");

        let config = plan_config(&read("config.rs")).expect("config.rs");
        assert!(
            config.contains("pub database: DatabaseSettings,"),
            "{config}"
        );

        let main = plan_main(&read("main.rs"), "__snake__").expect("main.rs");
        assert!(main.contains("Command::Migrate =>"), "{main}");
        assert!(
            main.contains("App::build(&settings, db, verifier)"),
            "{main}"
        );
        // Migrations run before the socket is served as well as on the explicit subcommand.
        assert_eq!(
            main.matches("migrate_with_lock::<Migrator>").count(),
            2,
            "{main}"
        );

        let startup = plan_startup(&read("startup.rs")).expect("startup.rs");
        assert!(
            startup.contains("pub fn register_services(router: Router, db: DatabaseConnection)"),
            "{startup}"
        );
        // Without the placeholder the parameter is unused, and this workspace denies
        // warnings — so a converted service would not compile until its first feature.
        assert!(startup.contains("let _ = &db;"), "{startup}");

        // The timeout `db.rs` reads. Installing the module without it is a service that does
        // not compile, which is what `just check-template` now catches — but it
        // catches it a minute and a proto run later than this does.
        let lib_with_timeout = plan_lib(&read("lib.rs")).expect("lib.rs");
        assert!(
            lib_with_timeout.contains("pub const DB_ACQUIRE: Duration"),
            "{lib_with_timeout}"
        );

        // The tests are edited too, and they are the half that compiles separately: a service
        // whose `src/` was converted and whose `tests/` was not builds perfectly and fails
        // `cargo test` with three signature errors.
        let test = |name: &str| {
            std::fs::read_to_string(template.join("tests/__name__").join(name))
                .unwrap_or_else(|error| panic!("reading the {name} test skeleton: {error}"))
        };

        let helpers = plan_test_helpers(&test("helpers.rs"), "__snake__").expect("helpers.rs");
        assert!(helpers.contains("pub fn dead_pool()"), "{helpers}");
        // One call site: every constructor goes through `build`, so the conversion has one
        // anchor and there is no per-constructor site rustfmt could have wrapped.
        assert_eq!(
            helpers
                .matches("build_router(dead_pool(), verifier, reflection, Web::Absent)")
                .count(),
            1,
            "the one call that assembles every router must take the pool: {helpers}"
        );
        // The third constructor exists as a constructor, not as a second `TestVerifier` call
        // that the anchor could not tell apart from the first two.
        assert!(
            helpers.contains("build(Arc::new(UnavailableVerifier), Reflection::Served)"),
            "the outage-shaped router must still exist and share the one call: {helpers}"
        );

        let auth = plan_test_auth(&test("auth.rs")).expect("auth.rs");
        assert!(
            auth.contains("register_services(Router::new(), dead_pool())"),
            "{auth}"
        );

        // The third test file `add-database` reaches — and the one whose breakage compiles.
        // `Settings` gains a required `database` field, this test deserialises a `Settings`
        // from a literal, and the assertion below is why the suite in the probe must be *run*:
        // a stale literal is invisible to `cargo check` and obvious to `cargo test`.
        let reflection = plan_test_reflection(&test("reflection.rs")).expect("reflection.rs");
        assert!(reflection.contains(r#""database": {"#), "{reflection}");
        assert!(
            reflection.contains(r#""max_connections": 1"#),
            "{reflection}"
        );
        assert!(
            !reflection.contains(r##"} }"#;"##),
            "the pre-database literal must be gone, not merely followed by a new one: {reflection}"
        );
    }

    // -----------------------------------------------------------------
    // RT-31: a conversion that refuses must not have written anything.
    //
    // Every test above asks a planner whether it produces the right text. None of them can
    // see the order the command writes in, and the order is the defect: `writes.commit` and
    // the seven splices land before `Cargo.toml`, `config/<name>.toml` and the `Justfile` are
    // read, so a service missing one of those three ends up with `src/db.rs` and a `lib.rs`
    // that does not declare it. The second run then refuses on the mess the first left,
    // which is the failure the header comment claims cannot happen.
    //
    // These drive `convert` against a rendered copy of the service skeleton in a temporary
    // directory. `Service::open` cannot be used for that: it resolves a name through
    // `repo::root()`, which is `env!("CARGO_MANIFEST_DIR")` baked in at compile time, so every
    // name resolves inside this repository. `convert` takes the `Service`, and all three of its
    // fields are public. The rendered skeleton and the tree snapshot live in `api::fixtures`,
    // shared with the three generators RT-38 converts — `new-migration` and `new-domain` need
    // this same fixture further down a chain of generators.
    // -----------------------------------------------------------------

    use crate::api::fixtures::{scaffolded_service, tree};

    #[test]
    fn a_scaffolded_service_converts() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // The control. Without it the two tests below could be passing because the fixture is
        // unusable rather than because the refusal is clean, and "it refused" is not by itself
        // evidence of anything.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());

        convert(&service).expect("the skeleton this repository ships must convert");

        assert!(service.src("db.rs").is_file(), "the schema plumbing landed");
        let manifest = std::fs::read_to_string(service.dir.join("Cargo.toml")).expect("manifest");
        assert!(manifest.contains("sea-orm.workspace"), "{manifest}");
        let settings =
            std::fs::read_to_string(service.dir.join("config/demo.toml")).expect("settings");
        assert!(settings.contains("[database]"), "{settings}");
        let justfile = std::fs::read_to_string(service.dir.join("Justfile")).expect("justfile");
        assert!(justfile.contains("\nmigrate:"), "{justfile}");
    }

    #[test]
    fn a_service_missing_its_settings_file_is_refused_without_writing_anything() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        std::fs::remove_file(service.dir.join("config/demo.toml"))
            .expect("delete the settings file");

        let before = tree(dir.path());
        let error = convert(&service).expect_err("a service with no config/<name>.toml is refused");
        let message = format!("{error:#}");
        assert!(message.contains("config/demo.toml"), "{message}");

        // The defect itself, named rather than inferred from a count: today this refusal
        // arrives after `writes.commit`, so the tree has a `src/db.rs` nothing declares.
        assert!(
            !service.src("db.rs").exists(),
            "a refused conversion left {} behind: {message}",
            service.rel(&service.src("db.rs"))
        );
        assert_eq!(
            tree(dir.path()),
            before,
            "a refused conversion must leave the service exactly as it found it: {message}"
        );
    }

    #[test]
    fn a_manifest_without_a_dependencies_table_is_refused_without_writing_anything() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        let manifest = service.dir.join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .expect("rewrite the manifest without a [dependencies] table");

        let before = tree(dir.path());
        let error = convert(&service).expect_err("a manifest with no [dependencies] is refused");
        let message = format!("{error:#}");
        assert!(message.contains("[dependencies]"), "{message}");
        assert!(message.contains("Cargo.toml"), "{message}");

        assert!(
            !service.src("db.rs").exists(),
            "a refused conversion left {} behind: {message}",
            service.rel(&service.src("db.rs"))
        );
        assert_eq!(
            tree(dir.path()),
            before,
            "a refused conversion must leave the service exactly as it found it: {message}"
        );
    }

    // -----------------------------------------------------------------
    // RT-39: two of these ten edits had no way to say they had done nothing.
    //
    // RT-31 fixed the order these edits resolve in; it could not fix what the two planners
    // were. `plan_settings_toml` and `plan_justfile` returned `String`, and each opened with a
    // substring guard written as idempotence. `add_database` refuses at its first line when
    // `service.has_database()`, so a second run never reaches either guard: what they actually
    // matched was a file carrying that text for some other reason, and a conflict of that kind
    // was accepted in silence — reported as `✓ wired into:`, because the report is built from the
    // path list rather than from what changed.
    //
    // These are the conflicts, each asserted twice: that the refusal names the file and the
    // conflict, and that the tree is exactly what it was. The second half is the RT-31 property,
    // re-asserted here rather than assumed — these two planners sit in the planned half, and a
    // later refactor should have to break that claim in a test before it breaks it in a service.
    // -----------------------------------------------------------------

    #[test]
    fn a_justfile_with_a_migrate_recipe_of_its_own_is_refused_without_writing_anything() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        let justfile = service.dir.join("Justfile");
        let original = std::fs::read_to_string(&justfile).expect("justfile");
        std::fs::write(
            &justfile,
            format!("{original}\n# the deploy thing, which happens to be called migrate\nmigrate:\n    ./deploy.sh staging\n"),
        )
        .expect("add a migrate recipe of the service's own");

        let before = tree(dir.path());
        let error = convert(&service)
            .expect_err("a `migrate:` recipe this command did not write is a conflict");
        let message = format!("{error:#}");

        assert!(message.contains("services/demo/Justfile"), "{message}");
        assert!(message.contains("./deploy.sh staging"), "{message}");
        // A refusal is only worth having if the reader can act on it. The other answer to this
        // conflict is installing the two recipes by hand, so the message quotes them.
        assert!(
            message.contains("@just rust::run {{ pkg }} -- migrate"),
            "{message}"
        );
        assert!(message.contains("sea-orm-cli generate entity"), "{message}");
        assert!(message.contains("entities:"), "{message}");

        assert!(
            !service.src("db.rs").exists(),
            "a refused conversion left {} behind: {message}",
            service.rel(&service.src("db.rs"))
        );
        assert_eq!(
            tree(dir.path()),
            before,
            "a refused conversion must leave the service exactly as it found it: {message}"
        );
    }

    #[test]
    fn a_justfile_whose_first_line_is_migrate_is_refused_without_writing_anything() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // The narrower case the ticket names: the guard was `"\nmigrate:"`, with a leading
        // newline, so a `Justfile` that opens with its own `migrate:` recipe did not match it at
        // all. Both recipes were appended, and `just` then refused the file for a duplicate
        // recipe — a failure this command produced and reported as a success.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        let justfile = service.dir.join("Justfile");
        let original = std::fs::read_to_string(&justfile).expect("justfile");
        std::fs::write(
            &justfile,
            format!("migrate:\n    ./deploy.sh staging\n\n{original}"),
        )
        .expect("put a migrate recipe on the first line");

        let before = tree(dir.path());
        let error =
            convert(&service).expect_err("the first line is a recipe too, leading newline or not");
        let message = format!("{error:#}");

        assert!(message.contains("services/demo/Justfile"), "{message}");
        assert!(message.contains("./deploy.sh staging"), "{message}");

        assert!(
            !service.src("db.rs").exists(),
            "a refused conversion left {} behind: {message}",
            service.rel(&service.src("db.rs"))
        );
        assert_eq!(
            tree(dir.path()),
            before,
            "a refused conversion must leave the service exactly as it found it: {message}"
        );
    }

    #[test]
    fn a_justfile_that_already_has_an_entities_recipe_is_refused_without_writing_anything() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // The twin of the case above, which the ticket does not name and which is the same
        // defect: one guard covered `migrate` alone, so a `Justfile` that already had `entities:`
        // and no `migrate:` took the guard's early return and got neither recipe — or, if the
        // guard had been per-recipe, would have had `entities:` appended a second time.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        let justfile = service.dir.join("Justfile");
        let original = std::fs::read_to_string(&justfile).expect("justfile");
        std::fs::write(
            &justfile,
            format!("{original}\nentities:\n    echo entities, but not from sea-orm-cli\n"),
        )
        .expect("add an entities recipe of the service's own");

        let before = tree(dir.path());
        let error = convert(&service)
            .expect_err("a `entities:` recipe this command did not write is a conflict");
        let message = format!("{error:#}");

        assert!(message.contains("services/demo/Justfile"), "{message}");
        assert!(message.contains("not from sea-orm-cli"), "{message}");
        // `migrate` is the name that is free, and the refusal still has to say both recipes are
        // missing, because installing only `migrate` is not an option this command takes.
        assert!(message.contains("`migrate` and `entities`"), "{message}");

        assert!(
            !service.src("db.rs").exists(),
            "a refused conversion left {} behind: {message}",
            service.rel(&service.src("db.rs"))
        );
        assert_eq!(
            tree(dir.path()),
            before,
            "a refused conversion must leave the service exactly as it found it: {message}"
        );
    }

    #[test]
    fn a_settings_file_with_a_database_section_of_its_own_is_refused_without_writing_anything() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        let settings = service.dir.join("config/demo.toml");
        let original = std::fs::read_to_string(&settings).expect("settings");
        std::fs::write(
            &settings,
            format!(
                "{}\n[database]\nhost = \"db.internal\"\nport = 6543\nusername = \"shared\"\ndatabase_name = \"shared\"\nmax_connections = 50\n",
                original.trim_end()
            ),
        )
        .expect("add a [database] section of the service's own");

        let before = tree(dir.path());
        let error = convert(&service)
            .expect_err("a [database] section this command did not write is a conflict");
        let message = format!("{error:#}");

        assert!(message.contains("config/demo.toml"), "{message}");
        assert!(
            message.contains("not the section this command writes"),
            "{message}"
        );
        // Quoting what is there is what makes it recognisable: a reader who has never seen this
        // generator has to be able to tell their own section from the one it wanted.
        assert!(message.contains("host = \"db.internal\""), "{message}");
        assert!(message.contains("max_connections = 5"), "{message}");

        assert!(
            !service.src("db.rs").exists(),
            "a refused conversion left {} behind: {message}",
            service.rel(&service.src("db.rs"))
        );
        assert_eq!(
            tree(dir.path()),
            before,
            "a refused conversion must leave the service exactly as it found it: {message}"
        );
    }

    #[test]
    fn a_settings_file_whose_database_section_is_missing_a_key_is_refused_without_writing_anything()
    {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // The case that used to be invisible in the worst way. `plan_config` adds a *required*
        // `pub database: DatabaseSettings`, so a section carrying four of the five keys leaves a
        // service that compiles, starts, and then fails on a deserialization error pointing at a
        // file the generator had decided not to touch.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        let settings = service.dir.join("config/demo.toml");
        let original = std::fs::read_to_string(&settings).expect("settings");
        std::fs::write(
            &settings,
            format!(
                "{}\n[database]\nhost = \"127.0.0.1\"\nport = 5432\nusername = \"demo\"\ndatabase_name = \"demo\"\n",
                original.trim_end()
            ),
        )
        .expect("add a [database] section with no max_connections");

        let before = tree(dir.path());
        let error = convert(&service)
            .expect_err("a [database] section missing a required key is a conflict");
        let message = format!("{error:#}");

        assert!(message.contains("config/demo.toml"), "{message}");
        // Actionable rather than accurate-in-the-abstract: "this section is not mine" would be
        // true and useless.
        assert!(message.contains("missing `max_connections`"), "{message}");
        assert!(message.contains("max_connections = 5"), "{message}");

        assert!(
            !service.src("db.rs").exists(),
            "a refused conversion left {} behind: {message}",
            service.rel(&service.src("db.rs"))
        );
        assert_eq!(
            tree(dir.path()),
            before,
            "a refused conversion must leave the service exactly as it found it: {message}"
        );
    }

    #[test]
    fn a_file_that_already_has_what_the_generator_would_write_is_left_alone() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // Not a test of the defect. A pin on the behaviour the fix must not lose: a service
        // that already has the `migrate:` recipe keeps the file it has. What is asserted is the
        // contents, so this says "the file survives unchanged" and nothing stronger — rewriting
        // identical bytes would satisfy it just as well. The write loop does skip the write, for
        // the mtime reason named there, but this test cannot tell the two apart and does not
        // claim to.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = scaffolded_service(dir.path());
        let justfile = service.dir.join("Justfile");
        let original = std::fs::read_to_string(&justfile).expect("justfile");
        let pre_amended = plan_justfile(&original, "demo")
            .expect("the skeleton's Justfile defines neither recipe");
        assert_ne!(
            pre_amended, original,
            "the recipe must not already be there"
        );
        std::fs::write(&justfile, &pre_amended).expect("add the recipes ahead of time");

        convert(&service).expect("a service whose Justfile already has the recipes still converts");

        assert_eq!(
            std::fs::read_to_string(&justfile).expect("justfile"),
            pre_amended,
            "the generator must not rewrite a file that already says what it would write"
        );
    }
}
