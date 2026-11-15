//! The names facet's Swift emitter writes, and the keywords it cannot write.
//!
//! One invariant — `facet_names_are_legal_swift_identifiers`, registered in `invariants.rs`
//! beside the rest — plus the scanner it needs: a comment-and-literal blanker, a member
//! splitter, an attribute walker. That supporting cast is why this is a module of its own
//! rather than another thousand lines of `invariants.rs`, whose other checks read directories
//! and manifests; `rename.rs`, `i18n_resources.rs` and `scaffold.rs` are split out on the same
//! reasoning. Only the registration line stays behind in `invariants.rs`, so the check is still
//! found where every other check is found.
//!
//! Three text scanners are borrowed from `invariants` rather than copied — `closing_paren`,
//! `split_arguments`, `leading_ident` — and are public for it. That is the reason
//! `i18n_keygen::variant_name` is public: a rule with a second reader must not grow a second
//! implementation, and here the rule is where one declaration ends and the next begins.

use std::path::Path;

use heck::ToLowerCamelCase;

use crate::invariants::{closing_paren, leading_ident, split_arguments};
use crate::{rename, repo};

/// The Swift keywords a `Facet` name must not lowerCamelCase into.
///
/// Copied from *The Swift Programming Language*, "Lexical Structure → Keywords and
/// Punctuation" (docs.swift.org/swift-book, read 2026-10-02) — the section that says which
/// words "are reserved and can't be used as identifiers, unless they're escaped with
/// backticks". Three of its lists are that: the keywords *used in declarations*, *used in
/// statements*, and *used in expressions and types*. Copied rather than remembered, because
/// a list from memory is this defect one layer up: this one was written down from memory
/// first, and was missing `await`, `borrowing`, `consuming`, `nonisolated` and
/// `precedencegroup` — all five words a Rust enum can plausibly be named. Re-read the section
/// and diff it against this before changing either.
///
/// Three things from that section are deliberately absent:
///
/// - *Keywords reserved in particular contexts* — `get`, `set`, `some`, `none`, `final`,
///   `lazy`, `override`, `async`, `weak`, `mutating`, `indirect`, … The reference says these
///   "can be used as identifiers" outside the context they appear in, and a case label is
///   outside it: `CounterApi::Get` is emitted as `case get` and matched as `case .get:` at
///   `CounterApiClient.swift:72`, and `ContentView.swift:99` matches `.unreachable` and
///   `.rejected` the same way. Including the group fires on the example app's most central
///   event and turns an invariant into a rename of code that works, so it is out by decision
///   rather than by oversight — widening this list to it is a regression, and
///   `the_contextual_group_is_out_by_decision` below is what says so.
/// - *Keywords that begin with a number sign*, and *keywords used in patterns* (`_`).
///   Neither is reachable: `to_lower_camel_case` of a Rust identifier produces neither a `#`
///   nor an `_`.
/// - `Any` and `Self`, the two capitalised words in the third list. A lowerCamelCase name
///   begins lowercase, so neither can be produced — and Rust will not let a type be named
///   `Self` anyway.
///
/// What the words are compared against is the emitter's own transformation rather than a
/// guess at it: `facet_generate` writes `case {name}` and `public var {name}:` where `name`
/// is `to_lower_camel_case()` of the Rust spelling (`src/generation/swift/emitter/mod.rs`),
/// and escapes nothing. The same crate *does* escape for C# — `src/generation/bincode/
/// csharp.rs`'s `escape_identifier` carries a reserved list and the same "contextual
/// keywords are intentionally omitted" reasoning — so Swift is the one target where a
/// collision reaches the generated file unrepaired.
const SWIFT_RESERVED_KEYWORDS: &[&str] = &[
    "as",
    "associatedtype",
    "await",
    "borrowing",
    "break",
    "case",
    "catch",
    "class",
    "consuming",
    "continue",
    "default",
    "defer",
    "deinit",
    "do",
    "else",
    "enum",
    "extension",
    "fallthrough",
    "false",
    "fileprivate",
    "for",
    "func",
    "guard",
    "if",
    "import",
    "in",
    "init",
    "inout",
    "internal",
    "is",
    "let",
    "nil",
    "nonisolated",
    "open",
    "operator",
    "precedencegroup",
    "private",
    "protocol",
    "public",
    "repeat",
    "rethrows",
    "return",
    "self",
    "static",
    "struct",
    "subscript",
    "super",
    "switch",
    "throw",
    "throws",
    "true",
    "try",
    "typealias",
    "var",
    "where",
    "while",
];

/// One name a `Facet` type hands to facet's Swift emitter.
struct FacetName {
    /// How it is written in Rust, for the message: `Code::Internal`, `ViewModel::count`,
    /// `Failure::Rejected { code }`.
    qualified: String,
    /// `variant` or `field` — the word the message tells the reader to rename.
    kind: &'static str,
    /// The identifier itself, as written.
    text: String,
    /// Its byte offset in the scanned text.
    at: usize,
}

/// A `Facet` variant or field name must not lowerCamelCase into a Swift keyword.
///
/// facet's Swift emitter writes `case {name}` and `public var {name}:` with `name` =
/// `to_lower_camel_case()` of the Rust spelling, and escapes nothing, so `Code::Internal`
/// arrives in the generated package as `case internal` — a reserved declaration modifier —
/// and the package does not compile. Nothing in this repository can see that: `just ci`
/// compiles no Swift, `just check-template` prints "apple: skipped — Xcode is
/// macOS only" off macOS, and the Apple project has never been built here at all. The
/// feedback loop for this defect class is "someone opens the project on a Mac, months
/// later", which is the shape invariant checks in this repository exist to close. `bea8c4f` renamed
/// one such variant after the fact and recorded the reason in the core's own doc comment;
/// this is what stops the next one being a matter of memory. [ADR 0007](https://forge.ugalabugala.org/manjo/rust-app-template/src/commit/9a40690b725310f872572c0b9450c35da9615e1f/docs/adr/0007-identifiers-stay-literal.md) has a name written
/// once in Rust reaching every platform and ADR 0009 has a platform project derive from an
/// app rather than be written by hand; both assume the name is one every generator can say,
/// and this is the only place that assumption is checked.
///
/// Scanned: every `#[derive(…Facet…)]` type, every `#[effect(facet_typegen)]` enum — which is
/// `crux_macros`' spelling of the same derive, so an effect's variants reach Swift exactly as
/// an event's do — and every `#[cfg_attr(…, derive(…Facet…))]`, which is what that derive looks
/// like when a product writes the feature gate by hand. All three are named in `facet_markers`,
/// and they have to be named there one at a time because that scan matches text rather than
/// attributes: a `derive(` nested inside a `cfg_attr(` is not a `#[derive(`, and until the third
/// spelling was scanned, `#[effect(` was found only because it is special-cased. Variant names,
/// struct field names, and the fields of a struct-like variant: `Failure::Rejected { code,
/// detail }` crosses too, as `case rejected(code:detail:)`. Templates are covered on purpose —
/// nothing compiles them, `just new` copies them verbatim, and a name that is legal today is not
/// the point, since the next product's core starts as that file.
///
/// Out of reach, stated rather than hidden:
///
/// - Type names, which the emitter upper-camel-cases. Every reserved keyword is lowercase, so
///   `Visibility` cannot collide and only its members can.
/// - `Model`, and anything else carrying neither marker. It never crosses.
/// - The boltffi bridge in a core's `ffi.rs`, whose Swift names come from a different
///   generator with rules of its own.
/// - A `#[facet(rename = …)]` or `rename_all`, which changes the emitted name and is
///   invisible to a source scan. Nothing in the tree uses one; the same limit is
///   `cores_stay_domain_level`'s, about a TOML table it does not parse.
/// - Tuple variants and tuple structs, which carry no names.
/// - A `Facet` derive emitted by a macro of the product's own, rather than by `crux_macros` or
///   written out. A source scan runs before expansion; `macro_call`'s members are skipped for
///   the same reason.
///
/// Swift-only, and there is no Kotlin equivalent, for two reasons. Kotlin's emitter writes
/// variant names upper-camel — a generated `Counter.kt` holds `sealed interface Code` with a
/// `data object PermissionDenied` inside it — and every Kotlin keyword is lowercase, so no
/// variant name can collide there. And where a Kotlin name could, a gate exists: `just
/// apps/<app>/android` runs the typegen and compiles what it wrote, and the CI `template` job
/// does the same for the skeleton on every push, so a second list here would restate a rule a
/// compiler on this machine already enforces. The one place Kotlin does emit a Rust name
/// verbatim is settled by that build rather than by reading: `CounterApi::Increment { by }`
/// becomes `val by: Long` at `Counter.kt:205` and `:226`, and `by` is a Kotlin *soft* keyword —
/// reserved only in delegation position (`class A : B by c`, `val x by lazy {}`) and an ordinary
/// identifier everywhere else. Evidence, not inference: `generated/` is a `:shared` source
/// directory (`shared/build.gradle.kts:46`), and both `just apps/<app>/android` and
/// `:shared:testDebugUnitTest` ran BUILD SUCCESSFUL against that file, so it is compiled and not
/// merely present. That is also how the next Kotlin-side collision gets found.
///
/// One conservatism that looks like a bug and is not: the same section of the reference says
/// "Keywords other than `inout`, `var`, and `let` can be used as parameter names … without
/// being escaped", and a struct-like variant's fields become parameter labels. So a payload
/// named `code` is legal in a position where a property named `code` would not be. This check
/// does not distinguish the two, because telling them apart means knowing what the emitter
/// writes for each position and the cost of guessing wrong is asymmetric — a rename nobody
/// needed against a Swift package nobody can compile. No payload label in the tree is a
/// reserved keyword today.
pub fn facet_names_are_legal_swift_identifiers(
    root: &Path,
    checked: &mut usize,
    problems: &mut Vec<String>,
) {
    let mut sources = Vec::new();
    // Every `.rs` file in the repository, not only the cores: a crate under `crates/` that
    // grows a `Facet` type is on the same surface, and would otherwise sit outside a check
    // that never looks in its directory. `walk` skips build output; templates are not build
    // output, and covering them is the point.
    rename::walk(root, &mut sources);
    sources.retain(|path| path.extension().is_some_and(|ext| ext == "rs"));
    sources.sort(); // stable diagnostics, since the walk order is the filesystem's

    for path in sources {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue; // not UTF-8, and no Crux core is written in that
        };
        // Comments and literals go first, and the copy keeps every offset, so the line
        // numbers below are the file's own.
        let scannable = blank_comments_and_literals(&text);
        for name in facet_names(&scannable) {
            // A name is one check whether or not it passes, so a clean tree still reports
            // every variant and field that was looked at.
            *checked += 1;
            let swift = swift_identifier(&name.text);
            if !SWIFT_RESERVED_KEYWORDS.contains(&swift.as_str()) {
                continue;
            }
            problems.push(format!(
                "{}:{}: `{}` — a `Facet` {} facet-typegen lowerCamelCases to `{swift}`, \
                 which Swift reserves: the generated Swift declares it unescaped (`case \
                 {swift}` / `public var {swift}:`) and does not compile. Rename the Rust \
                 {}; nothing in this repository compiles Swift, so no other gate will say so",
                repo::rel(root, &path),
                scannable[..name.at].matches('\n').count() + 1,
                name.qualified,
                name.kind,
                name.kind,
            ));
        }
    }
}

/// Every name facet's Swift emitter writes for a type in `text`, in source order.
///
/// `text` must have been through `blank_comments_and_literals`: this scan splits on commas
/// and braces, and the prose between two members holds both.
fn facet_names(text: &str) -> Vec<FacetName> {
    let mut names = Vec::new();
    for after_marker in facet_markers(text) {
        let Some(item) = declared_type(text, after_marker) else {
            continue;
        };
        let Some((body_start, body_end)) = item.body(text) else {
            continue; // a tuple or unit type: positional, so it names nothing
        };
        for (offset, member) in split_top_level(&text[body_start..body_end]) {
            let Some((ident_at, ident)) = first_named_ident(&member) else {
                continue;
            };
            let after_ident = ident_at + ident.len();
            if ITEM_KEYWORDS.contains(&ident.as_str()) || macro_call(&member, after_ident) {
                continue;
            }
            // In an enum body this is the associated-item test, and in a struct body it is
            // what makes a member a field: a variant is `Name`, `Name(…)`, `Name { … }` or
            // `Name = 0`, and a field is `name: Type`.
            if type_annotated(&member, after_ident) == item.is_enum {
                continue;
            }
            names.push(FacetName {
                qualified: format!("{}::{ident}", item.name),
                kind: if item.is_enum { "variant" } else { "field" },
                text: ident.clone(),
                at: body_start + offset + ident_at,
            });
            if !item.is_enum {
                continue;
            }
            // A struct-like variant's payload: the labels the emitter writes into
            // `case rejected(code: Code, detail: String)`, which is the same wall from the
            // other side.
            let Some((open, close)) = braced_span(&member, ident_at + ident.len()) else {
                continue;
            };
            let fields_start = body_start + offset + open + 1;
            for (field_offset, field) in split_top_level(&member[open + 1..close]) {
                let Some((field_ident_at, field_ident)) = first_named_ident(&field) else {
                    continue;
                };
                let after_field = field_ident_at + field_ident.len();
                if !type_annotated(&field, after_field) {
                    continue;
                }
                names.push(FacetName {
                    qualified: format!("{}::{ident} {{ {field_ident} }}", item.name),
                    kind: "field",
                    text: field_ident,
                    at: fields_start + field_offset + field_ident_at,
                });
            }
        }
    }
    names
}

/// The offset just past each attribute in `text` that puts the declaration below it on
/// facet's Swift surface.
///
/// Three spellings, because there are three ways to get there, and each has to be named here
/// because this scan matches text rather than attributes. Scanning only the first would quietly
/// exempt every `Effect` in the repository — `Effect` is the one enum every core has, and the
/// exemption would be invisible in a count that still moved when events were added. Scanning
/// the first two leaves out the form the second expands into: `crux_macros` gates its derive
/// behind `cfg_attr` (`effect/macro_impl.rs`), a product would plausibly hand-write that same
/// idiom for its own feature, and a `derive(` nested inside a `cfg_attr(` is not a `#[derive(`,
/// so it was invisible until `a_cfg_attr_derive_is_a_derive` put the spelling in a test.
fn facet_markers(text: &str) -> Vec<usize> {
    let mut markers = Vec::new();
    collect_markers(text, "#[derive(", derives_facet, &mut markers);
    collect_markers(text, "#[effect(", is_facet_effect, &mut markers);
    collect_markers(text, "#[cfg_attr(", cfg_attr_derives_facet, &mut markers);
    markers.sort_unstable();
    markers
}

/// The offset just past each `needle` attribute in `text` whose body `qualifies`.
fn collect_markers(
    text: &str,
    needle: &str,
    qualifies: impl Fn(&str) -> bool,
    out: &mut Vec<usize>,
) {
    for (start, _) in text.match_indices(needle) {
        let body_start = start + needle.len();
        let Some(close) = closing_paren(&text[body_start..]) else {
            continue; // unterminated, so not a declaration the compiler accepted either
        };
        if !qualifies(&text[body_start..body_start + close]) {
            continue;
        }
        // Past the attribute's own `]`, so the caller begins at whatever follows it.
        let after = body_start + close + 1;
        if let Some(bracket) = text[after..].find(']') {
            out.push(after + bracket + 1);
        }
    }
}

/// Whether one `#[derive(…)]` body asks for facet, in either spelling of the path.
fn derives_facet(body: &str) -> bool {
    split_arguments(body).iter().any(|entry| {
        entry
            .trim()
            // `Facet` or `facet::Facet`: the last segment is the derive's name either way.
            .rsplit("::")
            .next()
            .is_some_and(|last| last.trim() == "Facet")
    })
}

/// Whether one `#[cfg_attr(…)]` body contains a `derive(…)` that asks for facet.
///
/// The body is `condition, derive(…)`, and the inner `derive(` is the whole question:
/// `#[cfg_attr(feature = "facet_typegen", derive(::facet::Facet))]` puts the type on facet's
/// Swift surface whenever the feature is on, which is the same derive under another name. A
/// condition that merely *mentions* `derive` is not reachable — a cfg predicate is paths and
/// globs, and any string literal inside it has been blanked by the caller anyway.
fn cfg_attr_derives_facet(body: &str) -> bool {
    body.match_indices("derive(").any(|(at, _)| {
        let inside = &body[at + "derive(".len()..];
        closing_paren(inside).is_some_and(|close| derives_facet(&inside[..close]))
    })
}

/// Whether one `#[effect(…)]` body asks for the facet typegen. `crux_macros` panics on any
/// other argument, so this is `facet_typegen` or it is not a facet effect.
fn is_facet_effect(body: &str) -> bool {
    split_arguments(body)
        .iter()
        .any(|entry| entry.trim() == "facet_typegen")
}

/// The declaration a facet marker attribute belongs to.
struct DeclaredType {
    name: String,
    is_enum: bool,
    /// The offset of the `{` opening the body, when the type has one.
    open: Option<usize>,
}

impl DeclaredType {
    /// The contents of the body, when the type has named members.
    fn body(&self, text: &str) -> Option<(usize, usize)> {
        let open = self.open?;
        Some((open + 1, open + 1 + closing_paren(&text[open + 1..])?))
    }
}

/// The type declared at or after `from`, or `None` when what follows the marker is not one.
///
/// Between an attribute and its item may sit any number of further attributes — `#[repr(C)]`
/// does, in both cores — and a visibility, in either order. Anything else means the marker
/// was not on a type facet writes members for, and it is skipped rather than guessed at.
fn declared_type(text: &str, from: usize) -> Option<DeclaredType> {
    let mut at = skip_whitespace(text, from);
    let word = loop {
        if text[at..].starts_with("#[") {
            at = skip_whitespace(text, at + 2 + closing_paren(&text[at + 2..])? + 1);
            continue;
        }
        let word = leading_ident(&text[at..]);
        match word.as_str() {
            "pub" | "crate" => {
                at = skip_whitespace(text, at + word.len());
                if text[at..].starts_with('(') {
                    at = skip_whitespace(text, at + 1 + closing_paren(&text[at + 1..])? + 1);
                }
            }
            "unsafe" => at = skip_whitespace(text, at + word.len()),
            "enum" | "struct" => break word,
            // A `type` alias, an `impl`, a `const`, a macro call: no members facet writes.
            _ => return None,
        }
    };
    let is_enum = word == "enum";
    at = skip_whitespace(text, at + word.len());
    let name = leading_ident(&text[at..]);
    (!name.is_empty()).then_some(DeclaredType {
        name,
        is_enum,
        open: braced_body_opener(text, at),
    })
}

/// The offset of the `{` opening a declared type's body, or `None` when it has none.
///
/// Generics and a `where` clause sit between the name and the body, so this walks rather
/// than searches: the `(` of a bound such as `F: Fn()` belongs to the generics, and a tuple
/// struct's `(` or a unit struct's `;` means the type names no members at all.
fn braced_body_opener(text: &str, from: usize) -> Option<usize> {
    let mut generics = 0usize;
    for (at, ch) in text[from..].char_indices() {
        match ch {
            '<' => generics += 1,
            '>' => generics = generics.saturating_sub(1),
            '{' if generics == 0 => return Some(from + at),
            '(' | ';' if generics == 0 => return None,
            _ => {}
        }
    }
    None
}

/// `(offset of the member, its text)` for each top-level comma-separated member of a body.
///
/// `split_arguments` does this for a macro call and drops the offsets; this check reports
/// `file:line`, so it needs them, and adding them there would reshape a helper every reader
/// of that one relies on. Nothing is skipped and nothing is dropped from a member, which is
/// what keeps the offsets exact — the input has been through `blank_comments_and_literals`,
/// so a literal's contents are already spaces and a comma can only be a separator.
fn split_top_level(body: &str) -> Vec<(usize, String)> {
    let mut members = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    for (at, ch) in body.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                members.push((start, body[start..at].to_owned()));
                start = at + 1;
            }
            _ => {}
        }
    }
    members.push((start, body[start..].to_owned()));
    members
}

/// The name a member declaration gives its member, and that name's offset in `fragment`.
///
/// Skips what may precede it — attributes, and the `pub` a struct field carries — because
/// either may be there in either order, and a doc comment between them is already blanked.
fn first_named_ident(fragment: &str) -> Option<(usize, String)> {
    let mut at = skip_whitespace(fragment, 0);
    loop {
        if fragment[at..].starts_with("#[") {
            at = skip_whitespace(fragment, at + 2 + closing_paren(&fragment[at + 2..])? + 1);
            continue;
        }
        let word = leading_raw_ident(&fragment[at..]);
        match word.as_str() {
            "pub" | "crate" => {
                at = skip_whitespace(fragment, at + word.len());
                if fragment[at..].starts_with('(') {
                    at =
                        skip_whitespace(fragment, at + 1 + closing_paren(&fragment[at + 1..])? + 1);
                }
            }
            // `unsafe fn` in an enum body: the `fn` behind it is what `ITEM_KEYWORDS` sees.
            "unsafe" => at = skip_whitespace(fragment, at + word.len()),
            "" => return None,
            _ => return Some((at, word)),
        }
    }
}

/// The identifier a fragment starts with, keeping the `r#` a raw identifier is written
/// with.
///
/// `leading_ident` stops at the `#`, which would read `r#in` as a member called `r` and let
/// it through: a raw identifier is Rust's spelling of a name Rust itself reserves, so `r#in`
/// *is* `in`, and the Swift side breaks on either spelling — `#` is not an identifier
/// character there either. The byte length of what this returns is the length consumed, so
/// callers can carry on from it.
fn leading_raw_ident(fragment: &str) -> String {
    let hashed = fragment.strip_prefix("r#");
    let ident: String = hashed
        .unwrap_or(fragment)
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    match hashed {
        Some(_) if !ident.is_empty() => format!("r#{ident}"),
        _ => ident,
    }
}

/// Rust keywords that begin an associated item rather than a member.
///
/// An enum body may hold these and facet ignores them, so this does too. Skipping a member
/// that starts with one cannot hide a collision: each is a Rust keyword, so no variant or
/// field can carry the name.
const ITEM_KEYWORDS: &[&str] = &["const", "extern", "fn", "type"];

/// Whether the identifier ending at `after` is followed by `:` — the shape of a field, a
/// const and a type alias, and of the fragment a `where` clause leaves behind when the
/// splitter cuts it at one of its commas. A variant is never written `name: Type`: it is
/// `Name`, `Name(…)`, `Name { … }` or `Name = 0`.
fn type_annotated(fragment: &str, after: usize) -> bool {
    fragment[after..].trim_start().starts_with(':')
}

/// Whether the identifier ending at `after` is followed by `!` — a macro call, whose
/// expansion a source scan cannot see and does not pretend to.
fn macro_call(fragment: &str, after: usize) -> bool {
    fragment[after..].trim_start().starts_with('!')
}

/// The `{` and matching `}` of a struct-like variant's payload, when it has one.
fn braced_span(fragment: &str, from: usize) -> Option<(usize, usize)> {
    let open = from + fragment[from..].find('{')?;
    Some((open, open + 1 + closing_paren(&fragment[open + 1..])?))
}

/// The offset of the first non-whitespace byte at or after `from`, clamped to the end.
fn skip_whitespace(text: &str, from: usize) -> usize {
    let at = from.min(text.len());
    at + text[at..].len() - text[at..].trim_start().len()
}

/// The spelling facet's Swift emitter gives a Rust variant or field name.
///
/// `heck`'s `to_lower_camel_case`, because that is the call the emitter itself makes:
/// restating the transformation here would be a second implementation of a rule one crate
/// owns, which is what `generated_resources_are_wired` avoids by reading the emitter's own
/// table. A raw identifier loses its `r#` first — that prefix is Rust's way of spelling an
/// identifier Rust itself reserves, so `r#in` *is* `in`, and Swift breaks on either spelling,
/// since `#` is not an identifier character there either.
fn swift_identifier(rust: &str) -> String {
    rust.strip_prefix("r#")
        .unwrap_or(rust)
        .to_lower_camel_case()
}

/// `text` with every comment and every string or character literal replaced by spaces.
///
/// Same length and same newline positions, so an offset into the result points at the same
/// byte of the input and the line numbers printed from either agree — the property every
/// diagnostic in this file depends on.
///
/// Two reasons a structural scan needs this, and both are about prose. A doc comment between
/// two members holds the commas and braces the scan splits on: `Code`'s prose has both, and
/// one of its sentences reads "…a translated line, in parentheses…", so a scan that split on
/// it would find a member called `in` and report a Swift keyword collision that does not
/// exist. And the check must not fire on prose, or on a fixture about itself —
/// `gpui_labels_are_translated` has the same problem with its own needle and answers it with
/// a quote count, which suits a needle and not a scan that has to know where members begin
/// and end.
fn blank_comments_and_literals(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = bytes.to_vec();
    let mut at = 0usize;
    while at < bytes.len() {
        let span = match bytes[at] {
            b'/' if bytes.get(at + 1) == Some(&b'/') => Some((at, line_comment_end(bytes, at))),
            b'/' if bytes.get(at + 1) == Some(&b'*') => Some(block_comment_span(bytes, at)),
            b'"' => Some(string_span(bytes, at)),
            // A `'` opens a character literal or a lifetime, and only the first has a closing
            // quote: `Option<&'a str>` must not blank the rest of the file.
            b'\'' => char_literal_span(bytes, at),
            b'r' if raw_string_starts(bytes, at) => raw_string_span(bytes, at),
            _ => None,
        };
        at = match span {
            Some((from, to)) => {
                blank(&mut out, from, to);
                to.max(at + 1)
            }
            None => at + 1,
        };
    }
    // Only ASCII is ever written over a copy of the input, so this cannot fail; the fallback
    // is here because a check that cannot compile Swift must not become one that cannot run.
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}

/// Replace `[from, to)` with spaces, keeping newlines so line numbers survive.
fn blank(out: &mut [u8], from: usize, to: usize) {
    let end = to.min(out.len());
    for byte in &mut out[from..end] {
        if *byte != b'\n' {
            *byte = b' ';
        }
    }
}

/// The end of a `//` comment: its newline, which belongs to the line rather than the comment.
fn line_comment_end(bytes: &[u8], at: usize) -> usize {
    bytes[at..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |found| at + found)
}

/// The span of a `/* … */` comment, nesting as Rust does. An unterminated one runs to the
/// end of the file, which is what the compiler would have said about it too.
fn block_comment_span(bytes: &[u8], at: usize) -> (usize, usize) {
    let mut depth = 0usize;
    let mut cursor = at;
    while cursor < bytes.len() {
        match (bytes[cursor], bytes.get(cursor + 1)) {
            (b'/', Some(b'*')) => {
                depth += 1;
                cursor += 2;
            }
            (b'*', Some(b'/')) => {
                depth -= 1;
                cursor += 2;
                if depth == 0 {
                    return (at, cursor);
                }
            }
            _ => cursor += 1,
        }
    }
    (at, bytes.len())
}

/// The span of a `"…"` literal, escapes included. An unterminated one runs to the end of the
/// file; so does the compiler's.
const fn string_span(bytes: &[u8], at: usize) -> (usize, usize) {
    let mut cursor = at + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor += 2,
            b'"' => return (at, cursor + 1),
            _ => cursor += 1,
        }
    }
    (at, bytes.len())
}

/// The span of a `'x'` literal, or `None` when the quote opens a lifetime instead.
///
/// Bounded, and terminated by a newline, on purpose: a lifetime has no closing quote, and
/// looking for one would blank everything after it.
fn char_literal_span(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let limit = (at + 12).min(bytes.len());
    let mut cursor = at + 1;
    while cursor < limit {
        match bytes[cursor] {
            b'\n' => return None,
            b'\\' => cursor += 2,
            b'\'' => return Some((at, cursor + 1)),
            _ => cursor += 1,
        }
    }
    None
}

/// Whether `at` starts an `r"…"`, `r#"…"#` or `r##"…"##` literal. The byte before the `r`
/// has to be a non-identifier one, or the last letter of a name would look like one.
fn raw_string_starts(bytes: &[u8], at: usize) -> bool {
    let ends_an_ident = at
        .checked_sub(1)
        .and_then(|before| bytes.get(before))
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
    !ends_an_ident && raw_string_hashes(bytes, at + 1).is_some()
}

/// The `#`s between an `r` and the quote that opens its literal, and that quote's offset.
fn raw_string_hashes(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut hashes = 0usize;
    let mut at = from;
    while bytes.get(at) == Some(&b'#') {
        hashes += 1;
        at += 1;
    }
    (bytes.get(at) == Some(&b'"')).then_some((hashes, at))
}

/// The span of a raw string literal whose `r` is at `at`.
fn raw_string_span(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let (hashes, quote) = raw_string_hashes(bytes, at + 1)?;
    let terminator = format!("\"{}", "#".repeat(hashes));
    let end = bytes[quote + 1..]
        .windows(terminator.len())
        .position(|window| window == terminator.as_bytes())
        .map_or(bytes.len(), |found| quote + 1 + found + terminator.len());
    Some((at, end))
}

/// The scanners behind `facet_names_are_legal_swift_identifiers`.
///
/// Their own module rather than joining `tests` above, which is documented as the scanners
/// behind a different check and whose fixtures the tree walk reads as source text: every
/// fixture here holds the code this check must fire on, and it survives the walk only because
/// it sits inside a literal that `blank_comments_and_literals` erases. That is asserted below
/// rather than assumed, because the alternative is a gate that fails on its own test suite.
#[cfg(test)]
mod swift_keyword_tests {
    use super::*;

    /// Every shape a member declaration takes in the two cores, and the two shapes that must
    /// not be scanned: a `#[derive(Debug)]`-only enum, whose `Internal` is nobody's business,
    /// and a doc comment whose commas and keyword-ish words used to be read as members.
    const FIXTURE: &str = r"
/// The refusals the counter names: a kind, a code, and, in parentheses, an address.
#[derive(Facet, Serialize, Deserialize, Clone, Debug)]
#[repr(C)]
pub enum Code {
    /// There was no identity, or the one offered was refused.
    Unauthenticated,
    Rejected { code: String, detail: String },
}

#[effect(facet_typegen)]
#[derive(Debug)]
pub enum Effect {
    Render(RenderOperation),
    Api(CounterApi),
}

/// What a shell draws: **values**, not sentences.
#[derive(Facet, Clone, Default)]
pub struct ViewModel {
    pub count: i64,
    /// `None` when there is nothing to say.
    pub status: Option<Status>,
}

/// Not on the Swift surface: no facet marker, so this `Internal` is not this check's.
#[derive(Debug, Clone)]
pub enum Local {
    Internal,
}
";

    /// The transformation is `heck`'s because the emitter's is. These are the spellings the
    /// committed Swift carries, plus the shapes a Rust name takes on the way there.
    #[test]
    fn lowercamelcases_the_way_the_emitter_does() {
        assert_eq!(swift_identifier("ServerFault"), "serverFault");
        assert_eq!(swift_identifier("PermissionDenied"), "permissionDenied");
        assert_eq!(swift_identifier("NotFound"), "notFound");
        assert_eq!(swift_identifier("Get"), "get");
        assert_eq!(swift_identifier("Ok"), "ok");
        assert_eq!(swift_identifier("Internal"), "internal");
        assert_eq!(swift_identifier("is_active"), "isActive");
        assert_eq!(swift_identifier("by"), "by");
        // Rust's spelling of an identifier Rust itself reserves.
        assert_eq!(swift_identifier("r#in"), "in");
    }

    /// The decision [`SWIFT_RESERVED_KEYWORDS`] records, pinned from both sides: reserved
    /// words are in, and the contextual group is out because `case get` is legal Swift this
    /// repository already ships. A well-meaning widening for symmetry fails here first.
    #[test]
    fn the_contextual_group_is_out_by_decision() {
        for reserved in [
            "internal", "default", "class", "case", "in", "is", "as", "do", "guard", "repeat",
            "where", "static", "public", "private", "await", "nil", "self", "super",
        ] {
            assert!(
                SWIFT_RESERVED_KEYWORDS.contains(&reserved),
                "{reserved} is reserved and must stay in the list"
            );
        }
        for contextual in [
            "get", "set", "some", "none", "final", "lazy", "override", "weak", "mutating",
            "indirect", "optional", "required", "async", "infix", "prefix", "postfix",
        ] {
            assert!(
                !SWIFT_RESERVED_KEYWORDS.contains(&contextual),
                "{contextual} is usable as an identifier outside its own context"
            );
        }
        // Every entry is lowercase, which is why type names are out of reach: the emitter
        // upper-camel-cases those, and no reserved word begins with a capital.
        assert!(
            SWIFT_RESERVED_KEYWORDS
                .iter()
                .all(|word| !word.chars().any(|ch| ch.is_ascii_uppercase()))
        );
    }

    /// What the scan finds, in source order, and what it leaves alone.
    #[test]
    fn finds_every_name_facet_writes_swift_for() {
        let names = facet_names(&blank_comments_and_literals(FIXTURE));
        let found: Vec<(String, &str)> = names
            .iter()
            .map(|name| (name.qualified.clone(), name.kind))
            .collect();
        assert_eq!(
            found,
            [
                ("Code::Unauthenticated".to_owned(), "variant"),
                ("Code::Rejected".to_owned(), "variant"),
                ("Code::Rejected { code }".to_owned(), "field"),
                ("Code::Rejected { detail }".to_owned(), "field"),
                // An effect crosses the FFI exactly as an event does, so it is scanned too.
                ("Effect::Render".to_owned(), "variant"),
                ("Effect::Api".to_owned(), "variant"),
                ("ViewModel::count".to_owned(), "field"),
                ("ViewModel::status".to_owned(), "field"),
            ]
        );
        // `Local::Internal` is not there: no facet marker, nothing emitted, nothing to say.
        assert!(
            !found
                .iter()
                .any(|(qualified, _)| qualified.contains("Local"))
        );
        // And the line each name is reported at is the file's own, which is what makes the
        // message actionable for a build nobody can run here.
        let internal_line = FIXTURE[..names[0].at].matches('\n').count() + 1;
        assert_eq!(
            internal_line, 7,
            "`Unauthenticated` is on line 7 of the fixture"
        );
    }

    /// Prose is not code. The fixture's doc comments carry commas, braces and the word `in`,
    /// and a scan that read them would report a collision that does not exist.
    #[test]
    fn comments_and_literals_are_not_members() {
        let text = r"
/// A kind, a code, and, in parentheses, an address: `Failure::Rejected { code }`.
#[derive(Facet)]
pub enum Code {
    /// Not a sentence, in any language, and { not a member }.
    Other { name: String },
}
";
        let names = facet_names(&blank_comments_and_literals(text));
        assert_eq!(
            names
                .iter()
                .map(|name| name.text.as_str())
                .collect::<Vec<_>>(),
            ["Other", "name"]
        );
    }

    /// An enum body may also hold associated items, which facet ignores and so does this.
    /// Neither guard can hide a collision: `const`, `fn` and `type` are Rust keywords no
    /// variant may carry, and the `T: Clone` fragment — what the `where` clause leaves behind
    /// once the splitter cuts it at its comma — is `name: Type`, which no variant is.
    #[test]
    fn associated_items_are_not_variants() {
        let text = r"
#[derive(Facet)]
pub enum Code {
    Unauthenticated,
    fn describe<T>(&self, make: T) -> String
    where
        T: Fn() -> String,
        T: Clone,
    {
        make()
    }
    const NONE: Self = Self::Unauthenticated;
}
";
        let names = facet_names(&blank_comments_and_literals(text));
        assert_eq!(
            names
                .iter()
                .map(|name| name.qualified.as_str())
                .collect::<Vec<_>>(),
            ["Code::Unauthenticated"]
        );
    }

    /// A raw identifier is Rust's spelling of a name Rust reserves, so it reaches the emitter
    /// as that name. `in` is reserved in Swift; `type` is not — Swift's `Type` is contextual —
    /// and the pair is what proves the list is doing the deciding rather than the prefix.
    #[test]
    fn a_raw_identifier_is_the_name_it_spells() {
        let text = r"
#[derive(Facet)]
pub struct Query {
    pub r#in: String,
    pub r#type: String,
}
";
        let names = facet_names(&blank_comments_and_literals(text));
        assert_eq!(
            names
                .iter()
                .map(|name| name.text.as_str())
                .collect::<Vec<_>>(),
            ["r#in", "r#type"]
        );
        let reserved: Vec<&str> = names
            .iter()
            .filter(|name| SWIFT_RESERVED_KEYWORDS.contains(&swift_identifier(&name.text).as_str()))
            .map(|name| name.qualified.as_str())
            .collect();
        assert_eq!(reserved, ["Query::r#in"]);
    }

    /// The message has to carry the `file:line` and say what to do, because the reader cannot
    /// reproduce this in a Swift build — the diagnostic is the whole feedback loop. The
    /// fixture is the bug `bea8c4f` shipped and then renamed, put back.
    #[test]
    fn reports_the_line_and_the_rename() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let core = root.path().join("apps/probe/core/src");
        std::fs::create_dir_all(&core).expect("the fixture directory");
        std::fs::write(
            core.join("api.rs"),
            "#[derive(Facet)]\n#[repr(C)]\npub enum Code {\n    Internal,\n    ServerFault,\n}\n",
        )
        .expect("the fixture file");

        let (mut checked, mut problems) = (0usize, Vec::new());
        facet_names_are_legal_swift_identifiers(root.path(), &mut checked, &mut problems);

        assert_eq!(checked, 2, "the legal variant is looked at too");
        assert_eq!(problems.len(), 1, "{problems:?}");
        let problem = problems[0].as_str();
        assert!(
            problem.starts_with("apps/probe/core/src/api.rs:4: "),
            "{problem}"
        );
        assert!(problem.contains("`Code::Internal`"), "{problem}");
        assert!(problem.contains("`internal`"), "{problem}");
        assert!(problem.contains("Rename the Rust variant"), "{problem}");
    }

    /// The spelling that left the check green while `Code::Internal` sat under it — reported
    /// from outside this file, with the count unchanged, which is how it reads as "nothing to
    /// do". `collect_markers` matches a literal prefix, so the `derive(` inside the
    /// `cfg_attr(` was never looked at, and `#[effect(` was covered only because it is
    /// special-cased. The gate below the fixture is the negative control: a `cfg_attr` that
    /// derives something else must not put a type on the Swift surface.
    #[test]
    fn a_cfg_attr_derive_is_a_derive() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let core = root.path().join("apps/probe/core/src");
        std::fs::create_dir_all(&core).expect("the fixture directory");
        std::fs::write(
            core.join("api.rs"),
            r#"#[cfg_attr(feature = "facet_typegen", derive(::facet::Facet))]
pub enum Code {
    Internal,
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Local {
    Internal,
}
"#,
        )
        .expect("the fixture file");

        let (mut checked, mut problems) = (0usize, Vec::new());
        facet_names_are_legal_swift_identifiers(root.path(), &mut checked, &mut problems);

        assert_eq!(
            checked, 1,
            "only the facet-gated enum is scanned: {problems:?}"
        );
        assert_eq!(problems.len(), 1, "{problems:?}");
        let problem = problems[0].as_str();
        assert!(
            problem.starts_with("apps/probe/core/src/api.rs:3: "),
            "{problem}"
        );
        assert!(problem.contains("`Code::Internal`"), "{problem}");
    }

    /// This check reads every `.rs` file in the repository, including the two that implement
    /// it, and the fixtures above hold exactly the source text it must fire on. They are
    /// literals, so the blanker erases them and the file that tests the check is not a
    /// violation of it. Both files are read because the check used to live wholly in
    /// `invariants.rs` and now lives wholly here: whichever of the two prose moves to, the
    /// scan still has to stay quiet about it.
    #[test]
    fn this_file_is_not_a_violation_of_its_own_check() {
        for path in ["swift_keywords.rs", "invariants.rs"] {
            let names = facet_names(&blank_comments_and_literals(match path {
                "swift_keywords.rs" => include_str!("swift_keywords.rs"),
                _ => include_str!("invariants.rs"),
            }));
            assert!(
                names.is_empty(),
                "{path} mentions `Facet` types in prose and in fixtures: {:?}",
                names
                    .iter()
                    .map(|name| name.qualified.clone())
                    .collect::<Vec<_>>()
            );
        }
    }

    /// A lifetime is not a character literal, and mistaking one blanks the rest of the file.
    #[test]
    fn blanking_preserves_offsets_and_spares_lifetimes() {
        let text =
            "pub fn f<'a>(x: &'a str) -> &'a str { x } // a { comment, with commas\nlet y = 1;\n";
        let blanked = blank_comments_and_literals(text);
        assert_eq!(blanked.len(), text.len());
        assert_eq!(blanked.matches('\n').count(), text.matches('\n').count());
        assert!(blanked.contains("&'a str"), "{blanked}");
        assert!(!blanked.contains("comment"), "{blanked}");
        assert!(blanked.contains("let y = 1;"), "{blanked}");
    }
}
