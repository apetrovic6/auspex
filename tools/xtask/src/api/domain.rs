//! `cargo xtask new-domain <service> <feature> <name>`.
//!
//! [`super::feature`] writes a slice's first domain types. This writes the next ones: one
//! more DTO, in its own file, for an RPC the contract has grown since. It is the shape a
//! slice takes once it has more than a create and an update — one file per payload,
//! re-exported from `domain/mod.rs` — rather than one module that accumulates every
//! conversion the feature has ever needed.
//!
//! Unlike the other generators this one reads the contract rather than writing it: the
//! `.proto` is the input, because by the time a second DTO is wanted the method it serves
//! has already been designed. The fields come from the request message, which is what makes
//! the conversion exhaustive rather than a stub.
//!
//! # Why this one classifies nothing
//!
//! [`new_feature`] registers methods, and registering one is a decision about who may call
//! it, so it writes each of them into `PROTECTED` in `tests/<svc>/auth.rs` (see its own
//! module docs, and ADR 0012). This generator does the same thing with the same care — and
//! there is nothing for it to classify, which is the difference rather than a gap.
//!
//! It writes no RPC and no handler, so it registers nothing with `register_services`. The
//! method behind the payload it builds a DTO for is not mounted by this generator, and may
//! not be mounted at all yet. Putting its name in `PROTECTED` anyway would break
//! `every_registered_method_is_classified` from the other direction — a classification with
//! no method behind it — and a table with entries in it that route nowhere reads as a
//! decision somebody made.
//!
//! # A response the payload fills none of (RT-28)
//!
//! The outbound `From<Dto> for <Pascal>Response` takes each response field from the request
//! field of the same name. When the response has fields and not one of them has such a
//! counterpart, there is no mapping to generate. What used to be generated was
//! `Self { ..Default::default() }`: it compiled, passed every gate, and answered an empty
//! response on every call. `new-rpc` takes this path too — it renders the DTO file through
//! [`body`] — and `just check-template` walks it once, through `new-domain`; the
//! method it generates with `new-rpc` now maps one field and so takes the partial shape below.
//!
//! The decision: that impl is generated, with a whole-body `todo!()` that says it is the mapping
//! a human has to write. It compiles, and it panics on first use instead of shipping an empty
//! answer. The option that lost was refusing to write the impl at all. That would have made
//! `new-rpc`'s generated handler, which says to answer with `<Pascal>Response::from(..)`
//! "generated beside the DTO", false on exactly the path where it matters; `todo!` is already
//! this generator's answer for a value it has no defensible choice for — the inbound conversion
//! uses it for message fields, and [`report_gaps`] argues for it; and it was the larger change to
//! a renderer two generators share. The `todo!()` is the whole body, not one per field, because a
//! diverging expression inside a struct literal leaves the rest of it unreachable.
//!
//! Two neighbouring cases are deliberately different. A response with no fields at all has
//! nothing to map and nothing missing, so it gets an ordinary body. A response some of whose
//! fields map is a skeleton a human completes: each unmapped field is written out beside a
//! comment as its own type's `default()`, spelled out in [`default_of`] for the reason the
//! spelling is not the shorter one, so the gap is in the file and not only on the terminal.
//! None of the three shapes ends in `..Default::default()`, so a mapped field the generator
//! drops is E0063, as it has been for `new-feature`'s conversions since RT-27.

use anyhow::{Context as _, Result, bail};

use super::schema::{self, Field, Message};
use super::{Service, Writes, read_for_edit, report, rustedit, write_planned};
use crate::repo;

/// Scaffold one DTO and its conversions from an existing contract.
///
/// # Errors
///
/// Returns an error if the feature or its contract is missing, if the contract has no
/// request message for `name`, or if the file already exists.
pub fn new_domain(service: &str, feature: &str, name: &str) -> Result<()> {
    let service = Service::open(service)?;

    let outcome = scaffold_dto(&service, feature, name)?;
    let amended: Vec<&std::path::Path> = outcome
        .amended
        .iter()
        .map(std::path::PathBuf::as_path)
        .collect();
    report(&service, &outcome.written, &amended)?;

    report_gaps(&outcome.request, outcome.response.as_ref(), &outcome.pascal);

    // Said out loud rather than left as an absence, because "the generator did not classify
    // this" and "this needed no classification" look identical from here, and only one of
    // them is true of `new-feature`. See the module docs.
    println!(
        "\n  Nothing classified in services/{}/tests/{}/auth.rs:",
        service.name, service.name
    );
    println!("    this generator registers no method, so there is no routing-table entry to");
    println!(
        "    put in `PROTECTED`. The RPC behind `{}` is classified by whoever",
        outcome.request.name
    );
    println!("    mounts it — and mounting it without naming it there makes");
    println!("    `every_registered_method_is_classified` fail with its name in the message.");

    println!("\nNext:");
    println!("  cargo check -p {}", service.name);
    Ok(())
}

/// What one DTO produced, so the reporting can stay outside the generation.
///
/// `Debug` because a test asserts on the failure side of [`scaffold_dto`], and
/// `Result::expect_err` needs the success type to be printable.
#[derive(Debug)]
struct Outcome {
    /// New files, in the order they were written.
    written: Vec<std::path::PathBuf>,
    /// Every file the generation meant to edit, whether its contents actually changed.
    amended: Vec<std::path::PathBuf>,
    /// The request message the DTO was built from, for the gap report.
    request: Message,
    /// The response message beside it, if the contract declares one.
    response: Option<Message>,
    /// `<Pascal>` of the DTO, for the same report.
    pascal: String,
}

/// The generation itself, and the half a test can drive.
///
/// `new_domain` takes a name and resolves it through `Service::open`, which goes through
/// `repo::root()` — `env!("CARGO_MANIFEST_DIR")`, fixed at compile time — so nothing that takes
/// a name can be pointed at a temporary tree. This takes the `Service`, and it is the whole job
/// minus the entry point's one guard and the closing output.
///
/// # Errors
///
/// Same as `new_domain`.
fn scaffold_dto(service: &Service, feature: &str, name: &str) -> Result<Outcome> {
    let feature_snake = feature.replace('-', "_");
    let slice = service.src(&feature_snake);
    if !slice.is_dir() {
        bail!(
            "no feature at {} — `cargo xtask new-feature {} {feature}` creates one",
            service.rel(&slice),
            service.name,
        );
    }

    let snake = name.replace('-', "_");
    let pascal = repo::to_pascal(name);
    let dto = format!("{pascal}Dto");

    let contract = service
        .root
        .join(format!("proto/{feature_snake}/v1/{feature_snake}.proto"));
    let text = std::fs::read_to_string(&contract)
        .map_err(|error| anyhow::anyhow!("could not read {}: {error}", service.rel(&contract)))?;
    let declared = schema::parse_proto(&text);
    let messages = &declared.messages;

    let request = format!("{pascal}Request");
    let Some(request_message) = declared.message(&request) else {
        bail!(
            "{} has no `message {request}`.\n\
             \n\
             This generator reads the contract rather than writing it: the DTO's fields are \
             the request's fields, which is what makes the conversion exhaustive instead of \
             a stub. Add the message and its RPC to {}, run `just proto::generate`, then run \
             this again.\n\
             \n\
             Messages it did find: {}",
            service.rel(&contract),
            service.rel(&contract),
            if messages.is_empty() {
                "none".to_owned()
            } else {
                messages
                    .iter()
                    .map(|message| message.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
    };

    let response_message = declared.message(&format!("{pascal}Response"));

    // `<feature>/domain/mod.rs` is resolved before the payload file is committed. `Writes::commit`
    // puts a new file on disk, so an `amend` after it is an `amend` that can still refuse — and a
    // refused `new-domain` then leaves a DTO file no module declares, invisible to the compiler and
    // invisible to the next run, which refuses to overwrite a file nobody asked for. This is the
    // shape `add-database` was converted to in RT-31, for the same reason.
    let registry = service.src(&format!("{feature_snake}/domain/mod.rs"));
    let registered = read_for_edit(service, &registry)
        .and_then(|source| register(&source, &snake, &dto))
        .with_context(|| format!("editing {}", service.rel(&registry)))?;
    let planned = vec![(registry, registered)];

    let mut writes = Writes::default();
    writes.add(
        service.src(&format!("{feature_snake}/domain/{snake}.rs")),
        body(
            &feature_snake,
            &pascal,
            &dto,
            request_message,
            response_message,
        ),
    );
    let written = writes.commit(service)?;
    write_planned(service, &planned)?;

    Ok(Outcome {
        written,
        amended: planned.into_iter().map(|(path, _)| path).collect(),
        request: request_message.clone(),
        response: response_message.cloned(),
        pascal,
    })
}

/// Declare one payload module in `domain/mod.rs` and re-export its DTO.
///
/// Pure, and shared with `new-rpc`: that generator writes a payload module too, and it
/// computes every edit before it writes any of them — so what it needs is the text, not a
/// function that has already touched the disk.
pub(super) fn register(source: &str, snake: &str, dto: &str) -> Result<String> {
    // The anchor is only consulted for the *first* DTO — after that there is a block of
    // declarations to sort into — and it has to be a line nothing is attached to.
    // `pub struct` is not: every struct in the generated `domain/mod.rs` carries a doc
    // comment and a `#[derive(..)]`, and inserting above the struct means inserting
    // *below* its derive, which then applies to a `mod` declaration and fails to compile
    // with E0774. The wire import is the first line of the module body and has nothing
    // above it but the module doc, so that is where the block starts.
    let anchor = "use proto::";
    let source = rustedit::insert_mod(source, &format!("mod {snake};"), anchor)?;
    rustedit::insert_mod(&source, &format!("pub use self::{snake}::{dto};"), anchor)
}

/// Name the parts of the conversion the generator could not finish.
///
/// Both halves of this are the same kind of gap: a value the generator has no defensible
/// choice for. Printing them is the difference between a skeleton someone completes and a
/// skeleton someone ships believing it is finished — a `todo!` at least panics, but a
/// response field quietly defaulted to an empty string does not.
pub(super) fn report_gaps(request: &Message, response: Option<&Message>, pascal: &str) {
    let fallible: Vec<&Field> = request
        .fields
        .iter()
        .filter(|field| field.ty.is_message())
        .collect();
    if !fallible.is_empty() {
        println!("\n  ! the conversion is a `TryFrom`, because these carry a message:");
        for field in fallible {
            println!("      {}: {}", field.name, field.ty.proto());
        }
        println!("    A message field may be unset on the wire, and what an unset one means");
        println!("    is this feature's decision — so each is left as a `todo!` rather than");
        println!("    defaulted into something the caller did not send.");
    }

    let Some(response) = response else {
        println!("\n  ! no `message {pascal}Response`, so only the inbound conversion was");
        println!("    written. Add one and run this again to get the other half.");
        return;
    };

    match Outbound::classify(request, response) {
        Outbound::Empty => {}
        Outbound::NothingMaps => {
            println!("\n  ! no field of `{pascal}Response` has a field of the same name to take:");
            for field in &response.fields {
                println!("      {}", field.name);
            }
            println!(
                "    So `From<{pascal}Dto> for {pascal}Response` was written with a `todo!()`"
            );
            println!("    body, and it PANICS until the mapping is written by hand.");
        }
        Outbound::Maps { unmapped } if unmapped.is_empty() => {}
        Outbound::Maps { unmapped } => {
            println!("\n  ! these response fields have no field of the same name to take:");
            for field in unmapped {
                println!("      {}", field.name);
            }
            println!("    Each is left to its own type's `default()`, spelled out in");
            println!("    `From<{pascal}Dto> for {pascal}Response` — which for a response");
            println!("    usually means empty. Fill them in before the method is believed.");
        }
    }
}

/// The DTO file's source.
///
/// Shared with `new-rpc`, which declares the two messages first and then wants exactly this
/// file for them. One renderer rather than two: the conversions a method grows later are
/// then the same conversions a method born with the slice got.
pub(super) fn body(
    feature: &str,
    pascal: &str,
    dto: &str,
    request: &Message,
    response: Option<&Message>,
) -> String {
    let wire = format!("proto::r#gen::buffa::{feature}::v1");
    let fallible = request.fields.iter().any(|field| field.ty.is_message());

    let declarations = request
        .fields
        .iter()
        .map(|field| format!("    pub {}: {},", field.name, field.domain_type()))
        .collect::<Vec<_>>()
        .join("\n");

    let moves = request
        .fields
        .iter()
        .map(|field| {
            if field.ty.is_message() {
                format!(
                    "            // `{}` is `{}` on the wire, which may be unset. What an\n\
                     \x20           // unset one means is this feature's decision.\n\
                     \x20           {}: todo!(\"decide what an unset {} means\"),",
                    field.name,
                    field.ty.proto(),
                    field.name,
                    field.name
                )
            } else {
                format!("            {}: v.{},", field.name, field.name)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let imports = response.map_or_else(
        || format!("use {wire}::{pascal}Request;\n"),
        |_| format!("use {wire}::{{{pascal}Request, {pascal}Response}};\n"),
    );

    let inbound = if fallible {
        format!(
            "impl TryFrom<{pascal}Request> for {dto} {{\n\
             \x20   type Error = super::super::errors::{}Error;\n\
             \n\
             \x20   fn try_from(v: {pascal}Request) -> Result<Self, Self::Error> {{\n\
             \x20       Ok(Self {{\n\
             {moves}\n\
             \x20       }})\n\
             \x20   }}\n\
             }}\n",
            repo::to_pascal(feature)
        )
    } else {
        format!(
            "impl From<{pascal}Request> for {dto} {{\n\
             \x20   fn from(v: {pascal}Request) -> Self {{\n\
             \x20       Self {{\n\
             {moves}\n\
             \x20       }}\n\
             \x20   }}\n\
             }}\n"
        )
    };

    let outbound = response.map_or_else(String::new, |response| {
        outbound(pascal, dto, request, response)
    });

    format!(
        "//! The `{pascal}` payload.\n\
         //!\n\
         //! One file per payload rather than one module for the feature: a slice accumulates\n\
         //! these, and a `domain/mod.rs` holding every conversion the feature has ever needed\n\
         //! is the file nobody reads.\n\
         \n\
         {imports}\n\
         #[derive(Debug, Clone)]\n\
         pub struct {dto} {{\n\
         {declarations}\n\
         }}\n\
         \n\
         {inbound}{outbound}"
    )
}

/// How much of a response the payload can fill, which decides the shape of its `From`.
///
/// One classification for both [`body`] and [`report_gaps`], so that what the terminal says
/// was generated and what the file contains cannot drift apart.
enum Outbound<'a> {
    /// `message XResponse {}`: nothing to map and nothing missing.
    Empty,
    /// At least one response field, and not one of them shares a name with a request field.
    /// The impl is a `todo!()` body; see the module docs for why.
    NothingMaps,
    /// At least one field maps. `unmapped` are the rest, each written as its own type's
    /// `default()` — see [`default_of`] for the spelling and for why it is not the shorter one.
    Maps { unmapped: Vec<&'a Field> },
}

impl<'a> Outbound<'a> {
    fn classify(request: &Message, response: &'a Message) -> Self {
        let unmapped: Vec<&Field> = response
            .fields
            .iter()
            .filter(|field| !request.fields.iter().any(|f| f.name == field.name))
            .collect();
        if response.fields.is_empty() {
            Self::Empty
        } else if unmapped.len() == response.fields.len() {
            Self::NothingMaps
        } else {
            Self::Maps { unmapped }
        }
    }
}

/// `impl From<Dto> for <Pascal>Response`, in whichever of the three shapes the fields call for.
///
/// None of them ends in `..Default::default()`. The struct-update syntax is what let a mapped
/// field the generator dropped compile, and what let a conversion that fills nothing pass for
/// one that fills something; every field is named instead, as `new-feature`'s conversions do
/// since RT-27, so a dropped one is E0063.
fn outbound(pascal: &str, dto: &str, request: &Message, response: &Message) -> String {
    let unknown = super::feature::UNKNOWN_FIELDS;
    let unknown_default = super::feature::UNKNOWN_FIELDS_DEFAULT;
    match Outbound::classify(request, response) {
        // The receiver is `_v` in the two shapes that read nothing from it: this workspace's
        // lint set denies warnings, and a generated file that does not pass `just check` is a
        // generated file whose first act is to fail the gate someone runs next.
        Outbound::Empty => format!(
            "\nimpl From<{dto}> for {pascal}Response {{\n\
             \x20   fn from(_v: {dto}) -> Self {{\n\
             \x20       Self {{\n\
             \x20           // The generated message carries an unknown-fields store and nothing\n\
             \x20           // else: there is no response field to fill.\n\
             \x20           {unknown}: {unknown_default},\n\
             \x20       }}\n\
             \x20   }}\n\
             }}\n"
        ),
        // The whole body, not a `todo!()` per field: a diverging expression inside a struct
        // literal leaves the literal unreachable, and the code after it is what the
        // `unreachable_code` lint names.
        Outbound::NothingMaps => format!(
            "\nimpl From<{dto}> for {pascal}Response {{\n\
             \x20   // No field of `{pascal}Response` has a counterpart on the payload, so there\n\
             \x20   // is no mapping to generate, only one to write. A body of defaults would\n\
             \x20   // compile and answer an empty response on every call; this panics instead.\n\
             \x20   fn from(_v: {dto}) -> Self {{\n\
             \x20       todo!(\"write the mapping from {dto} to {pascal}Response: none of the response's fields has a counterpart on the payload\")\n\
             \x20   }}\n\
             }}\n"
        ),
        Outbound::Maps { unmapped } => {
            let assignments = response
                .fields
                .iter()
                .map(|field| {
                    if unmapped.iter().any(|f| f.name == field.name) {
                        format!(
                            "            // The payload has no `{}`, so it is left to its default.\n\
                             \x20           {}: {},",
                            field.name,
                            field.name,
                            default_of(field)
                        )
                    } else {
                        format!("            {}: v.{},", field.name, field.name)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "\nimpl From<{dto}> for {pascal}Response {{\n\
                 \x20   fn from(v: {dto}) -> Self {{\n\
                 \x20       Self {{\n\
                 {assignments}\n\
                 \x20           // The generated message carries an unknown-fields store, named\n\
                 \x20           // so that a field missing from this literal is a compile error.\n\
                 \x20           {unknown}: {unknown_default},\n\
                 \x20       }}\n\
                 \x20   }}\n\
                 }}\n"
            )
        }
    }
}

/// The default for a response field the payload cannot fill, spelled with its own type named.
///
/// `Default::default()` is the shorter spelling and the one that fails the gate: it trips
/// `clippy::default_trait_access` — nursery, warn in this workspace, `-D warnings` in every
/// crate's own `check` recipe — on every type that is not a primitive. Measured against this
/// workspace's lint set it fires for `String`, `Option<String>`, `Vec<u8>` and a map, and stays
/// quiet for `bool`, `i32` and `f64`, so the same expression is an error on half the fields of a
/// generated response and clean on the rest. One spelling for all of them is the cheaper thing
/// to read, and it is this.
///
/// The type is [`Field::domain_type`], the same spelling the DTO declares the field with. A
/// generic needs `::<` and not `<`: `Vec<u8>::default()` is not a path cargo parses, and the
/// error it answers with is "comparison operators cannot be chained".
///
/// A message field is the one type whose wire spelling `domain_type` does not give — it returns
/// the message name, while buffa stores an unset message in `MessageField<T>`, so naming the
/// message would be a type error at the field. `::buffa::MessageField::default()` is what
/// compiles, and it is reachable for the same reason
/// [`UNKNOWN_FIELDS_DEFAULT`](super::feature::UNKNOWN_FIELDS_DEFAULT) is: the service
/// depends on buffa directly.
fn default_of(field: &Field) -> String {
    if field.ty.is_message() && !field.repeated {
        return "::buffa::MessageField::default()".to_owned();
    }
    let ty = field.domain_type();
    match ty.split_once('<') {
        // A type-argument list on a path needs `::` in front of it: `Vec<u8>::default()` is
        // not source code, and the error cargo answers with is "comparison operators cannot
        // be chained".
        Some((base, arguments)) => {
            let arguments = arguments.strip_suffix('>').unwrap_or(arguments);
            format!("{base}::<{arguments}>::default()")
        }
        None => format!("{ty}::default()"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(fields: &str) -> Vec<Message> {
        schema::parse_proto(&format!(
            "message UpdateUserRequest {{\n{fields}\n}}\n\
             message UpdateUserResponse {{\n  string first_name = 1;\n  int32 version = 2;\n}}\n"
        ))
        .messages
    }

    #[test]
    fn a_scalar_payload_converts_infallibly() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let messages = request("  string first_name = 1;\n  string last_name = 2;");
        let source = body(
            "user",
            "UpdateUser",
            "UpdateUserDto",
            &messages[0],
            Some(&messages[1]),
        );

        assert!(
            source.contains("impl From<UpdateUserRequest> for UpdateUserDto"),
            "{source}"
        );
        assert!(
            !source.contains("TryFrom"),
            "nothing here can fail:\n{source}"
        );
        assert!(source.contains("first_name: v.first_name,"), "{source}");
        assert!(source.contains("last_name: v.last_name,"), "{source}");
    }

    #[test]
    fn a_message_field_makes_the_conversion_fallible() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // The reason it has to: a `MessageField` may be unset, and defaulting it would
        // invent a value the caller never sent.
        let messages = request("  string first_name = 1;\n  Position position = 2;");
        let source = body(
            "user",
            "UpdateUser",
            "UpdateUserDto",
            &messages[0],
            Some(&messages[1]),
        );

        assert!(
            source.contains("impl TryFrom<UpdateUserRequest> for UpdateUserDto"),
            "{source}"
        );
        assert!(
            source.contains("type Error = super::super::errors::UserError;"),
            "{source}"
        );
        assert!(
            source.contains("todo!(\"decide what an unset position means\")"),
            "{source}"
        );
        // The scalar beside it is still written out in full.
        assert!(source.contains("first_name: v.first_name,"), "{source}");
    }

    #[test]
    fn the_response_takes_only_the_fields_it_shares() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let messages = request("  string first_name = 1;");
        let source = body(
            "user",
            "UpdateUser",
            "UpdateUserDto",
            &messages[0],
            Some(&messages[1]),
        );

        assert!(
            source.contains("impl From<UpdateUserDto> for UpdateUserResponse"),
            "{source}"
        );
        assert!(source.contains("first_name: v.first_name,"), "{source}");
        // `version` has no counterpart on the payload, so it falls to the default rather
        // than being invented — and says so in the file, by name, rather than hiding behind
        // the struct-update syntax where a dropped mapped field would hide too.
        assert!(!source.contains("version: v.version"), "{source}");
        assert!(source.contains("version: i32::default(),"), "{source}");
        assert!(
            source.contains("// The payload has no `version`"),
            "the unmapped field carries its reason:\n{source}"
        );
        assert!(
            source.contains("__buffa_unknown_fields: ::buffa::UnknownFields::default(),"),
            "{source}"
        );
        assert!(source.contains("fn from(v: UpdateUserDto)"), "{source}");
        assert!(
            !source.contains("todo!"),
            "a partial mapping is a skeleton:\n{source}"
        );
        assert!(!source.contains("..Default::default()"), "{source}");
    }

    /// The request and its response, each from a field list of its own.
    fn pair(request: &str, response: &str) -> Vec<Message> {
        schema::parse_proto(&format!(
            "message UpdateUserRequest {{\n{request}\n}}\n\
             message UpdateUserResponse {{\n{response}\n}}\n"
        ))
        .messages
    }

    fn render(messages: &[Message]) -> String {
        body(
            "user",
            "UpdateUser",
            "UpdateUserDto",
            &messages[0],
            messages.get(1),
        )
    }

    /// The outbound impl alone, so an assertion about it cannot be satisfied by the inbound one.
    fn outbound_of(source: &str) -> &str {
        let start = source
            .find("impl From<UpdateUserDto> for UpdateUserResponse")
            .unwrap_or_else(|| panic!("no outbound impl:\n{source}"));
        &source[start..]
    }

    #[test]
    fn a_response_nothing_maps_to_is_a_todo_body() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // RT-28: the response has fields, and not one of them shares a name with the request.
        // A body of defaults compiled and answered an empty response on every call.
        let messages = pair("  string first_name = 1;", "  bool archived = 1;");
        let source = render(&messages);
        let outbound = outbound_of(&source);

        assert!(
            outbound.contains("fn from(_v: UpdateUserDto) -> Self {"),
            "{source}"
        );
        assert!(
            outbound.contains(
                "todo!(\"write the mapping from UpdateUserDto to UpdateUserResponse: \
                 none of the response's fields has a counterpart on the payload\")"
            ),
            "{source}"
        );
        // The whole body: a `todo!()` inside a struct literal would leave the rest of the
        // literal unreachable.
        assert!(
            !outbound.contains("        Self {"),
            "no struct literal:\n{source}"
        );
        assert!(!outbound.contains("Default::default()"), "{source}");
        assert!(!outbound.contains("archived"), "{source}");
    }

    #[test]
    fn a_response_with_no_fields_is_not_a_todo() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // Nothing to map and nothing missing: an empty response is a complete answer.
        let messages = pair("  string first_name = 1;", "");
        let source = render(&messages);
        let outbound = outbound_of(&source);

        assert!(!outbound.contains("todo!"), "{source}");
        assert!(
            outbound.contains("fn from(_v: UpdateUserDto) -> Self {"),
            "{source}"
        );
        assert!(
            outbound.contains("__buffa_unknown_fields: ::buffa::UnknownFields::default(),"),
            "{source}"
        );
    }

    #[test]
    fn an_unmapped_field_is_defaulted_by_its_own_type() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // RT-43. `Default::default()` trips `clippy::default_trait_access` on every type that
        // is not a primitive — measured against this workspace's lint set, it fires for
        // `String`, `Option<String>` and `Vec<u8>` and stays quiet for `bool` and `f64` — and
        // `just check` runs clippy with `-D warnings`. This partial shape is the only place a
        // generated response field is defaulted at all, so it is where a generated slice met
        // the lint, and the archetype probe never reached it while both its methods answered
        // fields nothing on the payload had.
        //
        // The two generic spellings are the reason this is a test and not a style choice: a
        // type-argument list on a path needs `::`, so `Vec<u8>::default()` is not source code
        // at all, and the error cargo answers with is "comparison operators cannot be
        // chained".
        let messages = pair(
            "  string reason = 1;",
            "  string reason = 1;\n  bool archived = 2;\n  string note = 3;\n  \
             optional string memo = 4;\n  repeated string tags = 5;\n  Position position = 6;",
        );
        let source = render(&messages);
        let outbound = outbound_of(&source);

        assert!(outbound.contains("reason: v.reason,"), "{outbound}");
        assert!(
            outbound.contains("archived: bool::default(),"),
            "{outbound}"
        );
        assert!(outbound.contains("note: String::default(),"), "{outbound}");
        assert!(
            outbound.contains("memo: Option::<String>::default(),"),
            "{outbound}"
        );
        assert!(
            outbound.contains("tags: Vec::<String>::default(),"),
            "{outbound}"
        );
        // A message field is the one field whose wire spelling `domain_type` does not give:
        // it returns `Position`, while buffa stores an unset message in `MessageField<T>`.
        assert!(
            outbound.contains("position: ::buffa::MessageField::default(),"),
            "not `Position::default()`, which is a type error at that field:\n{outbound}"
        );
        assert!(
            !outbound.contains(": Default::default()"),
            "no field is left to the bare trait:\n{outbound}"
        );
        assert!(
            outbound.contains("// The payload has no `note`"),
            "every one of them still carries its reason:\n{outbound}"
        );
    }

    #[test]
    fn a_response_that_maps_entirely_names_only_the_unknown_fields() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let messages = pair(
            "  string first_name = 1;\n  int32 version = 2;",
            "  string first_name = 1;\n  int32 version = 2;",
        );
        let source = render(&messages);
        let outbound = outbound_of(&source);

        assert!(outbound.contains("first_name: v.first_name,"), "{source}");
        assert!(outbound.contains("version: v.version,"), "{source}");
        assert!(!outbound.contains("todo!"), "{source}");
        assert!(!outbound.contains("The payload has no"), "{source}");
    }

    #[test]
    fn no_rendered_conversion_ends_in_struct_update_syntax() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // What pins the decision: `..Default::default()` is how a dropped mapped field
        // compiled and how a conversion that fills nothing passed for one that fills
        // something. Every shape, both directions, both inbound kinds.
        let cases = [
            pair("  string first_name = 1;", "  string first_name = 1;"),
            pair(
                "  string first_name = 1;",
                "  string first_name = 1;\n  int32 version = 2;",
            ),
            pair("  string first_name = 1;", "  bool archived = 1;"),
            pair("  string first_name = 1;", ""),
            pair("  Position position = 1;", "  bool archived = 1;"),
            pair(
                "  string first_name = 1;\n  Position position = 2;",
                "  string first_name = 1;",
            ),
            request("  string first_name = 1;")[..1].to_vec(),
        ];
        for messages in &cases {
            let source = render(messages);
            assert!(!source.contains("..Default::default()"), "{source}");
        }
    }

    #[test]
    fn without_a_response_only_half_is_written() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let messages = request("  string first_name = 1;");
        let source = body("user", "UpdateUser", "UpdateUserDto", &messages[0], None);
        assert!(source.contains("impl From<UpdateUserRequest>"), "{source}");
        assert!(!source.contains("for UpdateUserResponse"), "{source}");
        assert!(
            !source.contains("UpdateUserResponse};"),
            "unused import:\n{source}"
        );
    }

    // -----------------------------------------------------------------
    // RT-38: a generation that refuses must not have written anything.
    //
    // The order was the defect: `scaffold_dto` committed the new payload file and only then read
    // `<feature>/domain/mod.rs` to declare it. A module whose anchor is gone — a slice whose wire
    // import was rewritten, or a domain module someone restructured — was refused after the DTO had
    // landed, which left a payload file no module declares: invisible to the compiler, and invisible
    // to the next run too, because `Writes::commit` then refuses to overwrite a file nobody asked
    // for.
    //
    // The fixture is the service skeleton, `add-database`'s core, and `new-feature`'s core: the
    // file this generator edits is `new-feature`'s output, so the chain has to be run rather than
    // pasted.
    // -----------------------------------------------------------------

    use crate::api::fixtures::{service_with_feature, tree};

    const FEATURE: &str = "probe-lake";

    /// `<feature>/domain/<snake>.rs`, the file this generator writes.
    fn dto(service: &Service, snake: &str) -> std::path::PathBuf {
        service.src(&format!("probe_lake/domain/{snake}.rs"))
    }

    /// `<feature>/domain/mod.rs`, the file this generator edits.
    fn registry(service: &Service) -> std::path::PathBuf {
        service.src("probe_lake/domain/mod.rs")
    }

    #[test]
    fn a_slice_takes_a_second_dto_and_registers_it() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        // The control, and the same step `just check-template` walks: the second DTO
        // reads a message `new-feature` wrote into the contract.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = service_with_feature(dir.path(), FEATURE);

        scaffold_dto(&service, FEATURE, "update-probe-lake")
            .expect("a slice must take the next payload");

        let written =
            std::fs::read_to_string(dto(&service, "update_probe_lake")).expect("the DTO file");
        assert!(
            written.contains("impl From<UpdateProbeLakeRequest> for UpdateProbeLakeDto"),
            "{written}"
        );

        let amended = std::fs::read_to_string(registry(&service)).expect("the domain module");
        assert!(amended.contains("mod update_probe_lake;"), "{amended}");
        assert!(
            amended.contains("pub use self::update_probe_lake::UpdateProbeLakeDto;"),
            "{amended}"
        );
    }

    #[test]
    fn a_domain_module_with_no_wire_import_is_refused_without_writing_the_dto() {
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        if !crate::repo::root().join("templates/service").is_dir() {
            return;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let service = service_with_feature(dir.path(), FEATURE);
        let module = registry(&service);
        let original = std::fs::read_to_string(&module).expect("the domain module");
        // A real anchor: `register` puts the first DTO declaration above `use proto::`, because
        // every struct in a generated `domain/mod.rs` carries a doc comment and a derive, and
        // inserting above a struct lands below its derive. Drop the import and there is nowhere
        // left to put the declaration.
        std::fs::write(
            &module,
            original.replace(
                "use proto::r#gen::buffa::probe_lake::v1::{\n",
                "// the wire import, rewritten by hand\n",
            ),
        )
        .expect("rewrite the wire import");

        let before = tree(dir.path());
        let error = scaffold_dto(&service, FEATURE, "update-probe-lake")
            .expect_err("a domain module with no anchor is refused");
        let message = format!("{error:#}");
        assert!(message.contains("use proto::"), "{message}");
        assert!(message.contains("probe_lake/domain/mod.rs"), "{message}");

        // The defect itself, named rather than inferred from a count: before this change the
        // refusal arrived after `writes.commit`, and the tree had a payload file no module
        // declares. This assert is the one that fails on the old order.
        assert!(
            !dto(&service, "update_probe_lake").exists(),
            "a refused generation left {} behind: {message}",
            service.rel(&dto(&service, "update_probe_lake"))
        );
        assert_eq!(
            tree(dir.path()),
            before,
            "a refused generation must leave the service exactly as it found it: {message}"
        );
    }
}
