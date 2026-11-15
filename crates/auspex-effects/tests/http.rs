//! Prometheus and narinfo against a small HTTP server on 127.0.0.1, hand-rolled on a
//! `TcpListener`. Nothing here leaves the loopback interface.
//!
//! Skipped, with a message, when 127.0.0.1 cannot be bound (no loopback interface).

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
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
