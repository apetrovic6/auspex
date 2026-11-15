//! `cargo xtask new-feature <service> <name>`.
//!
//! A feature here is a **vertical slice**: its contract, its domain types, the conversions
//! between the two and the database, its errors, and the service implementation — all in
//! one directory, mounted from the one place that knows every feature.
//!
//! # The conversions are the point
//!
//! The slice has three spellings of the same data and two boundaries between them:
//!
//! ```text
//! <Pascal>Request  --From-->  <Pascal>Dto  --From-->  entities::<table>::ActiveModel
//! <Pascal>Response <--From--  <Pascal>     <--From--  entities::<table>::Model
//! ```
//!
//! Writing those four impls by hand is most of the work of adding a feature, all of it
//! mechanical, and every one of them is a place to drop a field silently — the conversion
//! still compiles, the column is just never written. Generating them from one field list
//! is the only way to be sure all four agree, which is why this generator exists rather
//! than a template to copy.
//!
//! And every generated struct literal names every field, so a field the generator drops is
//! E0063 on all four, not on two (RT-27). No `..Default::default()`: the `ActiveModel` spells
//! `NotSet` for exactly the infrastructure columns the entity has, and the wire messages name
//! buffa's unknown-fields store. That name is `#[doc(hidden)]`, which is the cost —
//! `UNKNOWN_FIELDS` says why it is paid. `just check-template` compiles the result.
//!
//! # The table has to exist first, and that is not a limitation
//!
//! The fields come from `src/entities/<table>.rs`, which sea-orm-cli generates from the
//! live schema. There is no `--field` escape hatch, and the reason is mechanical rather
//! than purist: `just services/<svc>/migrate` runs the service binary, so it needs the
//! whole crate to compile — and a slice written before its entity exists names a module
//! that is not there. Generating the slice first would therefore break the very command
//! that creates the table it is waiting for, leaving the only way out a hand-deletion of
//! the files this tool just wrote.
//!
//! So the order is the one that works, and this refuses the one that does not:
//!
//! ```text
//! cargo xtask new-migration <svc> create_<table> --field name:string
//! just services/<svc>/migrate
//! just services/<svc>/entities
//! cargo xtask new-feature <svc> <name>
//! ```
//!
//! # Registering a method is a decision about auth, so the generator makes it
//!
//! `tests/<svc>/auth.rs` holds two lists — `PUBLIC` and `PROTECTED` — and
//! `every_registered_method_is_classified` fails until every method `register_services`
//! answers to is in exactly one of them (ADR 0012: the default for a method nobody
//! classified must not be public). This generator registers four methods, so it writes all
//! four into `PROTECTED`, and it writes each generated handler to call `ctx.principal()?`
//! so the classification and the code agree on sight.
//!
//! `PROTECTED` is the fail-closed direction and it is also the one that passes as the suite
//! stands: `every_protected_method_refuses_an_anonymous_caller` asserts only that the
//! method answers `unauthenticated` to an anonymous caller, which a handler that calls
//! `ctx.principal()?` does with no verifier and no live identity — the shipped verifier
//! authenticates nobody (`crates/identity`).
//!
//! What it must not do is write them into `PUBLIC`, which is green with no further work and
//! is exactly the failure the gate was built against: a generator that publishes endpoints
//! because publishing is what costs nothing to do. Choosing `PROTECTED` means the generated
//! methods answer `unauthenticated` until a product supplies a verifier — and after one, to
//! every caller that verifier refuses; a verifier that reaches no verdict at all answers
//! `internal`, which is a fact about the service and not about the caller (`docs/adr/0012`).
//! Moving a name out of the list is a diff a reviewer can ask about. Before this, the same
//! decision left no trace at all.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use super::schema::{self, Field};
use super::{Service, Writes, add_dependencies, read_for_edit, report, rustedit, write_planned};
use crate::repo;

/// Scaffold a feature slice and mount it.
///
/// # Errors
///
/// Returns an error if the service has no schema plumbing, the table's entity has not been
/// generated yet, the feature already exists, or the slice cannot be mounted in
/// `startup.rs`.
pub fn new_feature(service: &str, name: &str, table: Option<&str>) -> Result<()> {
    let service = Service::open(service)?;
    service.require_database()?;

    let outcome = scaffold_slice(&service, name, table)?;
    let amended: Vec<&Path> = outcome
        .amended
        .iter()
        .map(std::path::PathBuf::as_path)
        .collect();

    report(&service, &outcome.written, &amended)?;

    next_steps(&service, &outcome.fields, &outcome.entity, &outcome.methods);
    Ok(())
}

/// What one slice produced, so the reporting can stay outside the generation.
///
/// `Debug` because a test asserts on the failure side of [`scaffold_slice`], and
/// `Result::expect_err` needs the success type to be printable.
#[derive(Debug)]
pub(super) struct Outcome {
    /// New files, in the order they were written.
    written: Vec<PathBuf>,
    /// Every existing file the slice was mounted into, whether its contents actually changed.
    amended: Vec<PathBuf>,
    /// The fields the slice converts, read off the entity.
    fields: Vec<Field>,
    /// The entity those fields came from, relative to the repository root.
    entity: String,
    /// The methods the slice declares, as `tests/<svc>/auth.rs` names them.
    methods: Vec<String>,
}

/// The generation itself, and the half a test can drive.
///
/// `new_feature` takes a name and resolves it through `Service::open`, which goes through
/// `repo::root()` — `env!("CARGO_MANIFEST_DIR")`, fixed at compile time — so nothing that takes
/// a name can be pointed at a temporary tree. This takes the `Service`, and it is the whole job
/// minus the two guards and the closing output.
///
/// `pub(super)`, and its `Outcome` with it, so `api::fixtures` can run this generator ahead of
/// `new-domain`, which refuses without a feature slice — and refuses, before RT-38, having already
/// written one DTO into a module that was never going to declare it.
///
/// # Errors
///
/// Same as `new_feature`, minus the two guards the entry point runs first.
pub(super) fn scaffold_slice(
    service: &Service,
    name: &str,
    table: Option<&str>,
) -> Result<Outcome> {
    validate("feature", "lake", name)?;
    let snake = name.replace('-', "_");
    let pascal = repo::to_pascal(name);
    let table = table.map_or_else(|| schema::pluralize(&snake), ToOwned::to_owned);

    let Some(entity) = service.entity(&table) else {
        bail!(
            "{} does not exist, so there is nothing to read the fields from.\n\
             \n\
             The table has to be there first, and the order is not negotiable: \
             `just services/{}/migrate` runs the service binary, so it needs this crate to \
             compile — and a slice generated now would name an entity that is not there and \
             break the command that would create it.\n\
             \n    cargo xtask new-migration {} create_{table} --field name:string\n\
             \x20   just services/{}/migrate\n\
             \x20   just services/{}/entities\n\
             \x20   cargo xtask new-feature {} {name}\n\
             \n\
             If the table exists under another name, pass `--table`.",
            service.rel(&service.src(&format!("entities/{table}.rs"))),
            service.name,
            service.name,
            service.name,
            service.name,
            service.name,
        );
    };

    let schema::Entity {
        fields,
        infrastructure,
    } = schema::fields_from_entity(&std::fs::read_to_string(&entity)?)?;
    let source = service.rel(&entity);

    if let Some(repeated) = fields.iter().find(|field| field.repeated) {
        bail!(
            "field {:?} is repeated, which this generator does not write a slice for. A \
             repeated field is a second table with a foreign key back to this one, so it is \
             its own feature with its own migration — and the conversions between them are \
             the part worth thinking about rather than generating.",
            repeated.name
        );
    }

    let names = Names {
        service: service.name.clone(),
        snake,
        pascal,
        table,
    };
    let methods = routed_methods(&names);

    let writes = slice_files(service, &names, &fields, &infrastructure);
    let planned = plan_mount(service, &names, &methods)?;

    let written = writes.commit(service)?;
    write_planned(service, &planned)?;

    Ok(Outcome {
        written,
        amended: planned.into_iter().map(|(path, _)| path).collect(),
        fields,
        entity: source,
        methods,
    })
}

/// The seven new files a slice is made of.
fn slice_files(
    service: &Service,
    names: &Names,
    fields: &[Field],
    infrastructure: &[String],
) -> Writes {
    let snake = &names.snake;
    let mut writes = Writes::default();
    writes.add(
        service.root.join(format!("proto/{snake}/v1/{snake}.proto")),
        contract(names, fields),
    );
    writes.add(service.src(&format!("{snake}/mod.rs")), slice_mod(names));
    writes.add(
        service.src(&format!("{snake}/domain/mod.rs")),
        domain_module(names, fields),
    );
    writes.add(service.src(&format!("{snake}/errors.rs")), errors(names));
    writes.add(
        service.src(&format!("{snake}/models.rs")),
        models(names, fields, infrastructure),
    );
    writes.add(
        service.src(&format!("{snake}/service/mod.rs")),
        "//! Versioned to match the proto package, so a `v2` implementation lands beside \
         this\n//! one rather than replacing it.\n\npub mod v1;\n"
            .to_owned(),
    );
    writes.add(
        service.src(&format!("{snake}/service/v1.rs")),
        implementation(names),
    );
    writes
}

/// The four existing files a slice is mounted into, every one of them resolved before a single
/// byte of the slice is written.
///
/// This is where RT-38's defect was, and the reason it is fixed by ordering rather than by a new
/// primitive: `Writes::commit` puts seven new files on disk, and an `amend` after it is an `amend`
/// that can still refuse. `new_feature` already had the shape for one of the four — the
/// classification was checked against `auth.rs` as it stood, "so that a service with no
/// classification table refuses the whole generator instead of getting half a feature slice on
/// disk and a compile error pointed at a test file" — and not for the other three, so the other
/// three could still leave a whole slice and a `lib.rs` that declares none of it.
///
/// The order is the order the edits are applied in, which is also the order `report` prints them
/// in: `lib.rs` declares the module, `startup.rs` reaches it, the manifest carries the dependency
/// `errors.rs` needs, and `auth.rs` classifies the methods that registration obliges.
///
/// # Errors
///
/// Every error here names the file it could not resolve.
fn plan_mount(
    service: &Service,
    names: &Names,
    methods: &[String],
) -> Result<Vec<(PathBuf, String)>> {
    let lib = service.src("lib.rs");
    let startup = service.src("startup.rs");
    let manifest = service.dir.join("Cargo.toml");
    let auth = service.auth_test();

    let declared = read_for_edit(service, &lib)
        .and_then(|source| {
            rustedit::insert_mod(
                &source,
                &format!("pub mod {};", names.snake),
                "pub mod startup;",
            )
        })
        .with_context(|| format!("editing {}", service.rel(&lib)))?;

    let registered = plan_startup(service, &startup, names)?;

    // `errors.rs` derives `thiserror::Error`. A service with no features has no error enum
    // and so no reason to depend on thiserror — `templates/service` rightly does not — which
    // makes the first feature the thing that adds it. Skipping this is not subtle at compile
    // time, but it is silent here: the slice lands, the generator reports success, and what
    // fails is `cargo check` with four errors about an unresolved crate and nothing pointing
    // at the command that wrote the code.
    let amended = read_for_edit(service, &manifest)
        .and_then(|source| add_dependencies(&source, &["thiserror"]))
        .with_context(|| format!("editing {}", service.rel(&manifest)))?;

    // Registering the four methods is what obliges the classification, so it is the last edit and
    // not an earlier one. `auth.rs` is a test file rather than source, so it gets the treatment
    // the other three got: edited in place, doc comments and all, then formatted with the rest of
    // what was touched. It is read here rather than amended, which is what makes "nothing has
    // been written yet" in `read_auth`'s own refusal true of every file this generator touches.
    let classified = classify(service, &auth, &read_auth(service, &auth)?, methods)?;

    Ok(vec![
        (lib, declared),
        (startup, registered),
        (manifest, amended),
        (auth, classified),
    ])
}

/// Resolve `startup.rs`: drop the no-feature placeholder, bring the module into scope, register
/// the service.
///
/// Split out of [`new_feature`] for the reason [`next_steps`] was — it is the half a reader of
/// that function does not need — and then kept out by arithmetic: RT-21 and RT-22 each added an
/// amendment to `new_feature`, and together they put it one line over `clippy::too_many_lines`.
/// Neither branch crossed it alone. RT-38 keeps the split for the same arithmetic: the four
/// resolutions in [`plan_mount`] are one function a reader can hold, and the `startup.rs` half is
/// the one with a placeholder string in it.
fn plan_startup(service: &Service, startup: &Path, names: &Names) -> Result<String> {
    let (snake, pascal) = (&names.snake, &names.pascal);
    read_for_edit(service, startup)
        .and_then(|source| {
            // The placeholder `add-database` leaves while no feature needs the connection. The
            // first one that does is this one, so it goes.
            let source = source.replace(
                "    // Threaded in for the features that need one. Until the first does, nothing\n\
                 \x20   // reads it — the binding keeps the signature stable so adding a\n\
                 \x20   // database-backed feature is a line here rather than a change through three\n\
                 \x20   // functions. `cargo xtask new-feature` removes this when it adds that line.\n\
                 \x20   let _ = &db;\n\n",
                "",
            );
            // The module has to be in scope before the chain can name it.
            let source = rustedit::insert_into_crate_use(&source, snake)?;
            rustedit::register_service(&source, &format!("{snake}::service::v1::{pascal}::new(db)"))
        })
        .with_context(|| format!("editing {}", service.rel(startup)))
}

/// Write this slice's methods into `auth.rs`'s `PROTECTED` list.
///
/// `pub(super)` because `new-rpc` adds a method to a slice this generator wrote, and a
/// method it registers is the same decision about who may call it. A second implementation
/// of "classify a generated method" is the thing RT-21 exists to prevent — two generators
/// that answered the question differently would leave no trace of having disagreed.
pub(super) fn classify(
    service: &Service,
    auth: &Path,
    source: &str,
    methods: &[String],
) -> Result<String> {
    let mut source = source.to_owned();
    for method in methods {
        source =
            rustedit::insert_into_const_list(&source, "PROTECTED", method).with_context(|| {
                format!(
                    "classifying {method} as protected in {} — every method \
                     `register_services` answers to has to be named in that file's PUBLIC or \
                     PROTECTED list, so a slice this generator cannot classify it will fail \
                     the suite for the same reason",
                    service.rel(auth)
                )
            })?;
    }
    Ok(source)
}

/// Read the classification table, with the refusal this generator owes a service that has
/// none.
///
/// Read *before* anything is written, and shared with `new-rpc` for that reason rather than
/// for the two lines of `read_to_string`: a generator that discovers halfway through that it
/// has nowhere to put the classification leaves a half-written method on disk and a compile
/// error pointing at a test file.
pub(super) fn read_auth(service: &Service, auth: &Path) -> Result<String> {
    std::fs::read_to_string(auth).map_err(|error| {
        anyhow::anyhow!(
            "could not read {}: {error}\n\
             \n\
             This generator registers methods, and a registered method is a decision about who \
             may call it, so this generator writes that decision into this file rather than \
             printing it. A service with no such file has nowhere for the decision to go.\
             \n\
             \n\
             Nothing has been written yet. Restore the table from `templates/service`, or say \
             what replaced it and where classification happens there.",
            service.rel(auth),
        )
    })
}

/// What to run now, in the order it has to happen.
///
/// Split out of [`new_feature`] because it is the half a reader of that function does not
/// need: the generator's logic is which files to write and where to mount them, and this
/// is the paragraph printed afterwards.
fn next_steps(service: &Service, fields: &[Field], source: &str, methods: &[String]) {
    println!("\n  Fields came from {source}:");
    for field in fields {
        println!("      {}: {}", field.name, field.domain_type());
    }

    println!("\nNext:");
    println!("  just proto::generate                    # the slice will not compile until you do");
    println!("  just services/{}/dev", service.name);

    // Naming the list it chose and what moving out of it means. What this replaces asked
    // for work the generator now does, and left the direction of that work open — which is
    // how "unauthenticated" becomes "public" when the green suite is the thing in front of
    // you (ADR 0012 says the default must not be public; the laziest available answer was
    // still public, and left no diff behind).
    println!(
        "\n  Classified {} methods PROTECTED in services/{}/tests/{}/auth.rs:",
        methods.len(),
        service.name,
        service.name
    );
    for method in methods {
        println!("      {method}");
    }
    println!("    Every generated handler calls `ctx.principal()?`, so the code says what the");
    println!("    list says. They refuse every anonymous caller until this service has a");
    println!("    verifier that can say yes — `NoIdentities` in `crates/identity` is the one");
    println!("    shipped, and it says no. Move a name to PUBLIC if the method should answer");
    println!("    anonymous callers: that list is the decision, and moving a name out of");
    println!("    PROTECTED is a diff a reviewer can ask about.");
}

/// Reject a name that would not be a legal module, package or type.
///
/// Shared with `new-rpc`, whose method name becomes exactly the same three things — hence
/// `what` and `example`, which are the only part of the three refusals that differs.
pub(super) fn validate(what: &str, example: &str, name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("the {what} needs a name, such as `{example}`");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        if !crate::repo::root().join("templates/service").is_dir() {
            return Ok(());
        }
        bail!(
            "{what} name {name:?} must be lower-case ASCII, digits and dashes — it becomes \
             a module, a type name and a declaration in the contract"
        );
    }
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        bail!("{what} name {name:?} must not start with a digit");
    }
    Ok(())
}

/// The spellings one feature needs, resolved once.
pub struct Names {
    /// The service being written into, for the file paths named inside generated comments:
    /// a comment that says `services/api/tests/api/auth.rs` can be opened, one that says
    /// `tests/<svc>/auth.rs` has to be resolved by hand.
    pub service: String,
    pub snake: String,
    pub pascal: String,
    pub table: String,
}

impl Names {
    /// Where the generated wire types live.
    fn wire(&self) -> String {
        format!("proto::r#gen::buffa::{}::v1", self.snake)
    }
}

/// The RPCs this generator declares, in the order it declares them.
///
/// One list rather than several spellings, because the same four names are needed in three
/// places that are edited at different times: the `service` block of the `.proto`, the
/// handlers that implement it, and the classification list in `tests/<svc>/auth.rs`, which
/// spells each method the way `Router::methods()` answers to it. Separate copies of that
/// can disagree, and the disagreement is a method the router serves and no test classifies.
fn rpcs(names: &Names) -> Vec<String> {
    let Names { pascal, .. } = names;
    [
        format!("Create{pascal}"),
        format!("Get{pascal}"),
        format!("List{pascal}s"),
        format!("Update{pascal}"),
    ]
    .into_iter()
    .collect()
}

/// One method, as `tests/<svc>/auth.rs` has to name it.
///
/// `<package>.v1.<Pascal>Service/<Method>` is the shape `connectrpc` routes on, so it is
/// the shape `every_registered_method_is_classified` compares the routing table against.
///
/// One definition rather than one per generator: `new-rpc` classifies the method it adds
/// into the same list, and two places that spell a routed method name can disagree. The
/// disagreement is a method the router serves and no test classifies — or a classification
/// for a name nothing routes — and `every_registered_method_is_classified` is what finds
/// out, long after.
pub(super) fn routed_method(snake: &str, pascal: &str, rpc: &str) -> String {
    format!("{snake}.v1.{pascal}Service/{rpc}")
}

/// The same methods as `tests/<svc>/auth.rs` has to name them, from [`rpcs`].
fn routed_methods(names: &Names) -> Vec<String> {
    let Names { snake, pascal, .. } = names;
    rpcs(names)
        .into_iter()
        .map(|rpc| routed_method(snake, pascal, &rpc))
        .collect()
}

/// The `service` block the contract ends with, from [`rpcs`].
fn service_block(names: &Names) -> String {
    let Names { pascal, .. } = names;
    let declares = rpcs(names)
        .into_iter()
        .map(|rpc| format!("  rpc {rpc}({rpc}Request) returns ({rpc}Response);"))
        .collect::<Vec<_>>();
    format!("service {pascal}Service {{\n{}\n}}", declares.join("\n"))
}

/// `proto/<snake>/v1/<snake>.proto`.
///
/// Four methods rather than one: a slice with only a create is not a worked example of
/// anything, and the read, list and update paths are precisely where the conversions
/// differ — one returns a message, one a repeated field, one reads a row before writing it.
fn contract(names: &Names, fields: &[Field]) -> String {
    let Names { snake, pascal, .. } = names;

    // `zip` with an open range rather than `enumerate`, so the field number is a `u32` from
    // the start: the cast `enumerate` would need is one clippy is right to object to, and
    // the number genuinely is a proto field number rather than an index.
    let declare = |fields: &[Field], from: u32| -> String {
        fields
            .iter()
            .zip(from..)
            .map(|(field, number)| {
                let label = if field.optional { "optional " } else { "" };
                format!("  {label}{} {} = {number};", field.ty.proto(), field.name)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let entity_fields = declare(fields, 2);
    let write_fields = declare(fields, 1);
    let update_fields = declare(fields, 2);
    let service_block = service_block(names);

    format!(
        "syntax = \"proto3\";\n\
         \n\
         package {snake}.v1;\n\
         \n\
         // One {snake}, as the contract carries it.\n\
         //\n\
         // Carried inside each RPC's own response message rather than returned directly: buf's\n\
         // STANDARD lint requires a dedicated <Method>Response per RPC, which also lets one\n\
         // method grow a field later without touching the others.\n\
         message {pascal} {{\n\
         \x20 int32 id = 1;\n\
         {entity_fields}\n\
         }}\n\
         \n\
         message Create{pascal}Request {{\n\
         {write_fields}\n\
         }}\n\
         \n\
         message Create{pascal}Response {{\n\
         \x20 {pascal} data = 1;\n\
         }}\n\
         \n\
         message Get{pascal}Request {{\n\
         \x20 int32 id = 1;\n\
         }}\n\
         \n\
         message Get{pascal}Response {{\n\
         \x20 {pascal} data = 1;\n\
         }}\n\
         \n\
         message List{pascal}sRequest {{}}\n\
         \n\
         message List{pascal}sResponse {{\n\
         \x20 repeated {pascal} data = 1;\n\
         }}\n\
         \n\
         message Update{pascal}Request {{\n\
         \x20 int32 id = 1;\n\
         {update_fields}\n\
         }}\n\
         \n\
         message Update{pascal}Response {{\n\
         \x20 {pascal} data = 1;\n\
         }}\n\
         \n\
         {service_block}\n"
    )
}

/// `<snake>/mod.rs`.
fn slice_mod(names: &Names) -> String {
    let Names { snake, .. } = names;
    format!(
        "//! The `{snake}` feature.\n\
         //!\n\
         //! A vertical slice: the domain types and their conversions (`domain`), the\n\
         //! behaviour hung off the generated entity (`models`), this feature's errors, and\n\
         //! the service implementation. Nothing here is reached from another feature —\n\
         //! `startup::register_services` is the only thing that knows this module exists.\n\
         \n\
         pub mod domain;\n\
         pub mod errors;\n\
         pub mod models;\n\
         pub mod service;\n"
    )
}

/// The field buffa generates on every message to hold the wire fields it did not recognise.
///
/// The generated wire conversions name it rather than end in `..Default::default()`, so that a
/// field dropped from one of them is E0063 instead of a response field that is always empty.
/// That couples the generated code to a `#[doc(hidden)]` name of buffa's, and the coupling is
/// accepted because it fails loudly: a buffa release that renames the field breaks
/// `just check-template` with E0560 naming it, where the struct-update syntax would
/// have let every field of every generated conversion drop silently. One constant, so that the
/// rename is one edit here — slices already generated in a product still need theirs by hand.
pub(super) const UNKNOWN_FIELDS: &str = "__buffa_unknown_fields";

/// [`UNKNOWN_FIELDS`] filled in.
///
/// Not `Default::default()`: `clippy::default_trait_access` (nursery, warn in this workspace,
/// and `-D warnings` in every crate's own `check` recipe) reports it whenever the type the
/// expression fills is inferable and is not a primitive — measured on this workspace's lint
/// set, it fires for `String`, `Option<String>`, `Vec<u8>` and a map, and stays quiet for
/// `bool`, `i32` and `f64`. A generated slice that trips it is a slice whose first act after
/// `cargo xtask new-feature` is to fail `just check`.
///
/// Clippy's own suggestion is `UnknownFields::default()`, which does not compile here: the
/// name is not imported. `::buffa::UnknownFields` does, because the service depends on buffa
/// directly (`templates/service/services/__name__/Cargo.toml`) — the same dependency that makes
/// the E0560 story above possible. So this carries [`UNKNOWN_FIELDS`]'s coupling to a
/// `#[doc(hidden)]` buffa name, and the same reason for paying it.
pub(super) const UNKNOWN_FIELDS_DEFAULT: &str = "::buffa::UnknownFields::default()";

/// `<snake>/domain/mod.rs` — the types and the contract half of the conversions.
#[expect(
    clippy::too_many_lines,
    reason = "almost all of it is one template of the file being written, and a reader \
              checking what the generated module looks like wants to see it in one piece"
)]
fn domain_module(names: &Names, fields: &[Field]) -> String {
    let Names { snake, pascal, .. } = names;
    let wire = names.wire();
    let unknown = UNKNOWN_FIELDS;
    let unknown_default = UNKNOWN_FIELDS_DEFAULT;

    let declare = |fields: &[Field]| -> String {
        fields
            .iter()
            .map(|field| format!("    pub {}: {},", field.name, field.domain_type()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let move_fields = |fields: &[Field], indent: &str| -> String {
        fields
            .iter()
            .map(|field| format!("{indent}{}: v.{},", field.name, field.name))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let domain_fields = declare(fields);
    let dto_fields = declare(fields);
    let from_request = move_fields(fields, "            ");
    let to_wire = move_fields(fields, "            ");

    format!(
        "//! The `{snake}` domain types, and their conversions to and from the contract.\n\
         //!\n\
         //! Two types, because a write and a read carry different things: [`{pascal}Dto`] is\n\
         //! what a caller supplies and has no identity yet, while [`{pascal}`] is a row that\n\
         //! exists and therefore has one. Collapsing them into one type with an\n\
         //! `Option<i32>` id is the shape this avoids — every reader of it then has to ask\n\
         //! whether the id is set, and the type system stops answering.\n\
         //!\n\
         //! The conversions live here rather than in the service implementation so that the\n\
         //! handler reads as the sequence of steps it is, and so that a field added to the\n\
         //! contract fails to compile in one place rather than in four.\n\
         \n\
         use {wire}::{{\n\
         \x20   Create{pascal}Request, Create{pascal}Response, Get{pascal}Response,\n\
         \x20   List{pascal}sResponse, Update{pascal}Request, Update{pascal}Response,\n\
         }};\n\
         \n\
         /// One `{snake}` that exists.\n\
         #[derive(Debug, Clone)]\n\
         pub struct {pascal} {{\n\
         \x20   pub id: i32,\n\
         {domain_fields}\n\
         }}\n\
         \n\
         /// What a create or an update carries.\n\
         ///\n\
         /// No `id`: a create does not have one yet, and an update takes it separately so\n\
         /// that the payload cannot disagree with the row being written.\n\
         #[derive(Debug, Clone)]\n\
         pub struct {pascal}Dto {{\n\
         {dto_fields}\n\
         }}\n\
         \n\
         impl From<Create{pascal}Request> for {pascal}Dto {{\n\
         \x20   fn from(v: Create{pascal}Request) -> Self {{\n\
         \x20       Self {{\n\
         {from_request}\n\
         \x20       }}\n\
         \x20   }}\n\
         }}\n\
         \n\
         impl From<Update{pascal}Request> for {pascal}Dto {{\n\
         \x20   fn from(v: Update{pascal}Request) -> Self {{\n\
         \x20       Self {{\n\
         {from_request}\n\
         \x20       }}\n\
         \x20   }}\n\
         }}\n\
         \n\
         impl From<{pascal}> for {wire}::{pascal} {{\n\
         \x20   fn from(v: {pascal}) -> Self {{\n\
         \x20       Self {{\n\
         \x20           id: v.id,\n\
         {to_wire}\n\
         \x20           // The generated message's store for fields it did not recognise on\n\
         \x20           // the wire. Empty, because this message is built here rather than\n\
         \x20           // relayed, so there is nothing unrecognised to carry.\n\
         \x20           {unknown}: {unknown_default},\n\
         \x20       }}\n\
         \x20   }}\n\
         }}\n\
         \n\
         impl From<{pascal}> for Create{pascal}Response {{\n\
         \x20   fn from(v: {pascal}) -> Self {{\n\
         \x20       Self {{\n\
         \x20           data: {wire}::{pascal}::from(v).into(),\n\
         \x20           {unknown}: {unknown_default},\n\
         \x20       }}\n\
         \x20   }}\n\
         }}\n\
         \n\
         impl From<{pascal}> for Get{pascal}Response {{\n\
         \x20   fn from(v: {pascal}) -> Self {{\n\
         \x20       Self {{\n\
         \x20           data: {wire}::{pascal}::from(v).into(),\n\
         \x20           {unknown}: {unknown_default},\n\
         \x20       }}\n\
         \x20   }}\n\
         }}\n\
         \n\
         impl From<{pascal}> for Update{pascal}Response {{\n\
         \x20   fn from(v: {pascal}) -> Self {{\n\
         \x20       Self {{\n\
         \x20           data: {wire}::{pascal}::from(v).into(),\n\
         \x20           {unknown}: {unknown_default},\n\
         \x20       }}\n\
         \x20   }}\n\
         }}\n\
         \n\
         /// Every `{snake}` a list returns.\n\
         ///\n\
         /// A function rather than `impl From<Vec<{pascal}>> for List{pascal}sResponse`,\n\
         /// which the orphan rule rejects: `Vec` and the response are both foreign to this\n\
         /// crate, and a local type *inside* a foreign type constructor does not make the\n\
         /// parameter local. It is the one conversion in this file that cannot be a `From`.\n\
         pub fn list_response(rows: Vec<{pascal}>) -> List{pascal}sResponse {{\n\
         \x20   List{pascal}sResponse {{\n\
         \x20       data: rows.into_iter().map({wire}::{pascal}::from).collect(),\n\
         \x20       {unknown}: {unknown_default},\n\
         \x20   }}\n\
         }}\n"
    )
}

/// `<snake>/errors.rs`.
fn errors(names: &Names) -> String {
    let Names { snake, pascal, .. } = names;
    format!(
        "use connectrpc::ConnectError;\n\
         \n\
         #[derive(Debug, thiserror::Error)]\n\
         pub enum {pascal}Error {{\n\
         \x20   /// Anything the database refused. The cause is logged; the wire error\n\
         \x20   /// deliberately says nothing about it, because a database message can carry\n\
         \x20   /// schema details and occasionally data.\n\
         \x20   #[error(\"the {snake} store is unavailable\")]\n\
         \x20   Store(#[from] sea_orm::DbErr),\n\
         \x20   /// No row with that id.\n\
         \x20   ///\n\
         \x20   /// Separate from `Store` because it is the one database outcome that is not a\n\
         \x20   /// failure: the caller asked about something that is not there, which is a\n\
         \x20   /// `not_found` on the wire rather than an `internal`.\n\
         \x20   #[error(\"no {snake} with that id\")]\n\
         \x20   NotFound,\n\
         }}\n\
         \n\
         impl From<{pascal}Error> for ConnectError {{\n\
         \x20   fn from(error: {pascal}Error) -> Self {{\n\
         \x20       match error {{\n\
         \x20           {pascal}Error::Store(ref cause) => {{\n\
         \x20               tracing::error!(%cause, \"{snake} store failure\");\n\
         \x20               Self::internal(error.to_string())\n\
         \x20           }}\n\
         \x20           {pascal}Error::NotFound => Self::not_found(error.to_string()),\n\
         \x20       }}\n\
         \x20   }}\n\
         }}\n"
    )
}

/// `<snake>/models.rs` — the database half of the conversions.
///
/// `infrastructure` is the list [`schema::fields_from_entity`] read off the entity, never a
/// fixed one: each is spelled `NotSet` by name, and naming a column the entity does not have is
/// E0560 on any table a hand-written migration made without it.
fn models(names: &Names, fields: &[Field], infrastructure: &[String]) -> String {
    let Names {
        snake,
        pascal,
        table,
        ..
    } = names;

    let set_fields = |indent: &str, receiver: &str| -> String {
        fields
            .iter()
            .map(|field| {
                format!(
                    "{indent}{receiver}{}: ActiveValue::Set(v.{}),",
                    field.name, field.name
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let assign_fields = fields
        .iter()
        .map(|field| {
            format!(
                "        self.{} = ActiveValue::Set(v.{});",
                field.name, field.name
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let read_fields = fields
        .iter()
        .map(|field| format!("            {}: v.{},", field.name, field.name))
        .collect::<Vec<_>>()
        .join("\n");
    let into_active = set_fields("            ", "");
    // No comment over an empty list: an entity with none of the infrastructure columns has
    // nothing left to the database, and a comment saying otherwise would sit over nothing.
    let left_to_database = if infrastructure.is_empty() {
        String::new()
    } else {
        let not_set = infrastructure
            .iter()
            .map(|column| format!("            {column}: ActiveValue::NotSet,"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "\n            // `NotSet`, so the database supplies these: the key on insert, and a\
             \n            // stamp from the default its migration installed — which setting it\
             \n            // here would override.\n{not_set}"
        )
    };

    format!(
        "//! Behaviour hung off the generated `{table}` entity.\n\
         //!\n\
         //! `entities/` is rewritten wholesale by `just services/<svc>/entities`, so nothing\n\
         //! worth keeping lives there. Coherence is per-crate rather than per-module, so a\n\
         //! foreign trait on a local type can be implemented from here instead — which is\n\
         //! also why entities live in the service rather than in a shared crate.\n\
         \n\
         use sea_orm::ActiveValue;\n\
         use sea_orm::entity::prelude::*;\n\
         \n\
         use crate::entities::{table}::{{ActiveModel, Model}};\n\
         use crate::{snake}::domain::{{{pascal}, {pascal}Dto}};\n\
         \n\
         /// Required by `DeriveEntityModel` whether or not it does anything.\n\
         ///\n\
         /// Deliberately empty: `created_at` and `updated_at` have database defaults, and a\n\
         /// hook here would be bypassed by `update_many`, raw SQL, migrations and anything\n\
         /// connecting outside this process — so the database is the right place for that\n\
         /// rule. sea-orm-cli is run with `--impl-active-model-behavior=false` precisely so\n\
         /// that this is the only definition.\n\
         impl ActiveModelBehavior for ActiveModel {{}}\n\
         \n\
         impl From<{pascal}Dto> for ActiveModel {{\n\
         \x20   fn from(v: {pascal}Dto) -> Self {{\n\
         \x20       Self {{\n\
         {into_active}{left_to_database}\n\
         \x20       }}\n\
         \x20   }}\n\
         }}\n\
         \n\
         impl From<Model> for {pascal} {{\n\
         \x20   fn from(v: Model) -> Self {{\n\
         \x20       Self {{\n\
         \x20           id: v.id,\n\
         {read_fields}\n\
         \x20       }}\n\
         \x20   }}\n\
         }}\n\
         \n\
         impl ActiveModel {{\n\
         \x20   /// Overwrite every writable column from a payload.\n\
         \x20   ///\n\
         \x20   /// Every field, unconditionally: a partial update needs the contract to\n\
         \x20   /// distinguish \"absent\" from \"empty\", which proto3 does only for an\n\
         \x20   /// `optional` field. Making that distinction is a decision about the API\n\
         \x20   /// rather than about this function, so what is generated is the total version.\n\
         \x20   pub fn apply_update(&mut self, v: {pascal}Dto) {{\n\
         {assign_fields}\n\
         \x20   }}\n\
         }}\n"
    )
}

/// The `ctx.principal()?` line, with the explanation that travels with it.
///
/// Repeated per handler on purpose — four times here, once more for every method `new-rpc`
/// adds — because whoever reads one handler has not read the other three. It exists because
/// the classification written into `auth.rs` says PROTECTED, and a table and a handler that
/// disagree is how a table stops being read:
/// `every_protected_method_refuses_an_anonymous_caller` is the only thing in the repository
/// that would find them out, and it runs long after whoever wrote the method has moved on.
///
/// Shared with `new-rpc` rather than copied into it: a method added later that gated without
/// saying why would be one handler in five that reads as an unexplained line.
pub(super) fn gate(service: &str, snake: &str) -> String {
    format!(
        "\x20       // Classified PROTECTED in services/{service}/tests/{service}/auth.rs: a\n\
         \x20       // protected method answers no caller without an identity, and this line\n\
         \x20       // is the whole meaning of that. Nothing about a {snake} row depends on\n\
         \x20       // who asked, so the principal is checked and then not used.\n\
         \x20       let _principal = ctx.principal()?;\n"
    )
}

/// `<snake>/service/v1.rs`.
#[expect(
    clippy::too_many_lines,
    reason = "one template of the handler file; splitting it would hide the shape of what \
              is written"
)]
fn implementation(names: &Names) -> String {
    let Names {
        service,
        snake,
        pascal,
        table,
    } = names;
    let wire = names.wire();

    let gate = gate(service, snake);

    format!(
        "use connectrpc::{{RequestContext, Response, ServiceRequest, ServiceResult}};\n\
         use identity::IdentityContext as _;\n\
         use proto::r#gen::connect::{snake}::v1::{pascal}Service;\n\
         use {wire}::{{\n\
         \x20   Create{pascal}Request, Create{pascal}Response, Get{pascal}Request, Get{pascal}Response,\n\
         \x20   List{pascal}sRequest, List{pascal}sResponse, Update{pascal}Request, Update{pascal}Response,\n\
         }};\n\
         use sea_orm::{{ActiveModelTrait as _, DatabaseConnection, EntityTrait as _, IntoActiveModel as _}};\n\
         \n\
         use crate::entities::{table};\n\
         use crate::{snake}::domain::{{self, {pascal}Dto}};\n\
         use crate::{snake}::errors::{pascal}Error;\n\
         \n\
         pub struct {pascal} {{\n\
         \x20   db: DatabaseConnection,\n\
         }}\n\
         \n\
         impl {pascal} {{\n\
         \x20   #[must_use]\n\
         \x20   pub const fn new(db: DatabaseConnection) -> Self {{\n\
         \x20       Self {{ db }}\n\
         \x20   }}\n\
         \n\
         \x20   /// Read one row, or say it is not there.\n\
         \x20   async fn row(&self, id: i32) -> Result<{table}::Model, {pascal}Error> {{\n\
         \x20       {table}::Entity::find_by_id(id)\n\
         \x20           .one(&self.db)\n\
         \x20           .await?\n\
         \x20           .ok_or({pascal}Error::NotFound)\n\
         \x20   }}\n\
         }}\n\
         \n\
         impl {pascal}Service for {pascal} {{\n\
         \x20   #[tracing::instrument(name = \"{snake}.create\", skip_all)]\n\
         \x20   async fn create_{snake}(\n\
         \x20       &self,\n\
         \x20       ctx: RequestContext,\n\
         \x20       request: ServiceRequest<'_, Create{pascal}Request>,\n\
         \x20   ) -> ServiceResult<Create{pascal}Response> {{\n\
         {gate}\
         \x20       // Owned before anything else: `to_owned_message` walks the already-decoded\n\
         \x20       // view and allocates the string fields, so this is a copy rather than a\n\
         \x20       // second parse of the request body — and the response may not borrow from\n\
         \x20       // the request.\n\
         \x20       let dto = {pascal}Dto::from(request.to_owned_message());\n\
         \n\
         \x20       let row = {table}::ActiveModel::from(dto)\n\
         \x20           .insert(&self.db)\n\
         \x20           .await\n\
         \x20           .map_err({pascal}Error::Store)?;\n\
         \n\
         \x20       Response::ok(Create{pascal}Response::from(domain::{pascal}::from(row)))\n\
         \x20   }}\n\
         \n\
         \x20   #[tracing::instrument(name = \"{snake}.get\", skip_all)]\n\
         \x20   async fn get_{snake}(\n\
         \x20       &self,\n\
         \x20       ctx: RequestContext,\n\
         \x20       request: ServiceRequest<'_, Get{pascal}Request>,\n\
         \x20   ) -> ServiceResult<Get{pascal}Response> {{\n\
         {gate}\
         \x20       let row = self.row(request.to_owned_message().id).await?;\n\
         \x20       Response::ok(Get{pascal}Response::from(domain::{pascal}::from(row)))\n\
         \x20   }}\n\
         \n\
         \x20   #[tracing::instrument(name = \"{snake}.list\", skip_all)]\n\
         \x20   async fn list_{snake}s(\n\
         \x20       &self,\n\
         \x20       ctx: RequestContext,\n\
         \x20       _request: ServiceRequest<'_, List{pascal}sRequest>,\n\
         \x20   ) -> ServiceResult<List{pascal}sResponse> {{\n\
         {gate}\
         \x20       // Unbounded, which is wrong for a table that grows. Pagination is a\n\
         \x20       // contract decision — a page token in the request, a next token in the\n\
         \x20       // response — so it is left to be made rather than guessed at here.\n\
         \x20       let rows = {table}::Entity::find()\n\
         \x20           .all(&self.db)\n\
         \x20           .await\n\
         \x20           .map_err({pascal}Error::Store)?;\n\
         \n\
         \x20       let {snake}s: Vec<domain::{pascal}> =\n\
         \x20           rows.into_iter().map(domain::{pascal}::from).collect();\n\
         \n\
         \x20       Response::ok(domain::list_response({snake}s))\n\
         \x20   }}\n\
         \n\
         \x20   #[tracing::instrument(name = \"{snake}.update\", skip_all)]\n\
         \x20   async fn update_{snake}(\n\
         \x20       &self,\n\
         \x20       ctx: RequestContext,\n\
         \x20       request: ServiceRequest<'_, Update{pascal}Request>,\n\
         \x20   ) -> ServiceResult<Update{pascal}Response> {{\n\
         {gate}\
         \x20       let request = request.to_owned_message();\n\
         \n\
         \x20       // Read before write, so that updating something that is not there is a\n\
         \x20       // `not_found` rather than an insert nobody asked for.\n\
         \x20       let mut row = self.row(request.id).await?.into_active_model();\n\
         \x20       row.apply_update({pascal}Dto::from(request));\n\
         \n\
         \x20       let row = row.update(&self.db).await.map_err({pascal}Error::Store)?;\n\
         \n\
         \x20       Response::ok(Update{pascal}Response::from(domain::{pascal}::from(row)))\n\
         \x20   }}\n\
         }}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Names {
        Names {
            service: "api".to_owned(),
            snake: "lake".to_owned(),
            pascal: "Lake".to_owned(),
            table: "lakes".to_owned(),
        }
    }

    /// The doc-comment lines of a file, in order.
    fn comments_of(text: &str) -> Vec<&str> {
        text.lines()
            .filter(|line| line.trim_start().starts_with("///"))
            .collect()
    }

    /// The quoted entries of a `const NAME: &[&str] = &[..]` list, as written.
    fn entries(source: &str, list: &str) -> Vec<String> {
        let (_, rest) = source
            .split_once(&format!("const {list}:"))
            .unwrap_or_else(|| panic!("no `const {list}:`"));
        let (list, _) = rest.split_once("];").expect("a closed list");
        list.split('"')
            .skip(1)
            .step_by(2)
            .map(ToOwned::to_owned)
            .collect()
    }

    fn fields() -> Vec<Field> {
        schema::parse_fields(&["name:string".to_owned(), "deactivated:bool".to_owned()])
            .expect("valid fields")
    }

    #[test]
    fn the_contract_numbers_every_message_from_one() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let proto = contract(&names(), &fields());

        // The entity message reserves 1 for the id, so its fields start at 2; a request
        // that carries no id starts at 1. A generator that got this wrong would produce a
        // contract that compiles and renumbers a field on the next run.
        assert!(
            proto.contains(
                "message Lake {\n  int32 id = 1;\n  string name = 2;\n  bool deactivated = 3;\n}"
            ),
            "{proto}"
        );
        assert!(
            proto.contains(
                "message CreateLakeRequest {\n  string name = 1;\n  bool deactivated = 2;\n}"
            ),
            "{proto}"
        );
        assert!(
            proto.contains("message UpdateLakeRequest {\n  int32 id = 1;\n  string name = 2;"),
            "{proto}"
        );

        // buf's STANDARD lint: one dedicated Request/Response per RPC, service suffixed.
        for method in ["CreateLake", "GetLake", "ListLakes", "UpdateLake"] {
            assert!(
                proto.contains(&format!(
                    "rpc {method}({method}Request) returns ({method}Response);"
                )),
                "{method} is not wired in the service block:\n{proto}"
            );
            assert!(
                proto.contains(&format!("message {method}Request")),
                "{proto}"
            );
            assert!(
                proto.contains(&format!("message {method}Response")),
                "{proto}"
            );
        }
    }

    /// The infrastructure columns of the fixture entity `just check-template`
    /// generates its slice from — read off it, as `new-feature` reads them, not listed here.
    fn infrastructure() -> Vec<String> {
        schema::fields_from_entity(include_str!("../../fixtures/entities/probe_lakes.rs"))
            .expect("the fixture is a readable entity")
            .infrastructure
    }

    #[test]
    fn the_round_trip_carries_every_field() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // The defect this generator exists to prevent: a field that reaches three of the
        // four conversions and is silently dropped by the fourth.
        //
        // No longer the only thing that would notice. Every generated struct literal now
        // names every field, so a dropped one is E0063 in `just check-template`
        // (`no_generated_conversion_leaves_a_field_to_default` keeps it that way). This stays
        // because it fails in `cargo test`, seconds after the edit, naming the field — where
        // the compiler only answers once the gate has generated and built a whole service.
        let domain = domain_module(&names(), &fields());
        let models = models(&names(), &fields(), &infrastructure());

        for field in fields() {
            let name = &field.name;
            assert_eq!(
                domain.matches(&format!("{name}: v.{name},")).count(),
                3,
                "{name} must move through both request conversions and the wire one:\n{domain}"
            );
            assert!(
                models.contains(&format!("{name}: ActiveValue::Set(v.{name}),")),
                "{name} is not written to the row:\n{models}"
            );
            assert!(
                models.contains(&format!("self.{name} = ActiveValue::Set(v.{name});")),
                "{name} is not covered by apply_update:\n{models}"
            );
            assert!(
                models.contains(&format!("{name}: v.{name},")),
                "{name} is not read back out of the row:\n{models}"
            );
        }
    }

    #[test]
    fn no_generated_conversion_leaves_a_field_to_default() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // RT-27. `..Default::default()` fills whatever a struct literal does not name, which
        // includes a field the generator forgot — so a conversion ending in it compiles with a
        // column never written or a response that is always empty. Every literal names every
        // field instead, and this is what stops the struct-update syntax coming back.
        let domain = domain_module(&names(), &fields());
        let models = models(&names(), &fields(), &infrastructure());
        for (file, text) in [("domain/mod.rs", &domain), ("models.rs", &models)] {
            assert!(
                !text.contains("..Default::default()"),
                "{file} leaves a field to the struct-update syntax:\n{text}"
            );
        }

        // What replaced it: the wire message, the three response wrappers and the list.
        assert_eq!(
            domain
                .matches(&format!("{UNKNOWN_FIELDS}: {UNKNOWN_FIELDS_DEFAULT},"))
                .count(),
            5,
            "every wire literal names the unknown-fields store, with its type spelled out:\n{domain}"
        );
        for column in infrastructure() {
            assert!(
                models.contains(&format!("{column}: ActiveValue::NotSet,")),
                "{column} is not left to the database by name:\n{models}"
            );
        }
    }

    #[test]
    fn a_feature_name_with_an_underscore_is_backticked_in_every_doc_comment() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // RT-43. `clippy::doc_markdown` reports an underscored word in a doc comment as "item
        // in documentation is missing backticks", and every crate's `check` recipe promotes
        // that to an error. It fires on `probe_lake` — the feature the archetype probe
        // generates, and the name the ticket's own reproduction used — and stays silent on
        // `lake`, the name every other test here renders with and the name the documentation's
        // example uses. A fix checked only against `lake` proves nothing about the lint.
        let names = Names {
            service: "api".to_owned(),
            snake: "probe_lake".to_owned(),
            pascal: "ProbeLake".to_owned(),
            table: "probe_lakes".to_owned(),
        };
        let files = [
            ("mod.rs", slice_mod(&names)),
            ("domain/mod.rs", domain_module(&names, &fields())),
            ("errors.rs", errors(&names)),
            ("models.rs", models(&names, &fields(), &infrastructure())),
            ("service/v1.rs", implementation(&names)),
        ];
        for (file, text) in files {
            for line in prose_of(&text) {
                assert!(
                    !line.contains(&names.snake),
                    "{file} writes `{}` into a doc comment outside backticks: {line}",
                    names.snake,
                );
            }
        }
    }

    /// The doc-comment lines of a file — `//!` and `///` — with every backticked span
    /// removed, so what is left is the prose clippy reads as prose.
    fn prose_of(text: &str) -> Vec<String> {
        text.lines()
            .filter(|line| {
                let line = line.trim_start();
                line.starts_with("///") || line.starts_with("//!")
            })
            .map(|line| line.split('`').step_by(2).collect())
            .collect()
    }

    #[test]
    fn only_the_infrastructure_columns_the_entity_has_are_named() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // An entity a hand-written migration made, with no `updated_at`. Naming it anyway
        // would be E0560 in the generated `models.rs`, on a table the generator was told to use.
        let models = models(
            &names(),
            &fields(),
            &["id".to_owned(), "created_at".to_owned()],
        );
        assert!(
            models.contains("created_at: ActiveValue::NotSet,"),
            "{models}"
        );
        assert!(!models.contains("updated_at: ActiveValue"), "{models}");
    }

    #[test]
    fn the_list_conversion_is_a_function_because_the_orphan_rule_forbids_the_impl() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // `impl From<Vec<Lake>> for ListLakesResponse` reads as the obvious thing to write
        // and does not compile: both sides are foreign to the service crate, and a local
        // type inside `Vec<_>` does not make the parameter local. Generated once, it broke
        // the whole slice — so the shape is asserted rather than remembered.
        let domain = domain_module(&names(), &fields());
        assert!(
            domain.contains("pub fn list_response(rows: Vec<Lake>) -> ListLakesResponse"),
            "{domain}"
        );
        // At the start of a line, so the doc comment that explains *why* there is no such
        // impl does not read as one.
        assert!(
            !domain
                .lines()
                .any(|line| line.starts_with("impl From<Vec<")),
            "the orphan rule rejects this:\n{domain}"
        );
        assert!(
            implementation(&names()).contains("domain::list_response("),
            "the handler must call it"
        );
    }

    #[test]
    fn the_dto_has_no_id_and_the_domain_type_does() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let domain = domain_module(&names(), &fields());
        let (before_dto, from_dto) = domain
            .split_once("pub struct LakeDto {")
            .expect("the DTO is declared");
        assert!(
            before_dto.contains("pub struct Lake {\n    pub id: i32,"),
            "{domain}"
        );

        let dto_body = from_dto.split_once('}').expect("the DTO is closed").0;
        assert!(
            !dto_body.contains("pub id"),
            "a write payload must not carry an identity:\n{dto_body}"
        );
    }

    #[test]
    fn the_row_is_read_before_it_is_written() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // An update that skips the read is an upsert, which is a different method with a
        // different contract. The generated handler must not quietly be one.
        let implementation = implementation(&names());
        let update = implementation
            .split_once("async fn update_lake")
            .expect("an update method")
            .1;
        let read_at = update.find("self.row(request.id)").expect("a read");
        let write_at = update.find("row.update(&self.db)").expect("a write");
        assert!(read_at < write_at, "{update}");
        assert!(update.contains("apply_update"), "{update}");
    }

    #[test]
    fn the_classification_names_are_the_contract_methods() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // The two spellings of the same four methods — `proto/<snake>/v1/<snake>.proto` and
        // a list in a test file in another crate — have to agree, or the router answers to
        // a name no one classified and the failure lands in `auth.rs`, in a crate this
        // generator never opened, blaming somebody who never touched either file.
        let methods = routed_methods(&names());
        assert_eq!(
            methods,
            [
                "lake.v1.LakeService/CreateLake",
                "lake.v1.LakeService/GetLake",
                "lake.v1.LakeService/ListLakes",
                "lake.v1.LakeService/UpdateLake",
            ]
        );

        let contract = contract(&names(), &fields());
        for (rpc, method) in rpcs(&names()).iter().zip(&methods) {
            assert!(
                contract.contains(&format!("rpc {rpc}({rpc}Request) returns ({rpc}Response);")),
                "{rpc} is not declared in the contract:\n{contract}"
            );
            assert!(
                method.ends_with(&format!("/{rpc}")),
                "{method} does not name {rpc}"
            );
        }
        assert_eq!(rpcs(&names()).len(), methods.len());
    }

    #[test]
    fn every_generated_handler_asks_for_the_principal() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // A `PROTECTED` entry next to a handler that does not call `ctx.principal()?` is a
        // table that lies, and `every_protected_method_refuses_an_anonymous_caller` is the
        // only thing in the repository that would notice — long after whoever wrote it.
        let implementation = implementation(&names());

        assert_eq!(
            implementation.matches("ctx.principal()?").count(),
            4,
            "{implementation}"
        );
        assert!(
            implementation.contains("use identity::IdentityContext as _;"),
            "without the trait in scope, `principal` does not resolve:\n{implementation}"
        );
        // `_ctx` is how a handler that does not gate is written. None may stay that way.
        assert!(
            !implementation.contains("_ctx: RequestContext"),
            "a handler takes the context without using it, so it is not gated:\n{implementation}"
        );
        // The comment names the file the classification lives in, with the real service in
        // it, so a reader of the handler can go and look.
        assert!(
            implementation.contains("Classified PROTECTED in services/api/tests/api/auth.rs"),
            "{implementation}"
        );
    }

    #[test]
    fn a_real_classification_table_takes_the_new_names_and_loses_nothing() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // Not a fixture: the two tables that actually ship. The example service's is
        // hand-written and commented at length; the skeleton's is what every product
        // rendered from `just add service` inherits, sentinels and all.
        let root = repo::root();
        for auth in [
            root.join("services/api/tests/api/auth.rs"),
            root.join("templates/service/services/__name__/tests/__name__/auth.rs"),
        ] {
            let source = std::fs::read_to_string(&auth)
                .unwrap_or_else(|error| panic!("{}: {error}", repo::rel(&root, &auth)));
            let public = entries(&source, "PUBLIC");
            let protected = entries(&source, "PROTECTED");
            assert!(
                !protected.is_empty() && !public.is_empty(),
                "{} has no classification lists to write into",
                repo::rel(&root, &auth)
            );

            let mut edited = source.clone();
            for method in routed_methods(&names()) {
                edited = rustedit::insert_into_const_list(&edited, "PROTECTED", &method)
                    .unwrap_or_else(|error| panic!("{}: {error}", repo::rel(&root, &auth)));
            }

            // Every doc comment, in order, character for character.
            assert_eq!(
                comments_of(&source),
                comments_of(&edited),
                "{} lost a comment: {}",
                repo::rel(&root, &auth),
                edited
            );
            // What was classified before is classified still — `Ping` and `Whoami` in the
            // skeleton, the counter and health methods in the example. The four new names
            // are appended to one list; no other list and no earlier entry moves.
            assert_eq!(
                public,
                entries(&edited, "PUBLIC"),
                "{}",
                repo::rel(&root, &auth)
            );
            let after = entries(&edited, "PROTECTED");
            assert_eq!(
                after.len(),
                protected.len() + 4,
                "{}",
                repo::rel(&root, &auth)
            );
            assert_eq!(&after[..protected.len()], protected.as_slice());
        }
    }

    #[test]
    fn not_found_is_not_an_internal_error() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let errors = errors(&names());
        assert!(errors.contains("NotFound => Self::not_found"), "{errors}");
        assert!(errors.contains("Store(ref cause)"), "{errors}");
        // The database's own message must not reach the wire.
        assert!(
            !errors.contains("Self::internal(cause"),
            "a DbErr can carry schema details and occasionally data:\n{errors}"
        );
    }

    // -----------------------------------------------------------------
    // RT-38: a generation that refuses must not have written anything.
    //
    // `new_feature` already knew the principle — the comment on the classification pre-check in
    // `scaffold_slice` says a service with no classification table has to refuse "the whole
    // generator instead of getting half a feature slice on disk and a compile error pointed at a
    // test file". It applied it to one of the four files this generator edits. The other three —
    // `lib.rs`, `startup.rs` and `Cargo.toml` — were still resolved after the seven new files had
    // landed, which leaves a whole feature slice and a contract `lib.rs` does not declare: a
    // service that does not compile, from a command that reported nothing.
    //
    // The fixture is the service skeleton plus `add-database`'s core plus the committed entity
    // fixture, because `new_feature` refuses a service with no schema plumbing and reads its field
    // list off `src/entities/<table>.rs`. The feature is the probe's (`probe-lake` →
    // `probe_lakes`) so the entity it reads is the one `just check-template` already
    // stands in, not one invented here.
    // -----------------------------------------------------------------

    use crate::api::fixtures::{service_with_entities, tree};

    const FEATURE: &str = "probe-lake";

    /// The seven files a slice is made of, as this test expects to find them.
    fn slice_files(service: &Service) -> Vec<std::path::PathBuf> {
        [
            service.root.join("proto/probe_lake/v1/probe_lake.proto"),
            service.src("probe_lake/mod.rs"),
            service.src("probe_lake/domain/mod.rs"),
            service.src("probe_lake/errors.rs"),
            service.src("probe_lake/models.rs"),
            service.src("probe_lake/service/mod.rs"),
            service.src("probe_lake/service/v1.rs"),
        ]
        .into_iter()
        .collect()
    }

    /// The four existing files a slice is mounted into.
    fn mount_points(service: &Service) -> Vec<std::path::PathBuf> {
        vec![
            service.src("lib.rs"),
            service.src("startup.rs"),
            service.dir.join("Cargo.toml"),
            service.auth_test(),
        ]
    }

    #[test]
    fn a_converted_service_takes_a_slice_and_mounts_it() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // The control. Without it the refusal tests below could be passing because the fixture is
        // unusable rather than because the refusal is clean, and "it refused" is not by itself
        // evidence of anything.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = service_with_entities(dir.path(), FEATURE);

        let outcome =
            scaffold_slice(&service, FEATURE, None).expect("a converted service must take a slice");

        for path in slice_files(&service) {
            assert!(path.is_file(), "{} was not written", service.rel(&path));
        }
        assert_eq!(
            outcome.amended,
            mount_points(&service),
            "the four files a slice is mounted into"
        );

        let lib = std::fs::read_to_string(service.src("lib.rs")).expect("lib.rs");
        assert!(lib.contains("pub mod probe_lake;"), "{lib}");

        let startup = std::fs::read_to_string(service.src("startup.rs")).expect("startup.rs");
        assert!(startup.contains("probe_lake"), "{startup}");
        assert!(
            startup.contains("probe_lake::service::v1::ProbeLake::new(db)"),
            "{startup}"
        );
        // The placeholder `add-database` left is gone, because this slice is the first feature
        // that reads the connection.
        assert!(!startup.contains("let _ = &db;"), "{startup}");

        let manifest = std::fs::read_to_string(service.dir.join("Cargo.toml")).expect("manifest");
        assert!(manifest.contains("thiserror.workspace"), "{manifest}");

        let auth = std::fs::read_to_string(service.auth_test()).expect("auth.rs");
        for method in [
            "probe_lake.v1.ProbeLakeService/CreateProbeLake",
            "probe_lake.v1.ProbeLakeService/GetProbeLake",
            "probe_lake.v1.ProbeLakeService/ListProbeLakes",
            "probe_lake.v1.ProbeLakeService/UpdateProbeLake",
        ] {
            assert!(
                entries(&auth, "PROTECTED").contains(&method.to_owned()),
                "{auth}"
            );
        }
    }

    #[test]
    fn a_manifest_with_no_dependencies_table_is_refused_without_writing_the_slice() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = service_with_entities(dir.path(), FEATURE);
        std::fs::write(
            service.dir.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .expect("rewrite the manifest without a [dependencies] table");

        let before = tree(dir.path());
        let error = scaffold_slice(&service, FEATURE, None)
            .expect_err("a manifest with no [dependencies] is refused");
        let message = format!("{error:#}");
        assert!(message.contains("[dependencies]"), "{message}");
        assert!(message.contains("Cargo.toml"), "{message}");

        // The defect itself, named rather than inferred from a count: before this change the
        // refusal arrived after `writes.commit`, so the tree had a contract and six modules — and a
        // `lib.rs` that declared none of them. This assert is the one that fails on the old order.
        let landed: Vec<String> = slice_files(&service)
            .into_iter()
            .filter(|path| path.exists())
            .map(|path| service.rel(&path))
            .collect();
        assert!(
            landed.is_empty(),
            "a refused generation left {landed:?} behind: {message}"
        );
        assert_eq!(
            tree(dir.path()),
            before,
            "a refused generation must leave the service exactly as it found it: {message}"
        );
    }

    #[test]
    fn a_startup_that_does_not_reach_its_features_through_a_use_group_is_refused_without_writing_the_slice()
     {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = service_with_entities(dir.path(), FEATURE);
        let startup = service.src("startup.rs");
        let original = std::fs::read_to_string(&startup).expect("startup.rs");
        // A real anchor: `register` brings the slice into scope through the `use crate::{..}`
        // group `templates/service` writes, and a product that has since split that group into
        // one line per module — which is what rustfmt does to a group with a path in it — has no
        // group left to add a name to.
        let (head, tail) = original
            .split_once("use crate::{")
            .expect("the crate use group");
        let (group, rest) = tail.split_once('}').expect("a closed group");
        let spelled_out = group
            .split(',')
            .map(|name| format!("use crate::{};", name.trim()))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&startup, format!("{head}{spelled_out}\n{rest}"))
            .expect("split the use group");

        let before = tree(dir.path());
        let error = scaffold_slice(&service, FEATURE, None)
            .expect_err("a startup.rs with no use group is refused");
        let message = format!("{error:#}");
        assert!(message.contains("use crate::{"), "{message}");
        assert!(message.contains("startup.rs"), "{message}");

        let landed: Vec<String> = slice_files(&service)
            .into_iter()
            .filter(|path| path.exists())
            .map(|path| service.rel(&path))
            .collect();
        assert!(
            landed.is_empty(),
            "a refused generation left {landed:?} behind: {message}"
        );
        assert_eq!(
            tree(dir.path()),
            before,
            "a refused generation must leave the service exactly as it found it: {message}"
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
        // Not a test of the defect. A pin on the behaviour the fix must not lose: `amend` returned
        // early when a rewrite produced identical bytes, and the planned half has to keep doing
        // that. A service whose manifest already lists thiserror is an ordinary thing to meet.
        //
        // The mtime is asserted as well as the contents because the two are different claims: a
        // generator that rewrote identical bytes would satisfy a contents-only assert, and the
        // reason not to is a build watching mtimes.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = service_with_entities(dir.path(), FEATURE);
        let manifest = service.dir.join("Cargo.toml");
        let original = std::fs::read_to_string(&manifest).expect("manifest");
        let pre_amended = add_dependencies(&original, &["thiserror"]).expect("a manifest");
        assert_ne!(pre_amended, original, "thiserror must not already be there");
        std::fs::write(&manifest, &pre_amended).expect("add the dependency ahead of time");
        let before = std::fs::metadata(&manifest)
            .expect("manifest metadata")
            .modified()
            .expect("a manifest with an mtime");

        scaffold_slice(&service, FEATURE, None)
            .expect("a service with thiserror still takes a slice");

        assert_eq!(
            std::fs::read_to_string(&manifest).expect("manifest"),
            pre_amended,
            "the generator must not rewrite a file that already says what it would write"
        );
        let after = std::fs::metadata(&manifest)
            .expect("manifest metadata")
            .modified()
            .expect("a manifest with an mtime");
        assert_eq!(after, before, "the file was rewritten, not left alone");
    }
}
