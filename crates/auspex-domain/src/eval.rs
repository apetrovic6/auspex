//! Evaluation results and the eval cache.
//!
//! `nix-eval-jobs` prints one JSON line per machine as each finishes; [`parse_eval_line`]
//! reads one. Evaluating is slow and no faster warm, so results are cached under a
//! [`CacheKey`] that [`cache_key`] computes from inputs the effects layer reads (`git
//! rev-parse HEAD`, `flake.lock`, `git diff HEAD`, the untracked file list).

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::observe::StorePath;
use crate::text::{sanitize_line, sanitized_tail};

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
    /// `attr` from nix-eval-jobs, for display only. Sanitised by [`parse_eval_line`], but
    /// not guaranteed safe for hand-built, deserialised or cache-file values. Nix quotes
    /// non-bare identifiers (`"\"1odd\""`), so never compare this with a machine name.
    pub attr: String,
    /// `attrPath`. From [`parse_eval_line`], exactly one element: the machine's name, which
    /// [`EvalOutcome::machine`] returns.
    pub attr_path: Vec<String>,
    /// The toplevel's `.drv`.
    pub drv_path: StorePath,
    /// The toplevel's output path (`outputs.out`): what `/run/current-system` will point to
    /// once deployed.
    pub out_path: StorePath,
    /// The nix system the toplevel is built for (`x86_64-linux`), from nix-eval-jobs'
    /// `system` field. What decides whether this machine's toplevel may be built here: auspex
    /// never builds another system's derivations. `None` only when the line carried none.
    /// Not part of the `--json` report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
}

/// A machine's toplevel failed to evaluate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalFailure {
    /// `attr` from nix-eval-jobs, for display only. Sanitised by [`parse_eval_line`] and
    /// [`EvalFailure::no_result`], but not guaranteed safe for hand-built or deserialised
    /// values. Nix quotes non-bare identifiers (`"\"1odd\""`); never use it for identity.
    pub attr: String,
    /// `attrPath`. From [`parse_eval_line`], exactly one element: the machine's name, which
    /// [`EvalOutcome::machine`] returns.
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
    /// Marks a machine for which the evaluator exited without a usable result line.
    ///
    /// The caller must supply this error for every such machine after draining the
    /// evaluator's output. `error_tail` is always the single line
    /// `"error: evaluator exited without a result for this machine"`.
    #[must_use]
    pub fn no_result(machine: &str) -> Self {
        Self {
            attr: sanitize_line(machine),
            attr_path: vec![machine.to_owned()],
            error_tail: vec![
                "error: evaluator exited without a result for this machine".to_owned(),
            ],
        }
    }

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

impl EvalSuccess {
    /// The machine this result is for: the one element of `attr_path`. See
    /// [`EvalOutcome::machine`].
    #[must_use]
    pub fn machine(&self) -> Option<&str> {
        single(&self.attr_path)
    }
}

fn single(path: &[String]) -> Option<&str> {
    match path {
        [m] => Some(m),
        _ => None,
    }
}

impl EvalOutcome {
    /// The `attr` either way, for display only; see [`EvalOutcome::machine`].
    #[must_use]
    pub fn attr(&self) -> &str {
        match self {
            Self::Success(s) => &s.attr,
            Self::Error(e) => &e.attr,
        }
    }

    /// The machine this line is about: the one element of `attrPath`. `None` only for a
    /// value not built by [`parse_eval_line`] whose `attr_path` is not a single element.
    #[must_use]
    pub fn machine(&self) -> Option<&str> {
        match self {
            Self::Success(s) => s.machine(),
            Self::Error(e) => single(&e.attr_path),
        }
    }
}

/// A line that is not a `nix-eval-jobs` result.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvalLineError {
    /// Not a JSON object.
    #[error("not a JSON object: {0}")]
    Malformed(String),
    /// A required field is missing or has the wrong type. Since a line that names its machine
    /// always comes back as an [`EvalOutcome`], this is only ever `Field("attrPath")`.
    #[error("missing or invalid field {0}")]
    Field(&'static str),
    /// `drvPath` or `outputs.out` is not a store path. No longer returned by
    /// [`parse_eval_line`], which reports that as an [`EvalOutcome::Error`] for the line's
    /// `attr`; kept so code matching on it still compiles.
    #[error("{0}")]
    StorePath(#[from] crate::observe::NotAStorePath),
}

/// Why a line that names its `attr` is not a usable success, as the text of its
/// [`EvalFailure`].
fn malformed(attr: &str, what: &str) -> String {
    format!("error: nix-eval-jobs printed a malformed result for {attr}: {what}")
}

fn success_fields(obj: &serde_json::Map<String, Value>) -> Result<(StorePath, StorePath), String> {
    let path = |v: Option<&Value>, name: &str| match v {
        None | Some(Value::Null) => Err(format!("{name} is missing or null")),
        Some(Value::String(s)) => StorePath::new(s.as_str()).map_err(|e| format!("{name}: {e}")),
        Some(_) => Err(format!("{name} is not a string")),
    };
    let drv = path(obj.get("drvPath"), "drvPath")?;
    let out = match obj.get("outputs") {
        Some(Value::Object(o)) => path(o.get("out"), "outputs.out")?,
        _ => return Err("outputs is missing or not an object".to_owned()),
    };
    Ok((drv, out))
}

/// Parses one line of `nix-eval-jobs` output.
///
/// Requires a flat attribute set mapping machine names to toplevel derivations, e.g.
/// `nix-eval-jobs --flake .#nixosConfigurations --select
/// 'cs: builtins.mapAttrs (_: c: c.config.system.build.toplevel) cs'`.
/// For a single machine, still wrap its toplevel in a machine-named attribute set:
/// never use that machine's toplevel as the root, which produces an empty `attrPath`.
/// Nested attribute sets are not supported either. Once the evaluator exits and its
/// output is drained, the caller must turn every machine with no usable line into an
/// [`EvalOutcome::Error`] using [`EvalFailure::no_result`], rather than leave it waiting.
///
/// The line's machine is the one element of its `attrPath` (see [`EvalOutcome::machine`]);
/// `attr` is kept for display only, and defaults to the machine when absent. Once a line
/// names its machine, it always comes back as an [`EvalOutcome`] for it, so the machine's row
/// leaves "evaluating" whatever else is wrong with it:
///
/// - An `error` field that is present and not null makes it an [`EvalOutcome::Error`],
///   whatever else the line carries, even ill-typed success fields. An `error` that is not
///   a string is still an error, with a note saying so.
/// - Otherwise `drvPath` and `outputs.out` must be store paths. If either is missing, null
///   (as `outputs.out` is for a content-addressed derivation), ill-typed or outside the
///   store, the line is an [`EvalOutcome::Error`] saying which.
///
/// Unknown fields are ignored.
///
/// # Errors
///
/// [`EvalLineError::Malformed`] for a line that is not a JSON object, and
/// `EvalLineError::Field("attrPath")` for one whose `attrPath` is not a list of exactly
/// one string: neither can be attributed to a machine.
pub fn parse_eval_line(line: &str) -> Result<EvalOutcome, EvalLineError> {
    let value: Value =
        serde_json::from_str(line.trim()).map_err(|e| EvalLineError::Malformed(e.to_string()))?;
    let Value::Object(obj) = value else {
        return Err(EvalLineError::Malformed("not an object".to_owned()));
    };
    // The machine is `attrPath`'s one element, never `attr`: nix-eval-jobs quotes an
    // attribute name that is not a bare Nix identifier (`attr` is `"\"1odd\""` for machine
    // `1odd`, with `attrPath` `["1odd"]`), so `attr` is only for display.
    let machine = match obj.get("attrPath") {
        Some(Value::Array(a)) => match a.as_slice() {
            [Value::String(m)] => m.clone(),
            _ => return Err(EvalLineError::Field("attrPath")),
        },
        _ => return Err(EvalLineError::Field("attrPath")),
    };
    let attr = sanitize_line(match obj.get("attr") {
        Some(Value::String(a)) => a,
        _ => &machine,
    });
    let attr_path = vec![machine.clone()];
    let failure = |text: &str| {
        EvalOutcome::Error(EvalFailure {
            attr: attr.clone(),
            attr_path: attr_path.clone(),
            error_tail: sanitized_tail(text, ERROR_TAIL_LINES),
        })
    };
    match obj.get("error") {
        None | Some(Value::Null) => {}
        Some(Value::String(error)) => return Ok(failure(error)),
        Some(_) => return Ok(failure(&malformed(&machine, "error is not a string"))),
    }
    match success_fields(&obj) {
        Ok((drv_path, out_path)) => Ok(EvalOutcome::Success(EvalSuccess {
            attr,
            attr_path,
            drv_path,
            out_path,
            system: match obj.get("system") {
                Some(Value::String(s)) if !s.is_empty() => Some(sanitize_line(s)),
                _ => None,
            },
        })),
        Err(what) => Ok(failure(&malformed(&machine, &what))),
    }
}

// ---------------------------------------------------------------------------------------
// The cache key
// ---------------------------------------------------------------------------------------

/// The cache format version. Part of every key and every entry, so bumping it orphans every
/// old entry rather than misreading one. Bump it whenever what is cached, or how it is
/// evaluated, changes meaning.
///
/// 2: an entry carries the toplevel's `system`, which `b` (build) needs to refuse another
/// system's derivation. A version-1 entry reads as having none and would make every build
/// refuse until the flake changed; orphaning it makes the next scan evaluate afresh.
pub const CACHE_FORMAT_VERSION: u32 = 2;

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
    /// The machine it belongs to. Checked on read for the same reason, and so is the
    /// result's own machine ([`EvalSuccess::machine`], from `attr_path`): a result for
    /// another machine is another machine's closure.
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
    /// Written under another key or for another machine, or holding a result whose machine
    /// (`attr_path`) is not this one.
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
        if &e.key != key || e.machine != machine || e.result.machine() != Some(machine) {
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
    fn display_attr_is_sanitised_without_changing_machine_identity() {
        let machine = "te\u{1b}]0;pwn\u{7}rra\u{202e}";
        for fields in [
            serde_json::json!({"drvPath": DRV, "outputs": {"out": OUT}}),
            serde_json::json!({"error": "error: boom"}),
        ] {
            for attr in [Some(format!("\"{machine}\"")), None] {
                let mut line = fields.clone();
                line["attrPath"] = serde_json::json!([machine]);
                if let Some(attr) = &attr {
                    line["attr"] = serde_json::json!(attr);
                }
                let result = parse_eval_line(&line.to_string()).unwrap();
                assert_eq!(result.machine(), Some(machine));
                assert_eq!(
                    result.attr(),
                    if attr.is_some() { "\"terra\"" } else { "terra" }
                );
            }
        }
    }

    #[test]
    fn error_line_keeps_tail() {
        let mut err = String::new();
        for i in 0..20 {
            err += &["… while evaluating ", &i.to_string(), "\n"].concat();
        }
        err.push_str("\u{1b}[31;1merror:\u{1b}[0m attribute 'foo' missing\n\n");
        let line =
            serde_json::json!({"attr": "mars", "attrPath": ["mars"], "error": err}).to_string();
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
        // No machine: `attrPath` missing, or not exactly one string. `attr` alone does not
        // name one.
        for no_machine in [
            r#"{"error":"x"}"#,
            r#"{"attr":"terra","error":"x"}"#,
            r#"{"attr":"terra","attrPath":null,"error":"x"}"#,
            r#"{"attr":"terra","attrPath":"terra","error":"x"}"#,
            r#"{"attr":"terra","attrPath":[],"error":"x"}"#,
            r#"{"attr":"a.b","attrPath":["a","b"],"error":"x"}"#,
            r#"{"attr":"terra","attrPath":[7],"error":"x"}"#,
        ] {
            assert_eq!(
                parse_eval_line(no_machine),
                Err(EvalLineError::Field("attrPath")),
                "{no_machine}"
            );
        }
    }

    fn attributed_error(line: &str) -> EvalFailure {
        match parse_eval_line(line) {
            Ok(EvalOutcome::Error(e)) => e,
            other => panic!("{line}: expected an attributed error, got {other:?}"),
        }
    }

    /// F8: a line that names its attr always comes back for that attr, so the row leaves
    /// "evaluating" and says what was wrong.
    #[test]
    fn malformed_lines_with_an_attr_are_errors_for_it() {
        let cases = [
            (
                r#"{"attr":"terra","attrPath":["terra"]}"#.to_owned(),
                "drvPath is missing or null",
            ),
            (
                format!(r#"{{"attr":"terra","attrPath":["terra"],"drvPath":"{DRV}","outputs":{{}}}}"#),
                "outputs.out is missing or null",
            ),
            (
                format!(r#"{{"attr":"terra","attrPath":["terra"],"drvPath":"{DRV}","outputs":{{"out":null}}}}"#),
                "outputs.out is missing or null",
            ),
            (
                format!(r#"{{"attr":"terra","attrPath":["terra"],"drvPath":"{DRV}","outputs":false}}"#),
                "outputs is missing or not an object",
            ),
            (
                format!(r#"{{"attr":"terra","attrPath":["terra"],"drvPath":"{DRV}","outputs":{{"out":7}}}}"#),
                "outputs.out is not a string",
            ),
            (
                r#"{"attr":"terra","attrPath":["terra"],"drvPath":"/tmp/x.drv","outputs":{"out":"/tmp/x"}}"#.to_owned(),
                "drvPath: not a store path",
            ),
            (
                r#"{"attr":"terra","attrPath":["terra"],"error":42}"#.to_owned(),
                "error is not a string",
            ),
        ];
        for (line, what) in cases {
            let e = attributed_error(&line);
            assert_eq!(e.attr, "terra", "{line}");
            let summary = e.summary().unwrap();
            assert!(
                summary.starts_with("error: nix-eval-jobs printed a malformed result for terra")
                    && summary.contains(what),
                "{line}: {summary}"
            );
        }
    }

    /// Correctness 4 and M82: an `error` wins over success fields, well typed or not.
    #[test]
    fn error_wins_over_success_fields() {
        let e = attributed_error(
            r#"{"attr":"terra","attrPath":["terra"],"error":"error: boom","drvPath":123,"outputs":false}"#,
        );
        assert_eq!(e.error_tail, ["error: boom"]);
        let e = attributed_error(&format!(
            r#"{{"attr":"terra","attrPath":["terra"],"attrPath":["terra"],"error":"error: boom","drvPath":"{DRV}","outputs":{{"out":"{OUT}"}}}}"#
        ));
        assert_eq!(e.error_tail, ["error: boom"]);
        assert_eq!(e.attr_path, ["terra"]);
        // A null error is no error.
        let line = format!(
            r#"{{"attr":"terra","attrPath":["terra"],"error":null,"drvPath":"{DRV}","outputs":{{"out":"{OUT}"}}}}"#
        );
        assert!(matches!(
            parse_eval_line(&line),
            Ok(EvalOutcome::Success(_))
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
        assert!(k.as_str().starts_with("v2-"));
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
        // M71: without the length prefix, these two lists feed the same bytes ("uabucud")
        // to the hash, because the tag byte `u` can also be a name's content.
        let a = ["ab".to_owned(), "cud".to_owned()];
        let b = ["abuc".to_owned(), "d".to_owned()];
        assert_ne!(k(&a), k(&b));
    }

    fn success() -> EvalSuccess {
        EvalSuccess {
            attr: "terra".to_owned(),
            attr_path: vec!["terra".to_owned()],
            drv_path: StorePath::new(DRV).unwrap(),
            out_path: StorePath::new(OUT).unwrap(),
            system: Some("x86_64-linux".to_owned()),
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
        // F9: the entry says terra but its result is luna's, and the other way round. Each
        // is caught by its own check.
        let mut luna = success();
        luna.attr = "luna".to_owned();
        luna.attr_path = vec!["luna".to_owned()];
        let swapped = CacheEntry::new(k.clone(), "terra".to_owned(), luna).to_json();
        assert_eq!(
            CacheEntry::read(swapped.as_bytes(), &k, "terra"),
            Err(CacheMiss::Mismatch)
        );
        let relabelled = CacheEntry::new(k.clone(), "luna".to_owned(), success()).to_json();
        assert_eq!(
            CacheEntry::read(relabelled.as_bytes(), &k, "terra"),
            Err(CacheMiss::Mismatch)
        );
        let mut nested = success();
        nested.attr_path.push("nested".to_owned());
        let nested = CacheEntry::new(k.clone(), "terra".to_owned(), nested).to_json();
        assert_eq!(
            CacheEntry::read(nested.as_bytes(), &k, "terra"),
            Err(CacheMiss::Mismatch)
        );
        let future = bytes.replacen("\"version\":2", "\"version\":3", 1);
        assert_eq!(
            CacheEntry::read(future.as_bytes(), &k, "terra"),
            Err(CacheMiss::Version(3))
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
        // N3: the same strict rule when reading a cache entry, so a cache file cannot put a
        // bidi override into `pending {name}`.
        let evil = bytes.replace(OUT, &format!("{OUT}\u{202e}x"));
        assert_ne!(evil, bytes);
        assert!(matches!(
            CacheEntry::read(evil.as_bytes(), &k, "terra"),
            Err(CacheMiss::Unreadable(_))
        ));
    }

    /// The machine comes from `attrPath`, not `attr`: nix-eval-jobs quotes a name that is not
    /// a bare Nix identifier, and such a machine must still round-trip through the cache.
    #[test]
    fn machine_comes_from_attr_path() {
        let line = format!(
            r#"{{"attr":"\"1odd\"","attrPath":["1odd"],"drvPath":"{DRV}","outputs":{{"out":"{OUT}"}}}}"#
        );
        let outcome = parse_eval_line(&line).unwrap();
        assert_eq!(outcome.machine(), Some("1odd"));
        assert_eq!(outcome.attr(), "\"1odd\"");
        let EvalOutcome::Success(s) = outcome else {
            panic!()
        };
        let k = key(HEAD, None, Tree::Clean);
        let bytes = CacheEntry::new(k.clone(), "1odd".to_owned(), s.clone()).to_json();
        assert_eq!(
            CacheEntry::read(bytes.as_bytes(), &k, "1odd"),
            Ok(s.clone())
        );
        assert_eq!(
            CacheEntry::read(bytes.as_bytes(), &k, "\"1odd\""),
            Err(CacheMiss::Mismatch)
        );
        // An error line is attributed the same way, and `attr` defaults to the machine.
        let e = parse_eval_line(r#"{"attrPath":["1odd"],"error":"error: boom"}"#).unwrap();
        assert_eq!(e.machine(), Some("1odd"));
        assert_eq!(e.attr(), "1odd");
        // A hand-built value with no single attr_path names no machine, and is a miss.
        let mut none = s;
        none.attr_path = vec![];
        assert_eq!(none.machine(), None);
        let bytes = CacheEntry::new(k.clone(), "1odd".to_owned(), none).to_json();
        assert_eq!(
            CacheEntry::read(bytes.as_bytes(), &k, "1odd"),
            Err(CacheMiss::Mismatch)
        );
    }
}
