//! The CI badge for the fleet flake's current commit: what the forge's combined commit status
//! means, which repository to ask it about, and when not to ask at all.
//!
//! Pure. The adapters obtain the inputs (`git rev-parse`, `git status`, the `origin` remote,
//! the forge's JSON), the core sequences them, and this module decides what each one means.
//!
//! The states, in the words of `--json`'s `fleet_ci.state`:
//!
//! | state | when |
//! |---|---|
//! | `success`, `failure`, `pending` | the forge's combined status for `HEAD` (`error` and `warning` count as `failure`: a commit that is not green must not look like one) |
//! | `none` | the forge has no status for that commit |
//! | `not_pushed` | `HEAD` is ahead of its upstream, so the forge cannot have a status for it; nothing is asked |
//! | `unknown` | the answer could not be had; `reason` says why |
//! | `off` | `[forge]` is not configured; nothing is read or asked |
//!
//! A dirty working tree is **not** a CI state: `HEAD`'s status is shown as it is, and the plan of
//! an update already warns about uncommitted changes.
//!
//! **Nothing here quotes the remote or the forge's text.** A remote can carry a password
//! (`https://user:token@host/…`) and a forge's answer is text from the network, so every
//! reason below is built from fixed words and from values this module validated itself.

use serde::{Deserialize, Serialize};

use crate::preflight::Upstream;
use crate::text::sanitize_line;

/// What every unknown that a configured `forge.repo` would fix ends with, so a shell can say
/// "unknown: set forge.repo" ([`FleetCi::wants_forge_repo`]) without reading prose.
pub const SET_FORGE_REPO: &str = "set forge.repo";

/// The longest `url` kept. A longer one is dropped, not cut: half a link is a wrong link.
pub const MAX_URL_BYTES: usize = 2048;

/// The state of the fleet's current commit; see the module's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiState {
    /// Every status the forge holds for the commit passed.
    Success,
    /// A status failed or errored (or warned).
    Failure,
    /// A status is still running.
    Pending,
    /// The forge holds no status for the commit.
    None,
    /// `HEAD` is ahead of its upstream.
    NotPushed,
    /// It could not be found out.
    Unknown,
    /// `[forge]` is not configured.
    Off,
}

impl CiState {
    /// The spelling `--json` uses.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Pending => "pending",
            Self::None => "none",
            Self::NotPushed => "not_pushed",
            Self::Unknown => "unknown",
            Self::Off => "off",
        }
    }
}

/// What the forge said about a commit: one of the four states it can have, and a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiStatus {
    /// [`CiState::Success`], [`CiState::Failure`], [`CiState::Pending`] or [`CiState::None`].
    pub state: CiState,
    /// A link to the check (`target_url` of a status), when the forge gave a usable one.
    pub url: Option<String>,
}

/// Forgejo's combined state, as a [`CiState`].
///
/// `statuses` is how many statuses the combined answer holds: with none, the forge's `state` is
/// empty (Forgejo) or `pending` (older Gitea), and either way there is nothing to show, so it is
/// [`CiState::None`]. Any other state with no status behind it contradicts itself, and is an
/// error rather than a guess either way. `warning` is `failure`: Forgejo ranks it below
/// `pending` and above `failure` in severity, but it is not a pass.
///
/// # Errors
///
/// A state this does not know (a newer forge): it is said, not guessed.
pub fn forge_state(raw: &str, statuses: usize) -> Result<CiState, String> {
    if statuses == 0 {
        return match raw {
            "" | "pending" => Ok(CiState::None),
            _ => Err("the forge's combined status has a state but no status behind it".to_owned()),
        };
    }
    match raw {
        "success" => Ok(CiState::Success),
        "failure" | "error" | "warning" => Ok(CiState::Failure),
        "pending" => Ok(CiState::Pending),
        _ => Err("the forge's combined status has a state auspex does not know".to_owned()),
    }
}

/// A link from the forge, kept only when it is safe to print and to hand on: `http` or `https`,
/// no credentials, no control or format characters, no whitespace, at most [`MAX_URL_BYTES`].
#[must_use]
pub fn clean_url(raw: &str) -> Option<String> {
    let rest = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let ok = raw.len() <= MAX_URL_BYTES
        && !authority.is_empty()
        && !authority.contains('@')
        && sanitize_line(raw) == raw
        && !raw.chars().any(char::is_whitespace);
    ok.then(|| raw.to_owned())
}

/// The fleet's CI badge: what `--json`'s `fleet_ci` holds and the status bar draws.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetCi {
    /// `HEAD`'s commit id, or `null` when it was not read (`off`, or git failed).
    pub commit: Option<String>,
    /// What the forge says, or why it does not.
    pub state: CiState,
    /// A link to the check, or `null`.
    pub url: Option<String>,
    /// One line on why, for `unknown` and `not_pushed`; `null` otherwise. Never holds the token
    /// or a remote's credentials.
    pub reason: Option<String>,
}

impl Default for FleetCi {
    fn default() -> Self {
        Self::off()
    }
}

impl FleetCi {
    /// `[forge]` is not configured.
    #[must_use]
    pub const fn off() -> Self {
        Self {
            commit: None,
            state: CiState::Off,
            url: None,
            reason: None,
        }
    }

    /// It could not be found out. `reason` is sanitised here: it can be a tool's own text.
    #[must_use]
    pub fn unknown(commit: Option<&str>, reason: &str) -> Self {
        Self {
            commit: commit.map(str::to_owned),
            state: CiState::Unknown,
            url: None,
            reason: Some(sanitize_line(reason)),
        }
    }

    /// `HEAD` has commits its upstream lacks.
    #[must_use]
    pub fn not_pushed(commit: &str, upstream: &str, ahead: u64) -> Self {
        Self {
            commit: Some(commit.to_owned()),
            state: CiState::NotPushed,
            url: None,
            reason: Some(format!(
                "HEAD is {ahead} {} ahead of {upstream}, so the forge has no status for it",
                if ahead == 1 { "commit" } else { "commits" }
            )),
        }
    }

    /// The forge answered (or did not): its state for `commit`, or the failure as `unknown`.
    #[must_use]
    pub fn from_answer(commit: &str, answer: Result<CiStatus, String>) -> Self {
        match answer {
            Ok(status) => Self {
                commit: Some(commit.to_owned()),
                state: status.state,
                url: status.url.as_deref().and_then(clean_url),
                reason: None,
            },
            Err(why) => Self::unknown(Some(commit), &why),
        }
    }

    /// The first eight characters of the commit, as the status bar shows it.
    #[must_use]
    pub fn short_commit(&self) -> Option<&str> {
        self.commit.as_deref().map(|c| c.get(..8).unwrap_or(c))
    }

    /// An `unknown` that setting `forge.repo` would fix: the remote is not usable as it is.
    #[must_use]
    pub fn wants_forge_repo(&self) -> bool {
        self.state == CiState::Unknown
            && self
                .reason
                .as_deref()
                .is_some_and(|r| r.ends_with(SET_FORGE_REPO))
    }
}

/// Whether `s` is a full git object id: 40 hex digits (SHA-1) or 64 (SHA-256), lower case.
#[must_use]
pub fn is_commit_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A repository on a forge, as a remote names it: the host, and `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// The host, lower case, without port, brackets or trailing dot.
    pub host: String,
    /// `owner/name`, without `.git`.
    pub repo: String,
}

/// What reading `HEAD` found, for [`plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadFacts {
    /// `HEAD`'s commit id ([`is_commit_id`]).
    pub commit: String,
    /// Where `HEAD` stands against its upstream (the same `git status` the update's checks use).
    pub upstream: Upstream,
    /// The `origin` remote, parsed, or why it could not be: `None` when it was not read
    /// (`forge.repo` is configured, so the remote is not needed).
    pub origin: Option<Result<Remote, String>>,
}

/// What to do after `HEAD` was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Ask the forge about `commit` in `repo`.
    Ask {
        /// The commit id.
        commit: String,
        /// `owner/name`.
        repo: String,
    },
    /// There is nothing to ask: this is the badge.
    Settled(FleetCi),
}

/// Decides whether the forge is asked, and about which repository.
///
/// `forge_host` is the host of `forge.url`; `configured_repo` is `forge.repo`. In order:
///
/// 1. A `HEAD` ahead of its upstream is `not_pushed`, and nothing is asked: the forge cannot
///    have a status for a commit it does not have.
/// 2. The repository is `forge.repo` when set (no remote is read then). Otherwise it is the
///    `origin` remote's, **only when that remote is on `forge.url`'s host**: a remote on
///    another host (a mirror, a different forge) would send the question, and the token, to a
///    repository that is not that remote's. Not guessed: `unknown`, "set forge.repo".
#[must_use]
pub fn plan(head: &HeadFacts, forge_host: &str, configured_repo: Option<&str>) -> Plan {
    let commit = &head.commit;
    if !is_commit_id(commit) {
        return Plan::Settled(FleetCi::unknown(
            None,
            "git printed something that is not a commit id",
        ));
    }
    if let Upstream::Tracking { name, ahead, .. } = &head.upstream
        && *ahead > 0
    {
        return Plan::Settled(FleetCi::not_pushed(commit, name, *ahead));
    }
    let unknown = |why: &str| Plan::Settled(FleetCi::unknown(Some(commit), why));
    let repo = match (configured_repo, &head.origin) {
        (Some(repo), _) => match parse_repo(repo) {
            Ok(repo) => repo,
            Err(e) => return unknown(&format!("forge.repo: {e}")),
        },
        (None, Some(Ok(remote))) if same_host(&remote.host, forge_host) => remote.repo.clone(),
        (None, Some(Ok(_))) => {
            return unknown(&format!(
                "the origin remote is on another host than forge.url: {SET_FORGE_REPO}"
            ));
        }
        (None, Some(Err(e))) => return unknown(&format!("{e}: {SET_FORGE_REPO}")),
        (None, None) => {
            return unknown(&format!("no repository to ask about: {SET_FORGE_REPO}"));
        }
    };
    Plan::Ask {
        commit: commit.clone(),
        repo,
    }
}

fn same_host(a: &str, b: &str) -> bool {
    let norm = |h: &str| h.trim_end_matches('.').to_ascii_lowercase();
    norm(a) == norm(b)
}

/// One path segment of an `owner/name`: letters, digits, `.`, `_` and `-`, not empty, not `.`
/// or `..`, at most 100 bytes. What a forge allows is narrower; this is what cannot do harm in
/// a URL path.
fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `forge.repo`: exactly `owner/name`.
///
/// # Errors
///
/// One line naming what is wrong. The text given is not quoted.
pub fn parse_repo(s: &str) -> Result<String, String> {
    let mut parts = s.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(name), None) if segment_ok(owner) && segment_ok(name) => {
            Ok(format!("{owner}/{name}"))
        }
        _ => Err("must be owner/name (letters, digits, `.`, `_` and `-`)".to_owned()),
    }
}

const NOT_A_FORGE_REMOTE: &str = "the origin remote is not an ssh or https address of a repository";

/// Reads a git remote URL into its host and `owner/name`.
///
/// Forms: `ssh://[user@]host[:port]/owner/name[.git]`, `[user@]host:owner/name[.git]` (the
/// scp-like form, `git@host:owner/name.git`) and `https://[user[:password]@]host[:port]/owner/name[.git]`
/// (`http` is read too, only to compare its host). Anything else is refused: a local path,
/// `file://`, `git://`, a path that is not exactly two segments (a forge under a sub-path, which
/// cannot be told from a group).
///
/// **Credentials in the remote are dropped and never quoted**, by this function or in its errors.
///
/// # Errors
///
/// One line of fixed words: the remote is not one of these forms, or its path is not
/// `owner/name`.
pub fn parse_remote(text: &str) -> Result<Remote, String> {
    let text = text.trim();
    let (authority, path) = if let Some((scheme, rest)) = text.split_once("://") {
        if !matches!(scheme, "ssh" | "https" | "http") {
            return Err(NOT_A_FORGE_REMOTE.to_owned());
        }
        rest.split_once('/')
            .ok_or_else(|| NOT_A_FORGE_REMOTE.to_owned())?
    } else {
        // scp-like: `host:path`, where the part before the first `:` has no `/` (or it is a
        // local path with a colon in a directory name).
        let (before, path) = text
            .split_once(':')
            .ok_or_else(|| NOT_A_FORGE_REMOTE.to_owned())?;
        if before.contains('/') {
            return Err(NOT_A_FORGE_REMOTE.to_owned());
        }
        (before, path)
    };
    let host = host_of(authority).ok_or_else(|| NOT_A_FORGE_REMOTE.to_owned())?;
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let repo =
        parse_repo(path).map_err(|_| "the origin remote's path is not owner/name".to_owned())?;
    Ok(Remote { host, repo })
}

/// The host of `[user[:password]@]host[:port]`, lower case, without brackets, port or a
/// trailing dot; `None` when it is not a host name or address.
fn host_of(authority: &str) -> Option<String> {
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
        let (addr, rest) = v6.split_once(']')?;
        (
            addr,
            rest.strip_prefix(':')
                .or_else(|| rest.is_empty().then_some("")),
        )
    } else {
        match hostport.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (hostport, Some("")),
        }
    };
    let port = port?;
    if !port.is_empty() && (port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let host = host.trim_end_matches('.');
    let ok = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'));
    ok.then(|| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn remote(host: &str, repo: &str) -> Remote {
        Remote {
            host: host.to_owned(),
            repo: repo.to_owned(),
        }
    }

    #[test]
    fn the_forge_states_map_to_the_four_badges() {
        assert_eq!(forge_state("success", 3), Ok(CiState::Success));
        assert_eq!(forge_state("failure", 3), Ok(CiState::Failure));
        assert_eq!(forge_state("error", 1), Ok(CiState::Failure));
        // Not green, so not shown as green.
        assert_eq!(forge_state("warning", 1), Ok(CiState::Failure));
        assert_eq!(forge_state("pending", 2), Ok(CiState::Pending));
    }

    #[test]
    fn no_statuses_is_none() {
        assert_eq!(forge_state("", 0), Ok(CiState::None));
        // Older Gitea answered `pending` for a commit with nothing on it.
        assert_eq!(forge_state("pending", 0), Ok(CiState::None));
        // A verdict with nothing behind it contradicts itself.
        assert!(forge_state("success", 0).is_err());
        assert!(forge_state("failure", 0).is_err());
    }

    #[test]
    fn a_state_nobody_knows_is_an_error_not_a_guess() {
        assert!(forge_state("flaky", 1).is_err());
        assert!(forge_state("", 1).is_err());
        assert!(forge_state("SUCCESS", 1).is_err());
    }

    #[test]
    fn links_are_kept_only_when_safe() {
        assert_eq!(
            clean_url("https://ci.example.org/owner/fleet/actions/runs/3").as_deref(),
            Some("https://ci.example.org/owner/fleet/actions/runs/3")
        );
        assert_eq!(clean_url("javascript:alert(1)"), None);
        assert_eq!(clean_url("https://user:pw@ci.example.org/x"), None);
        assert_eq!(clean_url("https://ci.example.org/\u{1b}[31mx"), None);
        assert_eq!(clean_url("https://ci.example.org/a b"), None);
        assert_eq!(clean_url("https:///x"), None);
        assert_eq!(
            clean_url(&format!("https://h/{}", "a".repeat(MAX_URL_BYTES))),
            None
        );
    }

    #[test]
    fn json_spellings() {
        for (state, label) in [
            (CiState::Success, "success"),
            (CiState::Failure, "failure"),
            (CiState::Pending, "pending"),
            (CiState::None, "none"),
            (CiState::NotPushed, "not_pushed"),
            (CiState::Unknown, "unknown"),
            (CiState::Off, "off"),
        ] {
            assert_eq!(state.label(), label);
            assert_eq!(serde_json::to_value(state).unwrap(), label);
        }
    }

    #[test]
    fn remote_forms() {
        let want = remote("forge.example.org", "owner/fleet");
        for text in [
            "ssh://forge.example.org/owner/fleet.git",
            "ssh://git@forge.example.org:2222/owner/fleet.git",
            "ssh://git@forge.example.org:2222/owner/fleet",
            "git@forge.example.org:owner/fleet.git",
            "forge.example.org:owner/fleet",
            "git@forge.example.org:/owner/fleet.git",
            "https://forge.example.org/owner/fleet.git",
            "https://forge.example.org/owner/fleet",
            "https://forge.example.org/owner/fleet/",
            "https://user:s3cret@forge.example.org:8443/owner/fleet.git",
            "https://FORGE.example.org./owner/fleet.git",
            "  https://forge.example.org/owner/fleet.git\n",
        ] {
            assert_eq!(parse_remote(text), Ok(want.clone()), "{text}");
        }
        assert_eq!(
            parse_remote("ssh://git@[fd00::1]:22/owner/fleet.git"),
            Ok(remote("fd00::1", "owner/fleet"))
        );
        assert_eq!(
            parse_remote("http://forge.example.org/owner/fleet"),
            Ok(want)
        );
    }

    #[test]
    fn remotes_that_are_not_a_repository_on_a_forge() {
        for text in [
            "",
            "/srv/git/fleet.git",
            "../fleet",
            "file:///srv/git/owner/fleet.git",
            "git://forge.example.org/owner/fleet.git",
            "ftp://forge.example.org/owner/fleet.git",
            "https://forge.example.org/fleet.git",
            "https://forge.example.org/a/b/c.git",
            "https://forge.example.org/",
            "https://forge.example.org",
            "https:///owner/fleet",
            "git@forge.example.org:fleet.git",
            "git@forge.example.org:a/b/c.git",
            "ssh://forge.example.org:notaport/owner/fleet.git",
            "https://forge.example.org/owner/..",
            "https://forge.example.org/owner/fl eet",
            "/mnt/a:b/owner/fleet",
            "ssh://[fd00::1/owner/fleet",
        ] {
            assert!(parse_remote(text).is_err(), "{text:?} was accepted");
        }
    }

    #[test]
    fn an_error_never_quotes_the_remote() {
        let secret = "s3cr3t-token-value";
        for text in [
            format!("https://user:{secret}@forge.example.org/a/b/c.git"),
            format!("ftp://user:{secret}@forge.example.org/owner/fleet"),
            format!("{secret}@forge.example.org:a/b/c"),
        ] {
            let e = parse_remote(&text).unwrap_err();
            assert!(
                !e.contains(secret) && !e.contains("forge.example.org"),
                "{e}"
            );
        }
    }

    #[test]
    fn a_configured_repo_is_exactly_owner_and_name() {
        assert_eq!(parse_repo("owner/fleet"), Ok("owner/fleet".to_owned()));
        for bad in [
            "", "fleet", "a/b/c", "/a/b", "a/", "a/..", "a/b c", "a/b?x=1", "a/b%2f",
        ] {
            assert!(parse_repo(bad).is_err(), "{bad:?}");
        }
    }

    fn head(upstream: Upstream, origin: Option<Result<Remote, String>>) -> HeadFacts {
        HeadFacts {
            commit: SHA.to_owned(),
            upstream,
            origin,
        }
    }

    fn tracking(ahead: u64, behind: u64) -> Upstream {
        Upstream::Tracking {
            name: "origin/main".to_owned(),
            ahead,
            behind,
        }
    }

    fn settled(plan: Plan) -> FleetCi {
        match plan {
            Plan::Settled(ci) => ci,
            other @ Plan::Ask { .. } => panic!("expected a settled badge, got {other:?}"),
        }
    }

    #[test]
    fn a_pushed_head_on_the_forge_hosts_remote_is_asked_about() {
        let h = head(
            tracking(0, 2),
            Some(Ok(remote("forge.example.org", "owner/fleet"))),
        );
        assert_eq!(
            plan(&h, "forge.example.org", None),
            Plan::Ask {
                commit: SHA.to_owned(),
                repo: "owner/fleet".to_owned()
            }
        );
        // The host compares without case or a trailing dot.
        assert!(matches!(
            plan(&h, "Forge.Example.Org.", None),
            Plan::Ask { .. }
        ));
    }

    #[test]
    fn a_head_ahead_of_its_upstream_is_not_pushed_and_nothing_is_asked() {
        let h = head(tracking(2, 0), None);
        let ci = settled(plan(&h, "forge.example.org", Some("owner/fleet")));
        assert_eq!(ci.state, CiState::NotPushed);
        assert_eq!(ci.commit.as_deref(), Some(SHA));
        assert!(
            ci.reason
                .unwrap()
                .contains("2 commits ahead of origin/main")
        );
        // Even when no repository could be found: pushing is the first thing to do.
        let h = head(tracking(1, 0), Some(Err("x".to_owned())));
        assert_eq!(
            settled(plan(&h, "forge.example.org", None)).state,
            CiState::NotPushed
        );
    }

    #[test]
    fn no_upstream_and_a_detached_head_are_still_asked_about() {
        for upstream in [
            Upstream::None,
            Upstream::Detached,
            Upstream::Gone("origin/x".to_owned()),
        ] {
            let h = head(upstream, None);
            assert!(matches!(
                plan(&h, "forge.example.org", Some("owner/fleet")),
                Plan::Ask { .. }
            ));
        }
    }

    #[test]
    fn a_remote_on_another_host_is_not_guessed() {
        let h = head(
            tracking(0, 0),
            Some(Ok(remote("mirror.example.net", "owner/fleet"))),
        );
        let ci = settled(plan(&h, "forge.example.org", None));
        assert_eq!(ci.state, CiState::Unknown);
        assert!(ci.wants_forge_repo(), "{ci:?}");
        assert!(ci.reason.as_deref().unwrap().ends_with("set forge.repo"));
        // `forge.repo` settles it: the remote is not consulted.
        assert_eq!(
            plan(&h, "forge.example.org", Some("owner/other")),
            Plan::Ask {
                commit: SHA.to_owned(),
                repo: "owner/other".to_owned()
            }
        );
    }

    #[test]
    fn a_remote_that_does_not_parse_or_is_missing_wants_forge_repo() {
        for origin in [Some(Err(NOT_A_FORGE_REMOTE.to_owned())), None] {
            let ci = settled(plan(
                &head(tracking(0, 0), origin),
                "forge.example.org",
                None,
            ));
            assert_eq!(ci.state, CiState::Unknown);
            assert!(ci.wants_forge_repo(), "{ci:?}");
        }
    }

    #[test]
    fn a_bad_configured_repo_is_unknown_with_the_key_named() {
        let ci = settled(plan(
            &head(tracking(0, 0), None),
            "forge.example.org",
            Some("not a repo"),
        ));
        assert_eq!(ci.state, CiState::Unknown);
        assert!(ci.reason.unwrap().starts_with("forge.repo:"));
    }

    #[test]
    fn something_that_is_not_a_commit_is_unknown() {
        let mut h = head(tracking(0, 0), None);
        h.commit = "not-a-commit".to_owned();
        let ci = settled(plan(&h, "forge.example.org", Some("owner/fleet")));
        assert_eq!(ci.state, CiState::Unknown);
        assert_eq!(ci.commit, None);
    }

    #[test]
    fn commit_ids() {
        assert!(is_commit_id(SHA));
        assert!(is_commit_id(&"a".repeat(64)));
        assert!(!is_commit_id(&SHA.to_uppercase()));
        assert!(!is_commit_id(&SHA[..39]));
        assert!(!is_commit_id(&format!("{SHA}\n")));
        assert!(!is_commit_id(""));
    }

    #[test]
    fn an_answer_becomes_the_badge() {
        let ok = FleetCi::from_answer(
            SHA,
            Ok(CiStatus {
                state: CiState::Success,
                url: Some("https://ci.example.org/owner/fleet/actions/runs/1".to_owned()),
            }),
        );
        assert_eq!(ok.state, CiState::Success);
        assert_eq!(ok.short_commit(), Some("01234567"));
        assert!(ok.url.is_some() && ok.reason.is_none());
        // A link that is not safe is dropped, the state stays.
        let bad_link = FleetCi::from_answer(
            SHA,
            Ok(CiStatus {
                state: CiState::Failure,
                url: Some("https://u:p@ci.example.org/".to_owned()),
            }),
        );
        assert_eq!((bad_link.state, bad_link.url), (CiState::Failure, None));
        let failed = FleetCi::from_answer(SHA, Err("no answer within 10000 ms".to_owned()));
        assert_eq!(failed.state, CiState::Unknown);
        assert_eq!(failed.commit.as_deref(), Some(SHA));
        assert!(!failed.wants_forge_repo());
    }

    #[test]
    fn a_reason_is_sanitised() {
        let ci = FleetCi::unknown(None, "bad \u{1b}[31mnews\u{1b}[0m\u{202e}");
        assert_eq!(ci.reason.as_deref(), Some("bad news"));
    }

    #[test]
    fn the_default_is_off() {
        let ci = FleetCi::default();
        assert_eq!(ci.state, CiState::Off);
        assert_eq!(ci.short_commit(), None);
    }
}
