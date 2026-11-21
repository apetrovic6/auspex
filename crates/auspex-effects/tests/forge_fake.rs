//! The CI badge's adapters against a fake forge on 127.0.0.1 (a hand-rolled server on a
//! `TcpListener`, which records every request it gets, headers included) and against fake and
//! real `git` programs in temporary directories. Nothing here leaves the loopback interface,
//! and no name, host or repository in it is real: `forge.example.org`, `owner/fleet`.
//!
//! Skipped, with a message, when 127.0.0.1 cannot be bound (no loopback interface).

#![cfg(unix)]

mod common;

use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use auspex_domain::ci::{CiState, Remote};
use auspex_domain::preflight::Upstream;
use auspex_effects::cache::GitParams;
use auspex_effects::forge::{self, BODY_CAP, Token, combined_status, read_token_file};
use auspex_effects::http::HttpClient;
use auspex_effects::nix::FlakeRef;
use auspex_effects::{CancellationToken, Cancelled};
use common::{Tools, find_on_path, in_nix_build, tempdir, tools};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const TOKEN: &str = "tkn-4f9a1c0de2b7";
const T: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    target: String,
    /// Header names lower-cased.
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone)]
enum Reply {
    Status(u16, String),
    /// Answer after ten seconds: longer than any timeout here.
    Slow,
    /// Headers, then bytes for as long as the client reads.
    Endless,
    /// A redirect to another host, to see that the token does not follow it.
    Redirect(String),
}

struct Server {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Server {
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn serve(route: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Option<Server> {
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) => {
            // The Nix sandbox has loopback; a skip there would be a false green.
            assert!(
                !in_nix_build(),
                "cannot bind 127.0.0.1 inside the Nix build: {e}"
            );
            eprintln!("SKIPPED: cannot bind 127.0.0.1 ({e}); no loopback here");
            return None;
        }
    };
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let route = Arc::new(route);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let (log, route) = (log.clone(), route.clone());
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 65536 {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let mut lines = head.lines();
                let mut first = lines.next().unwrap_or("").split(' ');
                let seen = Seen {
                    method: first.next().unwrap_or("").to_owned(),
                    target: first.next().unwrap_or("").to_owned(),
                    headers: lines
                        .take_while(|l| !l.is_empty())
                        .filter_map(|l| l.split_once(':'))
                        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
                        .collect(),
                };
                log.lock().unwrap().push(seen.clone());
                match route(&seen) {
                    Reply::Status(code, body) => {
                        let resp = format!(
                            "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        let _ = sock.shutdown().await;
                    }
                    Reply::Redirect(to) => {
                        let resp = format!(
                            "HTTP/1.1 301 X\r\nLocation: {to}\r\nContent-Length: 0\r\n\
                             Connection: close\r\n\r\n"
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        let _ = sock.shutdown().await;
                    }
                    Reply::Slow => tokio::time::sleep(Duration::from_secs(10)).await,
                    Reply::Endless => {
                        let _ = sock
                            .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                            .await;
                        let chunk = vec![b'a'; 65536];
                        while sock.write_all(&chunk).await.is_ok() {}
                    }
                }
            });
        }
    });
    Some(Server { addr, seen })
}

fn status_path() -> String {
    format!("/api/v1/repos/owner/fleet/commits/{SHA}/status")
}

/// A combined answer for `state`, with one status of that state.
fn combined(state: &str) -> String {
    serde_json::json!({
        "state": state,
        "sha": SHA,
        "total_count": 1,
        "statuses": [{
            "state": state,
            "context": "ci / build",
            "target_url": "https://ci.example.org/owner/fleet/actions/runs/12",
        }],
    })
    .to_string()
}

async fn ask(
    server: &Server,
    token: Option<&Token>,
    timeout: Duration,
) -> Result<Result<auspex_domain::ci::CiStatus, String>, Cancelled> {
    let client = HttpClient::new().unwrap();
    combined_status(
        &client,
        &server.url(),
        "owner/fleet",
        SHA,
        token,
        timeout,
        &CancellationToken::new(),
    )
    .await
}

fn token() -> Token {
    let dir = tempdir();
    let path = dir.path().join("forge-token");
    fs::write(&path, format!("{TOKEN}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    read_token_file(&path).unwrap()
}

// ---------------------------------------------------------------------------------------
// The combined status
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn each_state_the_forge_can_answer_with() {
    for (body, want) in [
        (combined("success"), CiState::Success),
        (combined("failure"), CiState::Failure),
        (combined("error"), CiState::Failure),
        (combined("pending"), CiState::Pending),
        (
            r#"{"state":"","sha":"x","total_count":0,"statuses":null}"#.to_owned(),
            CiState::None,
        ),
    ] {
        let reply = body.clone();
        let Some(server) = serve(move |_| Reply::Status(200, reply.clone())).await else {
            return;
        };
        let answer = ask(&server, None, T).await.unwrap().unwrap();
        assert_eq!(answer.state, want, "{body}");
        if want == CiState::None {
            assert_eq!(answer.url, None);
        } else {
            assert_eq!(
                answer.url.as_deref(),
                Some("https://ci.example.org/owner/fleet/actions/runs/12")
            );
        }
        // One request per question, to the one path, a GET.
        let seen = server.seen();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].method, "GET");
        assert_eq!(seen[0].target, status_path());
        assert_eq!(seen[0].header("accept"), Some("application/json"));
    }
}

#[tokio::test]
async fn the_token_is_sent_as_an_authorization_header_and_only_when_there_is_one() {
    let Some(server) = serve(|_| Reply::Status(200, combined("success"))).await else {
        return;
    };
    ask(&server, Some(&token()), T).await.unwrap().unwrap();
    ask(&server, None, T).await.unwrap().unwrap();
    let seen = server.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[0].header("authorization"),
        Some(format!("token {TOKEN}").as_str())
    );
    assert_eq!(seen[1].header("authorization"), None, "{:?}", seen[1]);
    // And it is in no part of the URL.
    assert!(!seen[0].target.contains(TOKEN), "{}", seen[0].target);
}

#[tokio::test]
async fn the_token_never_appears_in_an_answer_the_adapter_returns() {
    // A forge that echoes what it was sent, in the body, on every kind of failure.
    for code in [400, 401, 403, 404, 500, 502, 200] {
        let Some(server) = serve(move |seen| {
            let echoed = seen.header("authorization").unwrap_or("").to_owned();
            Reply::Status(
                code,
                format!(r#"{{"message":"bad credentials {echoed}","state":"{echoed}"}}"#),
            )
        })
        .await
        else {
            return;
        };
        let t = token();
        let answer = ask(&server, Some(&t), T).await.unwrap();
        let shown = format!("{answer:?} {t:?}");
        assert!(!shown.contains(TOKEN), "{code}: {shown}");
        // A 200 with no statuses behind its `state` contradicts itself too.
        assert!(answer.is_err(), "{code}: {answer:?}");
    }
}

#[tokio::test]
async fn a_401_and_a_404_say_what_to_check() {
    let Some(server) = serve(|seen| {
        if seen.target.contains("/repos/owner/fleet/") {
            Reply::Status(401, r#"{"message":"token is required"}"#.to_owned())
        } else {
            Reply::Status(404, String::new())
        }
    })
    .await
    else {
        return;
    };
    let with = ask(&server, Some(&token()), T).await.unwrap().unwrap_err();
    assert!(
        with.contains("refused the token") && with.contains("forge.token_file"),
        "{with}"
    );
    let without = ask(&server, None, T).await.unwrap().unwrap_err();
    assert!(
        without.contains("wants a token") && without.contains("forge.token_file"),
        "{without}"
    );

    let Some(missing) = serve(|_| Reply::Status(404, "{}".to_owned())).await else {
        return;
    };
    let why = ask(&missing, None, T).await.unwrap().unwrap_err();
    assert!(
        why.contains("HTTP 404") && why.contains("forge.repo"),
        "{why}"
    );

    let Some(down) = serve(|_| Reply::Status(503, "busy".to_owned())).await else {
        return;
    };
    let why = ask(&down, None, T).await.unwrap().unwrap_err();
    assert!(why.contains("HTTP 503"), "{why}");
    assert!(!why.contains("busy"), "{why}");
}

#[tokio::test]
async fn a_redirect_is_not_followed_and_the_token_goes_nowhere_else() {
    let Some(elsewhere) = serve(|_| Reply::Status(200, combined("success"))).await else {
        return;
    };
    let target = format!("{}{}", elsewhere.url(), status_path());
    let Some(server) = serve(move |_| Reply::Redirect(target.clone())).await else {
        return;
    };
    let why = ask(&server, Some(&token()), T).await.unwrap().unwrap_err();
    assert!(why.contains("redirect"), "{why}");
    assert!(elsewhere.seen().is_empty(), "{:?}", elsewhere.seen());
}

#[tokio::test]
async fn a_slow_forge_times_out_with_a_reason() {
    let Some(server) = serve(|_| Reply::Slow).await else {
        return;
    };
    let began = Instant::now();
    let why = ask(&server, None, Duration::from_millis(300))
        .await
        .unwrap()
        .unwrap_err();
    assert!(why.contains("no answer within 300 ms"), "{why}");
    assert!(began.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_huge_answer_is_refused_at_the_cap_and_an_endless_one_stops() {
    // Over the cap with a Content-Length: refused while it is read.
    let big = format!(r#"{{"state":"success","pad":"{}"}}"#, "a".repeat(BODY_CAP));
    let Some(server) = serve(move |_| Reply::Status(200, big.clone())).await else {
        return;
    };
    let why = ask(&server, None, T).await.unwrap().unwrap_err();
    assert!(why.contains("larger than"), "{why}");

    // No length at all and no end: the read stops at the cap instead of filling memory.
    let Some(endless) = serve(|_| Reply::Endless).await else {
        return;
    };
    let began = Instant::now();
    let why = ask(&endless, None, T).await.unwrap().unwrap_err();
    assert!(why.contains("larger than"), "{why}");
    assert!(
        began.elapsed() < Duration::from_secs(4),
        "{:?}",
        began.elapsed()
    );
}

#[tokio::test]
async fn malformed_answers_are_errors() {
    for body in [
        "",
        "<html>login</html>",
        "{}",
        r#"{"state":"flaky","total_count":1,"statuses":[{}]}"#,
    ] {
        let reply = body.to_owned();
        let Some(server) = serve(move |_| Reply::Status(200, reply.clone())).await else {
            return;
        };
        let why = ask(&server, None, T).await.unwrap().unwrap_err();
        assert!(why.starts_with("forge: "), "{body}: {why}");
    }
}

#[tokio::test]
async fn a_dead_forge_is_a_reason_not_a_hang() {
    // Bind and drop: nothing listens there now.
    let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") else {
        eprintln!("SKIPPED: no loopback");
        return;
    };
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let client = HttpClient::new().unwrap();
    let why = combined_status(
        &client,
        &format!("http://{addr}"),
        "owner/fleet",
        SHA,
        None,
        T,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(why.starts_with("forge: "), "{why}");
}

#[tokio::test]
async fn a_cancelled_question_returns_cancelled() {
    let Some(server) = serve(|_| Reply::Slow).await else {
        return;
    };
    let cancel = CancellationToken::new();
    let client = HttpClient::new().unwrap();
    let canceller = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        })
    };
    let answer =
        combined_status(&client, &server.url(), "owner/fleet", SHA, None, T, &cancel).await;
    assert_eq!(answer, Err(Cancelled));
    canceller.await.unwrap();
}

#[tokio::test]
async fn the_token_is_refused_over_plain_http_to_another_host_before_any_connection() {
    let client = HttpClient::new().unwrap();
    let t = token();
    let why = combined_status(
        &client,
        "http://forge.example.org",
        "owner/fleet",
        SHA,
        Some(&t),
        T,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(why.contains("plain http"), "{why}");
    assert!(!why.contains(TOKEN), "{why}");
}

// ---------------------------------------------------------------------------------------
// The token file, through the public function
// ---------------------------------------------------------------------------------------

#[test]
fn token_file_permission_refusals() {
    let dir = tempdir();
    let write = |name: &str, mode: u32| -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, format!("{TOKEN}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    };
    assert!(read_token_file(&write("ok-600", 0o600)).is_ok());
    assert!(read_token_file(&write("ok-400", 0o400)).is_ok());
    for (name, mode) in [
        ("group-read", 0o640),
        ("world-read", 0o604),
        ("both", 0o644),
        ("group-write", 0o620),
    ] {
        let why = read_token_file(&write(name, mode)).unwrap_err();
        assert!(
            why.contains("accessible to group or others"),
            "{name}: {why}"
        );
        assert!(why.contains(&format!("{mode:04o}")), "{name}: {why}");
        assert!(!why.contains(TOKEN), "{name}: {why}");
    }
}

// ---------------------------------------------------------------------------------------
// HEAD of the fleet checkout
// ---------------------------------------------------------------------------------------

fn git_params(program: &Path) -> GitParams<'_> {
    let mut p = GitParams::new();
    p.program = program.as_os_str();
    p.timeout = Duration::from_secs(10);
    p
}

/// A fake git that answers by subcommand and records every call's argv.
fn fake_git(t: &Tools, dir: &Path, rev_parse: &str, status: &str, remote: &str) -> PathBuf {
    t.script(
        dir,
        "git",
        &format!(
            "printf '%s\\n' \"$@\" >> '@dir@/git-argv'\n\
             printf -- '--\\n' >> '@dir@/git-argv'\n\
             case \" $* \" in\n\
               *\" config \"*) printf 'core.bare\\\\0'; exit 0;;\n\
               *\" rev-parse \"*) {rev_parse};;\n\
               *\" status \"*) {status};;\n\
               *\" remote \"*) {remote};;\n\
               *) echo unexpected >&2; exit 9;;\n\
             esac"
        ),
    )
}

fn local(dir: &Path) -> FlakeRef {
    FlakeRef::new(dir.to_str().unwrap()).unwrap()
}

#[tokio::test]
async fn head_reads_the_commit_the_upstream_and_the_origin() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let program = fake_git(
        &t,
        dir.path(),
        &format!("printf '{SHA}\\n'"),
        "printf '## main...origin/main [ahead 1, behind 2]\\0 M flake.nix\\0'",
        "printf 'https://user:s3cr3t-pw@forge.example.org/owner/fleet.git\\n'",
    );
    let head = forge::head(
        &git_params(&program),
        &local(dir.path()),
        true,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(head.commit, SHA);
    assert_eq!(
        head.upstream,
        Upstream::Tracking {
            name: "origin/main".to_owned(),
            ahead: 1,
            behind: 2
        }
    );
    assert_eq!(
        head.origin,
        Some(Ok(Remote {
            host: "forge.example.org".to_owned(),
            repo: "owner/fleet".to_owned()
        }))
    );
    // The password in the remote went no further than the parser.
    assert!(!format!("{head:?}").contains("s3cr3t-pw"), "{head:?}");
    // Read-only, hardened git: the cache module's hardening on each command.
    let argv = fs::read_to_string(dir.path().join("git-argv")).unwrap();
    assert!(
        argv.contains("rev-parse\n--verify\n--quiet\nHEAD^{commit}\n"),
        "{argv}"
    );
    assert!(argv.contains("remote\nget-url\norigin\n"), "{argv}");
    assert!(argv.contains("-c\ncore.fsmonitor=false\n"), "{argv}");
}

#[tokio::test]
async fn the_origin_is_not_read_when_forge_repo_is_configured() {
    let Some(t) = tools(&["cat"]) else { return };
    let dir = tempdir();
    let program = fake_git(
        &t,
        dir.path(),
        &format!("printf '{SHA}\\n'"),
        "printf '## main...origin/main\\0'",
        "echo remote must not be asked >&2; exit 9",
    );
    let head = forge::head(
        &git_params(&program),
        &local(dir.path()),
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(head.origin, None);
    let argv = fs::read_to_string(dir.path().join("git-argv")).unwrap();
    assert!(!argv.contains("remote"), "{argv}");
}

#[tokio::test]
async fn a_missing_or_odd_origin_is_an_answer_not_a_failure() {
    let Some(t) = tools(&["cat"]) else { return };
    for (remote, why) in [
        (
            "echo 'error: No such remote origin' >&2; exit 2",
            "no origin remote",
        ),
        ("printf '/srv/git/fleet.git\\n'", "not an ssh or https"),
        (
            "printf 'ssh://git@forge.example.org/a/b/c.git\\n'",
            "not owner/name",
        ),
    ] {
        let dir = tempdir();
        let program = fake_git(
            &t,
            dir.path(),
            &format!("printf '{SHA}\\n'"),
            "printf '## main\\0'",
            remote,
        );
        let head = forge::head(
            &git_params(&program),
            &local(dir.path()),
            true,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
        let origin = head.origin.expect("asked").unwrap_err();
        assert!(origin.contains(why), "{remote}: {origin}");
    }
}

#[tokio::test]
async fn head_failures_are_reasons() {
    let Some(t) = tools(&["cat"]) else { return };
    // No commit yet: `rev-parse --verify --quiet` exits 1 and prints nothing.
    let dir = tempdir();
    let program = fake_git(
        &t,
        dir.path(),
        "exit 1",
        "printf '## No commits yet on main\\0'",
        "exit 2",
    );
    let why = forge::head(
        &git_params(&program),
        &local(dir.path()),
        true,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(why.contains("git rev-parse"), "{why}");
    // Not a commit id.
    let dir = tempdir();
    let program = fake_git(
        &t,
        dir.path(),
        "printf 'HEAD\\n'",
        "printf '## main\\0'",
        "exit 2",
    );
    let why = forge::head(
        &git_params(&program),
        &local(dir.path()),
        true,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(why.contains("no commit yet"), "{why}");
    // `git status` fails: the state of the checkout is not known, so neither is "not pushed".
    let dir = tempdir();
    let program = fake_git(
        &t,
        dir.path(),
        &format!("printf '{SHA}\\n'"),
        "echo 'fatal: not a git repository' >&2; exit 128",
        "exit 2",
    );
    let why = forge::head(
        &git_params(&program),
        &local(dir.path()),
        true,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(why.contains("not a git repository"), "{why}");
    // A remote fleet has no checkout.
    let remote = FlakeRef::new("github:example/fleet").unwrap();
    let why = forge::head(
        &git_params(&program),
        &remote,
        true,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(why.contains("not a local directory"), "{why}");
}

fn real_git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=auspex", "-c", "user.email=auspex@invalid"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[tokio::test]
async fn head_of_a_real_repository() {
    if find_on_path("git").is_none() {
        assert!(!in_nix_build(), "git is not on PATH inside the Nix build");
        eprintln!("SKIPPED: git is not on PATH");
        return;
    }
    let dir = tempdir();
    let repo = dir.path().join("fleet");
    fs::create_dir(&repo).unwrap();
    real_git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("flake.nix"), "{}\n").unwrap();
    real_git(&repo, &["add", "flake.nix"]);
    real_git(&repo, &["commit", "-q", "-m", "one"]);
    real_git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "git@forge.example.org:owner/fleet.git",
        ],
    );
    let want = real_git(&repo, &["rev-parse", "HEAD"]);
    let program = find_on_path("git").unwrap();
    let head = forge::head(
        &git_params(&program),
        &local(&repo),
        true,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(head.commit, want);
    assert_eq!(head.upstream, Upstream::None);
    assert_eq!(
        head.origin,
        Some(Ok(Remote {
            host: "forge.example.org".to_owned(),
            repo: "owner/fleet".to_owned()
        }))
    );
    // `HEAD` is the commit, not the working tree: a dirty file does not change it.
    fs::write(repo.join("flake.nix"), "{ dirty = true; }\n").unwrap();
    let again = forge::head(
        &git_params(&program),
        &local(&repo),
        true,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(again.commit, want);
}
