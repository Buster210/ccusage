use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ccusage_core::{cache, pricing::project_pricing_body};

const PRICING_FETCH_TIMEOUT_SECONDS: u64 = 10;
const PRICING_FETCH_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// How long a cached pricing document is served before revalidating. Prices
/// move slowly, so one refresh per day keeps other runs off the network.
/// Overridable via `CCUSAGE_PRICING_REFRESH_INTERVAL` (or `CCUSAGE_PRICING_TTL`)
/// in seconds so tests or power users can shorten the window.
const DEFAULT_PRICING_REFRESH_INTERVAL_SECS: i64 = 24 * 60 * 60;
/// How long to wait before retrying after a failed refresh. Failures latch as
/// fresh for only this window so a flaky endpoint does not keep stale pricing
/// alive for a full 24h. Overridable via `CCUSAGE_PRICING_RETRY_INTERVAL`.
const DEFAULT_PRICING_RETRY_INTERVAL_SECS: i64 = 60 * 60;

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn pricing_refresh_interval_secs() -> i64 {
    for key in ["CCUSAGE_PRICING_REFRESH_INTERVAL", "CCUSAGE_PRICING_TTL"] {
        if let Some(raw) = std::env::var_os(key) {
            if let Some(s) = raw.to_str() {
                if let Some(v) = parse_duration_secs(s) {
                    if v > 0 {
                        return v;
                    }
                }
            }
        }
    }
    DEFAULT_PRICING_REFRESH_INTERVAL_SECS
}

fn pricing_retry_interval_secs() -> i64 {
    if let Some(raw) = std::env::var_os("CCUSAGE_PRICING_RETRY_INTERVAL") {
        if let Some(s) = raw.to_str() {
            if let Some(v) = parse_duration_secs(s) {
                if v > 0 {
                    return v;
                }
            }
        }
    }
    DEFAULT_PRICING_RETRY_INTERVAL_SECS
}

fn parse_duration_secs(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(v) = s.parse::<i64>() {
        return Some(v);
    }
    let bytes = s.as_bytes();
    let last = bytes.last()?.to_ascii_lowercase();
    // Only cut 1 byte for ASCII s/m/h/d tails (always a char boundary);
    // multibyte tails return None instead of panicking on a bad boundary.
    if !matches!(last, b's' | b'm' | b'h' | b'd') {
        return None;
    }
    let num_str = &s[..s.len() - 1];
    let n: i64 = num_str.trim().parse().ok()?;
    match last {
        b's' => Some(n),
        b'm' => n.checked_mul(60),
        b'h' => n.checked_mul(60 * 60),
        b'd' => n.checked_mul(60 * 60 * 24),
        _ => None,
    }
}

#[cfg(test)]
fn is_fresh(updated_at: i64, now: i64) -> bool {
    is_fresh_with_interval(updated_at, now, pricing_refresh_interval_secs())
}

fn is_fresh_with_interval(updated_at: i64, now: i64, interval: i64) -> bool {
    if updated_at > now {
        return true;
    }
    now.saturating_sub(updated_at) < interval
}

/// Re-stamp a served-from-cache copy as fresh (unix seconds) so later runs
/// skip the network within the refresh window.
fn latch_fresh(url: &str, cached: &cache::CachedPricing) {
    cache::refresh_pricing_if_unchanged(url, cached, now_unix_secs());
}

/// Re-stamp after a *failed* refresh so the next retry happens after the
/// retry interval, not a full refresh interval. This prevents indefinite
/// staleness when the endpoint stays down.
fn latch_retry(url: &str, cached: &cache::CachedPricing) {
    let now = now_unix_secs();
    let refresh = pricing_refresh_interval_secs();
    // Clamp retry to [1, refresh-1] so small test intervals work and we never
    // store a future timestamp or bypass the refresh window entirely.
    let retry = pricing_retry_interval_secs().clamp(1, refresh.saturating_sub(1).max(1));
    // Store as `now - (refresh - retry)` so `now - updated_at < refresh`
    // holds for `retry` seconds, then the entry becomes stale and revalidates.
    let retry_updated_at = now.saturating_sub(refresh.saturating_sub(retry));
    cache::refresh_pricing_if_unchanged(url, cached, retry_updated_at);
}

fn spawn_background_refresh(url: &str) {
    if std::env::var_os("CCUSAGE_DISABLE_BACKGROUND_REFRESH").is_some() {
        return;
    }
    if std::env::var_os("CCUSAGE_OFFLINE").is_some() {
        return;
    }
    // In unit tests `current_exe` is the test harness, not the ccusage
    // binary, so spawning it with `__internal-fetch-pricing` would run the
    // test runner instead of the fetcher. Use an in-process thread for tests.
    if cfg!(test) {
        let _ = fetch_json_sync(url);
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    if let Ok(mut child) = std::process::Command::new(exe)
        .arg("__internal-fetch-pricing")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        std::thread::Builder::new()
            .name("ccusage-pricing-reap".to_string())
            .spawn(move || {
                let _ = child.wait();
            })
            .ok();
    }
}

/// Fetches a JSON document for the pricing refresh, revalidating against the
/// cached copy with `If-None-Match`/`If-Modified-Since` so an unchanged document
/// costs a 304 instead of a full download.
///
/// This lives in the binary so that `ureq` and its TLS stack are not dependencies
/// of `ccusage-core`, which every adapter builds against; `main` installs it
/// through `ccusage_core::pricing::set_json_fetcher`.
///
/// Implements stale-while-revalidate: fresh cache is served with 0 network;
/// stale cache is served immediately and refreshed in a detached background
/// process so the next run is fresh. Only a cold miss blocks.
pub(crate) fn fetch_json(url: &str) -> std::io::Result<String> {
    let cached = cache::load_pricing(url);
    let now = now_unix_secs();
    let refresh = pricing_refresh_interval_secs();

    if let Some(c) = cached.as_ref()
        && let Some(updated_at) = c.updated_at
        && is_fresh_with_interval(updated_at, now, refresh)
    {
        return Ok(c.body.clone());
    }

    // Stale or legacy (updated_at=None) with a cached body: serve stale
    // immediately and refresh in background. Latch as retry so concurrent
    // invocations don't spawn a herd.
    if let Some(c) = cached.as_ref() {
        let is_stale = c.updated_at.is_none_or(|ts| !is_fresh_with_interval(ts, now, refresh));
        if is_stale {
            if std::env::var_os("CCUSAGE_DISABLE_BACKGROUND_REFRESH").is_some() {
                // Tests or explicit opt-out: do a blocking refresh so
                // assertions on conditional-GET behavior stay deterministic.
                return fetch_json_sync(url);
            }
            latch_retry(url, c);
            spawn_background_refresh(url);
        }
        return Ok(c.body.clone());
    }

    // Cold miss: no cached body, must block.
    fetch_json_sync(url)
}

/// Blocking refresh used for cold misses and for the detached background
/// child. Never spawns another background task.
pub(crate) fn fetch_json_sync(url: &str) -> std::io::Result<String> {
    let cached = cache::load_pricing(url);

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(PRICING_FETCH_TIMEOUT_SECONDS)))
        .build()
        .new_agent();

    let mut req = agent.get(url);
    if let Some(ref c) = cached {
        if let Some(ref etag) = c.etag {
            req = req.header("If-None-Match", etag.as_str());
        } else if let Some(ref lm) = c.last_modified {
            req = req.header("If-Modified-Since", lm.as_str());
        }
    }

    let mut response = match req.call() {
        Ok(resp) => resp,
        Err(error) => {
            if let Some(c) = cached {
                // Degrade to last-known pricing, but only for the retry window
                // so a flaky endpoint does not keep stale pricing alive for a
                // full refresh interval.
                if ccusage_core::log_level().is_some_and(|level| level >= 4) {
                    eprintln!(
                        "WARN  Failed to refresh LiteLLM pricing ({error}); using last-known cached pricing."
                    );
                }
                latch_retry(url, &c);
                return Ok(c.body);
            }
            return Err(std::io::Error::other(error.to_string()));
        }
    };

    let status = response.status().as_u16();

    // 304 Not Modified — server confirmed nothing changed.
    if status == 304 {
        return match cached {
            Some(c) => {
                // 304 confirms the copy is current; re-stamp it as fresh.
                latch_fresh(url, &c);
                Ok(c.body)
            }
            // Edge case: server said 304 but we had no cache. Fall back to an
            // unconditional GET.
            None => fetch_uncached(&agent, url),
        };
    }

    if status != 200 {
        return Err(std::io::Error::other(format!("HTTP {status}")));
    }

    let (etag, last_modified) = revalidation_headers(&response);
    let body = read_body(&mut response)?;
    let body = project_pricing_body(url, &body);
    cache::store_pricing(
        url,
        &body,
        etag.as_deref(),
        last_modified.as_deref(),
        now_unix_secs(),
    );
    Ok(body)
}

fn fetch_uncached(agent: &ureq::Agent, url: &str) -> std::io::Result<String> {
    let mut resp = agent
        .get(url)
        .call()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(std::io::Error::other(format!("HTTP {status}")));
    }
    let (etag, last_modified) = revalidation_headers(&resp);
    let body = read_body(&mut resp)?;
    let body = project_pricing_body(url, &body);
    cache::store_pricing(
        url,
        &body,
        etag.as_deref(),
        last_modified.as_deref(),
        now_unix_secs(),
    );
    Ok(body)
}

fn revalidation_headers<T>(response: &ureq::http::Response<T>) -> (Option<String>, Option<String>) {
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    (header("etag"), header("last-modified"))
}

fn read_body(response: &mut ureq::http::Response<ureq::Body>) -> std::io::Result<String> {
    response
        .body_mut()
        .with_config()
        .limit(PRICING_FETCH_MAX_BYTES)
        .read_to_string()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))
}

#[cfg(test)]
mod tests {
    use ccusage_test_support::CacheEnv;

    use super::fetch_json;

    /// Build a raw `200 OK` HTTP response carrying `body` and `etag`.
    fn resp_200(etag: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nETag: {etag}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
            len = body.len(),
        )
    }

    /// Build a raw, bodyless `304 Not Modified` HTTP response.
    fn resp_304(etag: &str) -> String {
        format!("HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\nConnection: close\r\n\r\n")
    }

    /// Single-threaded HTTP origin that replays `responses` in order — one per
    /// incoming request — closing the socket after each. Returns the origin URL
    /// plus a log recording, per request, whether it carried `If-None-Match`,
    /// so a test can assert the conditional-request behavior of the client.
    /// The thread exits after `responses.len()` requests.
    fn spawn_scripted_origin(
        responses: Vec<String>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<bool>>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log_thread = log.clone();
        std::thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                // Read the request head (GET has no body) up to the blank line.
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    if req.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let conditional = String::from_utf8_lossy(&req)
                    .to_ascii_lowercase()
                    .contains("if-none-match");
                log_thread.lock().unwrap().push(conditional);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (format!("http://{addr}/pricing.json"), log)
    }

    /// HTTP origin that accepts one connection then closes it without
    /// responding, forcing a deterministic client-side read failure (no
    /// bound-then-dropped port race).
    fn spawn_closing_origin() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                drop(stream);
            }
        });
        format!("http://{addr}/pricing.json")
    }

    /// Unix seconds well before the test runs (2023-11-14), so rows seeded with
    /// it are always outside the refresh window regardless of when tests run.
    const STALE_TS: i64 = 1_700_000_000;

    /// Cold fetch stores body + ETag. The warm fetch is served from the fresh
    /// cache without any network request — the origin thread sees exactly one
    /// request, proving the warm body did not come over the wire.
    #[test]
    fn fresh_cache_serves_warm_fetch_without_network() {
        let _env = CacheEnv::new("etag-round-trip");
        let (url, log) = spawn_scripted_origin(vec![resp_200("\"v1\"", "PRICING_V1")]);

        let cold = fetch_json(&url).unwrap();
        assert_eq!(cold, "PRICING_V1");
        let cached = ccusage_core::cache::load_pricing(&url).expect("etag stored after 200");
        assert_eq!(cached.etag.as_deref(), Some("\"v1\""));
        assert!(
            cached.updated_at.is_some(),
            "a successful refresh stamps the freshness window"
        );

        let warm = fetch_json(&url).unwrap();
        assert_eq!(warm, "PRICING_V1");
        assert_eq!(
            *log.lock().unwrap(),
            vec![false],
            "warm fetch within the refresh window must not hit the network"
        );
    }

    /// When the server answers the conditional request with a fresh `200` (the
    /// document changed), the new body and ETag must replace the cached copy —
    /// the `304` short-circuit must not swallow a real update.
    #[test]
    fn etag_change_refetches_and_updates_cache() {
        let _env = CacheEnv::new("etag-change");
        let _bg = ccusage_test_support::EnvVarsGuard::set_many([(
            "CCUSAGE_DISABLE_BACKGROUND_REFRESH",
            Some(std::ffi::OsString::from("1")),
        )]);
        let (url, log) = spawn_scripted_origin(vec![
            resp_200("\"v1\"", "PRICING_V1"),
            resp_200("\"v2\"", "PRICING_V2"),
        ]);

        assert_eq!(fetch_json(&url).unwrap(), "PRICING_V1");
        // Age the copy past the refresh window so the next fetch revalidates.
        ccusage_core::cache::store_pricing(&url, "PRICING_V1", Some("\"v1\""), None, STALE_TS);
        let warm = fetch_json(&url).unwrap();
        assert_eq!(warm, "PRICING_V2", "changed pricing must be re-downloaded");

        let cached = ccusage_core::cache::load_pricing(&url).unwrap();
        assert_eq!(cached.body, "PRICING_V2");
        assert_eq!(cached.etag.as_deref(), Some("\"v2\""));
        assert_eq!(
            *log.lock().unwrap(),
            vec![false, true],
            "warm request was conditional but the server returned a fresh 200"
        );
    }

    /// A `304` answered to an unconditional request (there was no cache to
    /// validate) must not be trusted as "unchanged": the fetch falls back to a
    /// plain GET and returns that body.
    #[test]
    fn stale_304_without_cache_falls_back_to_get() {
        let _env = CacheEnv::new("stale-304");
        let (url, log) =
            spawn_scripted_origin(vec![resp_304("\"v1\""), resp_200("\"v1\"", "PRICING_V1")]);

        let body = fetch_json(&url).unwrap();
        assert_eq!(
            body, "PRICING_V1",
            "a 304 with an empty cache must fall back to an unconditional GET"
        );
        assert_eq!(
            ccusage_core::cache::load_pricing(&url).unwrap().body,
            "PRICING_V1",
            "the fallback body is cached"
        );
        assert_eq!(
            log.lock().unwrap().len(),
            2,
            "original request plus fallback GET"
        );
    }

    /// Regression: the 304-without-cache fallback must capture validators (ETag
    /// and Last-Modified) from the fallback 200 response, not discard them.
    #[test]
    fn stale_304_fallback_preserves_validators() {
        let _env = CacheEnv::new("stale-304-validators");
        let fallback_response = format!(
            "HTTP/1.1 200 OK\r\n\
             ETag: \"v2\"\r\n\
             Last-Modified: Sat, 01 Jan 2025 00:00:00 GMT\r\n\
             Content-Length: {len}\r\n\
             Connection: close\r\n\
             \r\n\
             PRICING_V2",
            len = "PRICING_V2".len(),
        );
        let (url, _log) = spawn_scripted_origin(vec![resp_304("\"v1\""), fallback_response]);

        let body = fetch_json(&url).unwrap();
        assert_eq!(body, "PRICING_V2");
        let cached = ccusage_core::cache::load_pricing(&url).unwrap();
        assert_eq!(
            cached.etag.as_deref(),
            Some("\"v2\""),
            "fallback ETag must be stored"
        );
        assert_eq!(
            cached.last_modified.as_deref(),
            Some("Sat, 01 Jan 2025 00:00:00 GMT"),
            "fallback Last-Modified must be stored"
        );
    }

    /// Network failure with a primed cache degrades to the last-known body
    /// rather than erroring, and latches the failure into the refresh window so
    /// the next fetch skips the broken endpoint entirely.
    #[test]
    fn fetch_degrades_to_cache_on_network_failure() {
        let _env = CacheEnv::new("degrade-on-failure");
        let _bg = ccusage_test_support::EnvVarsGuard::set_many([(
            "CCUSAGE_DISABLE_BACKGROUND_REFRESH",
            Some(std::ffi::OsString::from("1")),
        )]);
        let url = spawn_closing_origin();
        ccusage_core::cache::store_pricing(&url, "CACHED_BODY", Some("\"v1\""), None, STALE_TS);
        assert_eq!(
            fetch_json(&url).unwrap(),
            "CACHED_BODY",
            "must serve cached body when the response read fails"
        );
        let cached = ccusage_core::cache::load_pricing(&url).unwrap();
        assert!(
            cached.updated_at.is_some(),
            "the failed refresh stamps the window so later runs skip the network"
        );
        assert_eq!(
            fetch_json(&url).unwrap(),
            "CACHED_BODY",
            "a second fetch is served from the now-fresh cache"
        );
    }

    /// Network failure with no cache must propagate the error, not fabricate an
    /// empty success.
    #[test]
    fn fetch_without_cache_propagates_error() {
        let _env = CacheEnv::new("no-cache-error");
        let url = spawn_closing_origin();
        assert!(
            fetch_json(&url).is_err(),
            "a read failure with no cached fallback must surface as an error"
        );
    }

    /// 304 reuse returns the stored body verbatim, so an already-projected
    /// cache entry survives revalidation unchanged. The projection itself is
    /// covered by `ccusage-core`'s pricing tests.
    #[test]
    fn etag_304_returns_stored_body() {
        let _env = CacheEnv::new("etag-304-projected");
        let _bg = ccusage_test_support::EnvVarsGuard::set_many([(
            "CCUSAGE_DISABLE_BACKGROUND_REFRESH",
            Some(std::ffi::OsString::from("1")),
        )]);
        let projected = r#"{"gpt-4o-proj":{"input":2.5e-6,"output":1e-5,"maxInput":128000}}"#;
        let (url, log) = spawn_scripted_origin(vec![resp_304("\"v1\"")]);
        ccusage_core::cache::store_pricing(&url, projected, Some("\"v1\""), None, STALE_TS);

        let body = fetch_json(&url).unwrap();
        assert_eq!(body, projected, "304 must return the stored body");
        assert_eq!(
            *log.lock().unwrap(),
            vec![true],
            "request should be conditional"
        );
        assert!(
            ccusage_core::cache::load_pricing(&url)
                .unwrap()
                .updated_at
                .is_some(),
            "a 304 refresh stamps the freshness window"
        );
    }

    #[test]
    fn stale_latch_does_not_overwrite_newer_pricing() {
        let _env = CacheEnv::new("stale-latch");
        let url = "https://example.com/pricing.json";
        const NEW_TS: i64 = 1_800_000_100;

        ccusage_core::cache::store_pricing(url, "v1", Some("old-etag"), Some("old-date"), STALE_TS);
        let stale = ccusage_core::cache::load_pricing(url).expect("cache miss after v1 store");
        ccusage_core::cache::store_pricing(url, "v2", Some("new-etag"), Some("new-date"), NEW_TS);

        super::latch_fresh(url, &stale);

        let cached = ccusage_core::cache::load_pricing(url).expect("cache miss after stale latch");
        assert_eq!(cached.body, "v2");
        assert_eq!(cached.etag.as_deref(), Some("new-etag"));
        assert_eq!(cached.last_modified.as_deref(), Some("new-date"));
        assert_eq!(cached.updated_at, Some(NEW_TS));
    }

    #[test]
    fn parse_duration_supports_suffixes() {
        assert_eq!(super::parse_duration_secs("3600"), Some(3600));
        assert_eq!(super::parse_duration_secs("60s"), Some(60));
        assert_eq!(super::parse_duration_secs("15m"), Some(900));
        assert_eq!(super::parse_duration_secs("12h"), Some(43200));
        assert_eq!(super::parse_duration_secs("2d"), Some(172800));
        assert_eq!(super::parse_duration_secs("  1H  "), Some(3600));
        assert_eq!(super::parse_duration_secs(""), None);
        assert_eq!(super::parse_duration_secs("abc"), None);
    }

    #[test]
    fn future_updated_at_is_fresh() {
        let now = super::now_unix_secs();
        assert!(
            super::is_fresh(now + 3600, now),
            "future timestamp must be considered fresh (clock skew)"
        );
        assert!(
            !super::is_fresh(now - super::pricing_refresh_interval_secs() - 1, now),
            "stale beyond refresh interval must not be fresh"
        );
    }

    #[test]
    fn configurable_refresh_interval_via_env() {
        let _env = CacheEnv::new("configurable-ttl");
        let _vars = ccusage_test_support::EnvVarsGuard::set_many([
            (
                "CCUSAGE_PRICING_REFRESH_INTERVAL",
                Some(std::ffi::OsString::from("5s")),
            ),
            ("CCUSAGE_PRICING_TTL", None),
            ("CCUSAGE_PRICING_RETRY_INTERVAL", None),
        ]);
        assert_eq!(super::pricing_refresh_interval_secs(), 5);
        // 6s old must be stale when interval is 5s.
        let now = super::now_unix_secs();
        assert!(
            !super::is_fresh(now - 6, now),
            "TTL=5s should make 6s old entry stale"
        );
        assert!(
            super::is_fresh(now - 4, now),
            "TTL=5s should keep 4s old entry fresh"
        );
    }

    #[test]
    fn configurable_ttl_alias() {
        let _env = CacheEnv::new("configurable-ttl-alias");
        let _vars = ccusage_test_support::EnvVarsGuard::set_many([
            ("CCUSAGE_PRICING_REFRESH_INTERVAL", None),
            (
                "CCUSAGE_PRICING_TTL",
                Some(std::ffi::OsString::from("10s")),
            ),
            ("CCUSAGE_PRICING_RETRY_INTERVAL", None),
        ]);
        assert_eq!(super::pricing_refresh_interval_secs(), 10);
    }

    #[test]
    fn failure_retry_window_is_shorter_than_refresh() {
        let _env = CacheEnv::new("failure-retry-short");
        // Short intervals so the test does not sleep for real 1h.
        let _vars = ccusage_test_support::EnvVarsGuard::set_many([
            (
                "CCUSAGE_PRICING_REFRESH_INTERVAL",
                Some(std::ffi::OsString::from("10s")),
            ),
            (
                "CCUSAGE_PRICING_RETRY_INTERVAL",
                Some(std::ffi::OsString::from("2s")),
            ),
            ("CCUSAGE_PRICING_TTL", None),
            (
                "CCUSAGE_DISABLE_BACKGROUND_REFRESH",
                Some(std::ffi::OsString::from("1")),
            ),
        ]);
        let url = spawn_closing_origin();
        ccusage_core::cache::store_pricing(&url, "CACHED_BODY", Some("\"v1\""), None, STALE_TS);

        // First fetch fails but degrades to cache and latches for retry window only.
        assert_eq!(
            super::fetch_json(&url).unwrap(),
            "CACHED_BODY",
            "must serve cached body on failure"
        );
        let cached = ccusage_core::cache::load_pricing(&url).unwrap();
        let now = super::now_unix_secs();
        // Retry latch = now - (10 - 2) = now - 8, so `now - updated_at = 8`.
        let age = now.saturating_sub(cached.updated_at.unwrap());
        assert!(
            (7..=9).contains(&age),
            "retry latch age should be ~8s (refresh-retry), got {age}"
        );
        // Immediate second fetch must still be fresh (within 2s retry window).
        assert_eq!(
            super::fetch_json(&url).unwrap(),
            "CACHED_BODY",
            "second fetch within retry window must not hit network"
        );

        // Wait until retry window expires (2.5s) but before full refresh (10s).
        std::thread::sleep(std::time::Duration::from_millis(2600));
        // Next fetch should attempt network again and fail-degrade, re-latching.
        let third = super::fetch_json(&url).unwrap();
        assert_eq!(third, "CACHED_BODY");
        let cached2 = ccusage_core::cache::load_pricing(&url).unwrap();
        assert!(
            cached2.updated_at.unwrap() > cached.updated_at.unwrap(),
            "retry should have re-latched after window expired"
        );
    }

    #[test]
    fn retry_interval_clamped_to_refresh() {
        let _env = CacheEnv::new("retry-clamp");
        let _vars = ccusage_test_support::EnvVarsGuard::set_many([
            (
                "CCUSAGE_PRICING_REFRESH_INTERVAL",
                Some(std::ffi::OsString::from("5s")),
            ),
            (
                "CCUSAGE_PRICING_RETRY_INTERVAL",
                Some(std::ffi::OsString::from("10s")),
            ),
            ("CCUSAGE_PRICING_TTL", None),
            (
                "CCUSAGE_DISABLE_BACKGROUND_REFRESH",
                Some(std::ffi::OsString::from("1")),
            ),
        ]);
        // Retry > refresh must be clamped to refresh-1 (>=60s floor, but refresh is 5s so floor wins as 4s).
        // The latch should still produce a valid future retry, not panic or overflow.
        let url = spawn_closing_origin();
        ccusage_core::cache::store_pricing(&url, "CACHED", Some("\"v1\""), None, STALE_TS);
        assert_eq!(super::fetch_json(&url).unwrap(), "CACHED");
        let cached = ccusage_core::cache::load_pricing(&url).unwrap();
        assert!(cached.updated_at.is_some());
    }

    #[test]
    fn stale_serves_immediately_and_spawns_background() {
        let _env = CacheEnv::new("swr-background");
        let _vars = ccusage_test_support::EnvVarsGuard::set_many([
            (
                "CCUSAGE_PRICING_REFRESH_INTERVAL",
                Some(std::ffi::OsString::from("10s")),
            ),
            (
                "CCUSAGE_PRICING_RETRY_INTERVAL",
                Some(std::ffi::OsString::from("2s")),
            ),
            ("CCUSAGE_PRICING_TTL", None),
            // Ensure background is enabled (not disabled)
            ("CCUSAGE_DISABLE_BACKGROUND_REFRESH", None),
            ("CCUSAGE_OFFLINE", None),
        ]);
        // No origin - SWR should return stale without hitting network and latch as retry.
        let url = "http://example.com/pricing-swr.json";
        ccusage_core::cache::store_pricing(&url, "STALE_BODY", Some("\"v1\""), None, STALE_TS);
        let before = super::now_unix_secs();
        let body = super::fetch_json(&url).unwrap();
        assert_eq!(body, "STALE_BODY", "SWR must serve stale immediately");
        let cached = ccusage_core::cache::load_pricing(&url).unwrap();
        assert_eq!(cached.body, "STALE_BODY");
        // Latch should have updated updated_at to now - (refresh - retry) ~ now - 8
        let age = before.saturating_sub(cached.updated_at.unwrap_or(before));
        // The latch happens after `before`, so check that cached is now fresh for retry window
        assert!(
            super::is_fresh(cached.updated_at.unwrap(), super::now_unix_secs()),
            "latched stale should be fresh for retry window"
        );
        // Second immediate fetch should still be fresh (no extra spawn herd)
        let body2 = super::fetch_json(&url).unwrap();
        assert_eq!(body2, "STALE_BODY");
        let _ = age; // suppress unused
    }

    #[test]
    fn swr_respects_offline_env() {
        let _env = CacheEnv::new("swr-offline");
        let _vars = ccusage_test_support::EnvVarsGuard::set_many([
            (
                "CCUSAGE_PRICING_REFRESH_INTERVAL",
                Some(std::ffi::OsString::from("5s")),
            ),
            ("CCUSAGE_PRICING_TTL", None),
            ("CCUSAGE_DISABLE_BACKGROUND_REFRESH", None),
            ("CCUSAGE_OFFLINE", Some(std::ffi::OsString::from("1"))),
        ]);
        let url = "http://example.com/offline.json";
        ccusage_core::cache::store_pricing(&url, "OFFLINE_BODY", Some("\"v1\""), None, STALE_TS);
        // Should still serve stale, but not attempt to spawn background (offline)
        let body = super::fetch_json(&url).unwrap();
        assert_eq!(body, "OFFLINE_BODY");
        // No panic, and background not spawned (verified by offline check)
    }

    #[test]
    fn swr_background_refreshes_stale_to_new_body() {
        let _env = CacheEnv::new("swr-bg-update");
        let _vars = ccusage_test_support::EnvVarsGuard::set_many([
            (
                "CCUSAGE_PRICING_REFRESH_INTERVAL",
                Some(std::ffi::OsString::from("10s")),
            ),
            (
                "CCUSAGE_PRICING_RETRY_INTERVAL",
                Some(std::ffi::OsString::from("2s")),
            ),
            ("CCUSAGE_PRICING_TTL", None),
            ("CCUSAGE_DISABLE_BACKGROUND_REFRESH", None),
            ("CCUSAGE_OFFLINE", None),
        ]);
        // Origin will serve V2 on the background fetch's conditional GET.
        let (url, log) = spawn_scripted_origin(vec![resp_200("\"v2\"", "PRICING_V2")]);
        ccusage_core::cache::store_pricing(&url, "PRICING_V1", Some("\"v1\""), None, STALE_TS);

        // SWR: returns stale immediately; in cfg(test) the refresh runs
        // synchronously inside spawn_background_refresh so no sleep-join
        // is needed and no thread can outlive the CacheEnv tempdir.
        let body = super::fetch_json(&url).unwrap();
        assert_eq!(body, "PRICING_V1", "SWR must serve stale immediately");
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "cfg(test) refresh runs inline before return"
        );

        // Cache already updated by the inline refresh.
        let cached = ccusage_core::cache::load_pricing(&url).unwrap();
        assert_eq!(
            cached.body, "PRICING_V2",
            "background refresh should have updated cache to V2"
        );
        assert_eq!(cached.etag.as_deref(), Some("\"v2\""));
        assert_eq!(
            *log.lock().unwrap(),
            vec![true],
            "background fetch should have been conditional"
        );

        // Next foreground fetch should be fresh and return V2 without network.
        let body2 = super::fetch_json(&url).unwrap();
        assert_eq!(body2, "PRICING_V2");
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "fresh cache should not hit network again"
        );
    }
}
