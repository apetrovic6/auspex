//! The fleet-wide Prometheus query: uptime, root disk, load and memory for every instance
//! in one round trip.
//!
//! The query, [`QUERY`], is a constant. It asks for the `node_exporter` metrics of **all**
//! instances and tags each series with an `auspex_metric` label; the instances the caller
//! asked for are selected in Rust. So no instance label from the config is ever spliced
//! into `PromQL`.
//!
//! A sample's age comes from `timestamp(node_boot_time_seconds)` (the time it was scraped:
//! an instant query stamps every *value* with the evaluation time, so the plain value's
//! timestamp would always look fresh) and a clock the caller passes in. An instance without
//! that series is left out, as if Prometheus had nothing for it, so the core falls back to
//! ssh rather than trusting values of unknown age.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use auspex_domain::observe::{DiskUsage, Load, Memory, PromSample};
use http::{Method, StatusCode};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::Cancelled;
use crate::http::{HttpClient, base_url, join};
use crate::text::last_line;

/// The largest answer read: 16 MiB (eleven series for each of hundreds of instances is well
/// under a megabyte).
pub const BODY_CAP: usize = 16 * 1024 * 1024;

/// The query.
///
/// Each part is labelled with `auspex_metric` so the parts stay apart under
/// `or` (which keeps a right-hand series only when its label set is new). The root
/// filesystem excludes `rootfs`, the initramfs's placeholder that some kernels still list.
pub const QUERY: &str = concat!(
    r#"label_replace(timestamp(node_boot_time_seconds), "auspex_metric", "scraped", "", "")"#,
    r#" or label_replace(node_time_seconds - node_boot_time_seconds, "auspex_metric", "uptime", "", "")"#,
    r#" or label_replace(node_filesystem_size_bytes{mountpoint="/",fstype!="rootfs"}, "auspex_metric", "fs_size", "", "")"#,
    r#" or label_replace(node_filesystem_free_bytes{mountpoint="/",fstype!="rootfs"}, "auspex_metric", "fs_free", "", "")"#,
    r#" or label_replace(node_filesystem_avail_bytes{mountpoint="/",fstype!="rootfs"}, "auspex_metric", "fs_avail", "", "")"#,
    r#" or label_replace(node_load1, "auspex_metric", "load1", "", "")"#,
    r#" or label_replace(node_load5, "auspex_metric", "load5", "", "")"#,
    r#" or label_replace(node_load15, "auspex_metric", "load15", "", "")"#,
    r#" or label_replace(node_memory_MemTotal_bytes, "auspex_metric", "mem_total", "", "")"#,
    r#" or label_replace(node_memory_MemAvailable_bytes, "auspex_metric", "mem_avail", "", "")"#,
);

/// Asks Prometheus for every instance's health and returns the requested ones. Serves the
/// core's `Effect::Prometheus`.
///
/// `url` is the server's base URL (`http` or `https`; a path prefix is kept). `clock` is
/// called once, when the answer has arrived, to compute each sample's age. An instance
/// Prometheus has no series for is absent from the map. Any failure (a bad URL, no
/// connection, a non-200 answer, Prometheus's own error, malformed JSON, the timeout) is an
/// `Err` with a reason to show.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired.
pub async fn query(
    client: &HttpClient,
    url: &str,
    instances: &BTreeSet<String>,
    clock: &(dyn Fn() -> SystemTime + Sync),
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Result<BTreeMap<String, PromSample>, String>, Cancelled> {
    let base = match base_url(url) {
        Ok(u) => u,
        Err(e) => return Ok(Err(format!("prometheus: {e}"))),
    };
    let mut url = join(&base, &["api", "v1", "query"]);
    url.query_pairs_mut().append_pair("query", QUERY);
    let (status, body) = match client
        .send(
            Method::GET,
            &url,
            "application/json",
            timeout,
            BODY_CAP,
            cancel,
        )
        .await?
    {
        Ok(r) => r,
        Err(e) => return Ok(Err(format!("prometheus: {e}"))),
    };
    let now = clock();
    // Prometheus answers its own errors with 400/422/503 and a JSON body that says why;
    // read that when there is one.
    let parsed = parse(&body, instances, now);
    Ok(match (status, parsed) {
        (StatusCode::OK, r) => r,
        (s, Err(e)) if e.starts_with("prometheus error") => Err(format!("{e} (HTTP {s})")),
        (s, _) => Err(format!("prometheus answered HTTP {s}")),
    })
}

/// One series' instance, metric and value, or `None` for anything unusable.
fn series(item: &Value) -> Option<(&str, &str, f64)> {
    let metric = item.get("metric")?;
    let instance = metric.get("instance")?.as_str()?;
    let which = metric.get("auspex_metric")?.as_str()?;
    let value = item.get("value")?.as_array()?;
    let v: f64 = value.get(1)?.as_str()?.parse().ok()?;
    v.is_finite().then_some((instance, which, v))
}

/// A non-negative finite value as whole units, rounding down; anything else is `None`.
fn whole(v: f64) -> Option<u64> {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "checked non-negative and finite; `as` saturates at u64::MAX"
    )]
    (v.is_finite() && v >= 0.0).then_some(v as u64)
}

/// Parses an instant-query answer into samples for the requested instances.
pub(crate) fn parse(
    body: &[u8],
    instances: &BTreeSet<String>,
    now: SystemTime,
) -> Result<BTreeMap<String, PromSample>, String> {
    let doc: Value =
        serde_json::from_slice(body).map_err(|e| format!("prometheus: malformed JSON: {e}"))?;
    match doc.get("status").and_then(Value::as_str) {
        Some("success") => {}
        Some("error") => {
            let what = doc.get("error").and_then(Value::as_str).unwrap_or("");
            let kind = doc.get("errorType").and_then(Value::as_str).unwrap_or("");
            return Err(format!(
                "prometheus error: {}",
                last_line(&format!("{kind}: {what}")).unwrap_or_default()
            ));
        }
        _ => return Err("prometheus: answer has no status".to_owned()),
    }
    let data = doc.get("data").ok_or("prometheus: answer has no data")?;
    if data.get("resultType").and_then(Value::as_str) != Some("vector") {
        return Err("prometheus: answer is not an instant vector".to_owned());
    }
    let result = data
        .get("result")
        .and_then(Value::as_array)
        .ok_or("prometheus: answer has no result list")?;

    // instance -> metric -> value. The first series for a pair wins (two exporters
    // scraping one instance would otherwise be averaged into nonsense).
    let mut by_instance: BTreeMap<&str, BTreeMap<&str, f64>> = BTreeMap::new();
    for item in result {
        let Some((instance, which, v)) = series(item) else {
            continue;
        };
        if !instances.contains(instance) {
            continue;
        }
        by_instance
            .entry(instance)
            .or_default()
            .entry(which)
            .or_insert(v);
    }

    let now_secs = now
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    let mut out = BTreeMap::new();
    for (instance, m) in by_instance {
        let Some(scraped) = m.get("scraped") else {
            continue;
        };
        // A sample from the future (clock skew) is as fresh as can be.
        let age_seconds = whole((now_secs - scraped).max(0.0)).unwrap_or(0);
        let get = |k: &str| m.get(k).copied().and_then(whole);
        let root_disk = match (get("fs_size"), get("fs_free"), get("fs_avail")) {
            (Some(total), Some(free), Some(avail)) => Some(DiskUsage {
                total_bytes: total,
                used_bytes: total.saturating_sub(free),
                available_bytes: avail,
            }),
            _ => None,
        };
        let load = match (m.get("load1"), m.get("load5"), m.get("load15")) {
            (Some(&one), Some(&five), Some(&fifteen))
                if one >= 0.0 && five >= 0.0 && fifteen >= 0.0 =>
            {
                Some(Load { one, five, fifteen })
            }
            _ => None,
        };
        let memory = match (get("mem_total"), get("mem_avail")) {
            (Some(total_bytes), Some(available_bytes)) => Some(Memory {
                total_bytes,
                available_bytes,
            }),
            _ => None,
        };
        out.insert(
            instance.to_owned(),
            PromSample {
                uptime_seconds: get("uptime"),
                root_disk,
                load,
                memory,
                age_seconds,
            },
        );
    }
    Ok(out)
}
