//! Package-level runtime closure differences, without obtaining either closure.
//!
//! Names split before the first digit-led version, or a trailing 7+ digit hexadecimal
//! revision. Known outputs and suffixes corroborated by a base version in the same
//! closure collapse. Version sets are compared without arbitrary pairing.
//!
//! Generated NixOS artefacts use `generated`; all other unversioned names are grouped
//! into added/removed counts, like nvd's non-selected paths. Single-integer directory
//! names (`dbus-1`) are ignored when that package has a dotted version in the closure.
//! Unversioned paths never enter version sets. Critical same-version hash changes are
//! listed as rebuilds; other rebuilds are invisible.
//!
//! Invalid store-name bytes are rejected, not sanitised into another identity. Missing
//! pending input is unknown, not empty. Inputs fail explicitly on size overflow; output
//! retains critical highlights, then major highlights, then alphabetical fill within
//! entry and JSON-byte caps. Counts always describe the complete inputs.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::text::sanitize_line;

/// Maximum paths per closure, including duplicates and non-packages.
pub const MAX_CLOSURE_PATHS: usize = 100_000;
/// Maximum bytes per input path; bounds per-path work and storage.
pub const MAX_PATH_BYTES: usize = 4096;
/// Maximum entries in each added, removed, changed and highlights list.
pub const MAX_PACKAGE_CHANGES: usize = 128;
/// Maximum versions retained per package and side.
pub const MAX_PACKAGE_VERSIONS: usize = 16;
/// Maximum bytes in a valid store name; larger identities fail rather than collide.
pub const MAX_PACKAGE_TEXT: usize = 255;
/// Maximum compact JSON bytes for a computed preview, including metadata.
pub const MAX_PREVIEW_BYTES: usize = 256 * 1024;

/// A package extracted from one store path; no version is represented by `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: Option<String>,
}

/// Why no trustworthy package preview could be computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DiffError {
    #[error("closure exceeds the path limit")]
    TooManyPaths,
    #[error("closure path exceeds the byte limit")]
    PathTooLong,
    #[error("store name exceeds the byte limit")]
    NameTooLong,
}

/// One versioned package. Empty `before` means added; empty `after` means removed.
/// Version members are always non-null; unversioned paths are counted separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageChange {
    pub name: String,
    /// Distinct deployed versions, sorted by Nix ordering with lexical tie-breaking.
    pub before: Vec<Option<String>>,
    /// Distinct pending versions, in the same order.
    pub after: Vec<Option<String>>,
    /// Whether either version set was capped; comparison and counts use complete sets.
    pub before_truncated: bool,
    pub after_truncated: bool,
}

/// Why a package deserves attention. Not a deployment-safety assessment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HighlightKind {
    Kernel,
    Systemd,
    Glibc,
    Nix,
    MajorVersion,
}

/// A highlighted package; kinds are in enum declaration order, at most two per package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageHighlight {
    pub name: String,
    pub kinds: Vec<HighlightKind>,
    pub before: Vec<Option<String>>,
    pub after: Vec<Option<String>>,
    pub before_truncated: bool,
    pub after_truncated: bool,
}

/// Exact package counts before output caps, not counts of individual versions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffCounts {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
}

/// Unversioned names are grouped by identity, ignoring hash-only changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnversionedCounts {
    pub added: usize,
    pub removed: usize,
}

/// Rejected paths per side, including malformed store roots and invalid name bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvalidCounts {
    pub deployed: usize,
    pub pending: usize,
}

/// Independent truncation flags for every top-level list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent per-list wire flags, not mutually exclusive states"
)]
pub struct DiffTruncation {
    pub added: bool,
    pub removed: bool,
    pub changed: bool,
    pub highlights: bool,
    pub rebuilt: bool,
    pub bytes: bool,
}

/// Computation state; unknown pending input is distinct from a known empty closure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffState {
    Computed,
    NotCached,
    Failed,
}

/// Optional JSON v1 package preview. `MachineReport`'s null means not requested yet.
///
/// Non-computed states have empty lists, zero counts and no truncation. Only failed has
/// a reason (one sanitised line, at most 512 UTF-8 bytes). Lists put highlights first;
/// versions are Nix-sorted with lexical ties. Null version members are never emitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageDiff {
    pub state: DiffState,
    pub reason: Option<String>,
    /// System toplevel in each closure: the unique one (null if absent or ambiguous) from
    /// `diff_closures`; the core then records the toplevel it checked (the probe's system and
    /// the evaluation's out path), which a closure holding other toplevels still contains.
    pub deployed: Option<String>,
    pub pending: Option<String>,
    pub added: Vec<PackageChange>,
    pub removed: Vec<PackageChange>,
    pub changed: Vec<PackageChange>,
    pub highlights: Vec<PackageHighlight>,
    pub rebuilt: Vec<String>,
    pub unversioned: UnversionedCounts,
    pub invalid: InvalidCounts,
    pub counts: DiffCounts,
    pub truncated: DiffTruncation,
}

impl PackageDiff {
    /// Unknown until the pending closure is built or cached.
    #[must_use]
    pub fn not_cached() -> Self {
        Self {
            state: DiffState::NotCached,
            reason: None,
            deployed: None,
            pending: None,
            added: Vec::new(),
            removed: Vec::new(),
            changed: Vec::new(),
            highlights: Vec::new(),
            rebuilt: Vec::new(),
            unversioned: UnversionedCounts::default(),
            invalid: InvalidCounts::default(),
            counts: DiffCounts::default(),
            truncated: DiffTruncation::default(),
        }
    }

    /// An adapter or pure-rule failure, with bounded terminal-safe text.
    #[must_use]
    pub fn failed(reason: &str) -> Self {
        let mut reason = sanitize_line(reason);
        let mut end = reason.len().min(512);
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason.truncate(end);
        Self {
            state: DiffState::Failed,
            reason: Some(reason),
            ..Self::not_cached()
        }
    }
}

/// Parses one direct `/nix/store/<32-character Nix hash>-<name>` path.
///
/// Malformed paths, invalid store-name bytes, children and generated artefacts are ignored.
/// Use [`diff_closures`] to count rejected paths and collapse corroborated custom outputs.
#[must_use]
pub fn package_from_store_path(path: &str) -> Option<Package> {
    package_from_name(store_name(path)?)
}

fn package_from_name(raw: &str) -> Option<Package> {
    if generated(raw) {
        return None;
    }
    let split = raw
        .as_bytes()
        .windows(2)
        .position(|w| w[0] == b'-' && w[1].is_ascii_digit())
        .or_else(|| {
            raw.rsplit_once('-')
                .filter(|(_, v)| git_revision(v))
                .map(|(name, _)| name.len())
        });
    match split {
        Some(0) => None,
        Some(i) => {
            let mut version = &raw[i + 1..];
            while let Some((base, suffix)) = version.rsplit_once('-') {
                if !known_output(suffix) {
                    break;
                }
                version = base;
            }
            Some(Package {
                name: raw[..i].into(),
                version: Some(version.into()),
            })
        }
        None => Some(Package {
            name: raw.into(),
            version: None,
        }),
    }
}

fn store_name(path: &str) -> Option<&str> {
    if path.len() > MAX_PATH_BYTES {
        return None;
    }
    let (hash, name) = path.strip_prefix("/nix/store/")?.split_once('-')?;
    (hash.len() == 32
        && hash
            .bytes()
            .all(|b| b"0123456789abcdfghijklmnpqrsvwxyz".contains(&b))
        && !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+._?=-".contains(&b)))
    .then_some(name)
}

fn git_revision(version: &str) -> bool {
    version.len() >= 7 && version.bytes().all(|b| b.is_ascii_hexdigit())
}

fn known_output(suffix: &str) -> bool {
    matches!(
        suffix,
        "out"
            | "man"
            | "dev"
            | "lib"
            | "bin"
            | "doc"
            | "info"
            | "debug"
            | "static"
            | "headers"
            | "modules"
            | "getent"
            | "libgcc"
            | "terminfo"
            | "data"
            | "py"
            | "lib32"
            | "login"
            | "mount"
            | "swap"
            | "lastlog"
            | "su"
            | "xxd"
            | "sandbox"
            | "env"
            | "nc"
            | "p11kit"
    )
}

fn generated(name: &str) -> bool {
    matches!(
        name,
        "etc" | "system-units" | "system-path" | "activation-script" | "initrd"
    ) || (name.starts_with("unit-")
        && !name
            .as_bytes()
            .windows(2)
            .any(|w| w[0] == b'-' && w[1].is_ascii_digit()))
        || ["nixos-system-", "darwin-system-", "etc-"]
            .iter()
            .any(|p| name.starts_with(p))
        || [
            ".drv", "-source", ".conf", ".service", ".target", ".socket", ".timer", ".path",
            ".mount",
        ]
        .iter()
        .any(|s| name.ends_with(s))
}

/// Compares versions using Nix's component ordering.
///
/// Skip dots/dashes; split digit and non-digit runs. Numeric components compare as signed
/// 32-bit integers (overflow falls back to text, as in Nix);
/// `pre` precedes everything, including an absent component. Numbers follow other text;
/// otherwise compare lexically. Separators and leading numeric zeroes are insignificant.
/// Source: <https://github.com/NixOS/nix/blob/2.34.7/src/libstore/names.cc>.
#[must_use]
pub fn compare_versions(mut left: &str, mut right: &str) -> Ordering {
    while !left.is_empty() || !right.is_empty() {
        let a = component(&mut left);
        let b = component(&mut right);
        if component_lt(a, b) {
            return Ordering::Less;
        }
        if component_lt(b, a) {
            return Ordering::Greater;
        }
    }
    Ordering::Equal
}

fn component<'a>(rest: &mut &'a str) -> &'a str {
    *rest = rest.trim_start_matches(['.', '-']);
    let digits = rest.as_bytes().first().is_some_and(u8::is_ascii_digit);
    let end = rest
        .bytes()
        .position(|b| b == b'.' || b == b'-' || b.is_ascii_digit() != digits)
        .unwrap_or(rest.len());
    let (part, tail) = rest.split_at(end);
    *rest = tail;
    part
}

fn component_lt(a: &str, b: &str) -> bool {
    let na = a.parse::<i32>().ok();
    let nb = b.parse::<i32>().ok();
    match (na, nb) {
        (Some(a), Some(b)) => a < b,
        _ if a.is_empty() && nb.is_some() => true,
        _ if a == "pre" && b != "pre" => true,
        _ if b == "pre" => false,
        _ if nb.is_some() => true,
        _ if na.is_some() => false,
        _ => a < b,
    }
}

type Versions = BTreeSet<String>;

#[derive(Default)]
struct Closure {
    packages: BTreeMap<String, Versions>,
    unversioned: BTreeSet<String>,
    critical_paths: BTreeMap<String, BTreeSet<String>>,
    toplevels: BTreeSet<String>,
    invalid: usize,
}

fn index<S: AsRef<str>>(paths: &[S]) -> Result<Closure, DiffError> {
    if paths.len() > MAX_CLOSURE_PATHS {
        return Err(DiffError::TooManyPaths);
    }
    let mut closure = Closure::default();
    for path in paths {
        let path = path.as_ref();
        if path.len() > MAX_PATH_BYTES {
            return Err(DiffError::PathTooLong);
        }
        let Some(raw) = store_name(path) else {
            closure.invalid += 1;
            continue;
        };
        if raw.len() > MAX_PACKAGE_TEXT {
            return Err(DiffError::NameTooLong);
        }
        if raw.starts_with("nixos-system-") || raw.starts_with("darwin-system-") {
            closure.toplevels.insert(path.to_owned());
        }
        if let Some(p) = package_from_name(raw) {
            if let Some(version) = p.version {
                if critical(&p.name).is_some() {
                    closure
                        .critical_paths
                        .entry(p.name.clone())
                        .or_default()
                        .insert(path.to_owned());
                }
                closure.packages.entry(p.name).or_default().insert(version);
            } else {
                closure.unversioned.insert(p.name);
            }
        }
    }
    for versions in closure.packages.values_mut() {
        let remove: Vec<_> = versions
            .iter()
            .filter(|v| {
                let output = v.rsplit_once('-').is_some_and(|(base, suffix)| {
                    suffix
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_lowercase)
                        && suffix
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                        && versions.contains(base)
                });
                let directory = v.bytes().all(|b| b.is_ascii_digit()) && {
                    let prefix = format!("{v}.");
                    versions
                        .range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
                        .next()
                        .is_some_and(|other| other.starts_with(&prefix))
                };
                output || directory
            })
            .cloned()
            .collect();
        for version in remove {
            versions.remove(&version);
        }
    }
    Ok(closure)
}

fn sorted(versions: &Versions) -> Vec<Option<String>> {
    let mut versions: Vec<_> = versions.iter().map(String::as_str).collect();
    if versions.len() > MAX_PACKAGE_VERSIONS {
        versions
            .select_nth_unstable_by(MAX_PACKAGE_VERSIONS, |a, b| version_order(Some(a), Some(b)));
        versions.truncate(MAX_PACKAGE_VERSIONS);
    }
    versions.sort_by(|a, b| version_order(Some(a), Some(b)));
    versions.into_iter().map(|v| Some(v.to_owned())).collect()
}

fn version_order(a: Option<&str>, b: Option<&str>) -> Ordering {
    compare_versions(a.unwrap_or(""), b.unwrap_or("")).then_with(|| a.cmp(&b))
}

fn majors(versions: &Versions) -> BTreeSet<&str> {
    versions
        .iter()
        .map(String::as_str)
        .filter(|v| !git_revision(v))
        .map(|v| {
            let mut rest = v;
            let first = component(&mut rest);
            if first.bytes().all(|b| b.is_ascii_digit()) {
                let trimmed = first.trim_start_matches('0');
                if trimmed.is_empty() { "0" } else { trimmed }
            } else {
                first
            }
        })
        .collect()
}

/// Computes a bounded preview; unknown pending input returns None, not removals.
///
/// Both inputs must be runtime closures. Counts and highlights use complete version sets.
/// Callers can represent absent cached input with [`PackageDiff::not_cached`] and map
/// errors to [`PackageDiff::failed`]. No inputs are obtained here.
///
/// # Errors
///
/// Returns [`DiffError`] if either known closure exceeds a documented input limit.
pub fn diff_closures<S: AsRef<str>>(
    deployed: &[S],
    pending: Option<&[S]>,
) -> Result<Option<PackageDiff>, DiffError> {
    let Some(pending) = pending else {
        return Ok(None);
    };
    let deployed = index(deployed)?;
    let pending = index(pending)?;
    let mut diff = PackageDiff {
        state: DiffState::Computed,
        deployed: unique_toplevel(&deployed),
        pending: unique_toplevel(&pending),
        invalid: InvalidCounts {
            deployed: deployed.invalid,
            pending: pending.invalid,
        },
        unversioned: UnversionedCounts {
            added: pending
                .unversioned
                .difference(&deployed.unversioned)
                .count(),
            removed: deployed
                .unversioned
                .difference(&pending.unversioned)
                .count(),
        },
        ..PackageDiff::not_cached()
    };
    for name in ["glibc", "linux", "nix", "systemd"] {
        if deployed.packages.contains_key(name)
            && deployed.packages.get(name) == pending.packages.get(name)
            && deployed.critical_paths.get(name) != pending.critical_paths.get(name)
        {
            diff.rebuilt.push(name.to_owned());
        }
    }
    let empty = Versions::new();
    let mut lists: [Vec<Candidate>; 3] = std::array::from_fn(|_| Vec::new());
    let mut highlight_count = 0;
    for name in deployed.packages.keys().chain(
        pending
            .packages
            .keys()
            .filter(|name| !deployed.packages.contains_key(*name)),
    ) {
        let old = deployed.packages.get(name).unwrap_or(&empty);
        let new = pending.packages.get(name).unwrap_or(&empty);
        if old == new {
            continue;
        }
        let category = if old.is_empty() {
            0
        } else if new.is_empty() {
            1
        } else {
            2
        };
        match category {
            0 => diff.counts.added += 1,
            1 => diff.counts.removed += 1,
            _ => diff.counts.changed += 1,
        }
        let mut kinds = [HighlightKind::MajorVersion; 2];
        let mut kind_count = 0;
        if let Some(kind) = critical(name) {
            kinds[kind_count] = kind;
            kind_count += 1;
        }
        if !old.is_empty() && !new.is_empty() {
            let before_majors = majors(old);
            let after_majors = majors(new);
            if !before_majors.is_empty()
                && !after_majors.is_empty()
                && before_majors != after_majors
            {
                kind_count += 1;
            }
        }
        highlight_count += usize::from(kind_count != 0);
        retain_candidate(&mut lists[category], name, old, new, &kinds[..kind_count]);
    }
    fill_preview(&mut diff, lists);
    diff.truncated.highlights |= diff.highlights.len() < highlight_count;
    Ok(Some(diff))
}

fn unique_toplevel(closure: &Closure) -> Option<String> {
    (closure.toplevels.len() == 1).then(|| closure.toplevels.first().unwrap().clone())
}

fn critical(name: &str) -> Option<HighlightKind> {
    match name {
        "linux" => Some(HighlightKind::Kernel),
        "systemd" => Some(HighlightKind::Systemd),
        "glibc" => Some(HighlightKind::Glibc),
        "nix" => Some(HighlightKind::Nix),
        _ => None,
    }
}

struct Candidate {
    change: PackageChange,
    kinds: Vec<HighlightKind>,
}

fn priority<'a>(name: &'a str, kinds: &[HighlightKind]) -> (u8, &'a str) {
    (
        if critical(name).is_some() {
            0
        } else if kinds.is_empty() {
            2
        } else {
            1
        },
        name,
    )
}

fn retain_candidate(
    list: &mut Vec<Candidate>,
    name: &str,
    old: &Versions,
    new: &Versions,
    kinds: &[HighlightKind],
) {
    let position =
        list.partition_point(|c| priority(&c.change.name, &c.kinds) < priority(name, kinds));
    if position >= MAX_PACKAGE_CHANGES {
        return;
    }
    if list.len() == MAX_PACKAGE_CHANGES {
        list.pop();
    }
    list.insert(
        position,
        Candidate {
            change: PackageChange {
                name: name.to_owned(),
                before: sorted(old),
                after: sorted(new),
                before_truncated: old.len() > MAX_PACKAGE_VERSIONS,
                after_truncated: new.len() > MAX_PACKAGE_VERSIONS,
            },
            kinds: kinds.to_vec(),
        },
    );
}

// Count serialized bytes without allocating a second copy of the preview.
struct ByteCount(usize);

impl std::io::Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_bytes(value: &impl Serialize) -> usize {
    let mut count = ByteCount(0);
    serde_json::to_writer(&mut count, value).expect("serializing package preview");
    count.0
}

fn fill_preview(diff: &mut PackageDiff, lists: [Vec<Candidate>; 3]) {
    let mut candidates: Vec<_> = lists
        .into_iter()
        .enumerate()
        .flat_map(|(category, list)| list.into_iter().map(move |c| (category, c)))
        .collect();
    candidates.sort_by(|(_, a), (_, b)| {
        priority(&a.change.name, &a.kinds).cmp(&priority(&b.change.name, &b.kinds))
    });
    let mut remaining = MAX_PREVIEW_BYTES - json_bytes(diff);
    for (category, candidate) in candidates {
        let change = candidate.change;
        let highlight = (!candidate.kinds.is_empty()).then(|| PackageHighlight {
            name: change.name.clone(),
            kinds: candidate.kinds,
            before: change.before.clone(),
            after: change.after.clone(),
            before_truncated: change.before_truncated,
            after_truncated: change.after_truncated,
        });
        let highlight_fits = highlight.is_none() || diff.highlights.len() < MAX_PACKAGE_CHANGES;
        let bytes = json_bytes(&change) + 1 + highlight.as_ref().map_or(0, |h| json_bytes(h) + 1);
        if bytes > remaining || !highlight_fits {
            diff.truncated.bytes |= bytes > remaining;
            diff.truncated.highlights |= highlight.is_some();
            continue;
        }
        remaining -= bytes;
        if let Some(highlight) = highlight {
            diff.highlights.push(highlight);
        }
        match category {
            0 => diff.added.push(change),
            1 => diff.removed.push(change),
            _ => diff.changed.push(change),
        }
    }
    diff.truncated.added = diff.added.len() < diff.counts.added;
    diff.truncated.removed = diff.removed.len() < diff.counts.removed;
    diff.truncated.changed = diff.changed.len() < diff.counts.changed;
}
