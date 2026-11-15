//! Splicing new declarations into a `.proto` that already exists.
//!
//! [`super::rustedit`]'s header argues for line-oriented edits over a parser, and every
//! word of it is about hand-maintained files rather than about Rust: a tool that rewrites a
//! file to add one line to it loses the comments that are the reason anyone keeps the file
//! by hand. A contract is the most commented file a feature has — `new-feature` writes
//! three paragraphs into the one it generates, and whoever grows it writes more — so the
//! same discipline applies, and so does the same contract:
//!
//! - **Total about failure.** Each function either makes the edit or returns an error
//!   naming what it could not find. None of them silently does nothing, because the
//!   generated Rust depends on the edit having happened and a no-op would surface as a
//!   codegen that produced nothing, a long way from here.
//! - **A test each.**
//!
//! It lives beside `rustedit` rather than inside it because that module is named for the
//! language it edits and this is a different one. The alternative — a `rustedit` function
//! that splices proto — would make the module's name a lie, and the next person to add a
//! Kotlin or Swift edit would have nowhere obvious to put it either.
//!
//! What this does **not** do is decide where things go. The caller parses the contract with
//! [`super::schema`] first and refuses a duplicate there, so by the time these run the only
//! question left is where in the text the new lines belong.

use anyhow::{Result, bail};

/// Add an `rpc` declaration to a `service` block that already exists.
///
/// The declaration lands last inside the block, at the indentation of the rpcs already
/// there, because rpc order in a `service` block is declaration order and nothing reads it
/// — and because inserting anywhere else would move the lines a reviewer is comparing.
///
/// # Errors
///
/// Returns an error when the file declares no `service <name>`, when its block is never
/// closed, or when the declaration is already in it.
pub fn insert_rpc(source: &str, service: &str, declaration: &str) -> Result<String> {
    let (open, close) = block(source, &format!("service {service}"))?;
    let lines: Vec<&str> = source.lines().collect();

    let declaration = declaration.trim();
    if lines[open..=close]
        .iter()
        .any(|line| line.trim() == declaration)
    {
        bail!("`{declaration}` is already in `service {service}`");
    }

    // Read off an rpc already there, so a contract indented with four spaces keeps them.
    // Two is what buf format produces and what `new-feature` writes, and it is the fallback
    // rather than the rule.
    let indent = lines[open..close]
        .iter()
        .find(|line| line.trim_start().starts_with("rpc "))
        .map_or_else(
            || "  ".to_owned(),
            |line| line[..line.len() - line.trim_start().len()].to_owned(),
        );

    let mut out: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
    if open == close {
        // `service X {}` — an empty block on one line, which has no "last line inside" to
        // insert above. Broken open rather than refused: buf format writes an empty block
        // that way, so a contract whose methods were all deleted comes back as one.
        let line = lines[open];
        let outer = &line[..line.len() - line.trim_start().len()];
        let (head, tail) = line.rsplit_once('}').unwrap_or((line, ""));
        out[open] = format!(
            "{}\n{outer}{indent}{declaration}\n{outer}}}{tail}",
            head.trim_end()
        );
    } else {
        out.insert(close, format!("{indent}{declaration}"));
    }
    Ok(join(&out, source))
}

/// Put a block of messages above the `service` block.
///
/// Above it rather than at the end of the file, because every contract this repository
/// writes declares its messages first and its service last — buf does not care, but a
/// contract whose messages are split either side of the service block is one nobody can
/// skim. The text is inserted verbatim, with one blank line after it.
///
/// # Errors
///
/// Returns an error when the file declares no `service <name>`.
pub fn insert_messages(source: &str, service: &str, messages: &str) -> Result<String> {
    let (open, _) = block(source, &format!("service {service}"))?;
    let mut out: Vec<String> = source.lines().map(ToOwned::to_owned).collect();

    let block: Vec<String> = messages
        .trim_end_matches('\n')
        .lines()
        .map(ToOwned::to_owned)
        .chain(std::iter::once(String::new()))
        .collect();
    out.splice(open..open, block);
    Ok(join(&out, source))
}

/// The line indices of a block's opening line and its closing brace.
///
/// By line, not by byte, because both edits here are "insert a line" and a line index is
/// what makes that a `Vec::insert` rather than a byte arithmetic exercise.
fn block(source: &str, header: &str) -> Result<(usize, usize)> {
    let lines: Vec<&str> = source.lines().collect();

    // The header has to open the line and be followed by whitespace or the brace itself:
    // `service LakeService {` and `service LakeServiceV2 {` differ in nothing else, and a
    // prefix match would splice a method into the wrong service.
    let open = lines
        .iter()
        .position(|line| {
            line.trim_start().strip_prefix(header).is_some_and(|rest| {
                rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with('{')
            })
        })
        .ok_or_else(|| {
            let declared: Vec<&str> = lines
                .iter()
                .map(|line| line.trim_start())
                .filter(|line| line.starts_with("service "))
                .collect();
            anyhow::anyhow!(
                "no `{header}` in this contract — it declares {}",
                if declared.is_empty() {
                    "no service block at all".to_owned()
                } else {
                    declared.join(", ")
                }
            )
        })?;

    let mut depth = 0_usize;
    let mut opened = false;
    for (index, line) in lines.iter().enumerate().skip(open) {
        depth += line.matches('{').count();
        opened |= depth > 0;
        depth = depth.saturating_sub(line.matches('}').count());
        if opened && depth == 0 {
            // The same line for a block written `service X {}`, which is what the caller
            // then has to break open.
            return Ok((open, index));
        }
    }
    bail!("`{header}`'s block is never closed")
}

/// Re-join lines, keeping the trailing newline the source had.
fn join(lines: &[String], source: &str) -> String {
    let mut out = lines.join("\n");
    if source.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract() -> String {
        "syntax = \"proto3\";\n\
         \n\
         package lake.v1;\n\
         \n\
         // One lake, as the contract carries it.\n\
         message Lake {\n\
         \x20 int32 id = 1;\n\
         }\n\
         \n\
         service LakeService {\n\
         \x20 rpc GetLake(GetLakeRequest) returns (GetLakeResponse);\n\
         }\n"
        .to_owned()
    }

    #[test]
    fn an_rpc_lands_last_in_the_block_it_names() {
        let out = insert_rpc(
            &contract(),
            "LakeService",
            "rpc ArchiveLake(ArchiveLakeRequest) returns (ArchiveLakeResponse);",
        )
        .expect("the block is there");

        assert!(
            out.contains(
                "  rpc GetLake(GetLakeRequest) returns (GetLakeResponse);\n\
                 \x20 rpc ArchiveLake(ArchiveLakeRequest) returns (ArchiveLakeResponse);\n}"
            ),
            "{out}"
        );
        // Every comment the file had, character for character.
        assert!(
            out.contains("// One lake, as the contract carries it."),
            "{out}"
        );
        assert!(out.ends_with('\n'), "the trailing newline is kept");
    }

    #[test]
    fn messages_land_above_the_service_block() {
        let out = insert_messages(
            &contract(),
            "LakeService",
            "message ArchiveLakeRequest {\n  int32 id = 1;\n}\n",
        )
        .expect("the block is there");

        let messages_at = out.find("message ArchiveLakeRequest").expect("inserted");
        let service_at = out.find("service LakeService").expect("still there");
        assert!(messages_at < service_at, "{out}");
        assert!(out.contains("}\n\nservice LakeService {"), "{out}");
        // The message it already had is untouched and still first.
        assert!(
            out.find("message Lake {").expect("kept") < messages_at,
            "{out}"
        );
    }

    #[test]
    fn a_missing_service_block_is_reported_with_what_is_there() {
        let error = insert_rpc(&contract(), "PondService", "rpc A(B) returns (C);")
            .expect_err("there is no PondService");
        let error = error.to_string();
        assert!(error.contains("no `service PondService`"), "{error}");
        assert!(error.contains("service LakeService {"), "{error}");
    }

    #[test]
    fn a_service_whose_name_is_a_prefix_is_not_the_one() {
        // `service LakeService` is a prefix of `service LakeServiceV2`, and splicing a
        // method into the wrong service is a method that routes nowhere anybody looked.
        let source = "service LakeServiceV2 {\n  rpc GetLake(A) returns (B);\n}\n";
        let error =
            insert_rpc(source, "LakeService", "rpc A(B) returns (C);").expect_err("not a match");
        assert!(
            error.to_string().contains("no `service LakeService`"),
            "{error}"
        );
    }

    #[test]
    fn the_same_rpc_twice_is_refused() {
        let error = insert_rpc(
            &contract(),
            "LakeService",
            "rpc GetLake(GetLakeRequest) returns (GetLakeResponse);",
        )
        .expect_err("already declared");
        assert!(error.to_string().contains("already"), "{error}");
    }

    #[test]
    fn an_empty_service_block_takes_its_first_rpc() {
        let source = "service LakeService {}\n";
        let out = insert_rpc(source, "LakeService", "rpc GetLake(A) returns (B);")
            .expect("an empty block is still a block");
        assert!(out.contains("rpc GetLake(A) returns (B);"), "{out}");
    }
}
