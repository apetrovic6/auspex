//! Splicing new declarations into Rust files that already exist.
//!
//! Three quarters of what these generators do is editing, not writing: a migration is
//! inert until it is in the `Migrator`, a feature slice is dead code until `lib.rs`
//! declares it and `register_services` mounts it. Those edits are the part a person
//! forgets, which is the whole reason to generate them.
//!
//! # Why text, and not a parser
//!
//! `syn` would turn these into tree edits, and then a printer would have to put the file
//! back — losing every comment in it. The files being edited here are among the most
//! heavily commented in the repository (`startup.rs` explains layer ordering at length),
//! and a tool that strips that to add one line is a bad trade. `toml_edit` exists for
//! exactly this reason on the manifest side; it has no Rust equivalent, so these are
//! line-oriented edits with a test each.
//!
//! Every function here is **total about failure**: it either makes the edit or returns an
//! error naming what it could not find. None of them silently does nothing, because the
//! generated code depends on the edit having happened, and a no-op would surface as a
//! compile error pointing at the wrong file.

use anyhow::{Result, bail};

/// Insert a `mod` or `pub mod` declaration, keeping the block sorted.
///
/// Sorted because rustfmt does not sort `mod` declarations — it sorts `use` — so the order
/// is whatever the last person left, and an appended line makes every future diff of this
/// file start with a reordering. `anchor` is where the declaration goes when there is no
/// block yet: the line it is inserted above, which for a freshly installed `Migrator` is
/// the struct itself.
pub fn insert_mod(source: &str, declaration: &str, anchor: &str) -> Result<String> {
    let name = declaration
        .trim_end_matches(';')
        .rsplit(' ')
        .next()
        .unwrap_or_default();
    let prefix = declaration
        .trim()
        .strip_suffix(&format!(" {name};"))
        .unwrap_or("mod");

    let mut lines: Vec<String> = source.lines().map(ToOwned::to_owned).collect();

    if lines.iter().any(|line| line.trim() == declaration.trim()) {
        bail!("`{}` is already declared", declaration.trim());
    }

    // The block of same-visibility declarations, if there is one.
    let block: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            let line = line.trim();
            line.starts_with(&format!("{prefix} ")) && line.ends_with(';')
        })
        .map(|(index, _)| index)
        .collect();

    let at = if block.is_empty() {
        let anchor_at = lines
            .iter()
            .position(|line| line.contains(anchor))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no `{prefix}` declarations and no line containing {anchor:?} to put \
                     one above"
                )
            })?;
        // A blank line after, so the new block is separated from whatever it sits above.
        lines.insert(anchor_at, String::new());
        anchor_at
    } else {
        // The first declaration that sorts after the new one, or just past the block.
        block
            .iter()
            .copied()
            .find(|&index| lines[index].trim() > declaration.trim())
            .unwrap_or_else(|| block[block.len() - 1] + 1)
    };

    lines.insert(at, declaration.trim().to_owned());

    let mut out = lines.join("\n");
    if source.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Append a migration to the `vec![..]` the `Migrator` returns.
///
/// Appended rather than sorted: migrations run in the order of this list, and the file
/// names are timestamps, so "newest last" is both the sorted order and the correct one.
/// Inserting anywhere else would reorder a schema's history.
pub fn push_migration(source: &str, module: &str) -> Result<String> {
    let entry = format!("Box::new({module}::Migration)");
    if source.contains(&entry) {
        bail!("`{entry}` is already in the migration list");
    }

    let start = source
        .find("vec![")
        .ok_or_else(|| anyhow::anyhow!("no `vec![` — is this a MigratorTrait implementation?"))?;
    let open = start + "vec![".len();
    let close = matching(&source[open..], '[', ']')
        .ok_or_else(|| anyhow::anyhow!("the migration list's `vec![` is never closed"))?
        + open;

    let inside = source[open..close].trim();
    let separator = if inside.is_empty() { "" } else { ", " };
    let rebuilt = format!("{}{separator}{entry}", inside.trim_end_matches(','));

    Ok(format!("{}{rebuilt}{}", &source[..open], &source[close..]))
}

/// Add a name to the `use crate::{..};` group.
///
/// `startup.rs` reaches its features through that group — `use crate::{API_PREFIX, counter,
/// health, timeouts}` — so a service added to the chain without its module named here does
/// not resolve. The failure says "cannot find module or crate `lake` in this scope" and
/// points at `lib.rs`, which *does* declare it, so the message sends a reader to the one
/// file that is already correct.
///
/// Appended rather than inserted in order: rustfmt sorts the names inside a use group, and
/// every caller runs it over the file afterwards.
pub fn insert_into_crate_use(source: &str, name: &str) -> Result<String> {
    insert_into_use_group(source, "crate::", name)
}

/// Add a name to the braced `use <path>{..};` group of that path.
///
/// The general form of [`insert_into_crate_use`], which is one caller of it. The other is
/// `new-rpc`, which has two groups to extend in a handler file it did not write the rest of:
/// the wire module the request and response types come from, and the feature's `domain`
/// module the new DTO lives in. A second implementation of "add a name to a use group"
/// would be a second set of edge cases about trailing commas, and the two would differ on
/// the first list that had one.
///
/// `path` carries its own `::`, so `crate::` and `proto::r#gen::buffa::lake::v1::` are both
/// written the way the file spells them — the brace is what this appends.
///
/// Appended rather than inserted in order: rustfmt sorts the names inside a use group, and
/// every caller runs it over the file afterwards.
pub fn insert_into_use_group(source: &str, path: &str, name: &str) -> Result<String> {
    let group = format!("use {path}{{");
    let start = source
        .find(&group)
        .ok_or_else(|| anyhow::anyhow!("no `use {path}{{..}}` group to add `{name}` to"))?;
    let open = start + group.len();
    let close = matching(&source[open..], '{', '}')
        .ok_or_else(|| anyhow::anyhow!("the `use {path}{{..}}` group is never closed"))?
        + open;

    let inside = source[open..close].trim();
    if inside.split(',').any(|existing| existing.trim() == name) {
        bail!("`{name}` is already in the `use {path}{{..}}` group");
    }

    let rebuilt = format!("{}, {name}", inside.trim_end_matches(','));
    Ok(format!("{}{rebuilt}{}", &source[..open], &source[close..]))
}

/// Add an item to the end of a block whose opening line is `header`.
///
/// Written for the `impl <Trait> for <Type> {` of a generated service implementation, which
/// is where `new-rpc` has to put one more handler. Appended at the end of the block rather
/// than sorted, because handler order in an impl is the order the methods were added and
/// nothing reads it — unlike a `mod` block, where rustfmt's silence makes the order
/// whatever the last person left.
///
/// `header` must appear exactly once, for the reason [`apply`] gives: zero means the file is
/// not the shape this edit was written for, and more than one means the edit would land in
/// an arbitrary place. The brace counting is naive about braces inside string literals, as
/// every edit in this module is — the files it is pointed at are generated Rust whose
/// strings are span names.
///
/// # Errors
///
/// Returns an error when `header` is absent or ambiguous, or when its block is never closed.
pub fn insert_into_block(source: &str, header: &str, item: &str) -> Result<String> {
    match source.matches(header).count() {
        1 => {}
        0 => bail!("no `{header}` to add to"),
        n => bail!("`{header}` appears {n} times, so the edit is ambiguous"),
    }

    let start = source.find(header).unwrap_or_default();
    let open = source[start..]
        .find('{')
        .map(|offset| start + offset + 1)
        .ok_or_else(|| anyhow::anyhow!("`{header}` opens no block"))?;
    let close = matching(&source[open..], '{', '}')
        .ok_or_else(|| anyhow::anyhow!("`{header}`'s block is never closed"))?
        + open;

    // A blank line before the item, so the new member is separated from the one above it the
    // way every other member of a generated impl is. rustfmt does not insert one.
    let inside = source[open..close].trim_end_matches([' ', '\n']);
    Ok(format!(
        "{}{inside}\n\n{}\n{}",
        &source[..open],
        item.trim_end_matches('\n'),
        &source[close..]
    ))
}

/// Add a service to the chain in `register_services`.
///
/// The whole chain is re-emitted rather than a line appended, and the reason is the last
/// element: every call but the final one passes `db.clone()`, and the final one moves `db`.
/// Appending a line would leave two moves of the same value — a compile error in a file the
/// generator did not otherwise touch. Re-emitting makes "the last one moves" a property of
/// this function instead of something the caller has to remember.
pub fn register_service(source: &str, constructor: &str) -> Result<String> {
    let entry = format!("Arc::new({constructor})");

    let signature = source
        .find("fn register_services")
        .ok_or_else(|| anyhow::anyhow!("no `fn register_services` to add the service to"))?;
    let body_open = source[signature..]
        .find('{')
        .map(|offset| signature + offset + 1)
        .ok_or_else(|| anyhow::anyhow!("`register_services` has no body"))?;
    let body_close = matching(&source[body_open..], '{', '}')
        .ok_or_else(|| anyhow::anyhow!("`register_services`'s body is never closed"))?
        + body_open;
    let body = &source[body_open..body_close];

    let mut entries = Vec::new();
    let mut cursor = 0;
    let (mut first, mut last) = (None, 0);
    while let Some(offset) = body[cursor..].find(".add_service(") {
        let open = cursor + offset + ".add_service(".len();
        let close = matching(&body[open..], '(', ')')
            .ok_or_else(|| anyhow::anyhow!("an `add_service(` call is never closed"))?
            + open;
        first.get_or_insert(cursor + offset);
        last = close + 1;
        // Canonical form moves `db`; the clones are a function of position, so they are
        // stripped here and re-applied on the way out.
        entries.push(body[open..close].replace("(db.clone())", "(db)"));
        cursor = close + 1;
    }

    let Some(first) = first else {
        bail!("`register_services` has no `.add_service(` calls to extend");
    };
    if entries.contains(&entry) {
        bail!("`{entry}` is already registered");
    }
    entries.push(entry);

    let count = entries.len();
    let chain: String = entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            let entry = if index + 1 == count {
                entry
            } else {
                entry.replace("(db)", "(db.clone())")
            };
            format!(".add_service({entry})")
        })
        .collect::<Vec<_>>()
        .join("\n");

    Ok(format!(
        "{}{chain}{}",
        &source[..body_open + first],
        &source[body_open + last..]
    ))
}

/// Add a name to a `const NAME: &[&str] = &[..]` list.
///
/// The lists this exists for are the classification lists in `tests/<service>/auth.rs`,
/// which are the only place a decision about *who may call an RPC* is written down, and
/// which a test fails when a registered method is in neither of them. A generator that
/// registers methods and leaves those lists alone therefore writes a slice that compiles
/// and fails the suite, and the repair it hands to a human is to spell again the names the
/// generator already knew — so the generator writes them.
///
/// The edit happens **inside the brackets, and only inside them**. Both lists carry long
/// doc comments explaining what a third list is for and why the assertion below them is an
/// equality; a primitive that re-emitted the `const` to add one entry would reprint the
/// file and lose them, which is the exact trade this module exists to refuse.
///
/// The list's own shape is kept rather than normalised: a list already broken over lines
/// gains a line at the indentation of the entries already there, a list on one line gains a
/// name on that line. `report` runs rustfmt over the file afterwards, and it is allowed to
/// have opinions about width — it is not allowed to have lost a comment.
///
/// A name already in the list is refused rather than added twice. The file asserts no
/// method is classified both public and protected, so a generated duplicate is that
/// failure's problem with a worse message.
///
/// # Errors
///
/// Returns an error when the file declares no such `const`, when its value is not a `[..]`
/// literal, when that literal is never closed, or when `entry` is already in it.
pub fn insert_into_const_list(source: &str, list: &str, entry: &str) -> Result<String> {
    let at = const_declaration(source, list).ok_or_else(|| {
        anyhow::anyhow!(
            "no `const {list}:` at the start of a line — this file has no such list to add \
             `{entry}` to"
        )
    })?;

    // `= &[`, not the first `[` after the name: the type annotation of `const X: &[&str]`
    // opens a bracket too, and it is the one bracket in the file that is never a value.
    let open = source[at..]
        .find("= &[")
        .map(|offset| at + offset + "= &[".len())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "`{list}`'s declaration has no `= &[` — its value is not a slice literal \
                 this edit can splice into"
            )
        })?;
    let close = matching(&source[open..], '[', ']')
        .ok_or_else(|| anyhow::anyhow!("the `{list}` list is never closed"))?
        + open;

    let inside = &source[open..close];
    // Split on the quotes rather than testing for a substring: `.../CreateLake` is a
    // substring of `".../CreateLakes"`, and refusing that would leave a method classified
    // under a name no router answers to.
    if inside.split('"').any(|existing| existing == entry) {
        bail!("`{entry}` is already in the `{list}` list");
    }
    let quoted = format!("\"{entry}\"");

    let rebuilt = if inside.contains('\n') {
        // Indentation read off the entries rather than assumed, so a const inside a module
        // or a `#[cfg]` block keeps the shape it had.
        let indent = inside
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map_or_else(
                || String::from("    "),
                |line| line[..line.len() - line.trim_start().len()].to_owned(),
            );
        let body = inside.trim_end();
        // A hand-written list already has a trailing comma, because that is the shape
        // rustfmt leaves it in; without this the next entry would be a syntax error.
        let body = if body.ends_with(',') {
            body.to_owned()
        } else if body.trim().is_empty() {
            String::new()
        } else {
            format!("{body},")
        };
        format!("{body}\n{indent}{quoted},\n")
    } else {
        let body = inside.trim().trim_end_matches(',').trim_end();
        if body.is_empty() {
            quoted
        } else {
            format!("{body}, {quoted}")
        }
    };

    Ok(format!("{}{rebuilt}{}", &source[..open], &source[close..]))
}

/// Read the entries of a `const NAME: &[&str] = &[..]` list, as written.
///
/// The read half of [`insert_into_const_list`]: the generators that register methods write
/// into these lists, and `add-auth-oidc` reads one back out to learn which contract a
/// service speaks. One parser for the shape, rather than two that could disagree about
/// where a list ends or what counts as an entry — the disagreement would be a generator
/// that writes a name its sibling cannot read, and a service the next generator miswires.
///
/// # Errors
///
/// Returns an error when the file declares no such `const`, when its value is not a `[..]`
/// literal, or when that literal is never closed.
pub fn read_const_list(source: &str, list: &str) -> Result<Vec<String>> {
    let at = const_declaration(source, list).ok_or_else(|| {
        anyhow::anyhow!(
            "no `const {list}:` at the start of a line — this file has no such list to read"
        )
    })?;
    let open = source[at..]
        .find("= &[")
        .map(|offset| at + offset + "= &[".len())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "`{list}`'s declaration has no `= &[` — its value is not a slice literal \
                 this edit can read"
            )
        })?;
    let close = matching(&source[open..], '[', ']')
        .ok_or_else(|| anyhow::anyhow!("the `{list}` list is never closed"))?
        + open;

    let inside = &source[open..close];
    Ok(inside
        .split('"')
        .skip(1)
        .step_by(2)
        .map(ToOwned::to_owned)
        .collect())
}

/// Offset of the line that declares `const NAME`, or `None` when there is none.
///
/// Anchored at the start of a line so neither a mention inside a doc comment nor a local
/// `const` inside a test counts as the list: an edit into a comment is a silent no-op, and
/// silent no-ops are what this module is written not to do.
fn const_declaration(source: &str, list: &str) -> Option<usize> {
    let declaration = format!("const {list}:");
    let mut offset = 0;
    for line in source.lines() {
        if line.trim_start().starts_with(&declaration) {
            return Some(offset);
        }
        offset += line.len() + 1;
    }
    None
}

/// One anchored edit: the text to find, and what to put in its place.
///
/// `what` names the edit in a diagnostic, because the failure a reader has to act on is
/// "this service's `main.rs` does not look like the one this edit expects", and an anchor
/// quoted back at them is evidence rather than an explanation.
pub struct Edit<'a> {
    pub what: &'a str,
    pub find: &'a str,
    pub replace: String,
}

/// Apply anchored edits to a file, all or none.
///
/// Every anchor must appear **exactly once**. Zero means the file is not the shape the
/// edit was written for; more than one means the edit is ambiguous and would land in an
/// arbitrary place. Both are refusals rather than best guesses, because the alternative is
/// a service that has been half converted and a second run that refuses because of the
/// mess the first one made.
pub fn apply(source: &str, edits: &[Edit<'_>]) -> Result<String> {
    for edit in edits {
        match source.matches(edit.find).count() {
            1 => {}
            0 => bail!(
                "could not {}: nothing matches\n\n{}\n",
                edit.what,
                indent(edit.find)
            ),
            n => bail!(
                "could not {}: {n} places match, so the edit is ambiguous\n\n{}\n",
                edit.what,
                indent(edit.find)
            ),
        }
    }

    let mut out = source.to_owned();
    for edit in edits {
        out = out.replacen(edit.find, &edit.replace, 1);
    }
    Ok(out)
}

/// Quote an anchor back at the reader, indented so it is obviously quoted text.
fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("      {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Offset of the closing delimiter matching the one just before `text`.
fn matching(text: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 1_usize;
    for (index, character) in text.char_indices() {
        if character == open {
            depth += 1;
        } else if character == close {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mod_lands_in_sorted_position() {
        let source = "use sea_orm_migration::prelude::*;\n\
                      \n\
                      mod m20260101_000001_a;\n\
                      mod m20260301_000001_c;\n\
                      \n\
                      pub struct Migrator;\n";
        let out = insert_mod(source, "mod m20260201_000001_b;", "pub struct Migrator;")
            .expect("the block exists");
        let mods: Vec<&str> = out
            .lines()
            .filter(|line| line.starts_with("mod "))
            .collect();
        assert_eq!(
            mods,
            [
                "mod m20260101_000001_a;",
                "mod m20260201_000001_b;",
                "mod m20260301_000001_c;"
            ]
        );
    }

    #[test]
    fn the_first_mod_goes_above_the_anchor() {
        // What `add-database` leaves behind: a Migrator with no migrations yet.
        let source = "use sea_orm_migration::prelude::*;\n\npub struct Migrator;\n";
        let out = insert_mod(source, "mod m20260101_000001_a;", "pub struct Migrator;")
            .expect("the anchor exists");
        assert!(
            out.contains("mod m20260101_000001_a;\n\npub struct Migrator;"),
            "{out}"
        );
    }

    #[test]
    fn a_duplicate_mod_is_refused() {
        let source = "mod a;\npub struct Migrator;\n";
        let error = insert_mod(source, "mod a;", "pub struct Migrator;")
            .expect_err("a duplicate must be refused");
        assert!(error.to_string().contains("already declared"), "{error}");
    }

    #[test]
    fn a_pub_mod_does_not_land_among_private_ones() {
        let source = "mod zzz_private;\n\npub mod a;\npub mod c;\n";
        let out = insert_mod(source, "pub mod b;", "pub mod a;").expect("the block exists");
        let pubs: Vec<&str> = out
            .lines()
            .filter(|line| line.starts_with("pub mod "))
            .collect();
        assert_eq!(pubs, ["pub mod a;", "pub mod b;", "pub mod c;"]);
        assert!(
            out.starts_with("mod zzz_private;"),
            "the private one stayed put"
        );
    }

    #[test]
    fn a_migration_is_appended_to_the_list() {
        let source = "fn migrations() -> Vec<Box<dyn MigrationTrait>> {\n\
                      vec![Box::new(m20260101_000001_a::Migration)]\n}\n";
        let out = push_migration(source, "m20260201_000001_b").expect("a list to extend");
        assert!(
            out.contains(
                "vec![Box::new(m20260101_000001_a::Migration), \
                 Box::new(m20260201_000001_b::Migration)]"
            ),
            "{out}"
        );
    }

    #[test]
    fn the_first_migration_lands_in_an_empty_list() {
        let source = "fn migrations() -> Vec<Box<dyn MigrationTrait>> {\n    vec![]\n}\n";
        let out = push_migration(source, "m20260101_000001_a").expect("an empty list");
        assert!(
            out.contains("vec![Box::new(m20260101_000001_a::Migration)]"),
            "{out}"
        );
    }

    #[test]
    fn a_duplicate_migration_is_refused() {
        let source = "vec![Box::new(m20260101_000001_a::Migration)]";
        let error =
            push_migration(source, "m20260101_000001_a").expect_err("a duplicate is refused");
        assert!(error.to_string().contains("already in"), "{error}");
    }

    #[test]
    fn registering_a_service_moves_the_clone_to_the_new_last_call() {
        let source = "pub fn register_services(router: Router, db: DatabaseConnection) -> Router {\n\
                          router\n\
                              .add_service(Arc::new(health::service::v1::Health::new(db.clone())))\n\
                              .add_service(Arc::new(counter::service::v1::Counter::new(db)))\n\
                      }\n";

        let out = register_service(source, "lake::service::v1::Lake::new(db)")
            .expect("a chain to extend");

        // The counter now clones, because it is no longer last.
        assert!(
            out.contains("counter::service::v1::Counter::new(db.clone())"),
            "{out}"
        );
        // And the new one moves.
        assert!(out.contains("lake::service::v1::Lake::new(db)))"), "{out}");
        // Exactly one call may move `db`, or the result does not compile.
        assert_eq!(
            out.matches("::new(db)))").count(),
            1,
            "exactly one service may move the connection:\n{out}"
        );
        assert!(out.contains("router"), "the receiver survived");
    }

    #[test]
    fn registering_a_service_twice_is_refused() {
        let source = "pub fn register_services(router: Router, db: DatabaseConnection) -> Router {\n\
                          router\n\
                              .add_service(Arc::new(counter::service::v1::Counter::new(db)))\n\
                      }\n";
        let error = register_service(source, "counter::service::v1::Counter::new(db)")
            .expect_err("a duplicate registration is refused");
        assert!(error.to_string().contains("already registered"), "{error}");
    }

    #[test]
    fn a_feature_reaches_the_use_group_as_well_as_the_chain() {
        // Registering without this produces "cannot find module or crate `lake`" pointing
        // at a `lib.rs` that already declares it — found the hard way, so it has a test.
        let source = "use crate::{API_PREFIX, counter, health, timeouts};\n";
        let out = insert_into_crate_use(source, "lake").expect("a group to extend");
        assert!(
            out.contains("use crate::{API_PREFIX, counter, health, timeouts, lake};"),
            "{out}"
        );

        let error = insert_into_crate_use(&out, "lake").expect_err("a duplicate is refused");
        assert!(error.to_string().contains("already in"), "{error}");
    }

    #[test]
    fn a_name_joins_the_group_its_path_names() {
        // `new-rpc` extends two groups in one file, and the wrong one is a type imported
        // from a module that does not have it — so the path, not the first `{`, decides.
        let source = "use proto::r#gen::buffa::lake::v1::{CreateLakeRequest};\n\
                      use crate::lake::domain::{self, LakeDto};\n";
        let out = insert_into_use_group(source, "crate::lake::domain::", "ArchiveLakeDto")
            .expect("the group is there");
        assert!(
            out.contains("use crate::lake::domain::{self, LakeDto, ArchiveLakeDto};"),
            "{out}"
        );
        assert!(
            out.contains("use proto::r#gen::buffa::lake::v1::{CreateLakeRequest};"),
            "the other group is untouched:\n{out}"
        );

        let error =
            insert_into_use_group(source, "crate::pond::domain::", "X").expect_err("no such group");
        assert!(
            error.to_string().contains("crate::pond::domain::"),
            "{error}"
        );
    }

    #[test]
    fn an_item_lands_at_the_end_of_the_block_it_names() {
        let source = "impl LakeService for Lake {\n\
                      \x20   async fn get_lake(&self) {}\n\
                      }\n\
                      \n\
                      impl Lake {\n\
                      \x20   fn row(&self) {}\n\
                      }\n";
        let out = insert_into_block(
            source,
            "impl LakeService for Lake {",
            "    async fn archive_lake(&self) {}\n",
        )
        .expect("the block is there");

        assert!(
            out.contains(
                "\x20   async fn get_lake(&self) {}\n\n\
                 \x20   async fn archive_lake(&self) {}\n}"
            ),
            "{out}"
        );
        // The impl below it is where it was, and still closed.
        assert!(
            out.ends_with("impl Lake {\n    fn row(&self) {}\n}\n"),
            "{out}"
        );
    }

    #[test]
    fn a_block_that_is_absent_or_ambiguous_is_refused() {
        let error = insert_into_block("fn main() {}", "impl LakeService for Lake {", "x")
            .expect_err("nothing to add to");
        assert!(
            error.to_string().contains("no `impl LakeService"),
            "{error}"
        );

        // Two matching headers would make the edit land in an arbitrary one, which is the
        // silent no-op's noisier cousin: a handler mounted on the wrong service.
        let twice = "impl A for B {\n}\nimpl A for B {\n}\n";
        let error = insert_into_block(twice, "impl A for B {", "x").expect_err("ambiguous");
        assert!(error.to_string().contains("2 times"), "{error}");
    }

    #[test]
    fn anchored_edits_are_all_or_nothing() {
        let source = "alpha\nbeta\ngamma\n";
        let edits = [
            Edit {
                what: "rename alpha",
                find: "alpha",
                replace: "ALPHA".to_owned(),
            },
            Edit {
                what: "rename delta",
                find: "delta",
                replace: "DELTA".to_owned(),
            },
        ];

        let error = apply(source, &edits).expect_err("a missing anchor is refused");
        assert!(error.to_string().contains("rename delta"), "{error}");
        // The point of all-or-nothing: the first edit must not have been applied either.
        assert!(!error.to_string().contains("ALPHA"), "{error}");
    }

    #[test]
    fn an_ambiguous_anchor_is_refused() {
        let error = apply(
            "x\nx\n",
            &[Edit {
                what: "replace x",
                find: "x",
                replace: "y".to_owned(),
            }],
        )
        .expect_err("two matches is ambiguous");
        assert!(error.to_string().contains("ambiguous"), "{error}");
    }

    #[test]
    fn a_missing_register_services_is_reported() {
        let error = register_service("fn main() {}", "x::New::new(db)")
            .expect_err("a missing function is reported");
        assert!(error.to_string().contains("register_services"), "{error}");
    }

    /// The classification table as `tests/<service>/auth.rs` actually writes it: three
    /// consts, two of them the lists, all of them commented.
    const AUTH: &str = "/// Every method this service registers that answers an anonymous caller.\n\
                        ///\n\
                        /// Not documentation. The test fails when a method appears in neither list.\n\
                        const PUBLIC: &[&str] = &[\n    \"counter.v1.CounterService/Get\",\n];\n\
                        \n\
                        /// Every method that requires an identity.\n///\n/// Moving a name `out` of here is the visible diff a reviewer asks about.\n\
                        const PROTECTED: &[&str] = &[\"counter.v1.CounterService/Whoami\"];\n";

    #[test]
    fn a_classification_lands_in_the_list_it_names() {
        let out = insert_into_const_list(AUTH, "PROTECTED", "lake.v1.LakeService/CreateLake")
            .expect("both lists are there");

        assert!(out.contains("\"lake.v1.LakeService/CreateLake\""), "{out}");
        // Which list it went into is the whole point of naming one. Everything above
        // `const PROTECTED` — including all of PUBLIC — has to be as it was.
        let (above, protected_list) = out.split_once("const PROTECTED").expect("PROTECTED");
        assert!(
            !above.contains("CreateLake"),
            "the new name landed above the list it was given:\n{above}"
        );
        assert!(
            protected_list.contains("CreateLake"),
            "the new name is not in the list it was given:\n{protected_list}"
        );
    }

    #[test]
    fn the_doc_comments_survive_the_edit() {
        // The whole reason this primitive is line-oriented rather than a tree edit: the
        // comment above each list says what the list is for, and a tool that reprints the
        // const to add one entry has made the file worse than the change it was asked for.
        let out = insert_into_const_list(AUTH, "PROTECTED", "lake.v1.LakeService/GetLake")
            .expect("both lists are there");
        for comment in [
            "/// Every method this service registers that answers an anonymous caller.",
            "/// Not documentation. The test fails when a method appears in neither list.",
            "/// Every method that requires an identity.",
            "/// Moving a name `out` of here is the visible diff a reviewer asks about.",
        ] {
            assert!(out.contains(comment), "lost: {comment}\n{out}");
        }
        assert_eq!(
            out.matches("///").count(),
            AUTH.matches("///").count(),
            "{out}"
        );
    }

    #[test]
    fn a_vertical_list_gains_a_line_at_its_own_indentation() {
        let out = insert_into_const_list(AUTH, "PUBLIC", "lake.v1.LakeService/GetLake")
            .expect("a vertical list");
        assert!(
            out.contains(
                "    \"counter.v1.CounterService/Get\",\n    \"lake.v1.LakeService/GetLake\",\n];"
            ),
            "the new entry should be a line of its own, at the entries' indentation:\n{out}"
        );
    }

    #[test]
    fn a_horizontal_list_gains_a_name_on_the_same_line() {
        let out = insert_into_const_list(AUTH, "PROTECTED", "lake.v1.LakeService/GetLake")
            .expect("a horizontal list");
        assert!(
            out.contains("\"counter.v1.CounterService/Whoami\", \"lake.v1.LakeService/GetLake\"];"),
            "the new name should join the line the list was on:\n{out}"
        );
    }

    #[test]
    fn every_entry_a_generated_list_needs_can_be_added() {
        // The generator adds four in a row, and each one re-reads what the last left behind.
        let mut source = AUTH.to_owned();
        for method in [
            "lake.v1.LakeService/CreateLake",
            "lake.v1.LakeService/GetLake",
            "lake.v1.LakeService/ListLakes",
            "lake.v1.LakeService/UpdateLake",
        ] {
            source = insert_into_const_list(&source, "PROTECTED", method)
                .expect("a list that keeps taking entries");
        }
        for method in [
            "lake.v1.LakeService/CreateLake",
            "lake.v1.LakeService/UpdateLake",
        ] {
            assert!(
                source.contains(&format!("\"{method}\"")),
                "{method} missing:\n{source}"
            );
        }
        assert_eq!(source.matches("const PROTECTED:").count(), 1, "{source}");
    }

    #[test]
    fn an_already_classified_method_is_refused() {
        let error = insert_into_const_list(AUTH, "PROTECTED", "counter.v1.CounterService/Whoami")
            .expect_err("a duplicate is refused");
        assert!(error.to_string().contains("already in"), "{error}");
    }

    #[test]
    fn a_name_that_shares_a_prefix_is_not_a_duplicate() {
        // `CreateLake` is a substring of `CreateLakes`. A substring test here would report
        // the classification as done and leave a method named in neither list.
        let source =
            String::from("const PUBLIC: &[&str] = &[\"lake.v1.LakeService/CreateLakes\"];\n");
        let source = insert_into_const_list(&source, "PUBLIC", "lake.v1.LakeService/CreateLake")
            .expect("a different method, not a duplicate");
        // `…CreateLake` also occurs inside `…CreateLakes`, hence two.
        assert_eq!(
            source.matches("lake.v1.LakeService/CreateLake").count(),
            2,
            "{source}"
        );
    }

    #[test]
    fn a_missing_list_is_reported_as_such() {
        let error = insert_into_const_list("fn main() {}", "PROTECTED", "x.Y/Z")
            .expect_err("a file with no such const is not an edit");
        assert!(error.to_string().contains("const PROTECTED:"), "{error}");

        // A list named only in prose is not a list.
        let error = insert_into_const_list(
            "// const PROTECTED: &[&str] = &[]; — a note about the old shape of this file\n",
            "PROTECTED",
            "x.Y/Z",
        )
        .expect_err("a comment is not a const");
        assert!(error.to_string().contains("const PROTECTED:"), "{error}");
    }

    #[test]
    fn an_empty_list_takes_its_first_entry() {
        let out = insert_into_const_list(
            "const PROTECTED: &[&str] = &[];\n",
            "PROTECTED",
            "lake.v1.LakeService/GetLake",
        )
        .expect("an empty list is still a list");
        assert!(out.contains("&[\"lake.v1.LakeService/GetLake\"]"), "{out}");

        let out = insert_into_const_list(
            "const PROTECTED: &[&str] = &[\n];\n",
            "PROTECTED",
            "lake.v1.LakeService/GetLake",
        )
        .expect("an empty vertical list is still a list");
        assert!(
            out.contains("&[\n    \"lake.v1.LakeService/GetLake\",\n];"),
            "{out}"
        );
    }

    #[test]
    fn a_const_list_reads_back_what_it_was_written_from() {
        // The read half and the write half must agree on the shape, or a generator that
        // writes a name its sibling cannot read miswires the next service silently.
        let written = insert_into_const_list(AUTH, "PROTECTED", "lake.v1.LakeService/GetLake")
            .expect("a list to extend");
        assert_eq!(
            read_const_list(&written, "PROTECTED").expect("the list is there"),
            [
                "counter.v1.CounterService/Whoami".to_owned(),
                "lake.v1.LakeService/GetLake".to_owned(),
            ]
        );
    }

    #[test]
    fn an_empty_const_list_reads_back_empty() {
        assert_eq!(
            read_const_list("const PROTECTED: &[&str] = &[];\n", "PROTECTED")
                .expect("an empty list is still a list"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn reading_a_missing_or_misshapen_list_is_refused() {
        let error = read_const_list("fn main() {}", "PROTECTED")
            .expect_err("a file with no such const is not a list");
        assert!(error.to_string().contains("const PROTECTED:"), "{error}");

        let error = read_const_list(
            "// const PROTECTED: &[&str] = &[]; — prose, not a list\n",
            "PROTECTED",
        )
        .expect_err("a comment is not a const");
        assert!(error.to_string().contains("const PROTECTED:"), "{error}");
    }
}
