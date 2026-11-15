//! The ADR numbering, parsed once.
//!
//! `docs/adr/` spells each record's number three times: in its filename, again in its
//! `# NNNN.` heading, and a third time in the label of its row in `README.md`'s table. Citations
//! overwhelmingly use the number — 267 of the form `ADR NNNN` outside `docs/adr/` itself when this
//! was measured, in AGENTS.md, `crates/identity`, `gates.toml` and most of xtask's own check docs,
//! against 33 that cite a path — so renumbering a record is a repository-wide find-and-replace,
//! and two branches opened in the same week both pick "the next free number" by looking at the
//! directory.
//!
//! Two readers need that numbering, from opposite ends. `decision_records_are_numbered` needs
//! **which numbers exist**, to prove the sequence is contiguous, unduplicated, and that each
//! file agrees with itself; `cargo xtask new-adr` needs **which number is next free**, which is
//! the same set read one step further. Neither is derivable without a parse of the directory,
//! and a directory parsed twice is the defect this repository keeps relearning: `skeletons.rs`
//! states the rule — *a file that parses two ways drifts in the gap* (RT-18) — and RT-25 paid
//! for it by replacing four drifting sentinel lists with one `Sentinels` constant.
//!
//! So the parse lives here and nowhere else: the filename's number, the heading's number, and
//! the index table's rows. `spelled` is the same argument on the output side — the four-digit
//! zero-padded form appears in a filename, a heading, an index row and every diagnostic, and a
//! padding rule written twice is a padding rule that can disagree with itself.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use heck::ToKebabCase as _;

use crate::repo;

/// One decision record, as its two spellings of the number describe it.
#[derive(Debug)]
pub struct Record {
    /// The number its **filename** carries, which is the one every citation in the repository
    /// uses and therefore the one a reference breaks against.
    pub number: u16,
    /// `NNNN-a-title.md`, exactly as the directory spells it.
    pub file_name: String,
    /// The number its `# NNNN.` heading carries, or `None` when the first line is not a numbered
    /// heading at all.
    ///
    /// `Option` rather than a comparison against `number`, because the two failures are not the
    /// same failure: a heading naming a *different* number is a renumber that stopped halfway,
    /// and a record with no numbered heading is a record whose number was never in its text.
    /// They read alike from the outside — "the file and its heading do not agree" — and only the
    /// first of them is a renumber, which is the one with citations to fix.
    pub heading_number: Option<u16>,
}

/// Every numbered record under `docs/adr/`, sorted by number (and by filename within a number,
/// so a duplicated number still produces a deterministic report).
///
/// Empty when there is no `docs/adr/`, matching `decision_records_are_indexed`'s
/// `if !dir.is_dir() { return Ok(()) }`: a tree with no decision records is not a numbering
/// defect, and the generator that refuses to run there is a separate decision it makes itself.
///
/// **Stated limit.** A `.md` whose name does not begin with a number is not a record and is
/// skipped: `Record` has a `u16` to fill, and inventing one for `notes.md` would report a record
/// that does not claim to be one. Such a file is not invisible to the gate either —
/// `decision_records_are_indexed` requires every `.md` in the directory except the index to be
/// linked from it, so a stray file has to be linked to pass, and a linked row in a table of
/// numbers is the thing a reader notices.
///
/// # Errors
///
/// Returns an error when the directory cannot be read, or a record inside it cannot be.
pub fn records(root: &Path) -> Result<Vec<Record>> {
    let dir = root.join("docs/adr");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).context("reading docs/adr")? {
        let entry = entry.context("reading a docs/adr directory entry")?;
        let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        // The index is not a record, and neither is anything that is not a Markdown file —
        // the same two filters `decision_records_are_indexed` applies to the same directory.
        if name == "README.md" || Path::new(&name).extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        let Some(number) = file_number(&name) else {
            continue;
        };
        let text = std::fs::read_to_string(dir.join(&name))
            .with_context(|| format!("reading docs/adr/{name}"))?;
        out.push(Record {
            number,
            file_name: name,
            heading_number: heading_number(&text),
        });
    }

    out.sort_by(|a, b| {
        a.number
            .cmp(&b.number)
            .then_with(|| a.file_name.cmp(&b.file_name))
    });
    Ok(out)
}

/// The number the next record should take.
///
/// One above the highest that exists, or `0001` when there are none. It is deliberately *not*
/// "the smallest gap": a hole in the sequence is a number something used to mean (ADR 0021
/// counts its own checks against the records that cite it), and handing a new record a number a
/// citation already points at is how a deleted record comes back as a different decision.
///
/// This does not prevent the collision the ticket names — two branches both computing the same
/// next free number from the same tree. It cannot: neither branch knows about the other. What
/// prevents that is `decision_records_are_numbered`, which fails the merge.
///
/// # Errors
///
/// Returns whatever [`records`] returns, plus a refusal once the four-digit spelling `spelled`
/// writes is full — 9999 records, which is a tree that has stopped being a set of decisions
/// worth reading and should be told so rather than handed a five-digit filename.
pub fn next_number(root: &Path) -> Result<u16> {
    let records = records(root)?;
    let highest = records.last().map_or(0, |record| record.number);
    // A product starts its own records at 0100 (the inherited set ends at 0023); the
    // template runs from 0001. `repo::is_product` (the devenv.yaml `template` pin) is the signal.
    let next = if repo::is_product(root) && highest < 100 {
        100
    } else {
        highest.saturating_add(1)
    };
    if next > 9999 {
        bail!(
            "docs/adr has {} records, and the four-digit spelling every one of them is cited by \
             is full",
            records.len()
        );
    }
    Ok(next)
}

/// A number as the filenames, the headings and the index rows spell it: four digits,
/// zero-padded.
///
/// One function because the spelling is asserted three ways by the invariant — filename,
/// heading, label — and a `{number:04}` written out in each of them is a padding rule with three
/// chances to drift.
#[must_use]
pub fn spelled(number: u16) -> String {
    format!("{number:04}")
}

/// One row of `docs/adr/README.md`'s table: the label a reader sees, and the file it points at.
#[derive(Debug)]
pub struct IndexRow {
    /// The link text as spelled, without its brackets — `[0012]` contributes `0012`.
    pub label: String,
    /// The number the label names, when it names one.
    pub label_number: Option<u16>,
    /// The file the link points at, as spelled.
    pub target: String,
    /// The number the target's name carries, when it carries one.
    pub target_number: Option<u16>,
}

/// Every row-shaped link in the index, in the order the file writes them.
///
/// Shared by the two checks that read the index: `decision_records_are_indexed` asks whether
/// each `target` resolves, `decision_records_are_numbered` asks whether each `label` agrees with
/// it. Both questions are about the same rows, and a link walk written twice is the RT-18 gap
/// again — the pair drifting apart would mean one of them silently reading a row the other
/// never saw.
///
/// The filter is the one the index check has always applied, moved here rather than
/// re-derived: a target with no `.md` extension, or one carrying a `/`, is not a row of this
/// table and is nobody's business here.
#[must_use]
pub fn index_rows(index: &str) -> Vec<IndexRow> {
    let mut rows = Vec::new();
    for line in index.lines() {
        let mut from = 0usize;
        while let Some(at) = line[from..].find("](") {
            let open = from + at;
            let rest = &line[open + 2..];
            let Some(close) = rest.find(')') else { break };
            let target = &rest[..close];
            from = open + 2 + close + 1;
            if target.is_empty() || target.contains('/') {
                continue;
            }
            if Path::new(target).extension().is_none_or(|ext| ext != "md") {
                continue;
            }
            // The label is the bracketed text immediately before the `](`, found by searching
            // backwards from the link rather than forwards from the line's start: a row's first
            // cell *is* its label, and a link mid-sentence has a label too — which is fine, since
            // the question is only whether that label names the same number as its target.
            let label = line[..open]
                .rfind('[')
                .map_or_else(String::new, |start| line[start + 1..open].trim().to_owned());
            rows.push(IndexRow {
                label_number: label_number(&label),
                label,
                target_number: file_number(target),
                target: target.to_owned(),
            });
        }
    }
    rows
}

/// The leading number in a filename or a link target: `0012-a-title.md` answers `Some(12)`.
///
/// The value, not the four characters — the padding is a spelling and `spelled` owns it, while
/// the invariant and the generator both want the number itself.
fn file_number(name: &str) -> Option<u16> {
    let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The number a row's label names, when it names one.
fn label_number(label: &str) -> Option<u16> {
    let digits: String = label
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// The number on a record's first line, as `# NNNN. Title`.
///
/// The **first** line, not the first non-blank one: a record whose number appears on line three
/// is a record whose heading is not where every reader — and every citation — looks for it, and
/// "not there" is the answer this has to give.
fn heading_number(text: &str) -> Option<u16> {
    let line = text.lines().next().unwrap_or_default().trim();
    // A BOM is one character of noise above the heading, and it is invisible in an editor.
    let line = line.strip_prefix('\u{feff}').unwrap_or(line).trim();
    let rest = line.strip_prefix("# ")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    // `# 0012.` and then the title: a heading that runs its number into a word is not this
    // heading, and reading a number out of it would report a disagreement where the file has
    // no heading to disagree with.
    let after = &rest[digits.len()..];
    if !after.starts_with('.') || !(after.len() == 1 || after[1..].starts_with(' ')) {
        return None;
    }
    digits.parse().ok()
}

/// `cargo xtask new-adr "<title>"` — start the next decision record.
///
/// # Errors
///
/// Returns an error when the title has nothing to name a file, when `--date` is not a
/// `YYYY-MM-DD`, when `docs/adr/` is absent, when the computed filename already exists, or when
/// the index has no table row to add after. Nothing is written when any of those hold: the file
/// and the row are one change, and a record whose row silently failed to land is a record nobody
/// reads.
pub fn new(title: &str, date: Option<&str>) -> Result<()> {
    let root = repo::root();
    let draft = create(&root, title, date)?;

    println!("  ✓ created {}", repo::rel(&root, &draft.path));
    println!(
        "  ✓ wired into docs/adr/README.md as [{}]",
        spelled(draft.number)
    );
    println!("\nNext:");
    // Not "the other N records", because N here would be this record's own number and read as a
    // count of everything above it, which it is not.
    println!("    fill in the sections — the shape is the one the records above this one use");
    println!("    cargo xtask check    # the numbering is already honest; the prose is yours");
    Ok(())
}

/// What [`new`] writes, against a root given to it rather than against this repository.
///
/// Root-parameterised for the same reason `scaffold`'s refusals are: the interesting behaviour
/// is the numbering and the index edit, and neither can be shown failing inside the tree the
/// invariant is protecting.
fn create(root: &Path, title: &str, date: Option<&str>) -> Result<Draft> {
    let title = title.trim();
    if title.is_empty() {
        bail!(
            "the record needs a title, such as `cargo xtask new-adr \"The cache is a decision\"`"
        );
    }
    if title.contains('|') || title.chars().any(char::is_control) {
        bail!(
            "the title {title:?} cannot be used — `|` would break the index table's row, and a \
             heading is one line"
        );
    }

    // heck, the same case conversion the Swift keyword invariant reads (`swift_keywords`
    // compares against what `facet_generate` itself produces with it). A second kebab-case rule
    // here would be a second definition of what a filename is allowed to look like.
    let slug = title.to_kebab_case();
    if slug.is_empty() {
        bail!("the title {title:?} has no words to name a file with");
    }

    let dir = root.join("docs/adr");
    if !dir.is_dir() {
        bail!(
            "no {} — this tree has no decision records to add to. Create the directory and its \
             README.md table first.",
            repo::rel(root, &dir)
        );
    }

    let number = next_number(root)?;
    let file_name = format!("{}-{slug}.md", spelled(number));
    let path = dir.join(&file_name);
    refuse_existing(root, &path, number)?;

    let date = date_stamp(date)?;
    let index_path = dir.join("README.md");
    let index = std::fs::read_to_string(&index_path)
        .with_context(|| format!("reading {}", repo::rel(root, &index_path)))?;
    let row = index_row(number, title, &file_name);
    let index = index_with_row(&index, &row, &repo::rel(root, &index_path))?;

    std::fs::write(&path, body(number, title, &date))
        .with_context(|| format!("writing {}", repo::rel(root, &path)))?;
    std::fs::write(&index_path, index)
        .with_context(|| format!("writing {}", repo::rel(root, &index_path)))?;

    Ok(Draft { number, path })
}

/// Refuse a destination that is already there.
///
/// Read straight after `next_number`, this is unreachable: the number came from a scan of this
/// same directory, so a file sitting at it is a file the scan counted and the number has moved
/// on. It is kept because the two operations are not one atomic act — the tree can change between
/// the read and the write (two runs against one checkout, or a file `records` skipped for a reason
/// its doc comment states) — and because every other generator in this repository refuses rather
/// than overwrites (`scaffold::create_app`, `add_platform`, `add_client`, `add_shell` all do).
/// The alternative to refusing is replacing a record whose reasoning somebody wrote.
fn refuse_existing(root: &Path, path: &Path, number: u16) -> Result<()> {
    if path.exists() {
        bail!(
            "{} already exists — refusing to overwrite. {} is taken: `cargo xtask check` names \
             every record that shares a number, and this one would have shared it with a file \
             this command did not write.",
            repo::rel(root, path),
            spelled(number)
        );
    }
    Ok(())
}

/// A draft, before it is printed.
#[derive(Debug)]
struct Draft {
    number: u16,
    path: PathBuf,
}

/// A new record's own text.
///
/// The sections are measured across all 22 records rather than invented: 22 have `## Context`
/// and `## Consequences`, 21 have `## Decision` and `## What was rejected, and why` — the comma
/// included, because the records carry it and the ticket's paraphrase drops it.
///
/// They are a norm this **emits** and not a rule the gate enforces, which is deliberate: 0020
/// argues under "Measured first, decided second" instead of `## Decision`, 0022 says `##
/// Rejected`, and 0009 has grown `## Where this does not reach`. Asserting the section list
/// would fail on records that deviated on purpose, and a check that has to be relaxed for the
/// three best records in the tree is a check nobody keeps.
///
/// `Accepted`, because that is what all 22 say and the status line is where a *later* change is
/// recorded — README's own "Writing one" says a decision that turns out wrong is superseded, not
/// deleted. A record drafted as `Proposed` would need a second pass to become a decision at all,
/// and nothing re-runs a generator.
fn body(number: u16, title: &str, date: &str) -> String {
    format!(
        "# {number}. {title}\n\
         \n\
         - **Status:** Accepted\n\
         - **Date:** {date}\n\
         \n\
         ## Context\n\
         \n\
         ## Decision\n\
         \n\
         ## Consequences\n\
         \n\
         ## What was rejected, and why\n",
        number = spelled(number),
    )
}

/// The index row for a new record, in the table's own shape.
fn index_row(number: u16, title: &str, file_name: &str) -> String {
    format!(
        "| [{}]({file_name}) | {title} | Accepted |",
        spelled(number)
    )
}

/// Add `row` after the last row of the index table.
///
/// Appended rather than sorted into the table: the table is in number order and a new record
/// takes the next number, so after the last row *is* the sorted position.
///
/// The contract is `api::rustedit`'s (RT-21, reused by RT-24's `splice_assignment`): error
/// rather than no-op when the file is not the shape the edit was written for. The failure this
/// refuses is the quiet one — a record written while its row failed to land, which is a record
/// no reader reaches, and the check that would have caught it is
/// `decision_records_are_indexed`, the one this command exists to make unnecessary.
fn index_with_row(source: &str, row: &str, file: &str) -> Result<String> {
    let mut lines: Vec<&str> = source.lines().collect();
    let Some(at) = lines.iter().rposition(|line| is_index_row(line)) else {
        bail!(
            "no `| [NNNN](….md) |` row in {file} — the table is the index, and this edit refuses \
             to guess where a new row belongs. Add the table, or one row, and run it again."
        );
    };
    lines.insert(at + 1, row);
    let mut out = lines.join("\n");
    if source.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Whether a line is a row of the index table, as opposed to its header or its separator.
fn is_index_row(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("| [") && line.contains("](") && line.ends_with('|')
}

/// The record's date, as `YYYY-MM-DD`.
///
/// There is no date crate to read it from: xtask has none and neither has
/// `[workspace.dependencies]` — no `chrono`, no `time`, no `jiff` — and adding one is a
/// `versions.toml` entry (ADR 0005) plus a version-agreement assertion `check-versions` would
/// have to learn about, for one line of output from a dev tool. Shelling out is the pattern
/// xtask already has: `repo::format_rust` runs `rustfmt` rather than reimplementing it.
///
/// Unlike that call, a failure here is fatal rather than a printed note. A file rustfmt could
/// not reach is fixed by `just fmt`; a record with no date is missing the one line that says
/// when the decision was made, and every amendment in `docs/adr/` is dated to a day someone has
/// to be able to argue about.
///
/// `--date` exists because a record is usually written after the decision it records — RT-24's
/// amendment to 0009 is dated to the day it was measured — and backdating by editing the file
/// afterwards is how a date gets copied from the record above it and wrong.
fn date_stamp(explicit: Option<&str>) -> Result<String> {
    if let Some(date) = explicit {
        return validate_date(date).map(|()| date.to_owned());
    }

    let outcome = std::process::Command::new("date")
        .arg("+%F")
        .output()
        .context("running `date +%F` — pass `--date YYYY-MM-DD` if this system has no date(1)")?;
    if !outcome.status.success() {
        bail!(
            "`date +%F` exited {} — pass `--date YYYY-MM-DD` instead",
            outcome.status
        );
    }
    let stamp = String::from_utf8_lossy(&outcome.stdout).trim().to_owned();
    validate_date(&stamp).with_context(|| {
        format!("`date +%F` printed {stamp:?} — pass `--date YYYY-MM-DD` instead")
    })?;
    Ok(stamp)
}

/// Refuse a date that is not the shape every record uses.
///
/// Checked rather than passed through: this value lands in a file whose whole purpose is to be
/// read years later, and `--date 12-10-26` would sit there looking like a date. Ranges are
/// checked loosely — day up to 31, no calendar arithmetic — because the point is the shape, and
/// a tool that rejects `2026-02-30` has to decide which date crate to be right about.
fn validate_date(date: &str) -> Result<()> {
    let parts: Vec<&str> = date.split('-').collect();
    let ok = parts.len() == 3
        && parts[0].len() == 4
        && parts[1].len() == 2
        && parts[2].len() == 2
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
    if !ok {
        bail!("date {date:?} is not `YYYY-MM-DD` — that is the shape every record carries");
    }
    let (Some(year), Some(month), Some(day)) = (
        parts[0].parse::<u16>().ok(),
        parts[1].parse::<u16>().ok(),
        parts[2].parse::<u16>().ok(),
    ) else {
        bail!("date {date:?} is not `YYYY-MM-DD` — that is the shape every record carries");
    };
    if !(1000..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        bail!(
            "date {date:?} is not a `YYYY-MM-DD` this repository would mean — four-digit year, \
             month 01..=12, day 01..=31"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway repository root, holding one `docs/adr/`.
    ///
    /// Same shape as `invariants.rs`'s fixture: the behaviour under test is a numbering rule, and
    /// this repository's own `docs/adr/` is the tree the invariant is not allowed to fail on.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static COUNTER: AtomicUsize = AtomicUsize::new(0);

            let unique = format!(
                "xtask-adr-{}-{name}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let root = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&root).expect("creating the fixture root");
            Self { root }
        }

        /// A record file, and the index row that names it, written the way the real ones are.
        fn record(&self, number: u16, slug: &str) -> &Self {
            let name = format!("{}-{slug}.md", spelled(number));
            self.file(
                &format!("docs/adr/{name}"),
                &format!(
                    "# {number}. {slug} title\n\
                     \n\
                     - **Status:** Accepted\n\
                     - **Date:** 2026-01-01\n\
                     \n\
                     ## Context\n",
                    number = spelled(number)
                ),
            )
        }

        fn index(&self, rows: &[&str]) -> &Self {
            let body = format!(
                "# Architecture decision records\n\n| # | Decision | Status |\n|---|---|---|\n{}\n\n\
                 ## Writing one\n\nProse.\n",
                rows.join("\n")
            );
            self.file("docs/adr/README.md", &body)
        }

        fn file(&self, relative: &str, contents: &str) -> &Self {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().expect("a file has a parent"))
                .expect("creating the fixture directory");
            std::fs::write(&path, contents).expect("writing the fixture file");
            self
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn records_are_parsed_sorted_and_carry_their_own_heading() {
        let fixture = Fixture::new("parse");
        // Written out of order on purpose: the sort is by number, not by what `read_dir`
        // happened to yield or by filename string order (`0010` sorts before `0002` as text).
        fixture.record(10, "later");
        fixture.record(2, "middle");
        fixture.record(1, "first");
        fixture.index(&[]);
        // Neither of these is a record, and neither may show up in the answer.
        fixture.file("docs/adr/notes.md", "prose with no number in its name\n");
        fixture.file("docs/adr/0003-not-markdown.md.txt", "text\n");

        let records = records(&fixture.root).expect("the fixture is readable");
        assert_eq!(
            records.iter().map(|r| r.number).collect::<Vec<_>>(),
            [1, 2, 10],
            "sorted by number, not by filename"
        );
        assert_eq!(records[2].file_name, "0010-later.md");
        assert_eq!(records[2].heading_number, Some(10));
        assert_eq!(records.len(), 3, "{records:?}");
    }

    #[test]
    fn a_heading_that_is_not_a_numbered_heading_reads_as_none() {
        let fixture = Fixture::new("heading");
        fixture.file(
            "docs/adr/0001-no-heading.md",
            "## Context\n\nBecause the heading is missing.\n",
        );
        fixture.file(
            "docs/adr/0002-blank-first-line.md",
            "\n# 0002. The number is on the second line\n",
        );
        fixture.file(
            "docs/adr/0003-number-runs-into-a-word.md",
            "# 0003No title\n",
        );
        fixture.file(
            "docs/adr/0004-bom.md",
            "\u{feff}# 0004. A BOM above the heading\n",
        );
        fixture.index(&[]);

        let records = records(&fixture.root).expect("the fixture is readable");
        let headings: Vec<Option<u16>> = records.iter().map(|r| r.heading_number).collect();
        assert_eq!(
            headings,
            [None, None, None, Some(4)],
            "the first line is the heading, and a number glued to a word is not one"
        );
    }

    #[test]
    fn the_next_number_is_one_above_the_highest() {
        let empty = Fixture::new("empty");
        std::fs::create_dir_all(empty.root.join("docs/adr")).expect("creating docs/adr");
        assert_eq!(next_number(&empty.root).expect("an empty directory"), 1);

        // A gap and a duplicate: the highest number still decides, because a number something
        // has already meant is not free.
        let fixture = Fixture::new("next");
        fixture.record(1, "a");
        fixture.record(1, "b");
        fixture.record(4, "d");
        fixture.index(&[]);
        assert_eq!(
            next_number(&fixture.root).expect("a tree with a hole in it"),
            5
        );

        // And a tree with no `docs/adr` at all is the invariant's business, not this one's.
        let absent = Fixture::new("absent");
        assert!(records(&absent.root).expect("no directory").is_empty());

        // The four-digit spelling is the convention, so the generator stops at its edge rather
        // than writing `10000-something.md` and letting the invariant call it a gap.
        let full = Fixture::new("full");
        full.record(9999, "last");
        full.index(&[]);
        let error = next_number(&full.root).expect_err("the spelling is full");
        assert!(error.to_string().contains("four-digit"), "{error}");
    }

    #[test]
    fn a_new_record_takes_the_next_number_and_the_house_sections() {
        let fixture = Fixture::new("write");
        fixture.record(1, "one");
        fixture.record(2, "two");
        fixture.index(&[
            "| [0001](0001-one.md) | One | Accepted |",
            "| [0002](0002-two.md) | Two | Accepted |",
        ]);

        let draft = create(&fixture.root, "The cache is a decision", Some("2026-03-04"))
            .expect("a free number and a table to add to");
        assert_eq!(draft.number, 3);
        assert_eq!(
            draft.path.file_name().and_then(|name| name.to_str()),
            Some("0003-the-cache-is-a-decision.md")
        );

        let text = std::fs::read_to_string(&draft.path).expect("the record was written");
        assert!(
            text.starts_with("# 0003. The cache is a decision\n"),
            "{text}"
        );
        for line in ["- **Status:** Accepted", "- **Date:** 2026-03-04"] {
            assert!(text.contains(line), "missing {line}\n{text}");
        }
        for section in [
            "## Context",
            "## Decision",
            "## Consequences",
            "## What was rejected, and why",
        ] {
            assert!(text.contains(section), "missing {section}\n{text}");
        }
        // The comma, because 21 of the 22 records have it and the ticket's paraphrase drops it.
        assert!(text.contains("## What was rejected, and why\n"), "{text}");
        assert!(!text.contains("## What was rejected and why"), "{text}");
    }

    #[test]
    fn the_index_gains_exactly_one_row_after_the_last() {
        let fixture = Fixture::new("index");
        fixture.record(1, "one");
        fixture.index(&["| [0001](0001-one.md) | One | Accepted |"]);

        let draft = create(
            &fixture.root,
            "A gate runs where a manifest says so",
            Some("2026-03-04"),
        )
        .expect("a table to add to");
        let index =
            std::fs::read_to_string(fixture.root.join("docs/adr/README.md")).expect("the index");

        let rows: Vec<&str> = index.lines().filter(|line| is_index_row(line)).collect();
        assert_eq!(rows.len(), 2, "{index}");
        assert_eq!(
            rows[1],
            "| [0002](0002-a-gate-runs-where-a-manifest-says-so.md) | A gate runs where a \
             manifest says so | Accepted |",
            "{index}"
        );
        assert_eq!(draft.number, 2);
        assert_eq!(
            draft.path.file_name().and_then(|name| name.to_str()),
            Some("0002-a-gate-runs-where-a-manifest-says-so.md")
        );
        // The row went into the table and not after the prose below it, which is what
        // "the last row" means when the file carries a section underneath the table.
        let table = index
            .split("## Writing one")
            .next()
            .expect("the section is there");
        assert!(
            table.contains(rows[1]),
            "the row landed below the table:\n{index}"
        );
        assert!(index.ends_with("Prose.\n"), "the tail survived:\n{index}");
    }

    #[test]
    fn a_filename_that_already_exists_is_refused() {
        let fixture = Fixture::new("exists");
        fixture.file(
            "docs/adr/0002-the-cache-is-a-decision.md",
            "# 0002. Someone wrote this by hand\n",
        );

        // Tested against the guard rather than through `create`, because `create` cannot reach it
        // on a tree it can read: the number it computes comes from scanning this same directory,
        // so a file already at that number is a file the scan counted. The guard is for the tree
        // that changed between the read and the write, and for a file `records` skipped — which is
        // exactly the state this fixture hands it.
        let error = refuse_existing(
            &fixture.root,
            &fixture
                .root
                .join("docs/adr/0002-the-cache-is-a-decision.md"),
            2,
        )
        .expect_err("0002 is taken");
        assert!(
            error
                .to_string()
                .contains("0002-the-cache-is-a-decision.md already exists"),
            "{error}"
        );
        assert!(
            error.to_string().contains("refusing to overwrite"),
            "{error}"
        );

        // And the free case, which is the one `create` actually walks.
        refuse_existing(
            &fixture.root,
            &fixture.root.join("docs/adr/0003-free.md"),
            3,
        )
        .expect("nothing is there");
    }

    #[test]
    fn an_index_with_no_rows_is_refused() {
        let fixture = Fixture::new("no-rows");
        fixture.record(1, "one");
        fixture.file(
            "docs/adr/README.md",
            "# Architecture decision records\n\nThere is no table here.\n",
        );

        let error = create(&fixture.root, "The cache is a decision", Some("2026-03-04"))
            .expect_err("there is no row to add after");
        assert!(error.to_string().contains("no `| [NNNN]"), "{error}");
        assert!(
            !fixture
                .root
                .join("docs/adr/0002-the-cache-is-a-decision.md")
                .exists(),
            "the record must not land when its row cannot"
        );
    }

    #[test]
    fn a_date_is_the_shape_every_record_carries() {
        assert!(validate_date("2026-03-04").is_ok());
        for bad in [
            "2026-3-4",
            "04-03-2026",
            "2026-13-01",
            "2026-03-32",
            "today",
            "",
        ] {
            let error = validate_date(bad).expect_err("{bad:?} is not a record date");
            assert!(error.to_string().contains("YYYY-MM-DD"), "{error}");
        }
        // The real thing, read the way the command reads it.
        let today = date_stamp(None).expect("date(1) is in the dev shell");
        assert_eq!(today.len(), 10, "{today}");
    }

    #[test]
    fn a_title_with_nothing_to_name_a_file_with_is_refused() {
        let fixture = Fixture::new("title");
        fixture.record(1, "one");
        fixture.index(&["| [0001](0001-one.md) | One | Accepted |"]);

        for bad in ["   ", "—?!"] {
            let error = create(&fixture.root, bad, Some("2026-03-04")).expect_err("{bad:?}");
            assert!(error.to_string().contains("title"), "{error}");
        }
    }

    #[test]
    fn index_rows_read_the_label_and_the_target_of_a_row() {
        let rows = index_rows(
            "| [0012](0012-a-title.md) | A title | Accepted |\n\
             | [0013](0019-another-title.md) | Another | Accepted |\n\
             |---|---|---|\n\
             | # | Decision | Status |\n\
             See [the guide](guide.html) and [0002](sub/0002-x.md).\n",
        );
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].label, "0012");
        assert_eq!(rows[0].label_number, Some(12));
        assert_eq!(rows[0].target, "0012-a-title.md");
        assert_eq!(rows[0].target_number, Some(12));
        // The disagreement the numbering check exists to catch.
        assert_eq!(rows[1].label_number, Some(13));
        assert_eq!(rows[1].target_number, Some(19));
        // A header row has no link, and a link outside the directory or not to a Markdown file
        // is not a row of this table — the filter `decision_records_are_indexed` has always
        // applied, now in one place.
        assert_eq!(
            rows.iter()
                .map(|row| row.target.as_str())
                .collect::<Vec<_>>(),
            ["0012-a-title.md", "0019-another-title.md"],
            "a header row has no link, and a link outside the directory or not to a Markdown file \
             is not a row of this table — the filter `decision_records_are_indexed` has always \
             applied, now in one place"
        );
    }
}
