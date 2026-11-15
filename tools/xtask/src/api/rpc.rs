//! `cargo xtask new-rpc <service> <feature> <name> [--field ..] [--response-field ..]`.
//!
//! The edit between the two generators either side of it. [`super::feature`] writes a whole
//! slice and [`super::domain`] writes one more DTO for a method somebody else declared;
//! adding one *method* to a slice that exists is neither, and it is the edit a product makes
//! most often — a feature is scaffolded once, and its methods accumulate for as long as the
//! product lives.
//!
//! Seven places, all mechanical, all hand-typed before this:
//!
//! ```text
//! proto/<feature>/v1/<feature>.proto    the rpc line, and a request and a response message
//! src/<feature>/domain/<method>.rs      the DTO and the conversions both ways
//! src/<feature>/domain/mod.rs           the module declaration and the re-export
//! src/<feature>/service/v1.rs           the two wire types, the DTO, and the handler
//! tests/<svc>/auth.rs                   the classification
//! ```
//!
//! # Where the fields come from, and why not from the entity
//!
//! From `--field` on the command line, which is the one thing that differs from
//! `new-feature`. That generator reads the shape off `src/entities/<table>.rs`, because the
//! four methods it writes are CRUD over a row and the row is the authority on its own
//! shape. A method added later is not: a `ListLakesRequest` carries a page token, not a
//! lake, and an `ArchiveLakeRequest` carries a reason. Inferring the request from the entity
//! would be right for the four methods that already exist and wrong for most of the ones
//! anybody adds afterwards (RT-23 rejects it as the default, and `--from-entity` as an
//! opt-in for the CRUD-shaped ones is left on the table).
//!
//! The response gets its own list for the same reason — a method's answer is not its
//! question — and an empty one is both legal and common, so no flag is required.
//!
//! # Nothing is written until everything is known
//!
//! The whole generator is a dry run followed by a commit: it parses the contract, refuses a
//! method the contract already declares, computes every one of the five edits in memory, and
//! only then writes. That is [`super::feature::read_auth`]'s discipline applied to the
//! file-set rather than to one file, and the reason is the same — a service this cannot
//! fully serve must be refused with *nothing* on disk, because the repair for a half-written
//! method is to find and delete the halves by hand.
//!
//! # What it deliberately does not write
//!
//! The body of the handler. The gate, the conversion and the response type are all
//! mechanical and all generated; what the method *does* is the part that is not, and it is
//! left a `todo!` rather than a plausible-looking stub that returns an empty response. A
//! `todo!` panics; a stub ships.

use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};

use super::schema::{self, Message};
use super::{Service, Writes, domain, feature, protoedit, report, rustedit};
use crate::repo;

/// Add one method to a feature that already exists.
///
/// # Errors
///
/// Returns an error if the service, the feature or its contract is missing, if the contract
/// already declares the method, if `tests/<svc>/auth.rs` has nowhere to classify it, or if
/// any of the files it has to edit is not the shape it expects. In every one of those cases
/// nothing has been written.
pub fn new_rpc(
    service: &str,
    feature: &str,
    name: &str,
    fields: &[String],
    response_fields: &[String],
) -> Result<()> {
    let service = Service::open(service)?;
    let plan = Plan::resolve(&service, feature, name, fields, response_fields)?;
    plan.commit(&service)
}

/// Every edit this generator will make, resolved before any of them happens.
struct Plan {
    /// The new file: the DTO and its conversions.
    dto_file: (PathBuf, String),
    /// The files that exist already, with their new contents.
    amended: Vec<(PathBuf, String)>,
    /// For the report afterwards.
    method: String,
    request: Message,
    response: Message,
    pascal: String,
    span: String,
}

impl Plan {
    #[expect(
        clippy::too_many_lines,
        reason = "it is one list of the five edits, in the order they have to be computed \
                  in, and the order is the interesting part — splitting it would hide which \
                  refusals happen before which"
    )]
    fn resolve(
        service: &Service,
        feature: &str,
        name: &str,
        fields: &[String],
        response_fields: &[String],
    ) -> Result<Self> {
        feature::validate("feature", "lake", feature)?;
        feature::validate("method", "archive-lake", name)?;

        let feature_snake = feature.replace('-', "_");
        let feature_pascal = repo::to_pascal(feature);
        let snake = name.replace('-', "_");
        let pascal = repo::to_pascal(name);
        let dto = format!("{pascal}Dto");

        let slice = service.src(&feature_snake);
        if !slice.is_dir() {
            bail!(
                "no feature at {} — `cargo xtask new-feature {} {feature}` creates one, and \
                 this generator adds a method to one that exists",
                service.rel(&slice),
                service.name,
            );
        }

        // The contract first, because it is the only input: everything below is derived from
        // the method name and the field lists, but *whether this method may be declared at
        // all* is a question only the .proto answers.
        let contract_path = service
            .root
            .join(format!("proto/{feature_snake}/v1/{feature_snake}.proto"));
        let contract_text = std::fs::read_to_string(&contract_path).map_err(|error| {
            anyhow::anyhow!("could not read {}: {error}", service.rel(&contract_path))
        })?;
        let contract = schema::parse_proto(&contract_text);

        let block = service_block(&contract, &feature_pascal, &service.rel(&contract_path))?;

        if let Some((declared_in, existing)) = contract.declares(&pascal) {
            bail!(
                "{} already declares `rpc {pascal}({}) returns ({})` in `service {}`.\n\
                 \n\
                 Nothing has been written. A second declaration of the same method is a \
                 contract buf refuses and a routing table with two entries for one name, so \
                 this refuses it here, where the name is still the thing in front of you. \
                 Pick another name, or edit the method that is there.",
                service.rel(&contract_path),
                existing.request,
                existing.response,
                declared_in.name,
            );
        }

        let request_name = format!("{pascal}Request");
        let response_name = format!("{pascal}Response");
        for message in [&request_name, &response_name] {
            if contract.message(message).is_some() {
                bail!(
                    "{} already declares `message {message}`, which this generator would \
                     write a second time.\n\
                     \n\
                     Nothing has been written. If that message is the payload of a method \
                     that is not mounted yet, `cargo xtask new-domain {} {feature} {name}` \
                     writes its DTO without touching the contract.",
                    service.rel(&contract_path),
                    service.name,
                );
            }
        }

        let request = Message {
            name: request_name.clone(),
            fields: schema::parse_fields(fields)?,
        };
        let response = Message {
            name: response_name.clone(),
            fields: schema::parse_fields(response_fields)?,
        };

        // The classification, checked against `auth.rs` as it stands and before any text is
        // produced, so a service with no classification table refuses the whole generator
        // rather than leaving a method half declared. `new-feature`'s discipline, and its
        // primitive: one answer to "who may call a generated method", not two.
        let auth = service.auth_test();
        let method = feature::routed_method(&feature_snake, &feature_pascal, &pascal);
        let classified = feature::classify(
            service,
            &auth,
            &feature::read_auth(service, &auth)?,
            std::slice::from_ref(&method),
        )?;

        // The contract: the two messages above the service block, then the rpc inside it.
        let proto = protoedit::insert_messages(
            &contract_text,
            &block,
            &format!(
                "{}\n\n{}\n",
                declare(&request, &format!("What `{pascal}` is asked")),
                declare(&response, &format!("What `{pascal}` answers")),
            ),
        )?;
        let proto = protoedit::insert_rpc(
            &proto,
            &block,
            &format!("rpc {pascal}({request_name}) returns ({response_name});"),
        )?;

        // The DTO, through `new-domain`'s renderer: this generator declares the messages and
        // then wants exactly the file that generator would have written for them, so the
        // conversions a method grows later are the ones a method born with the slice got.
        let dto_path = service.src(&format!("{feature_snake}/domain/{snake}.rs"));
        let dto_source = domain::body(&feature_snake, &pascal, &dto, &request, Some(&response));

        let registry = service.src(&format!("{feature_snake}/domain/mod.rs"));
        let registered = domain::register(&read(service, &registry)?, &snake, &dto)?;

        let v1 = service.src(&format!("{feature_snake}/service/v1.rs"));
        let span = span(&feature_snake, &snake);
        let implemented = implement(
            &read(service, &v1)?,
            &Method {
                service: service.name.clone(),
                feature_snake,
                feature_pascal,
                snake,
                pascal: pascal.clone(),
                dto,
                span: span.clone(),
            },
        )?;

        Ok(Self {
            dto_file: (dto_path, dto_source),
            amended: vec![
                (contract_path, proto),
                (registry, registered),
                (v1, implemented),
                (auth, classified),
            ],
            method,
            request,
            response,
            pascal,
            span,
        })
    }

    /// Write everything, new file first so the one refusal left — a DTO file that is already
    /// there — still arrives before an existing file has been touched.
    fn commit(mut self, service: &Service) -> Result<()> {
        let mut writes = Writes::default();
        let (path, source) = std::mem::take(&mut self.dto_file);
        writes.add(path, source);
        let written = writes.commit(service)?;

        for (path, contents) in &self.amended {
            std::fs::write(path, contents)
                .with_context(|| format!("writing {}", service.rel(path)))?;
        }

        let amended: Vec<&std::path::Path> = self
            .amended
            .iter()
            .map(|(path, _)| path.as_path())
            .collect();
        report(service, &written, &amended)?;

        domain::report_gaps(&self.request, Some(&self.response), &self.pascal);
        self.next_steps(service);
        Ok(())
    }

    /// What to run now, and what was decided on the caller's behalf.
    fn next_steps(&self, service: &Service) {
        println!("\n  The request carries:");
        if self.request.fields.is_empty() {
            println!("      nothing — `--field name:type` adds to it");
        }
        for field in &self.request.fields {
            println!("      {}: {}", field.name, field.domain_type());
        }

        println!("\nNext:");
        println!(
            "  just proto::generate                    # the method does not exist until you do"
        );
        println!("  just services/{}/dev", service.name);

        println!(
            "\n  Classified PROTECTED in services/{}/tests/{}/auth.rs:",
            service.name, service.name
        );
        println!("      {}", self.method);
        println!("    The handler calls `ctx.principal()?`, so the code says what the list says.");
        println!("    It refuses every caller until this service has a verifier that can say");
        println!(
            "    yes: `unauthenticated` for a credential that verifier refuses, `internal` if"
        );
        println!("    it reaches no verdict at all (`docs/adr/0012`). Move the name to PUBLIC if");
        println!("    the method should answer anonymous callers: that list is the decision, and");
        println!("    moving a name out of PROTECTED is a diff a reviewer can ask about.");

        println!("\n  ! the handler is a `todo!`.");
        println!("    Everything mechanical about the method is written — the gate, both");
        println!("    conversions, the response type. What it *does* is not, and a stub that");
        println!("    returned an empty response would be a method that ships looking finished.");
        println!(
            "    It is `{}` in the span name, so a trace will say so too.",
            self.span
        );
    }
}

/// The spellings one handler needs.
struct Method {
    service: String,
    feature_snake: String,
    feature_pascal: String,
    snake: String,
    pascal: String,
    dto: String,
    span: String,
}

/// Which `service` block to splice the method into.
///
/// `<Feature>Service` is what `new-feature` writes and what the routed name this generator
/// classifies is built from, so it is looked for by name first. A contract with exactly one
/// service block is served anyway — a feature renamed after its contract was written still
/// has one unambiguous answer — but a contract with several and none of the expected name is
/// refused, because guessing would mount the method on a service nobody meant.
fn service_block(contract: &schema::Contract, feature_pascal: &str, path: &str) -> Result<String> {
    let expected = format!("{feature_pascal}Service");
    if contract
        .services
        .iter()
        .any(|service| service.name == expected)
    {
        return Ok(expected);
    }

    match contract.services.as_slice() {
        [only] => Ok(only.name.clone()),
        [] => bail!(
            "{path} declares no `service` block, so there is nowhere to put an rpc. A \
             contract `cargo xtask new-feature` wrote has one; this one has been replaced \
             or emptied."
        ),
        several => bail!(
            "{path} has no `service {expected}`, and more than one service block to choose \
             from instead: {}.\n\
             \n\
             Nothing has been written. Which of them a method belongs to is not something \
             this generator may guess at — rename the one that serves `{feature_pascal}` or \
             add the rpc by hand.",
            several
                .iter()
                .map(|service| service.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// One message, as the contract declares it.
///
/// Numbered from 1: neither message carries an id the way the entity message does, because
/// a method's request is not a row — whatever identifies the thing being acted on is a field
/// the caller named like any other.
fn declare(message: &Message, comment: &str) -> String {
    let Message { name, fields } = message;
    if fields.is_empty() {
        return format!("// {comment}.\nmessage {name} {{}}");
    }

    let declared = fields
        .iter()
        .zip(1_u32..)
        .map(|(field, number)| {
            let label = if field.repeated {
                "repeated "
            } else if field.optional {
                "optional "
            } else {
                ""
            };
            format!("  {label}{} {} = {number};", field.ty.proto(), field.name)
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!("// {comment}.\nmessage {name} {{\n{declared}\n}}")
}

/// The tracing span's suffix.
///
/// `new-feature` names its four spans `<feature>.create`, `.get`, `.list` and `.update`, so a
/// method called `archive-lake` on the `lake` feature becomes `lake.archive` rather than
/// `lake.archive_lake`. The feature's name is stripped from either end, because both
/// `archive-lake` and `lake-archive` are names people give the same method.
fn span(feature_snake: &str, method_snake: &str) -> String {
    let stripped = method_snake
        .strip_suffix(&format!("_{feature_snake}"))
        .or_else(|| method_snake.strip_prefix(&format!("{feature_snake}_")))
        .unwrap_or(method_snake);
    let verb = if stripped.is_empty() {
        method_snake
    } else {
        stripped
    };
    format!("{feature_snake}.{verb}")
}

/// The three edits one handler needs in `service/v1.rs`.
///
/// Three rather than one, and none of them `insert_into_const_list`: the wire types are a
/// `use` group, the DTO is another, and the handler is a member of an `impl` block. They are
/// separate functions in [`rustedit`] for the same reason this is a separate generator —
/// each one knows a different thing about the shape of what it is editing.
fn implement(source: &str, method: &Method) -> Result<String> {
    let wire = format!("proto::r#gen::buffa::{}::v1::", method.feature_snake);
    let source =
        rustedit::insert_into_use_group(source, &wire, &format!("{}Request", method.pascal))
            .and_then(|source| {
                rustedit::insert_into_use_group(
                    &source,
                    &wire,
                    &format!("{}Response", method.pascal),
                )
            })
            .with_context(|| {
                format!(
                    "the generated handler names `{}Request` and `{}Response`, so both have to \
                 be imported from the wire module",
                    method.pascal, method.pascal
                )
            })?;

    let domain = format!("crate::{}::domain::", method.feature_snake);
    let source = rustedit::insert_into_use_group(&source, &domain, &method.dto)?;

    rustedit::insert_into_block(
        &source,
        &format!("impl {0}Service for {0} {{", method.feature_pascal),
        &handler(method),
    )
}

/// The handler, gate and all.
fn handler(method: &Method) -> String {
    let Method {
        feature_snake,
        snake,
        pascal,
        dto,
        span,
        ..
    } = method;
    let gate = feature::gate(&method.service, feature_snake);

    format!(
        "\x20   #[tracing::instrument(name = \"{span}\", skip_all)]\n\
         \x20   async fn {snake}(\n\
         \x20       &self,\n\
         \x20       ctx: RequestContext,\n\
         \x20       request: ServiceRequest<'_, {pascal}Request>,\n\
         \x20   ) -> ServiceResult<{pascal}Response> {{\n\
         {gate}\
         \x20       // Owned and converted where every other handler in this slice does it, so\n\
         \x20       // the generated `From` is wired up rather than merely written.\n\
         \x20       let dto = {dto}::from(request.to_owned_message());\n\
         \n\
         \x20       // What happens to it is the one part of a method no generator can write:\n\
         \x20       // this is where the {feature_snake} rows are read or written. Replace the\n\
         \x20       // `todo!` with that, and answer with `{pascal}Response::from(..)`, which\n\
         \x20       // is generated beside the DTO.\n\
         \x20       todo!(\"implement {snake}: {{dto:?}}\")\n\
         \x20   }}\n"
    )
}

/// Read a file this generator has to edit, naming it if it is not there.
fn read(service: &Service, path: &std::path::Path) -> Result<String> {
    std::fs::read_to_string(path)
        .with_context(|| format!("reading {} — nothing has been written", service.rel(path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method() -> Method {
        Method {
            service: "api".to_owned(),
            feature_snake: "lake".to_owned(),
            feature_pascal: "Lake".to_owned(),
            snake: "archive_lake".to_owned(),
            pascal: "ArchiveLake".to_owned(),
            dto: "ArchiveLakeDto".to_owned(),
            span: "lake.archive".to_owned(),
        }
    }

    fn v1() -> String {
        "use connectrpc::{RequestContext, Response, ServiceRequest, ServiceResult};\n\
         use identity::IdentityContext as _;\n\
         use proto::r#gen::connect::lake::v1::LakeService;\n\
         use proto::r#gen::buffa::lake::v1::{CreateLakeRequest, CreateLakeResponse};\n\
         \n\
         use crate::lake::domain::{self, LakeDto};\n\
         \n\
         pub struct Lake {\n\
         \x20   db: DatabaseConnection,\n\
         }\n\
         \n\
         impl Lake {\n\
         \x20   #[must_use]\n\
         \x20   pub const fn new(db: DatabaseConnection) -> Self {\n\
         \x20       Self { db }\n\
         \x20   }\n\
         }\n\
         \n\
         impl LakeService for Lake {\n\
         \x20   #[tracing::instrument(name = \"lake.create\", skip_all)]\n\
         \x20   async fn create_lake(&self) -> ServiceResult<CreateLakeResponse> {\n\
         \x20       let _principal = ctx.principal()?;\n\
         \x20       todo!()\n\
         \x20   }\n\
         }\n"
        .to_owned()
    }

    fn fields(specs: &[&str]) -> Vec<schema::Field> {
        schema::parse_fields(&specs.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
            .expect("valid fields")
    }

    #[test]
    fn the_handler_lands_in_the_impl_with_both_imports() {
        let out = implement(&v1(), &method()).expect("the file is the shape it was generated in");

        // The existing handler is still there, and the new one is after it inside the impl.
        let impl_at = out.find("impl LakeService for Lake {").expect("the impl");
        let created_at = out.find("async fn create_lake").expect("the old handler");
        let archived_at = out.find("async fn archive_lake").expect("the new handler");
        assert!(impl_at < created_at && created_at < archived_at, "{out}");
        assert!(
            archived_at < out.rfind('}').expect("a closing brace"),
            "the handler must be inside the impl:\n{out}"
        );

        assert!(out.contains("ArchiveLakeRequest"), "{out}");
        assert!(out.contains("ArchiveLakeResponse"), "{out}");
        assert!(
            out.contains("use crate::lake::domain::{self, LakeDto, ArchiveLakeDto};"),
            "the DTO has to be in scope:\n{out}"
        );
    }

    #[test]
    fn the_added_method_gates_and_says_why() {
        // Trap 4 of this ticket: the four-line explanation is repeated per handler on
        // purpose, and a fifth handler without it is one handler in five that gates without
        // saying why.
        let out = handler(&method());
        assert!(out.contains("let _principal = ctx.principal()?;"), "{out}");
        assert!(
            out.contains("Classified PROTECTED in services/api/tests/api/auth.rs"),
            "{out}"
        );
        // Not `_ctx`, which is how a handler that does not gate is written.
        assert!(out.contains("ctx: RequestContext,"), "{out}");
        assert!(!out.contains("_ctx: RequestContext"), "{out}");
    }

    #[test]
    fn the_body_is_a_todo_that_uses_what_it_was_given() {
        // A `todo!` rather than a stub that answers an empty response: one panics, the
        // other ships. The payload is named in it, which is also what keeps the conversion
        // from being an unused binding in a workspace that denies nothing and warns at
        // everything.
        let out = handler(&method());
        assert!(
            out.contains("let dto = ArchiveLakeDto::from(request.to_owned_message());"),
            "{out}"
        );
        // Spelled in two pieces because `{dto:?}` in a literal reads to clippy as a
        // formatting argument that lost its macro.
        let payload = "{dto:?}";
        assert!(
            out.contains(&format!("todo!(\"implement archive_lake: {payload}\")")),
            "{out}"
        );
    }

    #[test]
    fn a_handler_whose_impl_is_not_there_is_refused_rather_than_appended() {
        let error = implement("fn main() {}", &method()).expect_err("no use group, no impl");
        // The whole chain: the context names what the handler needs, and the cause names the
        // group that is not there.
        let error = format!("{error:#}");
        assert!(
            error.contains("use proto::r#gen::buffa::lake::v1::"),
            "{error}"
        );
    }

    #[test]
    fn the_messages_are_numbered_from_one_and_labelled() {
        let request = Message {
            name: "ArchiveLakeRequest".to_owned(),
            fields: fields(&["lake_id:int32", "reason:string?", "tags:string[]"]),
        };
        let declared = declare(&request, "What `ArchiveLake` is asked");
        assert!(
            declared.contains(
                "message ArchiveLakeRequest {\n  \
                 int32 lake_id = 1;\n  optional string reason = 2;\n  repeated string tags = 3;\n}"
            ),
            "{declared}"
        );
        assert!(
            declared.starts_with("// What `ArchiveLake` is asked."),
            "{declared}"
        );
    }

    #[test]
    fn a_response_with_no_fields_is_still_a_message() {
        // buf's STANDARD lint wants a dedicated response per rpc, and most methods answer
        // nothing in particular — so an empty one is the common case, not an error.
        let empty = Message {
            name: "ArchiveLakeResponse".to_owned(),
            fields: Vec::new(),
        };
        assert!(
            declare(&empty, "What `ArchiveLake` answers")
                .contains("message ArchiveLakeResponse {}"),
            "{}",
            declare(&empty, "x")
        );
    }

    #[test]
    fn the_span_reads_like_the_four_the_slice_was_born_with() {
        assert_eq!(span("lake", "archive_lake"), "lake.archive");
        assert_eq!(span("lake", "lake_archive"), "lake.archive");
        assert_eq!(span("lake", "archive"), "lake.archive");
        // A method named exactly for its feature has no verb to strip down to.
        assert_eq!(span("lake", "lake"), "lake.lake");
    }

    #[test]
    fn the_classified_name_is_the_one_the_router_answers_to() {
        // The same spelling `new-feature` writes, from the same function: two places that
        // spelled it would eventually disagree, and the disagreement is a method the router
        // serves and no test classifies.
        assert_eq!(
            feature::routed_method("lake", "Lake", "ArchiveLake"),
            "lake.v1.LakeService/ArchiveLake"
        );
    }

    #[test]
    fn the_service_block_is_chosen_by_name_and_otherwise_only_when_there_is_one() {
        let one = schema::parse_proto("service LakeService {\n  rpc A(B) returns (C);\n}\n");
        assert_eq!(
            service_block(&one, "Lake", "p").expect("by name"),
            "LakeService"
        );
        assert_eq!(
            service_block(&one, "Pond", "p").expect("the only one"),
            "LakeService",
            "a renamed feature still has one unambiguous answer"
        );

        let none = schema::parse_proto("message Lake {}\n");
        assert!(service_block(&none, "Lake", "p").is_err());

        let several = schema::parse_proto(
            "service PondService {\n  rpc A(B) returns (C);\n}\n\
             service RiverService {\n  rpc D(E) returns (F);\n}\n",
        );
        let error = service_block(&several, "Lake", "proto/lake/v1/lake.proto")
            .expect_err("guessing would mount the method on a service nobody meant");
        let error = error.to_string();
        assert!(error.contains("PondService, RiverService"), "{error}");
    }
}
