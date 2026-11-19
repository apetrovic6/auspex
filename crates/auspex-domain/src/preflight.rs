//! The checks that run after `u` and before its confirmation: what they read, and what each
//! one decides.
//!
//! Pure. The adapters obtain the inputs (`git status`, the pending closure's sizes), the core
//! adds what it already holds (the probe's free space, the machine addresses, the config's
//! `public` flag), and [`preflight`] turns the lot into **refusals** (no way to proceed) and
//! **warnings** (proceed with the confirmation).
//!
//! # The rules, and which way each one fails
//!
//! | check | refuses | warns |
//! |---|---|---|
//! | fleet tree | never | uncommitted changes to tracked files; `HEAD` ahead of its upstream; no upstream, or one that is gone; the check could not run |
//! | free disk | free < closure + [`FREE_MARGIN_BYTES`] | free space or closure size unknown |
//! | duplicate IP | the machine's address is shared with another machine's, **or is not known** | other machines' addresses are not known |
//!
//! A check that fails to *run* is a warning, not a refusal, because a refusal the operator
//! cannot act on teaches them to stop reading the dialog. The one exception is the duplicate
//! address rule, which **fails closed**: its job is to stop a deploy landing on the wrong
//! machine (a wildcard DNS record once put the wrong closure on the main server), and "I could
//! not tell which machine this is" is exactly that risk.
//!
//! # What is deliberately not here
//!
//! The plan also asks for typed confirmation when the update's *option diff* touches openssh,
//! the firewall or networking. That needs a diff of NixOS options between the deployed and the
//! pending system, which does not exist yet (AUS-22). [`option_diff_touches_network`] is the
//! named place it will be decided; it says `false` today, and [`typed_confirmation_required`]
//! already calls it, so the day it says something else nothing else changes. It is not an
//! approximation: a guess from package names would be a rule that looks like it works.

use std::collections::BTreeSet;

use crate::fleet::SharedIp;
use crate::observe::StorePath;
use crate::text::sanitize_line;

/// Free space an update must leave on the target's `/nix/store` beyond the closure: 1 GiB.
///
/// The closure's NAR size is not the space it needs (it is unpacked, the
/// store is optimised, generations are kept), so the margin is what makes "just fits" a
/// refusal rather than a deploy that fails half way.
pub const FREE_MARGIN_BYTES: u64 = 1 << 30;

/// What `git status` said about the fleet repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeState {
    /// Tracked files with changes, staged or not. Untracked files are not counted: the check
    /// is "what a dirty flake would deploy that git does not have", and nix ignores
    /// untracked files.
    pub changed_files: usize,
    /// Where `HEAD` stands against its upstream.
    pub upstream: Upstream,
}

/// A branch's upstream, as `git status --branch` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upstream {
    /// The branch has none, or there is no branch yet.
    None,
    /// `HEAD` is detached, so there is no branch and no upstream.
    Detached,
    /// The branch has one, and it was deleted on the remote (`[gone]`).
    Gone(String),
    /// The upstream and how far `HEAD` is from it.
    Tracking {
        /// The upstream, `origin/main`. Sanitised: a branch name is text from the repository.
        name: String,
        /// Commits in `HEAD` that the upstream does not have: unpushed.
        ahead: u64,
        /// Commits in the upstream that `HEAD` does not have.
        behind: u64,
    },
}

/// The arguments (after the program) of the read-only listing of every config key.
///
/// The tree check reads the filter driver names from it: `--name-only` so no value is printed,
/// `-z` so a name with an odd character cannot be split into two records and slip past
/// [`parse_filter_names`].
pub const LIST_CONFIG_ARGS: [&str; 4] = ["config", "--list", "--name-only", "-z"];

/// The filter drivers the repository's configuration defines, sorted and unique.
///
/// Every `<name>` of a key `filter.<name>.clean`, `.smudge` or `.process`, read from the output
/// of `git config --list --name-only -z` ([`LIST_CONFIG_ARGS`]).
///
/// A *filter driver* is a command git runs on file contents; `git status` runs the `clean` (or
/// `process`) one of a driver an attribute names on every file whose stat changed, to see
/// whether it really did.
///
/// # Errors
///
/// One line when a driver cannot be blanked (see [`status_args`]): its name has a character a
/// `-c key=value` argument cannot carry (`=`, or a control character). Failing is the safe
/// answer: the check then says it could not run, and no filter was left to run.
pub fn parse_filter_names(text: &str) -> Result<Vec<String>, String> {
    let mut names = BTreeSet::new();
    for key in text.split('\0') {
        let Some(rest) = key.strip_prefix("filter.") else {
            continue;
        };
        let Some(name) = [".clean", ".smudge", ".process"]
            .iter()
            .find_map(|suffix| rest.strip_suffix(suffix))
        else {
            continue;
        };
        if name.is_empty() || name.contains('=') || name.chars().any(char::is_control) {
            return Err(format!(
                "a filter driver of this repository ({:?}) has a name that cannot be turned off",
                sanitize_line(name)
            ));
        }
        names.insert(name.to_owned());
    }
    Ok(names.into_iter().collect())
}

/// The arguments of the one `git status` the tree check runs, after the program, with every
/// filter driver in `filters` ([`parse_filter_names`]) turned off.
///
/// - `--no-optional-locks`: `status` would refresh and rewrite the index otherwise. A
///   read-only check must not write to the operator's repository.
/// - `-c core.hooksPath=/dev/null` and the runner's `-c core.fsmonitor=false`: no hook command
///   is run. `fsmonitor` is the one `status` actually consults (it is a command from the
///   repository's config); `hooksPath` is belt and braces, since `status` runs no hook.
/// - **No filter can run.** `status` runs a clean filter on a file whose stat changed, and a
///   filter is a command named by config and chosen by an *attribute*, which can come from
///   three places: a tracked `.gitattributes`, `.git/info/attributes`, or a file named by
///   `core.attributesFile`. `-c core.attributesFile=/dev/null` closes the third; **blanking the
///   drivers closes all three at once**, because what an attribute names is a driver, and a
///   driver with no `clean`, `smudge` and `process` command is not run (measured: the file is
///   then compared as it is). So the drivers the repository defines are listed first (read-only:
///   `git config --list --name-only`, which runs nothing) and each is emptied here. A driver
///   defined by an included file or by `--global` config is listed too. (An earlier version
///   read attributes from the empty tree instead, with `--attr-source`; that does not stop
///   `.git/info/attributes` or the config route, needs git 2.40, and names SHA-1\'s empty tree,
///   so a SHA-256 repository could never be checked.)
/// - `--porcelain=v1 -b -z`: stable, machine-readable, NUL-separated, with the branch line
///   first. One command answers both "dirty" and "ahead".
/// - `--untracked-files=no`, `--ignore-submodules=all`, `--no-renames`: only tracked files of
///   this repository, one record per file.
#[must_use]
pub fn status_args(filters: &[String]) -> Vec<String> {
    let mut args: Vec<String> = [
        "--no-optional-locks",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.attributesFile=/dev/null",
    ]
    .iter()
    .map(|a| (*a).to_owned())
    .collect();
    for name in filters {
        for what in ["clean", "smudge", "process"] {
            args.push("-c".to_owned());
            args.push(format!("filter.{name}.{what}="));
        }
    }
    args.extend(
        [
            "status",
            "--porcelain=v1",
            "-b",
            "-z",
            "--untracked-files=no",
            "--ignore-submodules=all",
            "--no-renames",
        ]
        .iter()
        .map(|a| (*a).to_owned()),
    );
    args
}

/// Reads `git status --porcelain=v1 -b -z` output.
///
/// # Errors
///
/// One line when the text is not that output (no `## ` branch record first).
pub fn parse_tree_status(text: &str) -> Result<TreeState, String> {
    let mut records = text.split('\0');
    let header = records.next().unwrap_or_default();
    let Some(header) = header.strip_prefix("## ") else {
        return Err("git status printed something other than a branch line first".to_owned());
    };
    let changed_files = records.filter(|r| !r.is_empty()).count();
    Ok(TreeState {
        changed_files,
        upstream: parse_branch_line(header),
    })
}

fn parse_branch_line(line: &str) -> Upstream {
    if line.starts_with("No commits yet on ") || line.starts_with("Initial commit on ") {
        return Upstream::None;
    }
    if line.starts_with("HEAD (no branch") {
        return Upstream::Detached;
    }
    let Some((_local, rest)) = line.split_once("...") else {
        return Upstream::None;
    };
    let (name, brackets) = rest
        .split_once(" [")
        .map_or((rest, ""), |(n, b)| (n, b.strip_suffix(']').unwrap_or(b)));
    let name = sanitize_line(name);
    let (mut ahead, mut behind, mut gone) = (0, 0, false);
    for part in brackets.split(", ") {
        if part == "gone" {
            gone = true;
        } else if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n.parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n.parse().unwrap_or(0);
        }
    }
    if gone {
        Upstream::Gone(name)
    } else {
        Upstream::Tracking {
            name,
            ahead,
            behind,
        }
    }
}

/// Reads `nix path-info --recursive --size` output: one `<store path> <NAR size>` per line.
///
/// Returns every path with its size, so the caller can leave out the ones a machine already
/// has ([`estimate`]). Strict: a line that is not a store path and a number makes the whole
/// answer an error, because a sum that silently left a path out is an *underestimate*, and a
/// size check that errs low lets a full disk through.
///
/// # Errors
///
/// One line: no paths at all, an unreadable line, or a total that overflows 64 bits.
pub fn parse_closure_sizes(text: &str) -> Result<Vec<(StorePath, u64)>, String> {
    let mut total: u64 = 0;
    let mut paths = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(path), Some(size), None) = (fields.next(), fields.next(), fields.next()) else {
            return Err(format!(
                "a line of the size listing is not `<path> <size>`: {:?}",
                sanitize_line(line)
            ));
        };
        let Ok(path) = StorePath::new(path) else {
            return Err(format!(
                "a line of the size listing does not start with a store path: {:?}",
                sanitize_line(line)
            ));
        };
        let size: u64 = size.parse().map_err(|_| {
            format!(
                "a line of the size listing has no size: {:?}",
                sanitize_line(line)
            )
        })?;
        total = total
            .checked_add(size)
            .ok_or_else(|| "the closure's size does not fit in 64 bits".to_owned())?;
        paths.push((path, size));
    }
    if paths.is_empty() {
        return Err("the size listing has no store paths".to_owned());
    }
    Ok(paths)
}

/// What an update is estimated to add to the target's store, and how that was worked out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Need {
    /// The bytes the update is expected to add (before the margin).
    pub bytes: u64,
    /// What they were counted from.
    pub basis: Basis,
}

/// What a [`Need`] was counted from. The dialog says which, because the two differ by a lot and
/// a refusal is only as good as its estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// The whole pending closure: which of its paths the machine already has is not known (the
    /// machine's closure was not fetched, or does not match what its probe saw).
    WholeClosure,
    /// Only the paths of the pending closure that are not in the machine's closure.
    NotOnMachine {
        /// How many paths the machine does not have.
        new_paths: usize,
        /// How many paths the pending closure has.
        total_paths: usize,
        /// The whole closure's size, for comparison.
        whole_bytes: u64,
    },
}

/// The estimate of what the pending closure `sizes` adds to a machine.
///
/// With the machine's own closure (`deployed`, which the package preview already fetches) it
/// counts only the paths the machine does not have; without it, the whole closure.
///
/// Still an overestimate (the store holds older generations too, and paths are shared
/// between them), which is the safe direction. The margin is added by the rule, not here.
#[must_use]
pub fn estimate(sizes: &[(StorePath, u64)], deployed: Option<&BTreeSet<StorePath>>) -> Need {
    let whole = sizes.iter().fold(0_u64, |a, (_, n)| a.saturating_add(*n));
    let Some(deployed) = deployed else {
        return Need {
            bytes: whole,
            basis: Basis::WholeClosure,
        };
    };
    let fresh: Vec<&(StorePath, u64)> = sizes
        .iter()
        .filter(|(p, _)| !deployed.contains(p))
        .collect();
    Need {
        bytes: fresh.iter().fold(0_u64, |a, (_, n)| a.saturating_add(*n)),
        basis: Basis::NotOnMachine {
            new_paths: fresh.len(),
            total_paths: sizes.len(),
            whole_bytes: whole,
        },
    }
}

impl Basis {
    /// For the dialog: what the estimate counts.
    fn describe(self, bytes: u64) -> String {
        match self {
            Self::WholeClosure => format!(
                "the whole closure's {}; which of its paths the machine has already is not known",
                format_bytes(bytes)
            ),
            Self::NotOnMachine {
                new_paths,
                total_paths,
                whole_bytes,
            } => format!(
                "the {} of the {new_paths} of the closure's {total_paths} paths it does not have \
                 yet, of {} in all",
                format_bytes(bytes),
                format_bytes(whole_bytes)
            ),
        }
    }
}

/// `n` bytes for a person: `3.2 GiB`, `512 MiB`, `0 B`.
#[must_use]
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    #[expect(
        clippy::cast_precision_loss,
        reason = "shown to one decimal; the rounding is the point"
    )]
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Whether the update's option diff touches openssh, the firewall or networking, which the
/// plan makes a reason for typed confirmation.
///
/// **Not implemented**: it needs a diff of NixOS options between the deployed system and the
/// pending one (AUS-22), and nothing here may pretend otherwise. Always `false`, and
/// [`typed_confirmation_required`] consults it, so this is the one place to change.
#[must_use]
pub const fn option_diff_touches_network() -> bool {
    false
}

/// Whether the confirmation asks for the machine's name instead of `y`.
///
/// That is so for a machine marked
/// public (`machines.<m>.public = true`), or an update whose option diff touches openssh, the
/// firewall or networking ([`option_diff_touches_network`]).
#[must_use]
pub const fn typed_confirmation_required(public: bool) -> bool {
    public || option_diff_touches_network()
}

/// What the duplicate-address rule knows about the machine.
#[derive(Debug, Clone, Copy)]
pub struct Addresses<'a> {
    /// Whether the machine's own target has an address (an IP literal, or a hostname that
    /// resolved). Without one it cannot be told apart from any other machine.
    pub own_known: bool,
    /// [`crate::fleet::shared_ips`] of the whole fleet; only the entries naming the machine
    /// matter here.
    pub shared: &'a [SharedIp],
    /// How many *other* machines have no known address, so a collision with them cannot be
    /// ruled out.
    pub others_unknown: usize,
    /// Address resolution is still out for this refresh. "Not known yet" is not "resolution
    /// failed": until it answers, no collision can be ruled out, so the rule refuses.
    pub resolving: bool,
}

/// Everything the rules need, gathered by the core.
#[derive(Debug, Clone, Copy)]
pub struct Facts<'a> {
    /// The machine being updated.
    pub machine: &'a str,
    /// The fleet repository's state, or why it could not be read.
    pub tree: Result<&'a TreeState, &'a str>,
    /// What the pending closure is estimated to add to the target's store, or why it is not known.
    pub closure: Result<Need, &'a str>,
    /// Free bytes on the target's `/nix/store` filesystem as the last probe saw them, or
    /// `None` when the probe did not report it.
    pub free_bytes: Option<u64>,
    /// The machine's probe is still out for this refresh, so its free space is not "unknown"
    /// but not here yet. The disk rule refuses until it answers: a refresh clears the probes,
    /// and without this a refusal for disk would turn into a warning for a few seconds.
    pub probing: bool,
    /// The duplicate-address rule's inputs.
    pub addresses: Addresses<'a>,
    /// `machines.<m>.public`.
    pub public: bool,
}

/// What the checks decided.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Preflight {
    /// Reasons the update may not run. Non-empty means there is no way to proceed.
    pub refusals: Vec<String>,
    /// Things the operator should read before confirming.
    pub warnings: Vec<String>,
    /// The confirmation asks for the machine's name, not `y`.
    pub typed_confirmation: bool,
}

/// Runs every rule.
#[must_use]
pub fn preflight(facts: &Facts<'_>) -> Preflight {
    let mut out = Preflight {
        typed_confirmation: typed_confirmation_required(facts.public),
        ..Preflight::default()
    };
    tree_check(facts, &mut out);
    disk_check(facts, &mut out);
    address_check(facts, &mut out);
    out
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn tree_check(facts: &Facts<'_>, out: &mut Preflight) {
    let tree = match facts.tree {
        Ok(t) => t,
        Err(why) => {
            out.warnings.push(format!(
                "could not check the fleet repository, so whether it is dirty or unpushed is \
                 not known: {}",
                sanitize_line(why)
            ));
            return;
        }
    };
    if tree.changed_files > 0 {
        out.warnings.push(format!(
            "the fleet repository has uncommitted changes to {}: the update deploys them, and \
             no commit says what was deployed",
            plural(tree.changed_files as u64, "tracked file", "tracked files")
        ));
    }
    match &tree.upstream {
        Upstream::Tracking { name, ahead, .. } if *ahead > 0 => {
            out.warnings.push(format!(
                "the fleet repository's HEAD is {} ahead of {name}: what is deployed has not \
                 been pushed",
                plural(*ahead, "commit", "commits")
            ));
        }
        Upstream::Tracking { .. } => {}
        Upstream::Gone(name) => out.warnings.push(format!(
            "the fleet repository's upstream {name} no longer exists, so whether HEAD is \
             pushed cannot be checked"
        )),
        Upstream::None | Upstream::Detached => out.warnings.push(
            "the fleet repository has no upstream (no branch, or none tracked), so whether \
             HEAD is pushed cannot be checked"
                .to_owned(),
        ),
    }
}

fn disk_check(facts: &Facts<'_>, out: &mut Preflight) {
    let machine = facts.machine;
    if facts.probing {
        out.refusals.push(format!(
            "still probing {machine}, so its free disk is not known yet: try again in a moment"
        ));
        return;
    }
    let need = match facts.closure {
        Ok(n) => n,
        Err(why) => {
            out.warnings.push(format!(
                "the size of the pending closure is not known, so {machine}'s free disk is not \
                 checked against it: {}",
                sanitize_line(why)
            ));
            return;
        }
    };
    let Some(free) = facts.free_bytes else {
        out.warnings.push(format!(
            "free space on {machine}'s /nix/store is not known (its last probe did not report \
             it), so it is not checked against the {} the update is expected to add",
            format_bytes(need.bytes)
        ));
        return;
    };
    let needed = need.bytes.saturating_add(FREE_MARGIN_BYTES);
    if free < needed {
        out.refusals.push(format!(
            "{machine}'s /nix/store has {} free, and this update needs {} ({}, plus {} to spare)",
            format_bytes(free),
            format_bytes(needed),
            need.basis.describe(need.bytes),
            format_bytes(FREE_MARGIN_BYTES)
        ));
    }
}

fn address_check(facts: &Facts<'_>, out: &mut Preflight) {
    let machine = facts.machine;
    let a = facts.addresses;
    if a.resolving {
        out.refusals.push(format!(
            "still resolving addresses, so whether {machine}'s address is shared with another \
             machine cannot be told yet: try again in a moment"
        ));
        return;
    }
    if !a.own_known {
        out.refusals.push(format!(
            "{machine} has no known address (its target is missing, or a hostname that has not \
             resolved), so it \
             cannot be told apart from another machine's"
        ));
    }
    for s in a
        .shared
        .iter()
        .filter(|s| s.machines.iter().any(|m| m == machine))
    {
        let with: Vec<&str> = s
            .machines
            .iter()
            .filter(|m| *m != machine)
            .map(String::as_str)
            .collect();
        out.refusals.push(format!(
            "{machine}'s target resolves to {}, which {} also resolve{} to: an update could \
             land on the wrong machine",
            s.ip,
            with.join(" and "),
            if with.len() == 1 { "s" } else { "" }
        ));
    }
    if a.others_unknown > 0 {
        out.warnings.push(format!(
            "{} of the other machines have no known address, so a collision with them cannot \
             be ruled out",
            a.others_unknown
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;

    fn tree(changed: usize, upstream: Upstream) -> TreeState {
        TreeState {
            changed_files: changed,
            upstream,
        }
    }

    fn tracking(ahead: u64) -> Upstream {
        Upstream::Tracking {
            name: "origin/main".to_owned(),
            ahead,
            behind: 0,
        }
    }

    const NO_ADDRESS_TROUBLE: Addresses<'static> = Addresses {
        own_known: true,
        shared: &[],
        others_unknown: 0,
        resolving: false,
    };

    fn facts<'a>(
        tree: &'a TreeState,
        closure: Result<u64, &'a str>,
        free: Option<u64>,
    ) -> Facts<'a> {
        Facts {
            machine: "luna",
            tree: Ok(tree),
            closure: closure.map(|bytes| Need {
                bytes,
                basis: Basis::WholeClosure,
            }),
            free_bytes: free,
            probing: false,
            addresses: NO_ADDRESS_TROUBLE,
            public: false,
        }
    }

    const GIB: u64 = 1 << 30;

    #[test]
    fn a_clean_pushed_tree_with_room_has_nothing_to_say() {
        let t = tree(0, tracking(0));
        let p = preflight(&facts(&t, Ok(2 * GIB), Some(10 * GIB)));
        assert_eq!(p, Preflight::default());
    }

    #[test]
    fn status_header_forms() {
        let ok = |s: &str| parse_tree_status(s).unwrap();
        assert_eq!(ok("## main...origin/main\0"), tree(0, tracking(0)));
        assert_eq!(
            ok("## main...origin/main [ahead 2]\0 M flake.nix\0M  a.nix\0"),
            tree(2, tracking(2))
        );
        assert_eq!(
            ok("## main...origin/main [ahead 2, behind 1]\0"),
            tree(
                0,
                Upstream::Tracking {
                    name: "origin/main".to_owned(),
                    ahead: 2,
                    behind: 1
                }
            )
        );
        assert_eq!(
            ok("## main...origin/main [gone]\0"),
            tree(0, Upstream::Gone("origin/main".to_owned()))
        );
        assert_eq!(ok("## main\0"), tree(0, Upstream::None));
        assert_eq!(ok("## HEAD (no branch)\0"), tree(0, Upstream::Detached));
        assert_eq!(ok("## No commits yet on main\0"), tree(0, Upstream::None));
        assert_eq!(ok("## main"), tree(0, Upstream::None), "no trailing NUL");
        assert!(parse_tree_status("").is_err());
        assert!(parse_tree_status(" M flake.nix\0").is_err());
    }

    #[test]
    fn an_upstream_name_is_text_from_the_repository() {
        let t = parse_tree_status("## main...\u{1b}[31mevil [ahead 1]\0").unwrap();
        assert_eq!(
            t.upstream,
            Upstream::Tracking {
                name: "evil".to_owned(),
                ahead: 1,
                behind: 0
            }
        );
    }

    #[test]
    fn a_dirty_tree_warns_and_does_not_refuse() {
        let t = tree(3, tracking(0));
        let p = preflight(&facts(&t, Ok(GIB), Some(10 * GIB)));
        assert_eq!(p.refusals, Vec::<String>::new());
        assert_eq!(p.warnings.len(), 1);
        assert!(
            p.warnings[0].contains("3 tracked files"),
            "{:?}",
            p.warnings
        );
        let one = preflight(&facts(&tree(1, tracking(0)), Ok(GIB), Some(10 * GIB)));
        assert!(
            one.warnings[0].contains("1 tracked file:"),
            "{:?}",
            one.warnings
        );
    }

    #[test]
    fn unpushed_commits_warn() {
        let t = tree(0, tracking(2));
        let p = preflight(&facts(&t, Ok(GIB), Some(10 * GIB)));
        assert_eq!(p.refusals, Vec::<String>::new());
        assert!(
            p.warnings[0].contains("2 commits ahead of origin/main"),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn no_upstream_is_said_and_is_not_a_failure() {
        for u in [Upstream::None, Upstream::Detached] {
            let t = tree(0, u);
            let p = preflight(&facts(&t, Ok(GIB), Some(10 * GIB)));
            assert_eq!(p.refusals, Vec::<String>::new());
            assert!(p.warnings[0].contains("no upstream"), "{:?}", p.warnings);
        }
        let t = tree(0, Upstream::Gone("origin/x".to_owned()));
        let p = preflight(&facts(&t, Ok(GIB), Some(10 * GIB)));
        assert!(
            p.warnings[0].contains("no longer exists"),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn a_tree_check_that_could_not_run_warns() {
        let t = tree(0, tracking(0));
        let mut f = facts(&t, Ok(GIB), Some(10 * GIB));
        f.tree = Err("git status exited 128: not a git repository");
        let p = preflight(&f);
        assert_eq!(p.refusals, Vec::<String>::new());
        assert!(
            p.warnings[0].contains("could not check"),
            "{:?}",
            p.warnings
        );
        assert!(p.warnings[0].contains("not a git repository"));
    }

    #[test]
    fn disk_refuses_below_closure_plus_a_gibibyte_and_passes_at_it() {
        let t = tree(0, tracking(0));
        // Exactly closure + 1 GiB is enough; one byte less is not.
        let at = preflight(&facts(&t, Ok(3 * GIB), Some(4 * GIB)));
        assert_eq!(at, Preflight::default());
        let under = preflight(&facts(&t, Ok(3 * GIB), Some(4 * GIB - 1)));
        assert_eq!(under.refusals.len(), 1);
        assert!(
            under.refusals[0].contains("luna's /nix/store has 4.0 GiB free")
                && under.refusals[0].contains("needs 4.0 GiB"),
            "{:?}",
            under.refusals
        );
        // A closure bigger than the disk, and sums that would overflow.
        let big = preflight(&facts(&t, Ok(u64::MAX), Some(10 * GIB)));
        assert_eq!(big.refusals.len(), 1);
    }

    #[test]
    fn unknown_disk_inputs_warn_and_never_refuse() {
        let t = tree(0, tracking(0));
        let no_free = preflight(&facts(&t, Ok(3 * GIB), None));
        assert_eq!(no_free.refusals, Vec::<String>::new());
        assert!(
            no_free.warnings[0].contains("free space on luna's /nix/store is not known"),
            "{:?}",
            no_free.warnings
        );
        let no_closure = preflight(&facts(&t, Err("nix path-info timed out"), Some(GIB)));
        assert_eq!(no_closure.refusals, Vec::<String>::new());
        assert!(
            no_closure.warnings[0].contains("size of the pending closure is not known")
                && no_closure.warnings[0].contains("timed out"),
            "{:?}",
            no_closure.warnings
        );
    }

    fn shared(ip: &str, machines: &[&str]) -> SharedIp {
        SharedIp {
            ip: ip.parse::<IpAddr>().unwrap(),
            machines: machines.iter().map(|m| (*m).to_owned()).collect(),
        }
    }

    #[test]
    fn a_shared_address_refuses_and_names_the_machines() {
        let t = tree(0, tracking(0));
        let list = [shared("192.168.1.9", &["luna", "mars", "sol"])];
        let mut f = facts(&t, Ok(GIB), Some(10 * GIB));
        f.addresses.shared = &list;
        let p = preflight(&f);
        assert_eq!(p.refusals.len(), 1);
        assert!(
            p.refusals[0].contains("192.168.1.9")
                && p.refusals[0].contains("mars and sol also resolve to"),
            "{:?}",
            p.refusals
        );
        // Someone else's collision is not luna's.
        let others = [shared("192.168.1.9", &["mars", "sol"])];
        f.addresses.shared = &others;
        assert_eq!(preflight(&f).refusals, Vec::<String>::new());
        // Two machines: singular.
        let two = [shared("10.0.0.1", &["luna", "sol"])];
        f.addresses.shared = &two;
        assert!(preflight(&f).refusals[0].contains("sol also resolves to"));
    }

    #[test]
    fn pending_is_not_unknown_a_probe_still_out_refuses_the_disk_rule() {
        let t = tree(0, tracking(0));
        let mut f = facts(&t, Ok(GIB), None);
        f.probing = true;
        let p = preflight(&f);
        assert_eq!(p.refusals.len(), 1, "{:?}", p.refusals);
        assert!(
            p.refusals[0].contains("still probing luna"),
            "{:?}",
            p.refusals
        );
        // Unknown because the probe answered without it stays a warning.
        f.probing = false;
        let p = preflight(&f);
        assert_eq!(p.refusals, Vec::<String>::new());
        assert!(p.warnings[0].contains("not known"), "{:?}", p.warnings);
    }

    #[test]
    fn resolution_still_out_refuses_the_address_rule_and_resolution_failed_does_not() {
        let t = tree(0, tracking(0));
        let mut f = facts(&t, Ok(GIB), Some(10 * GIB));
        f.addresses.resolving = true;
        let p = preflight(&f);
        assert_eq!(p.refusals.len(), 1, "{:?}", p.refusals);
        assert!(
            p.refusals[0].contains("still resolving"),
            "{:?}",
            p.refusals
        );
        // Finished, and another machine's name failed: a warning, as decided.
        f.addresses.resolving = false;
        f.addresses.others_unknown = 1;
        let p = preflight(&f);
        assert_eq!(p.refusals, Vec::<String>::new());
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn the_address_rule_fails_closed_for_this_machine_only() {
        let t = tree(0, tracking(0));
        let mut f = facts(&t, Ok(GIB), Some(10 * GIB));
        f.addresses.own_known = false;
        let p = preflight(&f);
        assert_eq!(p.refusals.len(), 1);
        assert!(
            p.refusals[0].contains("no known address"),
            "{:?}",
            p.refusals
        );
        // Another machine's unknown address is a warning.
        f.addresses.own_known = true;
        f.addresses.others_unknown = 2;
        let p = preflight(&f);
        assert_eq!(p.refusals, Vec::<String>::new());
        assert!(
            p.warnings[0].contains("2 of the other machines"),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn typed_confirmation_follows_public_and_the_option_diff_hook_says_no() {
        assert!(typed_confirmation_required(true));
        assert!(!typed_confirmation_required(false));
        assert!(!option_diff_touches_network(), "AUS-22 is not implemented");
        let t = tree(0, tracking(0));
        let mut f = facts(&t, Ok(GIB), Some(10 * GIB));
        assert!(!preflight(&f).typed_confirmation);
        f.public = true;
        assert!(preflight(&f).typed_confirmation);
    }

    #[test]
    fn closure_sizes_are_read_strictly() {
        let a = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-a";
        let b = "/nix/store/1123456789abcdfghijklmnpqrsvwxyz-b";
        let sized = |p: &str, n: u64| (StorePath::new(p).unwrap(), n);
        assert_eq!(
            parse_closure_sizes(&format!("{a}\t100\n{b}\t2048\n\n")),
            Ok(vec![sized(a, 100), sized(b, 2048)])
        );
        assert_eq!(
            parse_closure_sizes(&format!("{a}   7\r\n")),
            Ok(vec![sized(a, 7)])
        );
        for bad in [
            "",
            "\n",
            &format!("{a}\n"),
            &format!("{a}\tx\n"),
            &format!("{a}\t1\t2\n"),
            &format!("{a}\t1\nwarning: something\n"),
            "/etc/passwd\t5\n",
            &format!("{a}\t{}\n{b}\t1\n", u64::MAX),
            &format!("{a}\t-1\n"),
        ] {
            assert!(parse_closure_sizes(bad).is_err(), "{bad:?}");
        }
    }

    fn p(c: char) -> StorePath {
        StorePath::new(format!("/nix/store/{c}123456789abcdfghijklmnpqrsvwxyz-{c}")).unwrap()
    }

    #[test]
    fn the_estimate_counts_only_what_the_machine_does_not_have() {
        let sizes = [(p('a'), 4 * GIB), (p('b'), 100), (p('c'), 50)];
        // The machine has the big path already: only b and c are new.
        let deployed: BTreeSet<StorePath> = [p('a'), p('z')].into_iter().collect();
        let need = estimate(&sizes, Some(&deployed));
        assert_eq!(need.bytes, 150);
        assert_eq!(
            need.basis,
            Basis::NotOnMachine {
                new_paths: 2,
                total_paths: 3,
                whole_bytes: 4 * GIB + 150
            }
        );
        // Without the machine's closure: all of it.
        let need = estimate(&sizes, None);
        assert_eq!(need.bytes, 4 * GIB + 150);
        assert_eq!(need.basis, Basis::WholeClosure);
        // A machine that has all of it needs nothing.
        let all: BTreeSet<StorePath> = sizes.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(estimate(&sizes, Some(&all)).bytes, 0);
        // Sums never overflow.
        let huge = [(p('a'), u64::MAX), (p('b'), u64::MAX)];
        assert_eq!(estimate(&huge, None).bytes, u64::MAX);
    }

    #[test]
    fn a_refusal_says_which_estimate_it_used() {
        let t = tree(0, tracking(0));
        let mut f = facts(&t, Ok(0), Some(GIB / 2));
        let whole = estimate(&[(p('a'), 4 * GIB)], None);
        f.closure = Ok(whole);
        let why = preflight(&f).refusals.remove(0);
        assert!(
            why.contains("the whole closure's 4.0 GiB") && why.contains("not known"),
            "{why}"
        );
        let deployed: BTreeSet<StorePath> = std::iter::once(p('z')).collect();
        let part = estimate(&[(p('a'), 4 * GIB), (p('b'), 1)], Some(&deployed));
        f.closure = Ok(part);
        let why = preflight(&f).refusals.remove(0);
        assert!(
            why.contains("the 4.0 GiB of the 2 of the closure's 2 paths it does not have yet"),
            "{why}"
        );
    }

    #[test]
    fn a_closure_mostly_on_the_machine_no_longer_refuses() {
        // 6 GiB closure, 5 GiB free: refused as a whole, fine when only 100 MiB is new.
        let t = tree(0, tracking(0));
        let sizes = [(p('a'), 6 * GIB - (100 << 20)), (p('b'), 100 << 20)];
        let mut f = facts(&t, Ok(0), Some(5 * GIB));
        f.closure = Ok(estimate(&sizes, None));
        assert_eq!(preflight(&f).refusals.len(), 1);
        let deployed: BTreeSet<StorePath> = std::iter::once(p('a')).collect();
        f.closure = Ok(estimate(&sizes, Some(&deployed)));
        assert_eq!(preflight(&f).refusals, Vec::<String>::new());
    }

    #[test]
    fn filter_drivers_are_listed_from_the_config_keys() {
        let keys = "core.bare\0filter.lfs.clean\0filter.lfs.smudge\0filter.lfs.required\0\
                    filter.my.driver.process\0filter.evil.clean\0user.name\0filter.x.y\0";
        assert_eq!(
            parse_filter_names(keys),
            Ok(vec![
                "evil".to_owned(),
                "lfs".to_owned(),
                "my.driver".to_owned()
            ])
        );
        assert_eq!(parse_filter_names(""), Ok(Vec::new()));
        // A name `-c key=value` cannot carry: the check cannot run, and says so.
        for bad in [
            "filter.a=b.clean\0",
            "filter.a\u{1b}b.clean\0",
            "filter..clean\0",
        ] {
            assert!(parse_filter_names(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn status_runs_with_every_listed_filter_blanked_and_nothing_else_changed() {
        assert_eq!(
            status_args(&[]),
            [
                "--no-optional-locks",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.attributesFile=/dev/null",
                "status",
                "--porcelain=v1",
                "-b",
                "-z",
                "--untracked-files=no",
                "--ignore-submodules=all",
                "--no-renames",
            ]
        );
        let args = status_args(&["a".to_owned(), "b.c".to_owned()]);
        let want = [
            "-c",
            "filter.a.clean=",
            "-c",
            "filter.a.smudge=",
            "-c",
            "filter.a.process=",
            "-c",
            "filter.b.c.clean=",
            "-c",
            "filter.b.c.smudge=",
            "-c",
            "filter.b.c.process=",
        ];
        let at = args.iter().position(|a| a == "filter.a.clean=").unwrap() - 1;
        assert_eq!(args[at..at + 12], want);
        // The status command comes after every override, or they would not apply to it.
        let status = args.iter().position(|a| a == "status").unwrap();
        assert!(args.iter().rposition(|a| a.starts_with("filter.")).unwrap() < status);
        // No attr-source: it names SHA-1's empty tree, which a SHA-256 repository rejects.
        assert!(!args.iter().any(|a| a.contains("attr-source")));
    }

    #[test]
    fn bytes_for_people() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(5 * GIB / 2), "2.5 GiB");
        assert_eq!(format_bytes(u64::MAX), "16777216.0 TiB");
    }
}
