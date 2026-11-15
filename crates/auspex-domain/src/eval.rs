//! Evaluation results and the eval cache.
//!
//! `nix-eval-jobs` prints one JSON line per machine as each finishes; [`parse_eval_line`]
//! reads one. Evaluating is slow and no faster warm, so results are cached under a
//! [`CacheKey`] that [`cache_key`] computes from inputs the effects layer reads (`git
//! rev-parse HEAD`, `flake.lock`, `git diff HEAD`, the untracked file list).

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::observe::StorePath;
use crate::text::last_lines;

// ---------------------------------------------------------------------------------------
// nix-eval-jobs lines
// ---------------------------------------------------------------------------------------

/// How many lines of nix's error text are kept for display. nix's errors end with the
/// useful part (`error: attribute 'x' missing`), and the trace above it can run to hundreds
/// of lines.
pub const ERROR_TAIL_LINES: usize = 8;

/// A machine's toplevel evaluated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSuccess {
    /// `attr` as nix-eval-jobs printed it.
    pub attr: String,
    /// `attrPath`, when present.
    pub attr_path: Vec<String>,
    /// The toplevel's `.drv`.
    pub drv_path: StorePath,
    /// The toplevel's output path (`outputs.out`): what `/run/current-system` will point to
    /// once deployed.
    pub out_path: StorePath,
}

/// A machine's toplevel failed to evaluate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalFailure {
    /// `attr` as nix-eval-jobs printed it.
    pub attr: String,
    /// `attrPath`, when present.
    pub attr_path: Vec<String>,
    /// The last [`ERROR_TAIL_LINES`] non-empty lines of `error`, with ANSI escapes and
    /// control characters stripped.
    pub error_tail: Vec<String>,
}

/// One line of `nix-eval-jobs` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvalOutcome {
    /// Evaluated.
    Success(EvalSuccess),
    /// Failed to evaluate; the other machines are unaffected.
    Error(EvalFailure),
}

impl EvalFailure {
    /// One line for a status reason: the last line that starts with `error:`, or else the
    /// last line. nix prints the location (`at file.nix:12:5:`) *after* the message, so the
    /// plain last line is usually a file position rather than the reason.
    #[must_use]
    pub fn summary(&self) -> Option<&str> {
        self.error_tail
            .iter()
            .rev()
            .find(|l| l.starts_with("error:"))
            .or_else(|| self.error_tail.last())
            .map(String::as_str)
    }
}

impl EvalOutcome {
    /// The `attr` either way.
    #[must_use]
    pub fn attr(&self) -> &str {
        match self {
            Self::Success(s) => &s.attr,
            Self::Error(e) => &e.attr,
        }
    }
}

/// A line that is not a `nix-eval-jobs` result.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvalLineError {
    /// Not a JSON object.
    #[error("not a JSON object: {0}")]
    Malformed(String),
    /// A required field is missing or has the wrong type.
    #[error("missing or invalid field {0}")]
    Field(&'static str),
    /// `drvPath` or `outputs.out` is not a store path.
    #[error("{0}")]
    StorePath(#[from] crate::observe::NotAStorePath),
}

#[derive(Deserialize)]
struct RawLine {
    attr: Option<String>,
    #[serde(default, rename = "attrPath")]
    attr_path: Option<Vec<String>>,
    #[serde(rename = "drvPath")]
    drv_path: Option<String>,
    outputs: Option<RawOutputs>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct RawOutputs {
    out: Option<String>,
}

/// Parses one line of `nix-eval-jobs` output. A line with an `error` string is an
/// [`EvalOutcome::Error`] whatever else it carries; otherwise `drvPath` and `outputs.out`
/// are required. Unknown fields are ignored.
///
/// # Errors
///
/// [`EvalLineError`] for a line that is not JSON, lacks `attr`, or is a success without a
/// valid `drvPath` and `outputs.out`.
pub fn parse_eval_line(line: &str) -> Result<EvalOutcome, EvalLineError> {
    let raw: RawLine =
        serde_json::from_str(line.trim()).map_err(|e| EvalLineError::Malformed(e.to_string()))?;
    let attr = raw.attr.ok_or(EvalLineError::Field("attr"))?;
    let attr_path = raw.attr_path.unwrap_or_default();
    if let Some(error) = raw.error {
        return Ok(EvalOutcome::Error(EvalFailure {
            attr,
            attr_path,
            error_tail: last_lines(&error, ERROR_TAIL_LINES),
        }));
    }
    let drv_path = StorePath::new(raw.drv_path.ok_or(EvalLineError::Field("drvPath"))?)?;
    let out_path = StorePath::new(
        raw.outputs
            .and_then(|o| o.out)
            .ok_or(EvalLineError::Field("outputs.out"))?,
    )?;
    Ok(EvalOutcome::Success(EvalSuccess {
        attr,
        attr_path,
        drv_path,
        out_path,
    }))
}

// ---------------------------------------------------------------------------------------
// The cache key
// ---------------------------------------------------------------------------------------

/// The cache format version. Part of every key and every entry, so bumping it orphans every
/// old entry rather than misreading one. Bump it whenever what is cached, or how it is
/// evaluated, changes meaning.
pub const CACHE_FORMAT_VERSION: u32 = 1;

/// The working tree's state, as the effects layer read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tree<'a> {
    /// `git status --porcelain` is empty.
    Clean,
    /// Anything else.
    Dirty {
        /// The bytes of `git diff HEAD` (staged and unstaged, including newly added files).
        diff: &'a [u8],
        /// The untracked files (`git ls-files --others --exclude-standard`), in any order.
        untracked: &'a [String],
    },
}

/// Everything an evaluation's result depends on, as far as the cache is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheKeyInputs<'a> {
    /// `git rev-parse HEAD`. Surrounding whitespace (the trailing newline) is ignored.
    pub head: &'a str,
    /// The bytes of `flake.lock`, or `None` when there is none.
    pub flake_lock: Option<&'a [u8]>,
    /// The tree's state.
    pub tree: Tree<'a>,
}

/// A cache key: `v<version>-<64 hex digits>`. Safe as a file name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CacheKey(String);

impl CacheKey {
    /// The key as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Feeds one length-prefixed, tagged field, so that no two different input sets can produce
/// the same byte stream (`head = "ab", lock = "c"` versus `head = "a", lock = "bc"`).
fn field(h: &mut Sha256, tag: u8, bytes: &[u8]) {
    h.update([tag]);
    h.update((bytes.len() as u64).to_be_bytes());
    h.update(bytes);
}

fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Combines the inputs into a key.
///
/// The encoding is SHA-256 over a domain-separation tag and then tagged, length-prefixed
/// fields: the format version, `HEAD`, the hash of `flake.lock` (or an "absent" tag), and the
/// tree state. A dirty tree contributes a "dirty" tag, the hash of the diff and the sorted,
/// de-duplicated untracked list; a clean tree contributes only a "clean" tag. So a dirty tree
/// and a clean tree at the same commit never share a key, even when the diff is empty.
#[must_use]
pub fn cache_key(inputs: &CacheKeyInputs<'_>) -> CacheKey {
    let mut h = Sha256::new();
    h.update(b"auspex eval cache key\0");
    field(&mut h, b'v', &CACHE_FORMAT_VERSION.to_be_bytes());
    field(&mut h, b'h', inputs.head.trim().as_bytes());
    match inputs.flake_lock {
        Some(lock) => field(&mut h, b'l', &Sha256::digest(lock)),
        None => field(&mut h, b'L', &[]),
    }
    match inputs.tree {
        Tree::Clean => field(&mut h, b'c', &[]),
        Tree::Dirty { diff, untracked } => {
            field(&mut h, b'd', &Sha256::digest(diff));
            let mut files: Vec<&str> = untracked.iter().map(String::as_str).collect();
            files.sort_unstable();
            files.dedup();
            field(&mut h, b'n', &(files.len() as u64).to_be_bytes());
            for f in files {
                field(&mut h, b'u', f.as_bytes());
            }
        }
    }
    CacheKey(format!("v{CACHE_FORMAT_VERSION}-{}", hex(&h.finalize())))
}

// ---------------------------------------------------------------------------------------
// The cache entry
// ---------------------------------------------------------------------------------------

/// One cached evaluation, as written to `$XDG_CACHE_HOME/auspex/`.
///
/// Only successes are cached. An eval error can be a transient fetch failure, and a cached
/// one would never be retried until the tree changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    /// [`CACHE_FORMAT_VERSION`] when written.
    pub version: u32,
    /// The key it was written under. Checked on read, so a file renamed or copied to the
    /// wrong key is a miss, not a wrong answer.
    pub key: CacheKey,
    /// The machine it belongs to. Checked on read for the same reason.
    pub machine: String,
    /// The result.
    pub result: EvalSuccess,
}

/// Why a cache file was not used. Every case is a miss: the caller re-evaluates.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CacheMiss {
    /// Not a cache entry (corrupt, truncated, or from a future format).
    #[error("unreadable cache entry: {0}")]
    Unreadable(String),
    /// Written by a different format version.
    #[error("cache entry version {0}, expected {CACHE_FORMAT_VERSION}")]
    Version(u32),
    /// Written under another key or for another machine.
    #[error("cache entry is for another key or machine")]
    Mismatch,
}

impl CacheEntry {
    /// An entry for `machine` under `key`.
    #[must_use]
    pub const fn new(key: CacheKey, machine: String, result: EvalSuccess) -> Self {
        Self {
            version: CACHE_FORMAT_VERSION,
            key,
            machine,
            result,
        }
    }

    /// The bytes to write.
    ///
    /// # Panics
    ///
    /// Never in practice: every field serialises infallibly.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("CacheEntry serialises")
    }

    /// Reads a cache file and returns its result if it is a current entry for exactly this
    /// key and machine.
    ///
    /// # Errors
    ///
    /// A [`CacheMiss`] saying why not.
    pub fn read(bytes: &[u8], key: &CacheKey, machine: &str) -> Result<EvalSuccess, CacheMiss> {
        #[derive(Deserialize)]
        struct Version {
            version: u32,
        }
        let v: Version =
            serde_json::from_slice(bytes).map_err(|e| CacheMiss::Unreadable(e.to_string()))?;
        if v.version != CACHE_FORMAT_VERSION {
            return Err(CacheMiss::Version(v.version));
        }
        let e: Self =
            serde_json::from_slice(bytes).map_err(|e| CacheMiss::Unreadable(e.to_string()))?;
        if &e.key != key || e.machine != machine {
            return Err(CacheMiss::Mismatch);
        }
        Ok(e.result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DRV: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-nixos-system-terra.drv";
    const OUT: &str = "/nix/store/zyxwvsrqpnmlkjihgfdcba9876543210-nixos-system-terra";

    #[test]
    fn success_line() {
        let line = format!(
            r#"{{"attr":"terra","attrPath":["terra"],"drvPath":"{DRV}","name":"nixos-system-terra","outputs":{{"out":"{OUT}"}},"system":"x86_64-linux"}}"#
        );
        let EvalOutcome::Success(s) = parse_eval_line(&line).unwrap() else {
            panic!()
        };
        assert_eq!(s.attr, "terra");
        assert_eq!(s.attr_path, ["terra"]);
        assert_eq!(s.drv_path.as_str(), DRV);
        assert_eq!(s.out_path.as_str(), OUT);
    }

    #[test]
    fn error_line_keeps_tail() {
        let mut err = String::new();
        for i in 0..20 {
            err += &["… while evaluating ", &i.to_string(), "\n"].concat();
        }
        err.push_str("\u{1b}[31;1merror:\u{1b}[0m attribute 'foo' missing\n\n");
        let line = serde_json::json!({"attr": "mars", "error": err}).to_string();
        let EvalOutcome::Error(e) = parse_eval_line(&line).unwrap() else {
            panic!()
        };
        assert_eq!(e.attr, "mars");
        assert_eq!(e.error_tail.len(), ERROR_TAIL_LINES);
        assert_eq!(
            e.error_tail.last().unwrap(),
            "error: attribute 'foo' missing"
        );
        assert_eq!(e.error_tail[0], "… while evaluating 13");
        assert_eq!(e.summary(), Some("error: attribute 'foo' missing"));
        let located = EvalFailure {
            attr: "a".to_owned(),
            attr_path: vec![],
            error_tail: vec!["error: boom".to_owned(), "at /x.nix:1:1:".to_owned()],
        };
        assert_eq!(located.summary(), Some("error: boom"));
    }

    #[test]
    fn bad_lines() {
        assert!(matches!(
            parse_eval_line("{\"attr\": "),
            Err(EvalLineError::Malformed(_))
        ));
        assert!(matches!(
            parse_eval_line("[]"),
            Err(EvalLineError::Malformed(_))
        ));
        assert!(matches!(
            parse_eval_line("warning: Git tree is dirty"),
            Err(EvalLineError::Malformed(_))
        ));
        assert_eq!(
            parse_eval_line(r#"{"error":"x"}"#),
            Err(EvalLineError::Field("attr"))
        );
        assert_eq!(
            parse_eval_line(r#"{"attr":"a"}"#),
            Err(EvalLineError::Field("drvPath"))
        );
        assert_eq!(
            parse_eval_line(&format!(
                r#"{{"attr":"a","drvPath":"{DRV}","outputs":{{}}}}"#
            )),
            Err(EvalLineError::Field("outputs.out"))
        );
        assert!(matches!(
            parse_eval_line(r#"{"attr":"a","drvPath":"/tmp/x.drv","outputs":{"out":"/tmp/x"}}"#),
            Err(EvalLineError::StorePath(_))
        ));
    }

    const HEAD: &str = "273b8ad0000000000000000000000000000000ff";

    fn key(head: &str, lock: Option<&[u8]>, tree: Tree<'_>) -> CacheKey {
        cache_key(&CacheKeyInputs {
            head,
            flake_lock: lock,
            tree,
        })
    }

    #[test]
    fn cache_key_shape_and_stability() {
        let k = key(HEAD, Some(b"{}"), Tree::Clean);
        assert!(k.as_str().starts_with("v1-"));
        assert_eq!(k.as_str().len(), 3 + 64);
        assert!(k.as_str()[3..].bytes().all(|b| b.is_ascii_hexdigit()));
        // Deterministic, and the trailing newline from git is ignored.
        assert_eq!(k, key(&format!("{HEAD}\n"), Some(b"{}"), Tree::Clean));
    }

    #[test]
    fn cache_key_distinguishes_every_input() {
        let lock: &[u8] = b"{\"nodes\":{}}";
        let untracked = vec!["new.nix".to_owned()];
        let none: Vec<String> = vec![];
        let clean = key(HEAD, Some(lock), Tree::Clean);
        let dirty_empty = key(
            HEAD,
            Some(lock),
            Tree::Dirty {
                diff: b"",
                untracked: &none,
            },
        );
        let dirty = key(
            HEAD,
            Some(lock),
            Tree::Dirty {
                diff: b"diff --git a/x b/x\n",
                untracked: &untracked,
            },
        );
        let all = [
            clean,
            dirty_empty,
            dirty,
            // changed HEAD
            key("0000000", Some(lock), Tree::Clean),
            // changed lock, and no lock
            key(HEAD, Some(b"{\"nodes\":{\"a\":1}}"), Tree::Clean),
            key(HEAD, None, Tree::Clean),
            key(HEAD, Some(b""), Tree::Clean),
            // changed diff
            key(
                HEAD,
                Some(lock),
                Tree::Dirty {
                    diff: b"diff --git a/y b/y\n",
                    untracked: &untracked,
                },
            ),
            // changed untracked list
            key(
                HEAD,
                Some(lock),
                Tree::Dirty {
                    diff: b"diff --git a/x b/x\n",
                    untracked: &["other.nix".to_owned()],
                },
            ),
            key(
                HEAD,
                Some(lock),
                Tree::Dirty {
                    diff: b"diff --git a/x b/x\n",
                    untracked: &none,
                },
            ),
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "inputs {i} and {j} share a key");
                }
            }
        }
    }

    #[test]
    fn cache_key_framing_is_unambiguous() {
        // Moving bytes between adjacent untracked names must change the key.
        let a = ["ab".to_owned(), "c".to_owned()];
        let b = ["a".to_owned(), "bc".to_owned()];
        let k = |u: &[String]| {
            key(
                HEAD,
                None,
                Tree::Dirty {
                    diff: b"",
                    untracked: u,
                },
            )
        };
        assert_ne!(k(&a), k(&b));
        // Order and duplicates of the untracked list do not matter.
        let c = ["c".to_owned(), "ab".to_owned(), "c".to_owned()];
        assert_eq!(k(&a), k(&c));
    }

    fn success() -> EvalSuccess {
        EvalSuccess {
            attr: "terra".to_owned(),
            attr_path: vec!["terra".to_owned()],
            drv_path: StorePath::new(DRV).unwrap(),
            out_path: StorePath::new(OUT).unwrap(),
        }
    }

    #[test]
    fn cache_entry_round_trip_and_misses() {
        let k = key(HEAD, None, Tree::Clean);
        let other = key("0", None, Tree::Clean);
        let bytes = CacheEntry::new(k.clone(), "terra".to_owned(), success()).to_json();
        assert_eq!(
            CacheEntry::read(bytes.as_bytes(), &k, "terra"),
            Ok(success())
        );
        assert_eq!(
            CacheEntry::read(bytes.as_bytes(), &other, "terra"),
            Err(CacheMiss::Mismatch)
        );
        assert_eq!(
            CacheEntry::read(bytes.as_bytes(), &k, "luna"),
            Err(CacheMiss::Mismatch)
        );
        let future = bytes.replacen("\"version\":1", "\"version\":2", 1);
        assert_eq!(
            CacheEntry::read(future.as_bytes(), &k, "terra"),
            Err(CacheMiss::Version(2))
        );
        assert!(matches!(
            CacheEntry::read(&bytes.as_bytes()[..bytes.len() / 2], &k, "terra"),
            Err(CacheMiss::Unreadable(_))
        ));
        let tampered = bytes.replace(OUT, "/tmp/evil");
        assert!(matches!(
            CacheEntry::read(tampered.as_bytes(), &k, "terra"),
            Err(CacheMiss::Unreadable(_))
        ));
    }
}
