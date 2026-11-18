//! Package-level runtime closure differences, without obtaining either closure.
//!
//! Identity is the sanitised store name before the first `-` followed by an ASCII digit
//! (the digit-only `parseDrvName` rule requested for this contract). Unversioned names
//! remain packages. Known output suffixes after a version collapse; unknown suffixes stay
//! in the version rather than guessing away upstream version text. Hash-only rebuilds are
//! intentionally invisible. Multiple versions are sets, never arbitrarily paired.
//!
//! Generated NixOS artefacts are excluded by an explicit, conservative list in
//! `generated`: not by requiring a version (which would lose real packages). This is a
//! package preview, not a complete closure or option diff. Custom outputs and generated
//! names outside that list may still appear.
//!
//! Missing pending input is `None` (unknown until built or cached), not an empty closure.
//! Oversized inputs fail explicitly; they never produce a deceptively complete diff.
//! Output is name-sorted and capped, with an exact total and a truncation flag.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::text::sanitize_line;

/// Maximum paths per closure, including duplicates and non-packages.
pub const MAX_CLOSURE_PATHS: usize = 100_000;
/// Maximum bytes per input path, before sanitising; bounds per-path work and storage.
pub const MAX_PATH_BYTES: usize = 4096;
/// Maximum entries in each added, removed, changed and highlights list.
pub const MAX_PACKAGE_CHANGES: usize = 128;
/// Maximum versions retained per package and side.
pub const MAX_PACKAGE_VERSIONS: usize = 16;
/// Maximum bytes in a sanitised store name; larger identities fail rather than collide.
pub const MAX_PACKAGE_TEXT: usize = 255;

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
    #[error("sanitised store name exceeds the byte limit")]
    NameTooLong,
}

/// One changed package. Empty `before` means added; empty `after` means removed;
/// otherwise the version set changed. A `null` set member means unversioned.
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
}

/// Exact package counts before output caps, not counts of individual versions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffCounts {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
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
/// a reason (one sanitised line, at most 512 UTF-8 bytes). Lists are name-sorted; versions
/// are Nix-sorted with lexical ties. Null version members mean unversioned packages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageDiff {
    pub state: DiffState,
    pub reason: Option<String>,
    pub added: Vec<PackageChange>,
    pub removed: Vec<PackageChange>,
    pub changed: Vec<PackageChange>,
    pub highlights: Vec<PackageHighlight>,
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
            added: Vec::new(),
            removed: Vec::new(),
            changed: Vec::new(),
            highlights: Vec::new(),
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
/// Malformed paths, children of store paths, and known generated artefacts are ignored.
/// Display text is
/// sanitised before interpreting the name; identities therefore use that same safe text.
/// Use [`diff_closures`] when a path-length error must be distinguished from an exclusion.
#[must_use]
pub fn package_from_store_path(path: &str) -> Option<Package> {
    if path.len() > MAX_PATH_BYTES {
        return None;
    }
    let path = path.strip_prefix("/nix/store/")?;
    let (hash, raw) = path.split_once('-')?;
    if hash.len() != 32
        || !hash
            .bytes()
            .all(|b| b"0123456789abcdfghijklmnpqrsvwxyz".contains(&b))
        || raw.contains('/')
    {
        return None;
    }
    let mut name = sanitize_line(raw);
    if name.is_empty() || generated(&name) {
        return None;
    }
    let split = name
        .as_bytes()
        .windows(2)
        .position(|w| w[0] == b'-' && w[1].is_ascii_digit());
    match split {
        Some(0) => None,
        Some(i) => {
            let mut version = &name[i + 1..];
            // Only known Nixpkgs output names: `-66` in glibc is a revision, not an output.
            while let Some((base, suffix)) = version.rsplit_once('-') {
                if !matches!(
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
                ) {
                    break;
                }
                version = base;
            }
            let version = Some(version.to_owned());
            name.truncate(i);
            Some(Package { name, version })
        }
        None => Some(Package {
            name,
            version: None,
        }),
    }
}

fn generated(name: &str) -> bool {
    matches!(
        name,
        "etc" | "system-units" | "system-path" | "activation-script" | "initrd"
    ) || ["nixos-system-", "darwin-system-", "unit-", "etc-"]
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

type Versions = BTreeSet<Option<String>>;
type Closures = BTreeMap<String, (Versions, Versions)>;

fn index<S: AsRef<str>>(
    paths: &[S],
    packages: &mut Closures,
    pending: bool,
) -> Result<(), DiffError> {
    if paths.len() > MAX_CLOSURE_PATHS {
        return Err(DiffError::TooManyPaths);
    }
    for path in paths {
        let path = path.as_ref();
        if path.len() > MAX_PATH_BYTES {
            return Err(DiffError::PathTooLong);
        }
        if let Some(p) = package_from_store_path(path) {
            if p.name.len() + p.version.as_ref().map_or(0, |v| v.len() + 1) > MAX_PACKAGE_TEXT {
                return Err(DiffError::NameTooLong);
            }
            let pair = packages.entry(p.name).or_default();
            let versions = if pending { &mut pair.1 } else { &mut pair.0 };
            versions.insert(p.version);
        }
    }
    Ok(())
}

fn sorted(versions: Versions) -> Vec<Option<String>> {
    let mut versions: Vec<_> = versions.into_iter().collect();
    if versions.len() > MAX_PACKAGE_VERSIONS {
        versions.select_nth_unstable_by(MAX_PACKAGE_VERSIONS, |a, b| {
            version_order(a.as_deref(), b.as_deref())
        });
        versions.truncate(MAX_PACKAGE_VERSIONS);
    }
    versions.sort_by(|a, b| version_order(a.as_deref(), b.as_deref()));
    versions
}

fn version_order(a: Option<&str>, b: Option<&str>) -> Ordering {
    compare_versions(a.unwrap_or(""), b.unwrap_or("")).then_with(|| a.cmp(&b))
}

fn majors(versions: &Versions) -> BTreeSet<&str> {
    versions
        .iter()
        .filter_map(Option::as_deref)
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
    let mut packages = Closures::new();
    index(deployed, &mut packages, false)?;
    index(pending, &mut packages, true)?;
    let mut diff = PackageDiff {
        state: DiffState::Computed,
        ..PackageDiff::not_cached()
    };
    for (name, (old, new)) in packages {
        if old == new {
            continue;
        }
        let mut kinds = Vec::new();
        match name.as_str() {
            "linux" => kinds.push(HighlightKind::Kernel),
            "systemd" => kinds.push(HighlightKind::Systemd),
            "glibc" => kinds.push(HighlightKind::Glibc),
            "nix" => kinds.push(HighlightKind::Nix),
            _ => {}
        }
        if !old.is_empty() && !new.is_empty() {
            let before_majors = majors(&old);
            let after_majors = majors(&new);
            if !before_majors.is_empty()
                && !after_majors.is_empty()
                && before_majors != after_majors
            {
                kinds.push(HighlightKind::MajorVersion);
            }
        }
        if !kinds.is_empty() {
            if diff.highlights.len() < MAX_PACKAGE_CHANGES {
                diff.highlights.push(PackageHighlight {
                    name: name.clone(),
                    kinds,
                });
            } else {
                diff.truncated.highlights = true;
            }
        }
        let (list, count, truncated) = if old.is_empty() {
            (
                &mut diff.added,
                &mut diff.counts.added,
                &mut diff.truncated.added,
            )
        } else if new.is_empty() {
            (
                &mut diff.removed,
                &mut diff.counts.removed,
                &mut diff.truncated.removed,
            )
        } else {
            (
                &mut diff.changed,
                &mut diff.counts.changed,
                &mut diff.truncated.changed,
            )
        };
        *count += 1;
        if list.len() == MAX_PACKAGE_CHANGES {
            *truncated = true;
            continue;
        }
        list.push(PackageChange {
            name,
            before_truncated: old.len() > MAX_PACKAGE_VERSIONS,
            after_truncated: new.len() > MAX_PACKAGE_VERSIONS,
            before: sorted(old),
            after: sorted(new),
        });
    }
    Ok(Some(diff))
}
