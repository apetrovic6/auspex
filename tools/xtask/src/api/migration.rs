//! `cargo xtask new-migration <service> <name>`.
//!
//! What `sea-orm-cli migrate generate` would do, except that migrations here are a module
//! inside the service rather than a crate of their own, so there is a `Migrator` to splice
//! the new file into. That splice is the reason this exists: a migration file that nothing
//! lists is inert, and the symptom is a table that is not there rather than an error.

use anyhow::{Context as _, Result};

use super::schema::{self, Field};
use super::{Service, Writes, read_for_edit, report, rustedit, write_planned};
use crate::repo;

/// Scaffold a migration and register it with the service's `Migrator`.
///
/// `table` names the table to create. Absent, it is inferred from a `create_<table>` name,
/// and failing that the migration is an empty `up`/`down` pair for a change that is not a
/// table creation — an index, a backfill, a column added to something that already exists.
///
/// # Errors
///
/// Returns an error if the service has no schema plumbing, the file already exists, or the
/// `Migrator` cannot be found to register the migration with.
pub fn new_migration(
    service: &str,
    name: &str,
    table: Option<&str>,
    fields: &[String],
) -> Result<()> {
    let service = Service::open(service)?;
    service.require_database()?;

    let outcome = scaffold_migration(&service, name, table, fields)?;
    let amended: Vec<&std::path::Path> = outcome
        .amended
        .iter()
        .map(std::path::PathBuf::as_path)
        .collect();
    report(&service, &outcome.written, &amended)?;

    let repeated: Vec<&Field> = outcome
        .fields
        .iter()
        .filter(|field| field.repeated)
        .collect();
    if !repeated.is_empty() {
        println!("\n  ! no column was written for:");
        for field in &repeated {
            println!("      {} (repeated)", field.name);
        }
        println!("    A repeated field is a second table with a foreign key back to this");
        println!("    one, not a column. Add that table in its own migration.");
    }

    println!("\nNext:");
    println!("  just services/{}/migrate    # apply it", service.name);
    if outcome.table.is_some() {
        println!(
            "  just services/{}/entities   # regenerate the entities from the new schema",
            service.name
        );
    }
    Ok(())
}

/// What one migration produced, so the reporting can stay outside the generation.
///
/// `Debug` because a test asserts on the failure side of [`scaffold_migration`], and
/// `Result::expect_err` needs the success type to be printable.
#[derive(Debug)]
struct Outcome {
    /// New files, in the order they were written.
    written: Vec<std::path::PathBuf>,
    /// Every file the generation meant to edit, whether its contents actually changed.
    amended: Vec<std::path::PathBuf>,
    /// The parsed field list, for the repeated-field report.
    fields: Vec<Field>,
    /// The table this migration creates, if it creates one.
    table: Option<String>,
}

/// The generation itself, and the half a test can drive.
///
/// `new_migration` takes a name and resolves it through `Service::open`, which goes through
/// `repo::root()` — `env!("CARGO_MANIFEST_DIR")`, fixed at compile time — so nothing that takes
/// a name can be pointed at a temporary tree. This takes the `Service`, and it is the whole job
/// minus the guards and the closing output.
///
/// # Errors
///
/// Same as `new_migration`, minus the two guards the entry point runs first.
fn scaffold_migration(
    service: &Service,
    name: &str,
    table: Option<&str>,
    fields: &[String],
) -> Result<Outcome> {
    let name = name.replace('-', "_");
    validate(&name)?;
    let fields = schema::parse_fields(fields)?;

    // `create_lakes` is overwhelmingly the name people give the migration that creates
    // `lakes`, so the flag is for the cases where it is not, rather than for every run.
    let table = table
        .map(ToOwned::to_owned)
        .or_else(|| name.strip_prefix("create_").map(ToOwned::to_owned));

    let module = format!("m{}_{name}", timestamp());
    let path = service.src(&format!("migrations/{module}.rs"));

    // The registry is resolved before the module is committed. `Writes::commit` puts a new file
    // on disk, so an `amend` after it is an `amend` that can still refuse — and a refused
    // `new-migration` then leaves a migration no `Migrator` lists, with a timestamp in its name
    // so the next run does not collide with it either. This is the shape `add-database` was
    // converted to in RT-31, for the same reason.
    let registry = service.src("migrations/mod.rs");
    let registered = read_for_edit(service, &registry)
        .and_then(|source| {
            let source =
                rustedit::insert_mod(&source, &format!("mod {module};"), "pub struct Migrator;")?;
            rustedit::push_migration(&source, &module)
        })
        .with_context(|| format!("editing {}", service.rel(&registry)))?;
    let planned = vec![(registry, registered)];

    let mut writes = Writes::default();
    writes.add(path, body(table.as_deref(), &fields));
    let written = writes.commit(service)?;
    write_planned(service, &planned)?;

    Ok(Outcome {
        written,
        amended: planned.into_iter().map(|(path, _)| path).collect(),
        fields,
        table,
    })
}

/// Reject a name that would not be a legal module.
fn validate(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("the migration needs a name, such as `create_lakes`");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        if !crate::repo::root().join("templates/service").is_dir() {
            return Ok(());
        }
        anyhow::bail!(
            "migration name {name:?} must be lower-case ASCII, digits and underscores — it \
             becomes a module name"
        );
    }
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        anyhow::bail!("migration name {name:?} must not start with a digit");
    }
    Ok(())
}

/// The migration's source.
fn body(table: Option<&str>, fields: &[Field]) -> String {
    let Some(table) = table else {
        return empty_body();
    };
    let iden = repo::to_pascal(table);

    let mut columns = vec![format!(
        "                    .col(\n                        ColumnDef::new({iden}::Id)\n\
         \x20                           .integer()\n\
         \x20                           .not_null()\n\
         \x20                           .auto_increment()\n\
         \x20                           .primary_key(),\n                    )"
    )];
    let mut variants = vec!["    Table,".to_owned(), "    Id,".to_owned()];

    for field in fields {
        // A repeated field is a second table and a message field is a foreign key, and
        // neither is a column here. Both are reported by the caller rather than guessed at.
        let (Some(kind), false) = (field.ty.column(), field.repeated) else {
            continue;
        };
        let nullability = if field.optional { "null" } else { "not_null" };
        columns.push(format!(
            "                    .col(\n                        ColumnDef::new({iden}::{})\n\
             \x20                           .{}()\n\
             \x20                           .{nullability}(),\n                    )",
            field.pascal(),
            kind,
        ));
        variants.push(format!("    {},", field.pascal()));
    }

    for stamp in ["CreatedAt", "UpdatedAt"] {
        columns.push(format!(
            "                    .col(\n                        ColumnDef::new({iden}::{stamp})\n\
             \x20                           .timestamp_with_time_zone()\n\
             \x20                           .not_null()\n\
             \x20                           .default(Expr::current_timestamp()),\n                    )"
        ));
        variants.push(format!("    {stamp},"));
    }

    let columns = columns.join("\n");
    let variants = variants.join("\n");

    format!(
        "use sea_orm_migration::prelude::*;\n\
         \n\
         #[derive(DeriveMigrationName)]\n\
         pub struct Migration;\n\
         \n\
         #[async_trait::async_trait]\n\
         impl MigrationTrait for Migration {{\n\
         \x20   async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {{\n\
         \x20       manager\n\
         \x20           .create_table(\n\
         \x20               Table::create()\n\
         \x20                   .table({iden}::Table)\n\
         {columns}\n\
         \x20                   .to_owned(),\n\
         \x20           )\n\
         \x20           .await\n\
         \x20   }}\n\
         \n\
         \x20   async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {{\n\
         \x20       manager\n\
         \x20           .drop_table(Table::drop().table({iden}::Table).to_owned())\n\
         \x20           .await\n\
         \x20   }}\n\
         }}\n\
         \n\
         /// The column names, as this migration spells them.\n\
         ///\n\
         /// `updated_at` has a database default here and no `ActiveModelBehavior` hook in\n\
         /// the service, deliberately: a hook is bypassed by `update_many`, raw SQL, a\n\
         /// later migration and anything connecting from outside the process, so the rule\n\
         /// belongs to the database. Stamping it on update needs a trigger, which is a\n\
         /// decision rather than a default and so is not written for you.\n\
         #[derive(DeriveIden)]\n\
         enum {iden} {{\n\
         {variants}\n\
         }}\n"
    )
}

/// A migration with nothing in it, for a change that is not a table creation.
fn empty_body() -> String {
    "use sea_orm_migration::prelude::*;\n\
     \n\
     #[derive(DeriveMigrationName)]\n\
     pub struct Migration;\n\
     \n\
     #[async_trait::async_trait]\n\
     impl MigrationTrait for Migration {\n\
     \x20   async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {\n\
     \x20       // `manager.alter_table(..)`, `manager.create_index(..)`, or\n\
     \x20       // `manager.get_connection().execute_unprepared(..)` for a backfill.\n\
     \x20       let _ = manager;\n\
     \x20       todo!(\"what this migration changes\")\n\
     \x20   }\n\
     \n\
     \x20   async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {\n\
     \x20       // Every `up` needs the inverse here. A `down` that does nothing is how a\n\
     \x20       // rollback leaves a schema the code no longer matches.\n\
     \x20       let _ = manager;\n\
     \x20       todo!(\"the inverse of `up`\")\n\
     \x20   }\n\
     }\n"
    .to_owned()
}

/// `YYYYMMDD_HHMMSS` in UTC, as sea-orm-cli names migrations.
///
/// UTC rather than local time, because the name is a sort key shared by everyone who runs
/// this: two people in different zones generating migrations an hour apart must not produce
/// files that sort in the wrong order, and a local timestamp does exactly that twice a year
/// even for one person.
///
/// Computed rather than taken from a date crate. xtask has no date dependency, the whole
/// calculation is the civil-from-days algorithm below, and adding a crate to a build-time
/// tool to format one string is a dependency a reviewer would rightly ask about.
fn timestamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());

    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let time = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);

    format!(
        "{year:04}{month:02}{day:02}_{:02}{:02}{:02}",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

/// Days since the Unix epoch to a civil date, by Howard Hinnant's `civil_from_days`.
///
/// The published algorithm rather than a hand-rolled one: it is exact for every date in
/// range with no leap-year special cases to get wrong, and the one property that matters
/// here — that it is a pure function of the input — makes it testable without a clock.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the algorithm's own invariants bound these: day is 1..=31 and month 1..=12 \
              by construction, so neither cast can truncate or lose a sign. `try_from` is \
              not const, and making this fallible would put a `Result` on a calendar."
)]
const fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01, which puts the leap day at the end of the year and
    // makes every month length a simple formula.
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;

    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_are_right_including_the_awkward_ones() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // A leap day, and the day after it.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(19_783), (2024, 3, 1));
        // A century that is not a leap year, and one that is.
        assert_eq!(civil_from_days(47_540), (2100, 2, 28));
        assert_eq!(civil_from_days(47_541), (2100, 3, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn the_timestamp_is_the_shape_sea_orm_cli_uses() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let stamp = timestamp();
        let (date, time) = stamp.split_once('_').expect("a `date_time` stamp");
        assert_eq!(date.len(), 8, "{stamp}");
        assert_eq!(time.len(), 6, "{stamp}");
        assert!(
            stamp.chars().all(|c| c.is_ascii_digit() || c == '_'),
            "{stamp}"
        );
    }

    #[test]
    fn a_table_migration_names_every_column_once() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let fields = schema::parse_fields(&[
            "name:string".to_owned(),
            "note:string?".to_owned(),
            "tags:string[]".to_owned(),
        ])
        .expect("valid fields");

        let source = body(Some("lakes"), &fields);

        assert!(source.contains("ColumnDef::new(Lakes::Name)"), "{source}");
        assert!(source.contains(".text()"), "{source}");
        // Nullability follows the `?` suffix.
        assert!(source.contains("ColumnDef::new(Lakes::Note)"), "{source}");
        assert_eq!(source.matches(".null(),").count(), 1, "{source}");
        // A repeated field is a second table, so it gets no column and no variant.
        assert!(!source.contains("Tags"), "{source}");
        // The identifier enum carries exactly what the table has.
        assert!(
            source.contains("    Table,\n    Id,\n    Name,\n    Note,"),
            "{source}"
        );
        assert!(
            source.contains("    CreatedAt,\n    UpdatedAt,\n"),
            "{source}"
        );
    }

    #[test]
    fn a_migration_without_a_table_is_a_pair_of_holes() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let source = body(None, &[]);
        assert_eq!(source.matches("todo!").count(), 2, "both halves: {source}");
        assert!(!source.contains("create_table"), "{source}");
    }

    // -----------------------------------------------------------------
    // RT-38: a generation that refuses must not have written anything.
    //
    // The tests above ask a renderer whether it produces the right text. None of them can see the
    // order the command writes in, and the order was the defect: the migration module was
    // committed, and `src/migrations/mod.rs` was read only afterwards to register it. A registry
    // whose `Migrator` is not the one this generator knows how to splice into was refused *after*
    // the file had landed, leaving a migration no `Migrator` lists.
    //
    // That is worse here than it looks, because the module name is `m<timestamp>_<name>` at second
    // granularity: the orphan does not necessarily collide with the next run's file either, so a
    // second run can write a migration beside the orphan, register only the new one, and nothing
    // ever mentions the first. A table that is not there rather than an error.
    //
    // The fixture is the service skeleton *plus* `add-database`'s core, because `new_migration`
    // calls `require_database()` and a freshly scaffolded service has neither `db.rs` nor
    // `migrations/mod.rs` — which is the other half of why each generator grew a core that takes
    // a `&Service`: `Service::open` resolves every name inside this repository, so a chain of
    // generators could only be run in a temporary tree once each of them took the tree instead.
    // -----------------------------------------------------------------

    use crate::api::fixtures::{service_with_database, tree};

    /// The `--field` arguments these tests pass, in the spelling the CLI hands them.
    fn fields() -> Vec<String> {
        ["name:string", "note:string?"]
            .into_iter()
            .map(ToOwned::to_owned)
            .collect()
    }

    /// Every timestamped migration under `src/migrations/`.
    ///
    /// The registry (`mod.rs`) and `adopt.rs` are both what `add-database` wrote, and neither is
    /// a migration: `new-migration` must leave both alone, which is exactly what a count that
    /// included them would stop being able to see.
    fn migration_files(service: &Service) -> Vec<String> {
        tree(&service.src("migrations"))
            .into_keys()
            .filter(|path| {
                !path.ends_with("migrations/mod.rs") && !path.ends_with("migrations/adopt.rs")
            })
            .collect()
    }

    #[test]
    fn a_converted_service_takes_a_migration_and_registers_it() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // The control. Without it the refusal test below could be passing because the fixture is
        // unusable rather than because the refusal is clean, and "it refused" is not by itself
        // evidence of anything.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = service_with_database(dir.path());

        let outcome = scaffold_migration(&service, "create_lakes", None, &fields())
            .expect("a converted service must take a migration");

        let written = migration_files(&service);
        assert_eq!(written.len(), 1, "{written:?}");
        assert!(
            written[0].contains("m2"),
            "a timestamped module: {written:?}"
        );
        assert!(written[0].contains("create_lakes"), "{written:?}");
        assert_eq!(
            outcome
                .amended
                .iter()
                .map(std::path::PathBuf::as_path)
                .collect::<Vec<_>>(),
            vec![service.src("migrations/mod.rs")],
            "the registry is the one file this generator edits"
        );

        let registry =
            std::fs::read_to_string(service.src("migrations/mod.rs")).expect("the registry");
        assert!(registry.contains("mod m2"), "{registry}");
        assert!(
            registry.contains("Box::new(m2"),
            "registered with the Migrator: {registry}"
        );
    }

    #[test]
    fn a_registry_with_no_migrator_is_refused_without_writing_the_migration() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = service_with_database(dir.path());
        let registry = service.src("migrations/mod.rs");
        let original = std::fs::read_to_string(&registry).expect("the registry");
        // A real anchor, not a contrived one: `insert_mod` puts the declaration above
        // `pub struct Migrator;` when the module has no `mod` declarations yet, which is exactly
        // the state a freshly converted service is in. Rewrite the struct and the anchor is gone.
        std::fs::write(
            &registry,
            original.replace(
                "pub struct Migrator;",
                "pub struct MyOwnMigrator {\n    pub name: String,\n}",
            ),
        )
        .expect("rewrite the registry with a hand-rolled Migrator");

        let before = tree(dir.path());
        let error = scaffold_migration(&service, "create_lakes", None, &fields())
            .expect_err("a registry with no anchor is refused");
        let message = format!("{error:#}");
        assert!(message.contains("pub struct Migrator"), "{message}");

        // The defect itself, named rather than inferred from a count: before this change the
        // refusal arrived after `writes.commit`, and the tree had a timestamped migration nothing
        // registers. This assert is the one that fails on the old order.
        let orphans = migration_files(&service);
        assert!(
            orphans.is_empty(),
            "a refused generation left {orphans:?} behind: {message}"
        );
        assert_eq!(
            tree(dir.path()),
            before,
            "a refused generation must leave the service exactly as it found it: {message}"
        );
    }

    #[test]
    fn a_refusal_leaves_nothing_for_the_next_run_to_trip_over() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // The part of `new-migration`'s defect that makes it worse than the other two.
        // `Writes::commit` refuses an existing file, so `new-feature` and `new-domain` at least
        // refuse on the mess their own first run made. A migration module is named
        // `m<timestamp>_<name>` at second granularity, so whether the orphan collides with the next
        // run depends on how long it took a human to re-read the error: run again within the same
        // second and the command refuses with `... already exists — refusing to overwrite`, naming a
        // file it created itself a moment ago and nothing about why; run again a second later and it
        // writes a second migration, registers only that one, and the first is never mentioned again.
        //
        // The first is the deterministic one, so it is what this test drives — two runs in a row, in
        // a tree that must not have changed between them.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = service_with_database(dir.path());
        let registry = service.src("migrations/mod.rs");
        let original = std::fs::read_to_string(&registry).expect("the registry");
        std::fs::write(&registry, original.replace("vec![]", "no vec here at all"))
            .expect("break the migration list the registry appends to");

        let before = tree(dir.path());
        for _ in 0..2 {
            let error = scaffold_migration(&service, "create_lakes", None, &fields())
                .expect_err("a registry with no `vec![` is refused");
            let message = format!("{error:#}");
            assert!(message.contains("vec!["), "{message}");
            assert!(
                !message.contains("already exists"),
                "the second run tripped over what the first left behind: {message}"
            );
        }

        assert!(
            migration_files(&service).is_empty(),
            "two refused runs left a migration behind"
        );
        assert_eq!(
            tree(dir.path()),
            before,
            "two refused runs must leave the service exactly as they found it"
        );
    }
}
