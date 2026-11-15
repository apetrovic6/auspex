//! Thin wrapper. Everything of substance is in the library target so the integration
//! tests can reach it — see `lib.rs`.

use std::sync::Arc;

use anyhow::Context as _;
use api::config;
use api::db::{self, MIGRATION_LOCK_KEY};
use api::migrations::Migrator;
use api::startup::App;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "api", about = "Connect RPC API service", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve requests (the default). Applies pending migrations first, under an advisory
    /// lock so concurrent replicas do not race.
    Serve,
    /// Apply pending migrations and exit, without serving.
    Migrate,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let settings = config::load().context("could not load configuration")?;

    // Before anything worth tracing. A failure here is fatal on purpose: running blind is
    // worse than not running. The binding is load-bearing — the guard flushes buffered
    // spans and metrics when `main` returns; `let _ = ..` would drop it here and lose the
    // last batch.
    let _telemetry = telemetry::init(
        &settings.telemetry,
        telemetry::ServiceInfo {
            name: env!("CARGO_PKG_NAME"),
            version: env!("CARGO_PKG_VERSION"),
        },
    )
    .context("could not initialise telemetry")?;
    telemetry::install_panic_hook();

    let db = db::connect(&settings.database).context("could not create the database pool")?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::Migrate => {
            db::migrate_with_lock::<Migrator>(&db, &settings.database, MIGRATION_LOCK_KEY)
                .await
                .context("migrations failed")?;
        }
        Command::Serve => {
            db::migrate_with_lock::<Migrator>(&db, &settings.database, MIGRATION_LOCK_KEY)
                .await
                .context("migrations failed")?;

            // The identity seam's one insertion point, and the shipped default denies
            // everyone: `NoIdentities` authenticates no credential, so a method that
            // requires an identity refuses every caller until this line names a real
            // verifier. That is deliberate rather than a gap — which credential a product
            // accepts is the most product-specific decision in the tree, and a template that
            // made it would be a template everybody ripped out. See `crates/identity` and
            // `docs/adr/0012`.
            //
            // The verifier replacing this line has to say which kind of "no" it means:
            // `VerifyFailure::Rejected` for a credential it refuses, `VerifyFailure::Unavailable`
            // for a dependency it could not reach. There is no default arm to fall into, and
            // `NoIdentities` is `Rejected` because it is absent by policy, not unreachable.
            let verifier = Arc::new(identity::NoIdentities);

            let app = App::build(&settings, db, verifier)
                .await
                .context("could not build the application")?;
            tracing::info!(port = app.port(), "listening");
            app.run_until_stopped().await.context("server failed")?;
        }
    }
    Ok(())
}
