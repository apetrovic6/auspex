//! The "already cached" badge: is a store path in the binary cache (ncps)?

use std::time::Duration;

use auspex_domain::observe::StorePath;
use auspex_domain::status::NarCache;
use http::{Method, StatusCode};
use tokio_util::sync::CancellationToken;

use crate::Cancelled;
use crate::http::{HttpClient, base_url, join};

/// `HEAD <cache_url>/<hash>.narinfo` for `path`. Serves the core's `Effect::CacheLookup`.
///
/// 200 is [`NarCache::Cached`], 404 is [`NarCache::NotCached`], and anything else (another
/// status, a redirect, no connection, a bad URL, the timeout) is [`NarCache::Failed`].
/// The hash is the store path's 32-character hash, which `StorePath` has already checked
/// is alphanumeric, so it cannot change the URL's shape.
///
/// # Errors
///
/// [`Cancelled`] when `cancel` fired.
pub async fn lookup(
    client: &HttpClient,
    cache_url: &str,
    path: &StorePath,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<NarCache, Cancelled> {
    let Ok(base) = base_url(cache_url) else {
        return Ok(NarCache::Failed);
    };
    let url = join(&base, &[&format!("{}.narinfo", path.hash())]);
    Ok(
        match client
            .send(Method::HEAD, &url, "text/x-nix-narinfo", timeout, 0, cancel)
            .await?
        {
            Ok((StatusCode::OK, _)) => NarCache::Cached,
            Ok((StatusCode::NOT_FOUND, _)) => NarCache::NotCached,
            Ok(_) | Err(_) => NarCache::Failed,
        },
    )
}
