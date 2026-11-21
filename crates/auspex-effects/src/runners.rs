//! The runners view's forge adapter (AUS-19): the runners a forge lists and how many jobs wait.
//!
//! Two reads of Forgejo 15's API (`GET /api/v1/{admin|user|orgs/{org}|repos/{owner}/{repo}}/
//! actions/runners`, paged by `page` and `limit`, and `…/actions/runners/jobs`, which is not),
//! through the shared [`HttpClient`] and the AUS-18 token ([`Token`], [`crate::forge::guard`]): the same
//! timeout, no redirects, the token only over `https` or to a loopback host.
//!
//! # Bounds
//!
//! - **Time**: the whole of the runners listing shares one `timeout` (its pages included), and the
//!   jobs request has one of its own; they run side by side, so the answer is at most `timeout`
//!   late.
//! - **Size**: a page is read up to [`BODY_CAP`] bytes.
//! - **Count**: at most [`MAX_RUNNERS`] runners over at most [`MAX_PAGES`] pages, and at most
//!   [`MAX_JOBS`] jobs, counted while the body is read: past any of them the answer is an error,
//!   never a truncated list that would read as the truth.
//!
//! # What reaches output
//!
//! Every reason goes through [`reason_line`] (sanitised, guarded, cut to 300 characters), and so does
//! every text the forge sent that is shown (a runner's name, label, version): one that holds the
//! token is replaced. Texts are sanitised and
//! cut by [`ForgeRunner::new`]. No error quotes the forge's answer.
//!
//! The runners and the jobs fail independently: a token that may list runners and not jobs still
//! shows the runners, with the queue unknown.

use std::time::Duration;

use auspex_domain::runners::{ForgeAnswer, ForgeRunner, Scope};
use http::{Method, StatusCode};
use serde::Deserialize;
use serde::de::{SeqAccess, Visitor};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::Cancelled;
use crate::forge::{Token, authorization, reason_line};
use crate::http::{Call, HttpClient, base_url, join};

/// The largest page (or jobs list) read: 1 MiB.
///
/// A runner is about 400 bytes of JSON, so [`PAGE_SIZE`] of them are 20 KiB.
pub const BODY_CAP: usize = 1024 * 1024;

/// The page size asked for. Forgejo clamps it to its own maximum (50 by default), which is why a
/// short page does not end the listing: only an empty one, or one with nothing new, does.
pub const PAGE_SIZE: usize = 50;

/// The most pages read: 20.
pub const MAX_PAGES: usize = 20;

/// The most runners an answer may hold: 1,000.
pub const MAX_RUNNERS: usize = 1_000;

/// The most waiting jobs counted: 10,000.
pub const MAX_JOBS: usize = 10_000;

/// What replaces a runner text that held the token.
const WITHHELD_TEXT: &str = "(withheld)";

/// One forge entry's runners and queue, within `timeout`.
///
/// `url` is the forge's base URL ([`crate::forge::forge_url`] is what the config accepts).
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired; every other failure is inside the answer.
pub async fn list(
    client: &HttpClient,
    url: &str,
    scope: &Scope,
    token: Option<&Token>,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<ForgeAnswer, Cancelled> {
    let (runners, queued) = tokio::join!(
        runners(client, url, scope, token, timeout, cancel),
        queued(client, url, scope, token, timeout, cancel),
    );
    Ok(ForgeAnswer {
        runners: runners?.map_err(|r| reason_line(token, &r)),
        queued: queued?.map_err(|r| reason_line(token, &r)),
    })
}

fn endpoint(url: &str, scope: &Scope, tail: &[&str]) -> Result<Url, String> {
    let base = base_url(url).map_err(|e| format!("forge: {e}"))?;
    let mut segments = vec!["api", "v1"];
    segments.extend(scope.segments());
    segments.extend(["actions", "runners"]);
    segments.extend(tail);
    Ok(join(&base, &segments))
}

/// One GET, with the statuses every forge request answers alike.
async fn get(
    client: &HttpClient,
    url: &Url,
    token: Option<&Token>,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Result<hyper::body::Bytes, String>, Cancelled> {
    let authorization = match authorization(url, token) {
        Ok(a) => a,
        Err(e) => return Ok(Err(format!("forge: {e}"))),
    };
    let call = Call {
        method: Method::GET,
        url,
        accept: "application/json",
        timeout,
        body_cap: BODY_CAP,
        authorization: authorization.as_ref(),
    };
    let (status, body) = match client.send_call(&call, cancel).await? {
        Ok(r) => r,
        Err(e) => return Ok(Err(format!("forge: {e}"))),
    };
    Ok(match status {
        StatusCode::OK => Ok(body),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(if token.is_some() {
            format!(
                "forge: the forge refused the token (HTTP {status}): check token_file and that \
                 its scope can read runners (admin needs read:admin)"
            )
        } else {
            format!("forge: the forge wants a token (HTTP {status}): set token_file")
        }),
        StatusCode::NOT_FOUND => Err(
            "forge: no runners endpoint for this scope, or the token cannot see it (HTTP 404): \
             check scope and the forge's version"
                .to_owned(),
        ),
        s if s.is_redirection() => Err(format!(
            "forge: the forge redirected (HTTP {s}) and auspex does not follow redirects: check \
             url"
        )),
        s => Err(format!("forge: the forge answered HTTP {s}")),
    })
}

#[derive(Deserialize)]
struct Item {
    id: Option<i64>,
    name: Option<String>,
    status: Option<String>,
    labels: Option<Vec<String>>,
    version: Option<String>,
}

/// A JSON array read into at most `max` items; the `max + 1`th is an error, raised while the body
/// is read.
fn capped<T: for<'de> Deserialize<'de>>(body: &[u8], max: usize) -> Result<Vec<T>, String> {
    struct Capped<T>(usize, std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for Capped<T> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "a list of at most {} items", self.0)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            let mut v = Vec::new();
            while let Some(item) = seq.next_element::<T>()? {
                if v.len() == self.0 {
                    return Err(serde::de::Error::custom("too many items"));
                }
                v.push(item);
            }
            Ok(v)
        }
    }
    let mut de = serde_json::Deserializer::from_slice(body);
    let items =
        serde::Deserializer::deserialize_seq(&mut de, Capped::<T>(max, std::marker::PhantomData))
            .map_err(|e| {
            if e.to_string().contains("too many items") {
                format!("forge: more than {max} entries in one answer")
            } else {
                "forge: the answer is not a list of the kind expected".to_owned()
            }
        })?;
    de.end()
        .map_err(|_| "forge: the answer is not a list of the kind expected".to_owned())?;
    Ok(items)
}

fn shown(token: Option<&Token>, text: &str) -> String {
    if token.is_some_and(|t| t.appears_in(text)) {
        WITHHELD_TEXT.to_owned()
    } else {
        text.to_owned()
    }
}

/// Every runner the scope lists, page by page, all within one `timeout`.
async fn runners(
    client: &HttpClient,
    url: &str,
    scope: &Scope,
    token: Option<&Token>,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Result<Vec<ForgeRunner>, String>, Cancelled> {
    // The pages are each given the whole timeout and this is what bounds them together: a
    // forge that answers every page just in time still ends when the listing's time is up.
    tokio::time::timeout(timeout, pages(client, url, scope, token, timeout, cancel))
        .await
        .unwrap_or_else(|_| {
            Ok(Err(format!(
                "forge: no answer within {} ms",
                timeout.as_millis()
            )))
        })
}

async fn pages(
    client: &HttpClient,
    url: &str,
    scope: &Scope,
    token: Option<&Token>,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Result<Vec<ForgeRunner>, String>, Cancelled> {
    let mut found: Vec<ForgeRunner> = Vec::new();
    let mut ids = std::collections::BTreeSet::new();
    for page in 1..=MAX_PAGES {
        let mut target = match endpoint(url, scope, &[]) {
            Ok(u) => u,
            Err(e) => return Ok(Err(e)),
        };
        target
            .query_pairs_mut()
            .append_pair("page", &page.to_string())
            .append_pair("limit", &PAGE_SIZE.to_string());
        let body = match get(client, &target, token, timeout, cancel).await? {
            Ok(b) => b,
            Err(e) => return Ok(Err(e)),
        };
        let items: Vec<Item> = match capped(&body, MAX_RUNNERS) {
            Ok(i) => i,
            Err(e) => return Ok(Err(e)),
        };
        if items.is_empty() {
            return Ok(Ok(found));
        }
        let before = found.len();
        for item in items {
            // A forge that ignores `page` sends the same runners again: those are not new.
            if let Some(id) = item.id
                && !ids.insert(id)
            {
                continue;
            }
            if found.len() == MAX_RUNNERS {
                return Ok(Err(format!("forge: more than {MAX_RUNNERS} runners")));
            }
            let labels: Vec<String> = item
                .labels
                .unwrap_or_default()
                .iter()
                .map(|l| shown(token, l))
                .collect();
            found.push(ForgeRunner::new(
                &shown(token, item.name.as_deref().unwrap_or("")),
                item.status.as_deref().unwrap_or(""),
                &labels,
                item.version.as_deref().map(|v| shown(token, v)).as_deref(),
            ));
        }
        if found.len() == before {
            return Ok(Ok(found));
        }
    }
    Ok(Err(format!(
        "forge: more than {MAX_PAGES} pages of runners (limit {PAGE_SIZE} each)"
    )))
}

/// A waiting job as far as it is read: its status.
#[derive(Deserialize)]
struct Job {
    status: Option<String>,
}

/// How many jobs wait: those whose `status` is `waiting` (or that have none, the list being the
/// waiting ones by its contract). The payload has no times, so there is no age.
async fn queued(
    client: &HttpClient,
    url: &str,
    scope: &Scope,
    token: Option<&Token>,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Result<u64, String>, Cancelled> {
    let target = match endpoint(url, scope, &["jobs"]) {
        Ok(u) => u,
        Err(e) => return Ok(Err(e)),
    };
    let body = match get(client, &target, token, timeout, cancel).await? {
        Ok(b) => b,
        Err(e) => return Ok(Err(e)),
    };
    let jobs: Vec<Job> = match capped(&body, MAX_JOBS) {
        Ok(j) => j,
        Err(e) => {
            return Ok(Err(e.replace(
                &format!("more than {MAX_JOBS} entries in one answer"),
                &format!("more than {MAX_JOBS} waiting jobs"),
            )));
        }
    };
    let waiting = jobs
        .iter()
        .filter(|j| {
            j.status
                .as_deref()
                .is_none_or(|s| s.eq_ignore_ascii_case("waiting"))
        })
        .count();
    Ok(Ok(u64::try_from(waiting).unwrap_or(u64::MAX)))
}
