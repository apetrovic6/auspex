//! The CI badge's adapters: `HEAD` of the fleet checkout, and the forge's combined commit status
//! for it.
//!
//! - [`head`]: three read-only git commands in the fleet directory, run as every other git here
//!   is (the hardened [`crate::cache::git`]): `HEAD`'s commit id, where it stands against its
//!   upstream (the update checks' own `git status`, [`crate::preflight::fleet_tree`]), and, only
//!   when `forge.repo` is not configured, the `origin` remote, parsed at once by
//!   [`auspex_domain::ci::parse_remote`] so that a password in it never leaves this function.
//! - [`combined_status`]: **one** `GET /api/v1/repos/{owner}/{repo}/commits/{sha}/status`
//!   (Forgejo), through the shared [`HttpClient`]: timeout, no redirects, the answer read up to
//!   [`BODY_CAP`] bytes and deserialised into only what is used.
//! - [`Token`] and [`read_token_file`]: the API token, which is a secret from the moment it is
//!   read.
//!
//! # The token
//!
//! [`Token`] is a newtype whose `Debug` is `Token(<redacted>)` and which has no `Display`, no
//! `Serialize` and no accessor but [`Token::header`], whose value is `Sensitive` to `Debug` too
//! (the `http` crate's own marker). **No error in this module quotes the forge's answer**, which
//! is the other way a token comes back out (a forge that echoes a bad credential), and none
//! quotes a remote. The token is sent only over `https`, or to a loopback address (a forge on
//! this machine, and the tests' fake server): anywhere else a plain-`http` URL is refused
//! before a connection is made. Redirects are not followed (the shared client does not), so a
//! forge cannot send the token on to another host.

use std::io::Read as _;
use std::net::IpAddr;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::time::Duration;

use auspex_domain::ci::{
    CiState, CiStatus, HeadFacts, clean_url, forge_state, is_commit_id, parse_remote,
};
use auspex_domain::text::sanitize_line;
use http::header::HeaderValue;
use http::{Method, StatusCode};
use rustix::fs::{Mode, OFlags};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use url::{Host, Url};

use crate::Cancelled;
use crate::cache::{GitFailure, GitParams, git, git_typed};
use crate::http::{Call, HttpClient, base_url, join};
use crate::nix::FlakeRef;
use crate::preflight::fleet_tree;

/// The largest answer read: 1 MiB.
///
/// A combined status is a short list (Forgejo pages it at 30 to 50 entries) of about 600 bytes
/// each, so a real answer is a few tens of KiB. [`MAX_STATUSES`] bounds what the parsed list
/// costs on top of the body.
pub const BODY_CAP: usize = 1024 * 1024;

/// The exit status of `git remote get-url <name>` for a name that is not a remote.
const NO_SUCH_REMOTE: i32 = 2;

/// How many times the guard decodes a link's percent-encoding looking for the token: a forge can
/// encode as deeply as it likes, so this is a bound on the work, not a promise about depth. A copy
/// encoded more times than this is not seen (nothing that is not an attack encodes a token even
/// once: it is unreserved characters).
pub(crate) const MAX_DECODE_PASSES: usize = 8;

/// The longest reason line from git or the transport, in characters (then `…`): enough for any
/// message of git's, short enough for a status line and for `--json`.
pub(crate) const MAX_REASON_CHARS: usize = 300;

/// What a reason is replaced with when it held the token.
const WITHHELD: &str = "forge: the error text held the token, so it is not shown";

/// `reason`, unless it holds the token ([`Token::appears_in`]): then fixed text.
///
/// **Every reason this module returns goes through here** (the forge's transport errors, git's
/// stderr), because text that comes from outside is where a reflected credential would come back
/// out: a TLS certificate whose names include it, a repository whose config does. With no token
/// sent there is nothing to guard.
pub(crate) fn guard(token: Option<&Token>, reason: String) -> String {
    if token.is_some_and(|t| t.appears_in(&reason)) {
        WITHHELD.to_owned()
    } else {
        reason
    }
}

/// A line of outside text made safe for a reason: sanitised (no escape sequences or control
/// characters), guarded, and cut to [`MAX_REASON_CHARS`].
pub(crate) fn reason_line(token: Option<&Token>, text: &str) -> String {
    let line = guard(token, sanitize_line(text));
    match line.char_indices().nth(MAX_REASON_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

/// The revision `git rev-parse` is asked for: `HEAD`, peeled to a commit.
const HEAD_COMMIT: &str = "HEAD^{commit}";

/// The most statuses an answer may hold: 1,000, twenty times what the forge sends. Past it the
/// answer is an error, counted while it is read.
pub const MAX_STATUSES: usize = 1_000;

/// The largest token file read: 4 KiB. A forge token is under a hundred bytes.
pub const MAX_TOKEN_FILE: usize = 4096;

/// A forge API token. See the module's *The token*.
pub struct Token(String);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// Whether `text` holds the token: bare (which is also what the header's `token <value>`
    /// carries), in any case, or percent-encoded however many times (up to
    /// [`MAX_DECODE_PASSES`]; decoding stops as soon as nothing changes).
    ///
    /// This is how text that came from outside is checked before it can reach output, without
    /// the secret having an accessor: a forge that reflects the credential must not be able to
    /// put it in the badge. A token this short is not told apart from ordinary text by chance
    /// (the shortest a forge issues is dozens of characters). Not caught, by nature: a copy the
    /// forge splits or re-encodes in a way it invents (base64, say); no guard can be complete
    /// against a forge that already holds the token and wants it shown.
    fn appears_in(&self, text: &str) -> bool {
        let needle = self.0.to_ascii_lowercase();
        let mut current = text.to_owned();
        for _ in 0..=MAX_DECODE_PASSES {
            if current.to_ascii_lowercase().contains(&needle) {
                return true;
            }
            let decoded = percent_decoded(&current);
            if decoded == current {
                return false;
            }
            current = decoded;
        }
        false
    }

    /// The `Authorization` header value, `token <the token>`, marked sensitive.
    ///
    /// # Errors
    ///
    /// The token holds a byte a header cannot carry. [`read_token_file`] refuses those, so this
    /// is only reachable for a token built another way; the error does not quote it.
    pub fn header(&self) -> Result<HeaderValue, String> {
        let mut value = HeaderValue::from_str(&format!("token {}", self.0))
            .map_err(|_| "the token holds a character a header cannot carry".to_owned())?;
        value.set_sensitive(true);
        Ok(value)
    }
}

/// Reads the token from `path`.
///
/// Refused, saying why and never quoting what the file holds:
///
/// - not a regular file (a FIFO is not opened, it is refused: the open is non-blocking);
/// - **readable or writable by group or others** (any of the mode's `0o077` bits): a token that
///   others can read is not a secret, and the fix is `chmod 600`;
/// - **owned by another user** than the one running auspex: a file someone else controls can be
///   swapped for another token, or read by them;
/// - empty, longer than [`MAX_TOKEN_FILE`], or holding anything but printable ASCII after the one
///   trailing newline that is stripped (a second newline, a space, a control character, a
///   non-ASCII byte).
///
/// The checks are made on the *open file* (`fstat`), so the file checked is the file read. A
/// symlink is followed (a secrets manager's path often is one), and it is the target that is
/// checked.
///
/// # Errors
///
/// One line: the path (which is not a secret) and what is wrong.
pub fn read_token_file(path: &Path) -> Result<Token, String> {
    let shown = path.display();
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|e| {
        format!(
            "forge token file {shown}: cannot open it: {}",
            errno_words(e)
        )
    })?;
    let mut file = std::fs::File::from(fd);
    let meta = file
        .metadata()
        .map_err(|e| format!("forge token file {shown}: cannot stat it: {e}"))?;
    if !meta.is_file() {
        return Err(format!("forge token file {shown}: is not a regular file"));
    }
    check_owner_and_mode(
        &shown.to_string(),
        meta.uid(),
        meta.mode(),
        rustix::process::geteuid().as_raw(),
    )?;
    let mut bytes = Vec::new();
    // One byte past the cap tells "exactly the cap" from "more".
    file.by_ref()
        .take(
            u64::try_from(MAX_TOKEN_FILE)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        )
        .read_to_end(&mut bytes)
        .map_err(|e| format!("forge token file {shown}: cannot read it: {e}"))?;
    if bytes.len() > MAX_TOKEN_FILE {
        return Err(format!(
            "forge token file {shown}: is larger than {MAX_TOKEN_FILE} bytes, so it is not a token"
        ));
    }
    // One trailing newline, as `echo` and an editor leave it; nothing else is trimmed, so a
    // file with two newlines or a space in it is refused rather than quietly altered.
    let token = bytes
        .strip_suffix(b"\r\n")
        .or_else(|| bytes.strip_suffix(b"\n"))
        .unwrap_or(&bytes);
    if token.is_empty() {
        return Err(format!("forge token file {shown}: is empty"));
    }
    if !token.iter().all(u8::is_ascii_graphic) {
        return Err(format!(
            "forge token file {shown}: holds a space, a control character or a non-ASCII byte \
             besides one trailing newline, so it is not a token"
        ));
    }
    // ASCII graphic, so UTF-8.
    Ok(Token(String::from_utf8_lossy(token).into_owned()))
}

/// The owner and mode rules, on values: `uid` and `mode` of the open file, `me` the user running
/// auspex.
fn check_owner_and_mode(shown: &str, uid: u32, mode: u32, me: u32) -> Result<(), String> {
    if uid != me {
        return Err(format!(
            "forge token file {shown}: is owned by uid {uid}, not by you (uid {me}); a file another \
             user owns can be read or replaced by them"
        ));
    }
    let mode = mode & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "forge token file {shown}: is accessible to group or others (mode {mode:04o}); a \
             token others can read is not a secret: chmod 600 it"
        ));
    }
    Ok(())
}

/// `text` with every valid `%XX` replaced by its byte, lossily as UTF-8. Invalid escapes are
/// left as they are.
fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| char::from(b).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
            && let Ok(byte) = u8::try_from(hi * 16 + lo)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn errno_words(e: rustix::io::Errno) -> String {
    match e {
        rustix::io::Errno::NOENT => "it does not exist".to_owned(),
        rustix::io::Errno::ACCESS => "permission denied".to_owned(),
        other => other.to_string(),
    }
}

/// `forge.url`: `https` only, with a host, without credentials; query and fragment dropped, a
/// path prefix kept (a forge under `/forgejo`).
///
/// # Errors
///
/// One line. The URL is not quoted: it could carry a password.
pub fn forge_url(s: &str) -> Result<Url, String> {
    let url = base_url(s)?;
    if url.scheme() != "https" {
        return Err("the URL must be https: the token is sent with every request".to_owned());
    }
    Ok(url)
}

/// Whether a URL's host is this machine: `localhost` or a loopback address.
fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => IpAddr::V4(ip).is_loopback(),
        Some(Host::Ipv6(ip)) => IpAddr::V6(ip).is_loopback(),
        None => false,
    }
}

/// The forge's combined commit status for `commit` of `repo` (`owner/name`), within `timeout`.
///
/// `url` is the forge's base URL (`http` or `https`; [`forge_url`] is what the config accepts).
/// With a `token`, it is sent as `Authorization: token …`, over `https` or to a loopback host
/// only. Without one the request is anonymous, which is how a public repository is read.
///
/// | answer | result |
/// |---|---|
/// | 200 | the state ([`forge_state`]) and a link |
/// | 401, 403 | "the forge refused the token", or "wants a token" when none was sent |
/// | 404 | "no such repository or commit, or the token cannot see it" |
/// | any redirect | an error: redirects are not followed |
/// | anything else | "answered HTTP n" |
///
/// A body that is not JSON, is not what Forgejo sends, is longer than [`BODY_CAP`] or holds more
/// than [`MAX_STATUSES`] statuses is an error. **No error quotes the answer**, and every reason,
/// the transport's included (a TLS error names the certificate's names), goes through [`guard`]:
/// one that holds the token is replaced.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired; every other failure is the inner `Err`.
pub async fn combined_status(
    client: &HttpClient,
    url: &str,
    repo: &str,
    commit: &str,
    token: Option<&Token>,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Result<CiStatus, String>, Cancelled> {
    Ok(
        combined_status_unguarded(client, url, repo, commit, token, timeout, cancel)
            .await?
            .map_err(|reason| guard(token, reason)),
    )
}

/// [`combined_status`] before every reason goes through [`guard`].
async fn combined_status_unguarded(
    client: &HttpClient,
    url: &str,
    repo: &str,
    commit: &str,
    token: Option<&Token>,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Result<CiStatus, String>, Cancelled> {
    let request = match build(url, repo, commit, token) {
        Ok(r) => r,
        Err(e) => return Ok(Err(format!("forge: {e}"))),
    };
    let call = Call {
        method: Method::GET,
        url: &request.url,
        accept: "application/json",
        timeout,
        body_cap: BODY_CAP,
        authorization: request.authorization.as_ref(),
    };
    let (status, body) = match client.send_call(&call, cancel).await? {
        Ok(r) => r,
        Err(e) => return Ok(Err(format!("forge: {e}"))),
    };
    Ok(match status {
        StatusCode::OK => parse(&body, token),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(if token.is_some() {
            format!("forge: the forge refused the token (HTTP {status}): check forge.token_file")
        } else {
            format!("forge: the forge wants a token (HTTP {status}): set forge.token_file")
        }),
        StatusCode::NOT_FOUND => Err(
            "forge: no such repository or commit, or the token cannot see it (HTTP 404): check \
             forge.repo"
                .to_owned(),
        ),
        s if s.is_redirection() => Err(format!(
            "forge: the forge redirected (HTTP {s}) and auspex does not follow redirects: check \
             forge.url"
        )),
        s => Err(format!("forge: the forge answered HTTP {s}")),
    })
}

struct Prepared {
    url: Url,
    authorization: Option<HeaderValue>,
}

fn build(url: &str, repo: &str, commit: &str, token: Option<&Token>) -> Result<Prepared, String> {
    let base = base_url(url)?;
    if !is_commit_id(commit) {
        return Err("not a commit id".to_owned());
    }
    let (owner, name) = repo
        .split_once('/')
        .filter(|(o, n)| !o.is_empty() && !n.is_empty() && !n.contains('/'))
        .ok_or_else(|| "the repository is not owner/name".to_owned())?;
    let authorization = match token {
        None => None,
        Some(t) => {
            // Before any connection: a token must not cross the network in the clear.
            if base.scheme() != "https" && !is_loopback(&base) {
                return Err("refusing to send the token over plain http to another host".to_owned());
            }
            Some(t.header()?)
        }
    };
    Ok(Prepared {
        url: join(
            &base,
            &[
                "api", "v1", "repos", owner, name, "commits", commit, "status",
            ],
        ),
        authorization,
    })
}

/// Forgejo's combined status, typed: only what is used is kept and the rest is skipped without
/// being built.
#[derive(Deserialize)]
struct Answer {
    state: Option<String>,
    #[serde(default, deserialize_with = "capped_statuses")]
    statuses: Option<Vec<Status>>,
    total_count: Option<u64>,
}

#[derive(Deserialize)]
struct Status {
    state: Option<String>,
    target_url: Option<String>,
}

fn capped_statuses<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Vec<Status>>, D::Error> {
    struct Capped;
    impl<'de> serde::de::Visitor<'de> for Capped {
        type Value = Option<Vec<Status>>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "a list of at most {MAX_STATUSES} statuses")
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut v = Vec::new();
            while let Some(s) = seq.next_element::<Status>()? {
                if v.len() == MAX_STATUSES {
                    return Err(serde::de::Error::custom("too many statuses"));
                }
                v.push(s);
            }
            Ok(Some(v))
        }
    }
    d.deserialize_any(Capped)
}

/// Reads a 200 answer.
///
/// `token` is the one that was sent, if any: a link that holds it (a forge that reflects the
/// credential) is dropped, so it cannot reach `--json`. Of what the *body* holds, the link is the
/// only text that reaches output: the state is mapped to a fixed vocabulary, every error built
/// here is fixed text, and the other fields are not read. Text from outside the body (the
/// transport's errors, git's stderr) does not come through here; [`combined_status`] and [`head`]
/// send every reason they return through [`guard`].
pub(crate) fn parse(body: &[u8], token: Option<&Token>) -> Result<CiStatus, String> {
    let doc: Answer = serde_json::from_slice(body)
        .map_err(|_| "forge: the answer is not a combined commit status".to_owned())?;
    let Some(raw) = doc.state.as_deref() else {
        return Err("forge: the answer has no state".to_owned());
    };
    let statuses = doc.statuses.unwrap_or_default();
    let count = if statuses.is_empty() {
        usize::try_from(doc.total_count.unwrap_or(0)).unwrap_or(usize::MAX)
    } else {
        statuses.len()
    };
    let state = forge_state(raw, count).map_err(|e| format!("forge: {e}"))?;
    // A link only for a state that has something to link to: the first status of the combined
    // state's own kind, else the first with a usable link.
    let url = (state != CiState::None)
        .then(|| {
            let link = |s: &Status| {
                s.target_url
                    .as_deref()
                    .and_then(clean_url)
                    .filter(|u| !token.is_some_and(|t| t.appears_in(u)))
            };
            statuses
                .iter()
                .find(|s| s.state.as_deref() == Some(raw) && link(s).is_some())
                .or_else(|| statuses.iter().find(|s| link(s).is_some()))
                .and_then(link)
        })
        .flatten();
    Ok(CiStatus { state, url })
}

/// `HEAD` of the fleet checkout: its commit, where it stands against its upstream, and, when
/// `want_origin`, the `origin` remote.
///
/// Each git command is bounded by `params.timeout` and runs as the update's checks' do. A fleet
/// that is not a local directory has no checkout to ask.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired (the process group is killed and reaped). Anything else is
/// the inner `Err`, one line: not a repository, no commit yet, git failed, or `git status` did.
/// What git said is sanitised, cut to [`MAX_REASON_CHARS`] and, when a `token` is given, replaced
/// if it holds it ([`guard`]): a repository's own config can put anything in git's stderr.
/// A missing or unreadable `origin` is not an error here: it is [`HeadFacts::origin`], because
/// it only matters when `forge.repo` is not set.
pub async fn head(
    params: &GitParams<'_>,
    flake: &FlakeRef,
    want_origin: bool,
    token: Option<&Token>,
    cancel: &CancellationToken,
) -> Result<Result<HeadFacts, String>, Cancelled> {
    let Some(dir) = flake.local_dir() else {
        return Ok(Err(
            "the fleet is not a local directory, so it has no checkout to take a commit from"
                .to_owned(),
        ));
    };
    let out = match git(
        params,
        &dir,
        &["rev-parse", "--verify", "--quiet", HEAD_COMMIT],
        256,
        cancel,
    )
    .await?
    {
        Ok(out) => out,
        Err(why) => return Ok(Err(reason_line(token, &why))),
    };
    let commit = String::from_utf8_lossy(&out).trim().to_owned();
    if !is_commit_id(&commit) {
        return Ok(Err(
            "the fleet repository has no commit yet, or git rev-parse printed something else"
                .to_owned(),
        ));
    }
    let tree = match fleet_tree(params, flake, cancel).await? {
        Ok(t) => t,
        Err(why) => return Ok(Err(reason_line(token, &why))),
    };
    let origin = if want_origin {
        let asked = git_typed(params, &dir, &["remote", "get-url", "origin"], 4096, cancel).await?;
        Some(match asked {
            Ok(out) => parse_remote(&String::from_utf8_lossy(&out)),
            // `git remote get-url` exits 2 for a remote that does not exist; anything else (a
            // timeout, too much output, a broken config) is not the same thing and says so.
            Err(GitFailure::Exited {
                code: Some(NO_SUCH_REMOTE),
                ..
            }) => Err("the fleet repository has no origin remote".to_owned()),
            // git's own words, which a repository's config can shape: bounded, clean, and kept
            // from holding the token.
            Err(other) => Err(format!(
                "could not read the origin remote: {}",
                reason_line(token, &other.into_message())
            )),
        })
    } else {
        None
    };
    Ok(Ok(HeadFacts {
        commit,
        upstream: tree.upstream,
        origin,
    }))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn file_with(dir: &Path, name: &str, contents: &[u8], mode: u32) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn secret(t: &Token) -> String {
        t.0.clone()
    }

    #[test]
    fn a_token_file_is_read_and_one_newline_is_stripped() {
        let dir = tempfile::tempdir().unwrap();
        for (name, contents, want) in [
            ("plain", &b"abc123"[..], "abc123"),
            ("lf", b"abc123\n", "abc123"),
            ("crlf", b"abc123\r\n", "abc123"),
        ] {
            for mode in [0o600, 0o400] {
                let p = file_with(dir.path(), &format!("{name}{mode:o}"), contents, mode);
                assert_eq!(
                    secret(&read_token_file(&p).unwrap()),
                    want,
                    "{name} {mode:o}"
                );
            }
        }
    }

    #[test]
    fn a_token_file_others_can_reach_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [0o640, 0o604, 0o660, 0o644, 0o700 | 0o010, 0o602] {
            let p = file_with(dir.path(), &format!("t{mode:o}"), b"abc123\n", mode);
            let e = read_token_file(&p).unwrap_err();
            assert!(e.contains("accessible to group or others"), "{mode:o}: {e}");
            assert!(e.contains("chmod 600"), "{e}");
            assert!(!e.contains("abc123"), "{e}");
        }
    }

    #[test]
    fn a_token_file_with_anything_odd_is_refused_without_quoting_it() {
        let dir = tempfile::tempdir().unwrap();
        for (name, contents) in [
            ("empty", &b""[..]),
            ("only-newline", b"\n"),
            ("two-newlines", b"abc123\n\n"),
            ("space", b"abc 123\n"),
            ("tab", b"abc\t123"),
            ("nul", b"abc\x00123"),
            ("utf8", "tökén".as_bytes()),
            ("long", &[b'a'; MAX_TOKEN_FILE + 1]),
        ] {
            let p = file_with(dir.path(), name, contents, 0o600);
            let e = read_token_file(&p).unwrap_err();
            assert!(!e.contains("abc"), "{name}: {e}");
            assert!(!e.contains("tökén"), "{name}: {e}");
        }
        // Exactly the cap is accepted.
        let p = file_with(dir.path(), "cap", &[b'a'; MAX_TOKEN_FILE], 0o600);
        assert!(read_token_file(&p).is_ok());
    }

    #[test]
    fn a_missing_file_a_directory_and_a_fifo_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let e = read_token_file(&dir.path().join("nope")).unwrap_err();
        assert!(e.contains("does not exist"), "{e}");
        let e = read_token_file(dir.path()).unwrap_err();
        assert!(e.contains("not a regular file"), "{e}");
        let fifo = dir.path().join("fifo");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            Mode::from_bits_truncate(0o600),
            0,
        )
        .unwrap();
        // Returns, rather than waiting for a writer. On a thread, so that a regression (a
        // blocking open) fails in seconds instead of hanging the whole test binary until the
        // harness kills it.
        let (tx, rx) = std::sync::mpsc::channel();
        let path = fifo;
        std::thread::spawn(move || {
            let _ = tx.send(read_token_file(&path));
        });
        let e = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("read_token_file(fifo) must return, not wait for a writer")
            .unwrap_err();
        assert!(e.contains("not a regular file"), "{e}");
    }

    #[test]
    fn a_symlink_is_followed_and_its_target_checked() {
        let dir = tempfile::tempdir().unwrap();
        let good = file_with(dir.path(), "good", b"tok\n", 0o600);
        let bad = file_with(dir.path(), "bad", b"tok\n", 0o644);
        let link = |name: &str, to: &Path| {
            let l = dir.path().join(name);
            std::os::unix::fs::symlink(to, &l).unwrap();
            l
        };
        assert!(read_token_file(&link("to-good", &good)).is_ok());
        assert!(read_token_file(&link("to-bad", &bad)).is_err());
    }

    #[test]
    fn a_file_owned_by_someone_else_is_refused() {
        // Whoever owns it, even with a mode that is otherwise fine.
        let e = check_owner_and_mode("/x/token", 0, 0o600, 1000).unwrap_err();
        assert!(
            e.contains("owned by uid 0") && e.contains("uid 1000"),
            "{e}"
        );
        assert!(check_owner_and_mode("/x/token", 1000, 0o600, 1000).is_ok());
        assert!(check_owner_and_mode("/x/token", 1000, 0o100_400, 1000).is_ok());
        // The owner is checked before the mode.
        assert!(
            check_owner_and_mode("/x/token", 0, 0o644, 1000)
                .unwrap_err()
                .contains("owned by")
        );
    }

    #[test]
    fn the_guard_sees_a_copy_in_any_case_and_through_repeated_encoding() {
        let t = Token("0a1b2c3d4e5f60718293a4b5c6d7e8f901234567".to_owned());
        let bare = "0a1b2c3d4e5f60718293a4b5c6d7e8f901234567";
        assert!(t.appears_in(bare));
        assert!(t.appears_in(&bare.to_ascii_uppercase()));
        assert!(t.appears_in("x=0A1b2C3d4e5F60718293a4b5c6d7e8f901234567&y"));
        let once = bare.replacen('a', "%61", 1);
        assert!(t.appears_in(&once));
        let twice = once.replace('%', "%25");
        assert!(t.appears_in(&twice));
        let mut deep = once;
        for _ in 0..MAX_DECODE_PASSES {
            deep = deep.replace('%', "%25");
        }
        assert!(
            !t.appears_in(&deep),
            "more levels than the bound are not followed (a forge can always out-encode a guard)"
        );
        assert!(!t.appears_in("https://ci.example.org/owner/fleet/actions/runs/12"));
        assert!(!t.appears_in(""));
    }

    #[test]
    fn decoding_until_stable_is_bounded() {
        let t = Token("secret-value-0123".to_owned());
        // `%25` repeated decodes to `%` repeated, one level per pass: it must stop, and cheaply.
        let hostile = "%25".repeat(200_000);
        let began = std::time::Instant::now();
        assert!(!t.appears_in(&hostile));
        assert!(
            began.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            began.elapsed()
        );
    }

    #[test]
    fn the_guard_replaces_a_reason_that_holds_the_token() {
        let t = Token("0a1b2c3d4e5f60718293a4b5c6d7e8f901234567".to_owned());
        // What rustls writes for a certificate whose names include the token.
        let tls = "forge: error trying to connect: certificate not valid for name \"forge.example.org\"; \
                   certificate is only valid for 0A1B2C3D4E5F60718293A4B5C6D7E8F901234567.anything.example";
        let shown = guard(Some(&t), tls.to_owned());
        assert!(!shown.to_ascii_lowercase().contains("0a1b2c3d"), "{shown}");
        assert!(
            shown.starts_with("forge: ") && shown.contains("not shown"),
            "{shown}"
        );
        // Anything else is untouched, and with no token there is nothing to guard.
        let fine = "forge: no answer within 10000 ms".to_owned();
        assert_eq!(guard(Some(&t), fine.clone()), fine);
        assert_eq!(guard(None, tls.to_owned()), tls);
    }

    #[test]
    fn a_reason_line_is_clean_short_and_guarded() {
        let t = Token("0a1b2c3d4e5f60718293a4b5c6d7e8f901234567".to_owned());
        let long = format!(
            "git remote exited 128: \u{1b}[31m{}\u{1b}]0;x\u{7}",
            "9".repeat(5000)
        );
        let shown = reason_line(None, &long);
        assert!(
            shown.chars().count() <= MAX_REASON_CHARS + 1,
            "{}",
            shown.chars().count()
        );
        assert!(!shown.chars().any(char::is_control), "{shown:?}");
        assert!(shown.ends_with('…'), "{shown}");
        assert_eq!(reason_line(None, "short line"), "short line");
        let held = reason_line(Some(&t), "fatal: 0A1B2C3D4E5F60718293A4B5C6D7E8F901234567");
        assert!(!held.to_ascii_lowercase().contains("0a1b2c3d"), "{held}");
        // The cut is on a character boundary.
        let wide = "é".repeat(MAX_REASON_CHARS + 50);
        assert!(reason_line(None, &wide).chars().count() <= MAX_REASON_CHARS + 1);
    }

    #[test]
    fn the_token_does_not_debug_print() {
        let t = Token("hunter2-abc".to_owned());
        assert_eq!(format!("{t:?}"), "Token(<redacted>)");
        assert_eq!(format!("{t:#?}"), "Token(<redacted>)");
        let h = t.header().unwrap();
        assert!(!format!("{h:?}").contains("hunter2"), "{h:?}");
        assert_eq!(h.to_str().unwrap(), "token hunter2-abc");
        assert!(h.is_sensitive());
    }

    #[test]
    fn forge_urls_are_https_only() {
        assert_eq!(
            forge_url("https://forge.example.org/forgejo/?x=1#y")
                .unwrap()
                .as_str(),
            "https://forge.example.org/forgejo/"
        );
        for bad in [
            "http://forge.example.org",
            "ftp://forge.example.org",
            "https://user:pw@forge.example.org",
            "https://",
            "forge.example.org",
        ] {
            let e = forge_url(bad).unwrap_err();
            assert!(!e.contains("pw@"), "{e}");
        }
    }

    #[test]
    fn the_status_url_is_built_from_validated_parts() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let p = build(
            "https://forge.example.org/forgejo",
            "owner/fleet",
            sha,
            None,
        )
        .unwrap();
        assert_eq!(
            p.url.as_str(),
            format!(
                "https://forge.example.org/forgejo/api/v1/repos/owner/fleet/commits/{sha}/status"
            )
        );
        assert!(p.authorization.is_none());
        for (repo, commit) in [
            ("owner", sha),
            ("owner/", sha),
            ("/fleet", sha),
            ("a/b/c", sha),
            ("owner/fleet", "HEAD"),
            (
                "owner/fleet",
                "0123456789abcdef0123456789abcdef0123456/../x",
            ),
        ] {
            assert!(
                build("https://h", repo, commit, None).is_err(),
                "{repo} {commit}"
            );
        }
        // A segment cannot escape its place.
        let p = build("https://h", "own er/..%2f", sha, None).unwrap();
        assert!(
            p.url.as_str().contains("/repos/own%20er/..%252f/"),
            "{}",
            p.url
        );
    }

    #[test]
    fn the_token_is_never_sent_in_the_clear_to_another_host() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let t = Token("tok".to_owned());
        let e = build("http://forge.example.org", "o/n", sha, Some(&t))
            .err()
            .unwrap();
        assert!(e.contains("plain http"), "{e}");
        assert!(!e.contains("tok\n") && !e.ends_with("tok"), "{e}");
        assert!(build("https://forge.example.org", "o/n", sha, Some(&t)).is_ok());
        for local in [
            "http://127.0.0.1:3000",
            "http://[::1]:3000",
            "http://localhost:3000",
        ] {
            assert!(build(local, "o/n", sha, Some(&t)).is_ok(), "{local}");
        }
        // Anonymous over http is fine: nothing secret is sent.
        assert!(build("http://forge.example.org", "o/n", sha, None).is_ok());
    }

    fn answer(body: &str) -> Result<CiStatus, String> {
        parse(body.as_bytes(), None)
    }

    #[test]
    fn answers_become_states() {
        let one = |state: &str| {
            format!(
                r#"{{"state":"{state}","sha":"x","total_count":1,"statuses":[
                    {{"state":"{state}","target_url":"https://ci.example.org/owner/fleet/actions/runs/9","context":"ci"}}]}}"#
            )
        };
        for (raw, want) in [
            ("success", CiState::Success),
            ("failure", CiState::Failure),
            ("error", CiState::Failure),
            ("pending", CiState::Pending),
        ] {
            let s = answer(&one(raw)).unwrap();
            assert_eq!(s.state, want, "{raw}");
            assert_eq!(
                s.url.as_deref(),
                Some("https://ci.example.org/owner/fleet/actions/runs/9")
            );
        }
        // Nothing on the commit: Forgejo says an empty state, with null or [] statuses.
        for body in [
            r#"{"state":"","statuses":null,"total_count":0}"#,
            r#"{"state":"","statuses":[],"total_count":0}"#,
            r#"{"state":"pending","statuses":[],"total_count":0}"#,
        ] {
            assert_eq!(
                answer(body).unwrap(),
                CiStatus {
                    state: CiState::None,
                    url: None
                }
            );
        }
    }

    #[test]
    fn the_link_is_of_the_status_that_explains_the_state() {
        let body = r#"{"state":"failure","total_count":2,"statuses":[
            {"state":"success","target_url":"https://ci.example.org/ok"},
            {"state":"failure","target_url":"https://ci.example.org/bad"}]}"#;
        assert_eq!(
            answer(body).unwrap().url.as_deref(),
            Some("https://ci.example.org/bad")
        );
        // No usable link on it: the first usable one, never an unsafe one.
        let body = r#"{"state":"failure","total_count":2,"statuses":[
            {"state":"success","target_url":"https://ci.example.org/ok"},
            {"state":"failure","target_url":"https://u:p@ci.example.org/bad"}]}"#;
        assert_eq!(
            answer(body).unwrap().url.as_deref(),
            Some("https://ci.example.org/ok")
        );
        let body = r#"{"state":"failure","total_count":1,"statuses":[
            {"state":"failure","target_url":"javascript:alert(1)"}]}"#;
        assert_eq!(answer(body).unwrap().url, None);
    }

    #[test]
    fn malformed_answers_are_errors_that_do_not_quote_them() {
        for body in [
            "",
            "not json",
            "[]",
            "{}",
            r#"{"state":5}"#,
            r#"{"state":"flaky","statuses":[{"state":"flaky"}],"total_count":1}"#,
            r#"{"state":"success","statuses":"SECRET-LOOKING"}"#,
            r#"{"state":"SECRET-LOOKING","statuses":[{"state":"x"}]}"#,
        ] {
            let e = answer(body).unwrap_err();
            assert!(e.starts_with("forge: "), "{body}: {e}");
            assert!(!e.contains("SECRET"), "{e}");
        }
    }

    #[test]
    fn too_many_statuses_are_refused_while_they_are_read() {
        let many = format!(
            r#"{{"state":"success","statuses":[{}]}}"#,
            vec!["{}"; MAX_STATUSES + 1].join(",")
        );
        assert!(answer(&many).is_err());
        let fits = format!(
            r#"{{"state":"success","statuses":[{}]}}"#,
            vec![r#"{"state":"success"}"#; MAX_STATUSES].join(",")
        );
        assert_eq!(answer(&fits).unwrap().state, CiState::Success);
    }
}
