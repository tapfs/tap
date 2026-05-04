//! Shared HTTP retry logic for connector implementations.
//!
//! Before this module, `rest.rs`, `google.rs`, and `atlassian_auth.rs` each
//! had their own near-identical 429/503/Retry-After/backoff loop — roughly
//! 600 lines of code with subtle drift between implementations (jitter only
//! in one, 502 handling only in another). One bug fix had to be applied
//! three times.
//!
//! `execute` consolidates that into a single closure-based helper. The
//! caller owns auth + token-refresh decisions (the closure produces a fresh
//! `RequestBuilder` per attempt); this module owns when-to-retry and
//! how-long-to-wait.

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::{RequestBuilder, Response};

/// Retry policy parameters. The default values match what `rest.rs` was
/// using (3 retries, 500 ms initial delay, 2x multiplier).
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(500),
        }
    }
}

/// Send a request with retries on transient failures.
///
/// Retries on:
/// - HTTP 429 (Too Many Requests) — honors the `Retry-After` header when
///   present; falls back to exponential backoff otherwise.
/// - HTTP 502 (Bad Gateway), 503 (Service Unavailable).
/// - reqwest network errors that are transient: timeout, connect failure,
///   request build error (which covers DNS).
///
/// Non-retryable failures (4xx other than 429, malformed URL, etc.) bubble
/// up immediately.
///
/// The closure must be `Fn` (not `FnOnce`) because it's invoked once per
/// attempt — `RequestBuilder` is consumed by `.send()` so we can't reuse it
/// across attempts.
pub async fn execute<F>(policy: &RetryPolicy, build_request: F) -> Result<Response>
where
    F: Fn() -> RequestBuilder,
{
    let mut delay = policy.base_delay;
    for attempt in 0..=policy.max_retries {
        let response = build_request().send().await;
        match response {
            Ok(resp) if is_retryable_status(resp.status().as_u16()) => {
                if attempt == policy.max_retries {
                    // Final attempt — return the response so the caller
                    // can extract the body for the error message.
                    return Ok(resp);
                }
                let wait = retry_after_or(&resp, delay);
                tracing::warn!(
                    status = resp.status().as_u16(),
                    attempt,
                    wait_ms = wait.as_millis() as u64,
                    "retrying after transient HTTP error"
                );
                tokio::time::sleep(wait).await;
                delay *= 2;
            }
            Ok(resp) => return Ok(resp),
            Err(e)
                if attempt < policy.max_retries
                    && (e.is_timeout() || e.is_connect() || e.is_request()) =>
            {
                tracing::warn!(
                    error = %e,
                    attempt,
                    wait_ms = delay.as_millis() as u64,
                    "retrying after transient network error"
                );
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
            Err(e) => return Err(e).context("HTTP request failed"),
        }
    }
    unreachable!("retry loop exits via return on max_retries")
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 502 | 503)
}

fn retry_after_or(resp: &Response, fallback: Duration) -> Duration {
    resp.headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn returns_immediately_on_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/ok", server.uri());
        let resp = execute(&RetryPolicy::default(), || client.get(&url))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        drop(server);
    }

    #[tokio::test]
    async fn retries_on_503_then_succeeds() {
        let server = MockServer::start().await;
        // First call: 503. Second call: 200.
        Mock::given(method("GET"))
            .and(path("/flaky"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/flaky"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/flaky", server.uri());
        let policy = RetryPolicy {
            max_retries: 3,
            base_delay: Duration::from_millis(1),
        };
        let resp = execute(&policy, || client.get(&url)).await.unwrap();
        assert_eq!(resp.status(), 200);
        drop(server);
    }

    #[tokio::test]
    async fn returns_4xx_without_retry() {
        // Non-retryable status (404) — must come back on the first attempt.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/missing", server.uri());
        let resp = execute(&RetryPolicy::default(), || client.get(&url))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        drop(server);
    }

    #[tokio::test]
    async fn honors_retry_after_header() {
        // 429 with Retry-After: 0 (avoid actually waiting in tests). Just
        // verifies the helper accepted the header path and retried.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/throttled"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/throttled"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/throttled", server.uri());
        let policy = RetryPolicy {
            max_retries: 3,
            base_delay: Duration::from_millis(1),
        };
        let resp = execute(&policy, || client.get(&url)).await.unwrap();
        assert_eq!(resp.status(), 200);
        drop(server);
    }

    #[tokio::test]
    async fn exhausts_retries_and_returns_last_response() {
        // All attempts return 503. After max_retries, return the 503 to the
        // caller (so they can render the body in their error).
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/dead"))
            .respond_with(ResponseTemplate::new(503))
            .expect(2) // initial + 1 retry
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/dead", server.uri());
        let policy = RetryPolicy {
            max_retries: 1,
            base_delay: Duration::from_millis(1),
        };
        let resp = execute(&policy, || client.get(&url)).await.unwrap();
        assert_eq!(resp.status(), 503);
        drop(server);
    }
}
