//! The runners view's forge adapter against a fake forge on 127.0.0.1 (a hand-rolled server on a
//! `TcpListener` that records every request, headers included). Nothing here leaves the loopback
//! interface, and no name, host or runner in it is real: `forge.example.org`, `alpha`, `beta`.
//!
//! Skipped, with a message, when 127.0.0.1 cannot be bound (no loopback interface).

#![cfg(unix)]

mod common;

use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use auspex_domain::runners::{ForgeAnswer, RunnerStatus, Scope};
use auspex_effects::forge::{Token, read_token_file};
use auspex_effects::http::HttpClient;
use auspex_effects::runners::{self, MAX_JOBS, MAX_PAGES, MAX_RUNNERS, PAGE_SIZE};
use auspex_effects::{CancellationToken, Cancelled};
use common::{in_nix_build, tempdir};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

const TOKEN: &str = "tkn-4f9a1c0de2b7";
const T: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
struct Seen {
    target: String,
    /// Header names lower-cased.
    headers: Vec<(String, String)>,
}

impl Seen {
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }

    fn query(&self, key: &str) -> Option<String> {
        let q = self.target.split_once('?')?.1;
        q.split('&')
            .filter_map(|p| p.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.to_owned())
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone)]
enum Reply {
    Json(String),
    Status(u16, String),
    /// Answer after ten seconds: longer than any timeout here.
    Slow,
    /// Headers, then bytes for as long as the client reads.
    Endless,
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
                let _method = first.next();
                let seen = Seen {
                    target: first.next().unwrap_or("").to_owned(),
                    headers: lines
                        .take_while(|l| !l.is_empty())
                        .filter_map(|l| l.split_once(':'))
                        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
                        .collect(),
                };
                log.lock().unwrap().push(seen.clone());
                let respond = |code: u16, body: String| {
                    format!(
                        "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                match route(&seen) {
                    Reply::Json(body) => {
                        let _ = sock.write_all(respond(200, body).as_bytes()).await;
                        let _ = sock.shutdown().await;
                    }
                    Reply::Status(code, body) => {
                        let _ = sock.write_all(respond(code, body).as_bytes()).await;
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

fn token() -> Token {
    token_with(TOKEN)
}

fn token_with(text: &str) -> Token {
    let dir = tempdir();
    let path = dir.path().join("forge-token");
    fs::write(&path, format!("{text}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    read_token_file(&path).unwrap()
}

fn runner(id: i64, name: &str, status: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id, "name": name, "status": status, "labels": ["docker", "linux"],
        "version": "v9.0.0", "uuid": "00000000-0000-0000-0000-000000000000",
        "ephemeral": false, "owner_id": 0, "repo_id": 0, "description": "",
    })
}

fn json(v: &serde_json::Value) -> Reply {
    Reply::Json(v.to_string())
}

async fn ask(
    server: &Server,
    scope: &Scope,
    token: Option<&Token>,
) -> Result<ForgeAnswer, Cancelled> {
    let client = HttpClient::new().unwrap();
    runners::list(
        &client,
        &server.url(),
        scope,
        token,
        T,
        &CancellationToken::new(),
    )
    .await
}

/// A route for a forge with `all` runners paged by `page` and `limit`, and `jobs` jobs.
fn paged(all: Vec<serde_json::Value>, jobs: serde_json::Value) -> impl Fn(&Seen) -> Reply {
    move |seen| {
        if seen.path().ends_with("/jobs") {
            return json(&jobs);
        }
        let page: usize = seen.query("page").and_then(|p| p.parse().ok()).unwrap_or(1);
        let limit: usize = seen
            .query("limit")
            .and_then(|p| p.parse().ok())
            .unwrap_or(50);
        let chunk: Vec<_> = all
            .iter()
            .skip((page - 1) * limit)
            .take(limit)
            .cloned()
            .collect();
        json(&serde_json::Value::Array(chunk))
    }
}

// ---------------------------------------------------------------------------------------
// What is listed
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn each_status_the_forge_can_report_and_the_rest_of_a_runner() {
    let all = vec![
        runner(1, "alpha", "idle"),
        runner(2, "alpha-docker", "active"),
        runner(3, "beta", "offline"),
        runner(4, "gamma", "paused"),
    ];
    let Some(s) = serve(paged(all, serde_json::json!([]))).await else {
        return;
    };
    let answer = ask(&s, &Scope::Admin, None).await.unwrap();
    let got = answer.runners.unwrap();
    let status: Vec<_> = got.iter().map(|r| (r.name.as_str(), r.status)).collect();
    assert_eq!(
        status,
        [
            ("alpha", RunnerStatus::Idle),
            ("alpha-docker", RunnerStatus::Active),
            ("beta", RunnerStatus::Offline),
            // A word the API does not define is not read as online or as offline.
            ("gamma", RunnerStatus::Unknown),
        ]
    );
    assert_eq!(got[0].labels, ["docker", "linux"]);
    assert_eq!(got[0].version.as_deref(), Some("v9.0.0"));
    assert_eq!(answer.queued, Ok(0));
}

#[tokio::test]
async fn the_scope_chooses_the_endpoint() {
    for (scope, path) in [
        (Scope::Admin, "/api/v1/admin/actions/runners"),
        (Scope::User, "/api/v1/user/actions/runners"),
        (
            Scope::Org("acme".into()),
            "/api/v1/orgs/acme/actions/runners",
        ),
        (
            Scope::Repo {
                owner: "acme".into(),
                name: "fleet".into(),
            },
            "/api/v1/repos/acme/fleet/actions/runners",
        ),
    ] {
        let Some(s) = serve(paged(vec![], serde_json::json!([]))).await else {
            return;
        };
        ask(&s, &scope, None).await.unwrap().runners.unwrap();
        let seen = s.seen();
        assert!(seen.iter().any(|r| r.path() == path), "{scope}: {seen:?}");
        assert!(
            seen.iter().any(|r| r.path() == format!("{path}/jobs")),
            "{scope}"
        );
    }
}

#[tokio::test]
async fn pages_are_followed_until_an_empty_one() {
    let all: Vec<_> = (0..120)
        .map(|i| runner(i, &format!("r{i:03}"), "idle"))
        .collect();
    let Some(s) = serve(paged(all, serde_json::json!([]))).await else {
        return;
    };
    let got = ask(&s, &Scope::Admin, None).await.unwrap().runners.unwrap();
    assert_eq!(got.len(), 120);
    let pages: Vec<_> = s
        .seen()
        .iter()
        .filter(|r| !r.path().ends_with("/jobs"))
        .map(|r| (r.query("page").unwrap(), r.query("limit").unwrap()))
        .collect();
    let limit = PAGE_SIZE.to_string();
    assert_eq!(
        pages,
        [("1", &limit), ("2", &limit), ("3", &limit), ("4", &limit)]
            .map(|(p, l)| (p.to_owned(), l.clone()))
    );
}

#[tokio::test]
async fn a_forge_that_clamps_the_page_size_is_still_read_whole() {
    // It answers 30 a page whatever `limit` says: a short page must not end the listing.
    let all: Vec<_> = (0..70)
        .map(|i| runner(i, &format!("r{i:03}"), "idle"))
        .collect();
    let Some(s) = serve(move |seen| {
        if seen.path().ends_with("/jobs") {
            return Reply::Json("[]".into());
        }
        let page: usize = seen.query("page").and_then(|p| p.parse().ok()).unwrap();
        json(&serde_json::Value::Array(
            all.iter().skip((page - 1) * 30).take(30).cloned().collect(),
        ))
    })
    .await
    else {
        return;
    };
    assert_eq!(
        ask(&s, &Scope::Admin, None)
            .await
            .unwrap()
            .runners
            .unwrap()
            .len(),
        70
    );
}

#[tokio::test]
async fn a_forge_that_ignores_the_page_is_not_read_twice() {
    let all: Vec<_> = (0..3)
        .map(|i| runner(i, &format!("r{i}"), "idle"))
        .collect();
    let Some(s) = serve(move |seen| {
        if seen.path().ends_with("/jobs") {
            Reply::Json("[]".into())
        } else {
            json(&serde_json::Value::Array(all.clone()))
        }
    })
    .await
    else {
        return;
    };
    let got = ask(&s, &Scope::Admin, None).await.unwrap().runners.unwrap();
    assert_eq!(got.len(), 3, "no runner twice");
    // The page that added nothing new ended it: two pages, not twenty.
    let pages = s
        .seen()
        .iter()
        .filter(|r| !r.path().ends_with("/jobs"))
        .count();
    assert_eq!(pages, 2);
}

#[tokio::test]
async fn an_endless_listing_is_an_error_not_a_truncated_list() {
    // Every page has runners nobody has seen: past MAX_PAGES that is an error.
    let Some(s) = serve(|seen| {
        if seen.path().ends_with("/jobs") {
            return Reply::Json("[]".into());
        }
        let page: i64 = seen.query("page").and_then(|p| p.parse().ok()).unwrap();
        json(&serde_json::Value::Array(
            (0..3)
                .map(|i| runner(page * 10 + i, &format!("r{page}-{i}"), "idle"))
                .collect(),
        ))
    })
    .await
    else {
        return;
    };
    let err = ask(&s, &Scope::Admin, None)
        .await
        .unwrap()
        .runners
        .unwrap_err();
    assert!(err.contains(&format!("{MAX_PAGES} pages")), "{err}");
    let pages = s
        .seen()
        .iter()
        .filter(|r| !r.path().ends_with("/jobs"))
        .count();
    assert_eq!(pages, MAX_PAGES);
}

#[tokio::test]
async fn more_runners_in_a_page_than_the_cap_is_refused_while_it_is_read() {
    let many = format!("[{}]", vec!["{}"; MAX_RUNNERS + 1].join(","));
    assert!(many.len() < 1024 * 1024);
    let Some(s) = serve(move |seen| {
        Reply::Json(if seen.path().ends_with("/jobs") {
            "[]".into()
        } else {
            many.clone()
        })
    })
    .await
    else {
        return;
    };
    let err = ask(&s, &Scope::Admin, None)
        .await
        .unwrap()
        .runners
        .unwrap_err();
    assert!(err.contains("more than 1000"), "{err}");
}

// ---------------------------------------------------------------------------------------
// The queue
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn waiting_jobs_are_counted_and_have_no_age() {
    let jobs = serde_json::json!([
        {"id": 1, "name": "build", "status": "waiting", "runs_on": ["docker"], "task_id": 0},
        {"id": 2, "name": "test", "status": "waiting", "runs_on": ["docker"], "task_id": 0},
        {"id": 3, "name": "lint", "status": "WAITING"},
        {"id": 4, "name": "deploy", "status": "blocked"},
        {"id": 5, "name": "x", "status": "running"},
        {"id": 6, "name": "no status at all"},
    ]);
    let Some(s) = serve(paged(vec![runner(1, "alpha", "idle")], jobs)).await else {
        return;
    };
    // Waiting ones, and one without a status (the list is the waiting jobs by its contract);
    // blocked (waiting for another job, not for a runner) and running are not queued.
    assert_eq!(ask(&s, &Scope::Admin, None).await.unwrap().queued, Ok(4));
}

#[tokio::test]
async fn too_many_jobs_are_an_error_not_a_guess() {
    let many = format!("[{}]", vec!["{}"; MAX_JOBS + 1].join(","));
    let Some(s) = serve(move |seen| {
        if seen.path().ends_with("/jobs") {
            Reply::Json(many.clone())
        } else {
            Reply::Json("[]".into())
        }
    })
    .await
    else {
        return;
    };
    let a = ask(&s, &Scope::Admin, None).await.unwrap();
    assert!(a.runners.is_ok());
    assert!(a.queued.unwrap_err().contains("waiting jobs"));
}

#[tokio::test]
async fn the_runners_survive_a_queue_that_is_refused() {
    let Some(s) = serve(|seen| {
        if seen.path().ends_with("/jobs") {
            Reply::Status(403, "{}".into())
        } else {
            json(&serde_json::json!([runner(1, "alpha", "idle")]))
        }
    })
    .await
    else {
        return;
    };
    let a = ask(&s, &Scope::Admin, Some(&token())).await.unwrap();
    assert_eq!(a.runners.unwrap().len(), 1);
    assert!(a.queued.unwrap_err().contains("refused the token"));
}

// ---------------------------------------------------------------------------------------
// What the forge answers with when it says no
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn refusals_say_what_to_check_and_never_quote_the_answer() {
    for (code, with_token, want) in [
        (401, true, "refused the token (HTTP 401"),
        (403, true, "refused the token (HTTP 403"),
        (403, false, "wants a token (HTTP 403"),
        (404, true, "no runners endpoint for this scope"),
        (500, true, "answered HTTP 500"),
        (502, false, "answered HTTP 502"),
    ] {
        // A forge that echoes what it was sent, and more.
        let body = format!("{{\"message\": \"bad credential {TOKEN} and secret-body-text\"}}");
        let Some(s) = serve(move |_| Reply::Status(code, body.clone())).await else {
            return;
        };
        let t = token();
        let a = ask(&s, &Scope::Admin, with_token.then_some(&t))
            .await
            .unwrap();
        for result in [a.runners.map(|_| ()), a.queued.map(|_| ())] {
            let err = result.unwrap_err();
            assert!(err.contains(want), "{code}: {err}");
            assert!(
                !err.contains(TOKEN) && !err.contains("secret-body-text"),
                "{err}"
            );
        }
    }
}

#[tokio::test]
async fn a_redirect_is_an_error_and_the_token_does_not_follow() {
    let Some(other) = serve(|_| Reply::Json("[]".into())).await else {
        return;
    };
    let to = format!("{}/stolen", other.url());
    let Some(s) = serve(move |_| Reply::Redirect(to.clone())).await else {
        return;
    };
    let a = ask(&s, &Scope::Admin, Some(&token())).await.unwrap();
    assert!(a.runners.unwrap_err().contains("redirected"));
    assert!(
        other.seen().is_empty(),
        "nothing was sent to the other host"
    );
}

#[tokio::test]
async fn a_body_that_is_not_runners_is_an_error_that_does_not_quote_it() {
    for body in [
        "not json",
        "{\"runners\": []}",
        "[1, 2]",
        "[{\"id\": \"x\"}]",
        "[] trailing",
    ] {
        let Some(s) = serve(move |_| Reply::Json(body.to_owned())).await else {
            return;
        };
        let a = ask(&s, &Scope::Admin, None).await.unwrap();
        let err = a.runners.unwrap_err();
        assert!(err.contains("not a list"), "{body}: {err}");
        assert!(!err.contains("trailing"));
    }
}

// ---------------------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn an_endless_body_is_cut_at_the_cap() {
    let Some(s) = serve(|_| Reply::Endless).await else {
        return;
    };
    let a = ask(&s, &Scope::Admin, None).await.unwrap();
    assert!(a.runners.unwrap_err().contains("larger than"));
    assert!(a.queued.unwrap_err().contains("larger than"));
}

#[tokio::test]
async fn a_slow_forge_is_a_timeout_for_both_reads_together() {
    let Some(s) = serve(|_| Reply::Slow).await else {
        return;
    };
    let client = HttpClient::new().unwrap();
    let started = Instant::now();
    let a = runners::list(
        &client,
        &s.url(),
        &Scope::Admin,
        None,
        Duration::from_millis(300),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    // Side by side: one timeout, not two.
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "{:?}",
        started.elapsed()
    );
    assert!(a.runners.unwrap_err().contains("no answer within 300 ms"));
    assert!(a.queued.unwrap_err().contains("no answer within 300 ms"));
}

#[tokio::test]
async fn the_pages_share_one_timeout() {
    // Each page takes 200 ms, there is always another, and the whole is allowed 500 ms.
    let Some(s) = serve(|seen| {
        if seen.path().ends_with("/jobs") {
            return Reply::Json("[]".into());
        }
        let page: i64 = seen.query("page").and_then(|p| p.parse().ok()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        json(&serde_json::json!([runner(
            page,
            &format!("r{page}"),
            "idle"
        )]))
    })
    .await
    else {
        return;
    };
    let client = HttpClient::new().unwrap();
    let started = Instant::now();
    let a = runners::list(
        &client,
        &s.url(),
        &Scope::Admin,
        None,
        Duration::from_millis(500),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
    assert!(a.runners.unwrap_err().contains("no answer within 500 ms"));
}

#[tokio::test]
async fn cancelling_stops_the_reads() {
    let Some(s) = serve(|_| Reply::Slow).await else {
        return;
    };
    let client = HttpClient::new().unwrap();
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        c.cancel();
    });
    let started = Instant::now();
    let r = runners::list(&client, &s.url(), &Scope::Admin, None, T, &cancel).await;
    assert!(matches!(r, Err(Cancelled)));
    assert!(started.elapsed() < Duration::from_secs(2));
}

// ---------------------------------------------------------------------------------------
// The token
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn the_token_is_sent_as_a_header_on_every_request_and_only_then() {
    let Some(s) = serve(paged(
        vec![runner(1, "alpha", "idle")],
        serde_json::json!([]),
    ))
    .await
    else {
        return;
    };
    ask(&s, &Scope::Admin, Some(&token())).await.unwrap();
    let seen = s.seen();
    assert!(seen.len() >= 3, "two listing pages and the jobs: {seen:?}");
    for r in &seen {
        assert_eq!(r.header("authorization"), Some(&*format!("token {TOKEN}")));
        assert!(!r.target.contains(TOKEN), "never in the URL");
    }
    let Some(s) = serve(paged(vec![], serde_json::json!([]))).await else {
        return;
    };
    ask(&s, &Scope::Admin, None).await.unwrap();
    assert!(s.seen().iter().all(|r| r.header("authorization").is_none()));
}

#[tokio::test]
async fn the_token_never_goes_over_plain_http_to_another_host() {
    let client = HttpClient::new().unwrap();
    let a = runners::list(
        &client,
        "http://forge.example.org",
        &Scope::Admin,
        Some(&token()),
        T,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    for e in [a.runners.unwrap_err(), a.queued.unwrap_err()] {
        assert!(
            e.contains("refusing to send the token over plain http"),
            "{e}"
        );
        assert!(!e.contains(TOKEN));
    }
}

#[tokio::test]
async fn a_forge_that_reflects_the_token_in_a_runner_does_not_get_it_shown() {
    let encoded = TOKEN.replace('-', "%2D");
    let mut named = runner(1, TOKEN, "idle");
    named["labels"] = serde_json::json!(["docker", TOKEN.to_ascii_uppercase(), encoded]);
    named["version"] = serde_json::json!(format!("v-{TOKEN}"));
    let Some(s) = serve(paged(
        vec![named, runner(2, "alpha", "idle")],
        serde_json::json!([]),
    ))
    .await
    else {
        return;
    };
    let got = ask(&s, &Scope::Admin, Some(&token()))
        .await
        .unwrap()
        .runners
        .unwrap();
    let shown = format!("{got:?}");
    assert!(!shown.to_ascii_lowercase().contains(TOKEN), "{shown}");
    assert!(!shown.contains("%2D"), "{shown}");
    assert_eq!(got[0].name, "(withheld)");
    assert_eq!(got[0].labels, ["docker", "(withheld)", "(withheld)"]);
    assert_eq!(got[0].version.as_deref(), Some("(withheld)"));
    // The honest runner is as it was.
    assert_eq!(got[1].name, "alpha");
}

#[tokio::test]
async fn a_bad_url_is_an_error_that_does_not_quote_it() {
    let client = HttpClient::new().unwrap();
    let a = runners::list(
        &client,
        "https://user:hunter2@forge.example.org",
        &Scope::Admin,
        None,
        T,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let e = a.runners.unwrap_err();
    assert!(e.contains("credentials") && !e.contains("hunter2"), "{e}");
}

#[tokio::test]
async fn runners_over_several_pages_past_the_cap_are_an_error() {
    // A forge that ignores `limit` and sends 600 new runners a page: the second page takes the
    // total past the cap, which is an error and not a list cut at 1,000.
    let Some(s) = serve(|seen| {
        if seen.path().ends_with("/jobs") {
            return Reply::Json("[]".into());
        }
        let page: i64 = seen.query("page").and_then(|p| p.parse().ok()).unwrap();
        json(&serde_json::Value::Array(
            (0..600)
                .map(|i| runner(page * 1000 + i, &format!("r{page}-{i}"), "idle"))
                .collect(),
        ))
    })
    .await
    else {
        return;
    };
    let err = ask(&s, &Scope::Admin, None)
        .await
        .unwrap()
        .runners
        .unwrap_err();
    assert!(err.contains("more than 1000 runners"), "{err}");
    // It stopped at the page that crossed the cap, not twenty pages later.
    let pages = s
        .seen()
        .iter()
        .filter(|r| !r.path().ends_with("/jobs"))
        .count();
    assert_eq!(pages, 2);
}

#[tokio::test]
async fn a_transport_error_that_quotes_the_url_is_guarded_and_cut() {
    // A URI the HTTP layer refuses as too long makes the client's own error quote the whole URL,
    // which here holds the token: the one way a reason from outside can carry it.
    let client = HttpClient::new().unwrap();
    let url = format!("http://127.0.0.1:1/{}{TOKEN}", "a".repeat(70_000));
    let a = runners::list(
        &client,
        &url,
        &Scope::Admin,
        Some(&token()),
        T,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    for e in [a.runners.unwrap_err(), a.queued.unwrap_err()] {
        assert!(!e.contains(TOKEN), "{}", &e[..e.len().min(200)]);
        assert!(e.contains("held the token"), "{e}");
    }
    // Without a token there is nothing to guard, but the reason is still one short line.
    let a = runners::list(
        &client,
        &url,
        &Scope::Admin,
        None,
        T,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let e = a.runners.unwrap_err();
    assert!(e.chars().count() <= 301, "{}", e.chars().count());
}

// ---------------------------------------------------------------------------------------
// Fix round 1 (review of PR #43)
// ---------------------------------------------------------------------------------------

/// B1: the guard runs on the text as it will be shown, after the sanitiser has taken out what it
/// takes out. A token with a zero-width space, a bell or an escape sequence in the middle is not
/// the token to the guard, and is the token again once sanitised.
#[tokio::test]
async fn a_token_split_by_what_the_sanitiser_removes_is_still_withheld() {
    let (head, tail) = TOKEN.split_at(8);
    let mut named = runner(1, &format!("{head}\u{200b}{tail}"), "idle");
    named["labels"] = serde_json::json!(["docker", format!("{head}\u{7}{tail}")]);
    named["version"] = serde_json::json!(format!("v{head}\u{1b}[0m{tail}"));
    let Some(s) = serve(paged(
        vec![named, runner(2, "alpha", "idle")],
        serde_json::json!([]),
    ))
    .await
    else {
        return;
    };
    let got = ask(&s, &Scope::Admin, Some(&token()))
        .await
        .unwrap()
        .runners
        .unwrap();
    let shown = format!("{got:?}");
    assert!(!shown.to_ascii_lowercase().contains(TOKEN), "{shown}");
    assert_eq!(got[0].name, "(withheld)");
    assert_eq!(got[0].labels, ["docker", "(withheld)"]);
    assert_eq!(got[0].version.as_deref(), Some("(withheld)"));
    assert_eq!(got[1].name, "alpha", "the honest runner is as it was");
}

/// B3: Forgejo's lists are Go slices, and an empty one that was never built is `null`.
#[tokio::test]
async fn a_null_list_is_an_empty_one() {
    let Some(s) = serve(|_| Reply::Json("null".into())).await else {
        return;
    };
    let a = ask(&s, &Scope::Admin, None).await.unwrap();
    assert_eq!(a.queued, Ok(0), "no jobs is an empty queue, not an error");
    assert_eq!(a.runners, Ok(vec![]), "no runners is an empty list");
    // Only `null` is: anything else that is not a list still is not.
    let Some(s) = serve(|_| Reply::Json("null null".into())).await else {
        return;
    };
    let a = ask(&s, &Scope::Admin, None).await.unwrap();
    assert!(a.queued.is_err() && a.runners.is_err());
}

/// B4: the swagger's default for the runners list is only what the scope owns directly; `visible`
/// asks for every runner visible to it, which is what `admin` is documented to list. Every scope
/// has the parameter.
#[tokio::test]
async fn every_scope_asks_for_all_the_visible_runners() {
    for scope in [
        Scope::Admin,
        Scope::User,
        Scope::Org("acme".into()),
        Scope::Repo {
            owner: "acme".into(),
            name: "fleet".into(),
        },
    ] {
        let Some(s) = serve(paged(vec![], serde_json::json!([]))).await else {
            return;
        };
        ask(&s, &scope, None).await.unwrap().runners.unwrap();
        let listing: Vec<_> = s
            .seen()
            .into_iter()
            .filter(|r| !r.path().ends_with("/jobs"))
            .collect();
        assert!(!listing.is_empty(), "{scope}");
        for r in listing {
            assert_eq!(
                r.query("visible").as_deref(),
                Some("true"),
                "{scope}: {}",
                r.target
            );
            assert!(r.query("page").is_some() && r.query("limit").is_some());
        }
    }
}

/// N8: a queue of several thousand jobs is counted, as the bound on jobs says. A job is a few
/// hundred bytes, so the body cap of a page (1 MiB) would have refused it first.
#[tokio::test]
async fn a_queue_of_several_thousand_jobs_is_counted() {
    let jobs: Vec<_> = (0..6_000)
        .map(|i| {
            serde_json::json!({
                "id": i, "name": "x".repeat(250), "status": "waiting",
                "runs_on": ["docker"], "needs": [], "task_id": 0,
            })
        })
        .collect();
    let body = serde_json::Value::Array(jobs).to_string();
    assert!(body.len() > 1024 * 1024, "{}", body.len());
    let Some(s) = serve(move |seen| {
        Reply::Json(if seen.path().ends_with("/jobs") {
            body.clone()
        } else {
            "[]".into()
        })
    })
    .await
    else {
        return;
    };
    assert_eq!(
        ask(&s, &Scope::Admin, None).await.unwrap().queued,
        Ok(6_000)
    );
}
