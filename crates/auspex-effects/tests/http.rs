//! Prometheus and narinfo against a small HTTP server on 127.0.0.1, hand-rolled on a
//! `TcpListener`. Nothing here leaves the loopback interface.
//!
//! Skipped, with a message, when 127.0.0.1 cannot be bound (no loopback interface).

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use auspex_domain::observe::{DiskUsage, Load, Memory, PromSample, StorePath};
use auspex_domain::status::NarCache;
use auspex_effects::http::HttpClient;
use auspex_effects::prometheus::{QUERY, query};
use auspex_effects::{CancellationToken, Cancelled, narinfo};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

#[derive(Clone)]
enum Reply {
    Status(u16, String),
    /// Answer after ten seconds: longer than any timeout here.
    Slow,
}

struct Server {
    addr: SocketAddr,
    /// `METHOD target` of every request.
    seen: Arc<Mutex<Vec<String>>>,
}

async fn serve(route: fn(&str, &str) -> Reply) -> Option<Server> {
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) => {
            // The Nix sandbox has loopback; a skip there would be a false green.
            assert!(
                std::env::var_os("NIX_BUILD_TOP").is_none(),
                "cannot bind 127.0.0.1 inside the Nix build: {e}"
            );
            eprintln!("SKIPPED: cannot bind 127.0.0.1 ({e}); no loopback here");
            return None;
        }
    };
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let log = log.clone();
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
                let mut first = head.lines().next().unwrap_or("").split(' ');
                let method = first.next().unwrap_or("").to_owned();
                let target = first.next().unwrap_or("").to_owned();
                log.lock().unwrap().push(format!("{method} {target}"));
                let (code, body) = match route(&method, &target) {
                    Reply::Status(c, b) => (c, b),
                    Reply::Slow => {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        (200, String::new())
                    }
                };
                let resp = format!(
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    if method == "HEAD" { "" } else { &body }
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    Some(Server { addr, seen })
}

const NOW: u64 = 1_700_000_100;

fn clock() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(NOW)
}

fn series(instance: &str, metric: &str, value: &str) -> serde_json::Value {
    serde_json::json!({
        "metric": {"instance": instance, "auspex_metric": metric, "job": "node"},
        "value": [1_700_000_100.0, value],
    })
}

fn good_answer() -> String {
    let mut r = vec![
        series("terra:9100", "scraped", "1700000090.123"),
        series("terra:9100", "uptime", "3600.7"),
        series("terra:9100", "fs_size", "100000000000"),
        series("terra:9100", "fs_free", "40000000000"),
        series("terra:9100", "fs_avail", "30000000000"),
        series("terra:9100", "load1", "0.5"),
        series("terra:9100", "load5", "1"),
        series("terra:9100", "load15", "1.5"),
        series("terra:9100", "mem_total", "16000000000"),
        series("terra:9100", "mem_avail", "8000000000"),
        // Stale: scraped ten minutes ago, and only load.
        series("luna:9100", "scraped", "1699999500"),
        series("luna:9100", "load1", "2"),
        series("luna:9100", "load5", "2"),
        series("luna:9100", "load15", "2"),
        series("luna:9100", "uptime", "NaN"),
        // Not asked for.
        series("other:9100", "scraped", "1700000090"),
        // No scrape timestamp: its age is unknown, so it is left out.
        series("nodata:9100", "load1", "1"),
    ];
    // Something that is not a usable series at all.
    r.push(serde_json::json!({"metric": {}, "value": [0, "1"]}));
    serde_json::json!({
        "status": "success",
        "data": {"resultType": "vector", "result": r},
    })
    .to_string()
}

fn prom_route(_method: &str, target: &str) -> Reply {
    match target.split('?').next().unwrap_or("") {
        "/prom/api/v1/query" => Reply::Status(200, good_answer()),
        "/fail/api/v1/query" => Reply::Status(500, "oops".to_owned()),
        "/badq/api/v1/query" => Reply::Status(
            400,
            r#"{"status":"error","errorType":"bad_data","error":"parse error\u001b[31m at char 3"}"#
                .to_owned(),
        ),
        "/junk/api/v1/query" => Reply::Status(200, "{not json".to_owned()),
        "/matrix/api/v1/query" => Reply::Status(
            200,
            r#"{"status":"success","data":{"resultType":"matrix","result":[]}}"#.to_owned(),
        ),
        "/slow/api/v1/query" => Reply::Slow,
        // A syntactically fine answer larger than the cap: one long label value.
        "/huge/api/v1/query" => Reply::Status(
            200,
            format!(
                r#"{{"status":"success","data":{{"resultType":"vector","result":[{{"metric":{{"instance":"{}"}},"value":[0,"1"]}}]}}}}"#,
                "x".repeat(3 * 1024 * 1024)
            ),
        ),
        // Amplification: tiny values that a JSON tree would make expensive.
        "/zeros/api/v1/query" => Reply::Status(
            200,
            format!(
                r#"{{"status":"success","data":{{"resultType":"vector","result":[{}0]}}}}"#,
                "0,".repeat(700_000)
            ),
        ),
        _ => Reply::Status(404, String::new()),
    }
}

fn wanted() -> BTreeSet<String> {
    ["terra:9100", "luna:9100", "mars:9100", "nodata:9100"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
}

async fn ask(
    client: &HttpClient,
    url: &str,
    timeout: Duration,
) -> Result<BTreeMap<String, PromSample>, String> {
    query(
        client,
        url,
        &wanted(),
        &clock,
        timeout,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn prometheus_good_data_missing_and_stale_instances() {
    let Some(s) = serve(prom_route).await else {
        return;
    };
    let client = HttpClient::new().unwrap();
    let got = ask(
        &client,
        &format!("http://{}/prom/", s.addr),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        ["luna:9100", "terra:9100"],
        "mars has no series, nodata no timestamp, other was not asked for"
    );
    assert_eq!(
        got["terra:9100"],
        PromSample {
            uptime_seconds: Some(3600),
            root_disk: Some(DiskUsage {
                total_bytes: 100_000_000_000,
                used_bytes: 60_000_000_000,
                available_bytes: 30_000_000_000,
            }),
            load: Some(Load {
                one: 0.5,
                five: 1.0,
                fifteen: 1.5
            }),
            memory: Some(Memory {
                total_bytes: 16_000_000_000,
                available_bytes: 8_000_000_000,
            }),
            age_seconds: 9,
        }
    );
    let luna = got["luna:9100"];
    assert_eq!(
        luna.age_seconds, 600,
        "stale, and the core decides what that means"
    );
    assert_eq!(luna.uptime_seconds, None, "NaN is no value");
    assert_eq!(luna.root_disk, None);
    assert!(luna.load.is_some());

    // One request, for the constant query, under the configured path prefix.
    let seen = s.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let url = url::Url::parse(&format!("http://h{}", seen[0].trim_start_matches("GET "))).unwrap();
    assert_eq!(url.path(), "/prom/api/v1/query");
    let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    assert_eq!(pairs, [("query".to_owned(), QUERY.to_owned())]);
}

#[tokio::test]
async fn prometheus_failures_are_errors_to_show() {
    let Some(s) = serve(prom_route).await else {
        return;
    };
    let client = HttpClient::new().unwrap();
    let base = format!("http://{}", s.addr);
    let t = Duration::from_secs(5);

    let e = ask(&client, &format!("{base}/fail"), t).await.unwrap_err();
    assert_eq!(e, "prometheus answered HTTP 500 Internal Server Error");

    let e = ask(&client, &format!("{base}/badq"), t).await.unwrap_err();
    assert_eq!(
        e, "prometheus error: bad_data: parse error at char 3 (HTTP 400 Bad Request)",
        "Prometheus's own reason, sanitised"
    );

    let e = ask(&client, &format!("{base}/junk"), t).await.unwrap_err();
    assert!(e.starts_with("prometheus: malformed JSON"), "{e}");

    let e = ask(&client, &format!("{base}/matrix"), t)
        .await
        .unwrap_err();
    assert_eq!(e, "prometheus: answer is not an instant vector");

    let start = std::time::Instant::now();
    let e = ask(&client, &format!("{base}/slow"), Duration::from_millis(300))
        .await
        .unwrap_err();
    assert_eq!(e, "prometheus: no answer within 300 ms");
    assert!(start.elapsed() < Duration::from_secs(3));

    let e = ask(&client, "ftp://127.0.0.1/", t).await.unwrap_err();
    assert!(e.contains("must be http or https"), "{e}");

    // Nothing listening: bind a port, then close it.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let e = ask(&client, &format!("http://{closed}"), t)
        .await
        .unwrap_err();
    assert!(e.starts_with("prometheus: client error (Connect)"), "{e}");
}

#[tokio::test]
async fn prometheus_cancel() {
    let Some(s) = serve(prom_route).await else {
        return;
    };
    let client = HttpClient::new().unwrap();
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        c.cancel();
    });
    let r = query(
        &client,
        &format!("http://{}/slow", s.addr),
        &wanted(),
        &clock,
        Duration::from_secs(30),
        &cancel,
    )
    .await;
    assert_eq!(r, Err(Cancelled));
}

const HIT: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-nixos-system-a";
const MISS: &str = "/nix/store/1123456789abcdfghijklmnpqrsvwxyz-nixos-system-b";
const BROKEN: &str = "/nix/store/2123456789abcdfghijklmnpqrsvwxyz-nixos-system-c";
const SLOW: &str = "/nix/store/3123456789abcdfghijklmnpqrsvwxyz-nixos-system-d";

fn nar_route(method: &str, target: &str) -> Reply {
    if method != "HEAD" {
        return Reply::Status(405, String::new());
    }
    match target {
        "/cache/0123456789abcdfghijklmnpqrsvwxyz.narinfo" => Reply::Status(200, String::new()),
        "/cache/1123456789abcdfghijklmnpqrsvwxyz.narinfo" => Reply::Status(404, String::new()),
        "/cache/3123456789abcdfghijklmnpqrsvwxyz.narinfo" => Reply::Slow,
        _ => Reply::Status(500, String::new()),
    }
}

#[tokio::test]
async fn narinfo_200_404_500_and_timeout() {
    let Some(s) = serve(nar_route).await else {
        return;
    };
    let client = HttpClient::new().unwrap();
    let base = format!("http://{}/cache", s.addr);
    let c = CancellationToken::new();
    let t = Duration::from_secs(5);
    let ask = |p: &str, t| {
        let p = StorePath::new(p).unwrap();
        let client = &client;
        let base = &base;
        let c = &c;
        async move { narinfo::lookup(client, base, &p, t, c).await.unwrap() }
    };
    assert_eq!(ask(HIT, t).await, NarCache::Cached);
    assert_eq!(ask(MISS, t).await, NarCache::NotCached);
    assert_eq!(ask(BROKEN, t).await, NarCache::Failed);
    let start = std::time::Instant::now();
    assert_eq!(
        ask(SLOW, Duration::from_millis(300)).await,
        NarCache::Failed
    );
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(
        s.seen
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.starts_with("HEAD ")),
        "{:?}",
        s.seen
    );
    // A bad cache URL is unknown, not a panic.
    let p = StorePath::new(HIT).unwrap();
    assert_eq!(
        narinfo::lookup(&client, "gopher://x", &p, t, &c)
            .await
            .unwrap(),
        NarCache::Failed
    );
}

/// F7: an answer over the cap is an error, and one that is all tiny values is refused
/// without being built into a tree.
#[tokio::test]
async fn prometheus_answers_are_bounded() {
    let Some(s) = serve(prom_route).await else {
        return;
    };
    let client = HttpClient::new().unwrap();
    let t = Duration::from_secs(5);
    let e = ask(&client, &format!("http://{}/huge", s.addr), t)
        .await
        .unwrap_err();
    assert!(e.contains("larger than"), "{e}");
    let e = ask(&client, &format!("http://{}/zeros", s.addr), t)
        .await
        .unwrap_err();
    assert!(e.starts_with("prometheus: malformed"), "{e}");
}

/// F8: credentials in a URL are refused (they would not be sent), and never echoed.
#[tokio::test]
async fn urls_with_credentials_are_refused_and_never_shown() {
    let client = HttpClient::new().unwrap();
    let t = Duration::from_secs(1);
    for url in [
        "http://admin:s3cret@127.0.0.1:9/",
        "http://admin:s3cret@[::1",
        "http://admin:s3cret@/",
        "https://admin:s3cret@exa mple.invalid/",
    ] {
        let e = ask(&client, url, t).await.unwrap_err();
        assert!(!e.contains("s3cret") && !e.contains("admin"), "{url}: {e}");
    }
    let e = ask(&client, "http://admin:s3cret@127.0.0.1:9/", t)
        .await
        .unwrap_err();
    assert!(e.contains("credentials"), "{e}");
    let p = StorePath::new(HIT).unwrap();
    assert_eq!(
        narinfo::lookup(
            &client,
            "http://u:p@127.0.0.1:9/",
            &p,
            t,
            &CancellationToken::new()
        )
        .await
        .unwrap(),
        NarCache::Failed
    );
}

/// TLS verification is on: a server with a self-signed certificate (for 127.0.0.1, in
/// `tests/fixtures/tls`, trusted by nobody) is refused. Run with verification disabled,
/// this answers 200 and the test fails.
#[tokio::test]
async fn an_untrusted_certificate_is_refused() {
    use std::sync::Arc;
    let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") else {
        assert!(std::env::var_os("NIX_BUILD_TOP").is_none());
        eprintln!("SKIPPED: no loopback");
        return;
    };
    let addr = listener.local_addr().unwrap();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tls");
    let cert = rustls::pki_types::CertificateDer::from(
        std::fs::read(dir.join("self-signed.crt.der")).unwrap(),
    );
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
        std::fs::read(dir.join("self-signed.key.der"))
            .unwrap()
            .into(),
    );
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap(),
    );
    std::thread::spawn(move || {
        for sock in listener.incoming().take(2) {
            let Ok(sock) = sock else { return };
            let conn = rustls::ServerConnection::new(config.clone()).unwrap();
            let mut tls = rustls::StreamOwned::new(conn, sock);
            let mut buf = [0u8; 1024];
            if std::io::Read::read(&mut tls, &mut buf).is_ok() {
                let _ = std::io::Write::write_all(
                    &mut tls,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        }
    });
    let client = HttpClient::new().unwrap();
    let p = StorePath::new(HIT).unwrap();
    let t = Duration::from_secs(5);
    let e = ask(&client, &format!("https://{addr}/"), t)
        .await
        .unwrap_err();
    assert!(e.contains("certificate"), "{e}");
    assert_eq!(
        narinfo::lookup(
            &client,
            &format!("https://{addr}/"),
            &p,
            t,
            &CancellationToken::new()
        )
        .await
        .unwrap(),
        NarCache::Failed
    );
}

fn vm_hwm_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmHWM:"))
                .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse().ok())
        })
        .unwrap_or(0)
}

/// The answers that cost the most, each just under the 2 MiB body cap.
fn worst_route(_method: &str, target: &str) -> Reply {
    let cap = auspex_effects::prometheus::BODY_CAP - 1024;
    let body = match target.split('?').next().unwrap_or("") {
        // Empty series, as many as fit: refused at MAX_SERIES while reading.
        "/empty/api/v1/query" => {
            let n = (cap - 80) / 3;
            format!(
                r#"{{"status":"success","data":{{"resultType":"vector","result":[{}{{}}]}}}}"#,
                "{},".repeat(n - 1)
            )
        }
        // MAX_SERIES realistic series, each with label and value strings.
        "/many/api/v1/query" => {
            let one = r#"{"metric":{"instance":"host-00000:9100","auspex_metric":"load1"},"value":[1,"0.5"]}"#;
            let n = auspex_effects::prometheus::MAX_SERIES.min((cap - 80) / (one.len() + 1));
            let list = vec![one; n].join(",");
            format!(r#"{{"status":"success","data":{{"resultType":"vector","result":[{list}]}}}}"#)
        }
        // One series with a 2 MiB label.
        "/long/api/v1/query" => format!(
            r#"{{"status":"success","data":{{"resultType":"vector","result":[{{"metric":{{"instance":"{}"}},"value":[0,"1"]}}]}}}}"#,
            "x".repeat(cap - 200)
        ),
        _ => String::new(),
    };
    Reply::Status(200, body)
}

/// Review finding N5: what the worst answers cost, measured as peak RSS in a fresh child
/// process (this binary, re-run on the inner test). The figure is printed and bounded.
#[test]
fn the_worst_answer_is_bounded() {
    if std::env::var_os("AUSPEX_TEST_PROM_INNER").is_some() {
        return;
    }
    // One process per shape: peak RSS is a high-water mark, so one shape would hide the next.
    for shape in ["empty", "many", "long"] {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worst_answer_inner",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("AUSPEX_TEST_PROM_INNER", shape)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        println!("{text}");
        assert!(out.status.success(), "{text}");
        assert!(text.contains("1 passed"), "{text}");
    }
}

#[tokio::test]
async fn worst_answer_inner() {
    let Ok(shape) = std::env::var("AUSPEX_TEST_PROM_INNER") else {
        return;
    };
    let Some(s) = serve(worst_route).await else {
        return;
    };
    let client = HttpClient::new().unwrap();
    // Warm up the client and the allocator on a small answer first.
    let _ = ask(
        &client,
        &format!("http://{}/none", s.addr),
        Duration::from_secs(5),
    )
    .await;
    let before = vm_hwm_kb();
    let r = ask(
        &client,
        &format!("http://{}/{shape}", s.addr),
        Duration::from_secs(10),
    )
    .await;
    let grew = vm_hwm_kb().saturating_sub(before);
    println!(
        "auspex-measure {shape}: peak RSS grew by {grew} kB ({:?})",
        r.as_ref().map(BTreeMap::len)
    );
    // The body is held twice (the server's copy and the client's), 2 MiB each.
    assert!(grew < 16 * 1024, "{shape}: {grew} kB");
}
