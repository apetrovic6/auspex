//! Hostname resolution for the shared-IP lint.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::time::Duration;

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::Cancelled;

/// Resolves each machine's hostname. Serves the core's `Effect::ResolveAddresses`.
///
/// `hosts` maps a machine to its target's hostname. Names are resolved concurrently through
/// the system resolver (`getaddrinfo`, so `/etc/hosts` and the configured DNS apply), each
/// within `per_name`. A name that does not resolve, or not in time, is simply absent from
/// the result. Addresses come back sorted and without duplicates.
///
/// A lookup that times out keeps its resolver thread busy until `getaddrinfo` returns: the
/// call cannot be interrupted, only abandoned.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired.
pub async fn resolve(
    hosts: &BTreeMap<String, String>,
    per_name: Duration,
    cancel: &CancellationToken,
) -> Result<BTreeMap<String, Vec<IpAddr>>, Cancelled> {
    resolve_with(hosts, per_name, cancel, |host| async move {
        tokio::net::lookup_host((host.as_str(), 0))
            .await
            .map(|addrs| addrs.map(|a| a.ip()).collect())
    })
    .await
}

/// [`resolve`] with the lookup supplied, so tests can stand in for the system resolver.
pub(crate) async fn resolve_with<F, Fut>(
    hosts: &BTreeMap<String, String>,
    per_name: Duration,
    cancel: &CancellationToken,
    lookup: F,
) -> Result<BTreeMap<String, Vec<IpAddr>>, Cancelled>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = io::Result<Vec<IpAddr>>> + Send + 'static,
{
    // Dropping the set (on cancel) aborts every lookup still running.
    let mut set = JoinSet::new();
    for (machine, host) in hosts {
        let machine = machine.clone();
        let fut = lookup(host.clone());
        set.spawn(async move {
            match tokio::time::timeout(per_name, fut).await {
                Ok(Ok(mut ips)) if !ips.is_empty() => {
                    ips.sort_unstable();
                    ips.dedup();
                    Some((machine, ips))
                }
                _ => None,
            }
        });
    }
    let mut out = BTreeMap::new();
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(Cancelled),
            next = set.join_next() => match next {
                None => return Ok(out),
                Some(Ok(Some((machine, ips)))) => {
                    out.insert(machine, ips);
                }
                Some(_) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(m, h)| ((*m).to_owned(), (*h).to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn resolves_literals_and_drops_failures_and_slow_names() {
        let cancel = CancellationToken::new();
        let h = hosts(&[("a", "one"), ("b", "fails"), ("c", "slow"), ("d", "dup")]);
        let out = resolve_with(&h, Duration::from_millis(200), &cancel, |host| async move {
            match host.as_str() {
                "one" => Ok(vec![
                    "10.0.0.2".parse().unwrap(),
                    "10.0.0.1".parse().unwrap(),
                ]),
                "dup" => Ok(vec![
                    "10.0.0.1".parse().unwrap(),
                    "10.0.0.1".parse().unwrap(),
                ]),
                "slow" => {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(vec!["10.0.0.9".parse().unwrap()])
                }
                _ => Err(io::Error::other("no such host")),
            }
        })
        .await
        .unwrap();
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(
            out["a"],
            [
                "10.0.0.1".parse::<IpAddr>().unwrap(),
                "10.0.0.2".parse().unwrap()
            ]
        );
        assert_eq!(out["d"], ["10.0.0.1".parse::<IpAddr>().unwrap()]);
    }

    #[tokio::test]
    async fn cancel_returns_at_once() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let h = hosts(&[("a", "slow")]);
        let out = resolve_with(&h, Duration::from_secs(30), &cancel, |_| async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(vec![])
        })
        .await;
        assert_eq!(out, Err(Cancelled));
    }

    /// `resolve` end to end on IP literals. tokio's `lookup_host` parses a literal itself, so
    /// this does not reach `getaddrinfo` or the blocking pool: the system resolver path is
    /// untested, because every name it would resolve could leave the machine.
    #[tokio::test]
    async fn ip_literals_resolve_to_themselves() {
        let cancel = CancellationToken::new();
        let h = hosts(&[("lit", "127.0.0.1"), ("v6", "::1")]);
        let out = resolve(&h, Duration::from_secs(5), &cancel).await.unwrap();
        assert_eq!(out["lit"], ["127.0.0.1".parse::<IpAddr>().unwrap()]);
        assert_eq!(out["v6"], ["::1".parse::<IpAddr>().unwrap()]);
    }
}
