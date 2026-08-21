use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ccusage_core::{cache, pricing::project_pricing_body};

const PRICING_FETCH_TIMEOUT_SECONDS: u64 = 10;
const PRICING_FETCH_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// How long a cached pricing document is served before revalidating. Prices
/// move slowly, so one refresh per day keeps other runs off the network.
const PRICING_REFRESH_INTERVAL_SECS: i64 = 24 * 60 * 60;

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Re-stamp a served-from-cache copy as fresh (unix seconds) so later runs
/// skip the network within the refresh window.
fn latch_fresh(url: &str, cached: &cache::CachedPricing) {
    cache::refresh_pricing_if_unchanged(url, cached, now_unix_secs());
}

/// Fetches a JSON document for the pricing refresh, revalidating against the
/// cached copy with `If-None-Match`/`If-Modified-Since` so an unchanged document
/// costs a 304 instead of a full download.
///
/// This lives in the binary so that `ureq` and its TLS stack are not dependencies
/// of `ccusage-core`, which every adapter builds against; `main` installs it
/// through `ccusage_core::pricing::set_json_fetcher`.
pub(crate) fn fetch_json(url: &str) -> std::io::Result<String> {
    let cached = cache::load_pricing(url);

    // Serve a recently refreshed copy without touching the network. Revalidating
    // on every run turns each invocation into a latency bet on the endpoint.
    if let Some(c) = cached.as_ref()
        && let Some(updated_at) = c.updated_at
        && now_unix_secs().saturating_sub(updated_at) < PRICING_REFRESH_INTERVAL_SECS
    {
        return Ok(c.body.clone());
    }

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
                // Degrade to last-known pricing, latching into the freshness
                // window so a flaky endpoint costs one slow run, not every run.
                if ccusage_core::log_level().is_some_and(|level| level >= 4) {
                    eprintln!(
                        "WARN  Failed to refresh LiteLLM pricing ({error}); using last-known cached pricing."
                    );
                }
                latch_fresh(url, &c);
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
}
