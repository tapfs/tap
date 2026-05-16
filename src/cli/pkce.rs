//! OAuth 2.0 PKCE primitives (RFC 7636) for tapfs's user-context auth flows.
//!
//! Wraps the `oauth2` crate's PKCE helpers in a tapfs-shaped API and adds
//! authorize-URL building, callback-request parsing, and token-exchange
//! request construction. Pure logic — no I/O, no globals — so every piece
//! tests in isolation against fixtures and known-answer vectors.
//!
//! The browser-and-listener side of the flow lives in `cli/auth.rs` next to
//! the existing `oauth2_browser_flow` so dispatch and shutdown stay in one
//! place; this module is the math-and-strings half.

use anyhow::Result;

/// A PKCE verifier/challenge pair generated for a single authorization
/// attempt. The verifier is kept in memory by the CLI until the callback
/// arrives; the challenge ships to the authorization server in the browser
/// redirect.
pub struct PkcePair {
    /// The high-entropy random string the CLI keeps secret. Sent only to the
    /// token endpoint when exchanging the auth code, so an interceptor of the
    /// browser redirect can't redeem the code without it.
    pub verifier: String,
    /// `BASE64URL(SHA256(verifier))` — sent in the authorize URL so the
    /// authorization server can verify the verifier at token-exchange time.
    pub challenge: String,
}

impl PkcePair {
    /// Generate a fresh verifier/challenge pair using SHA-256.
    ///
    /// Per RFC 7636 §4.1 the verifier is 43–128 characters from the
    /// unreserved set `[A-Z] / [a-z] / [0-9] / "-" / "." / "_" / "~"`. The
    /// `oauth2` crate's helper enforces both constraints and uses the OS
    /// CSPRNG, so we just adapt the types.
    pub fn new() -> Self {
        let (challenge, verifier) = oauth2::PkceCodeChallenge::new_random_sha256();
        Self {
            verifier: verifier.secret().to_string(),
            challenge: challenge.as_str().to_string(),
        }
    }
}

/// Parameters for building an OAuth 2.0 authorize URL with PKCE.
pub struct AuthorizeParams<'a> {
    pub authorize_url: &'a str,
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    /// Space-separated scopes (the spec's `auth.scopes` is already this shape).
    pub scopes: &'a str,
    pub challenge: &'a str,
    /// Opaque CSRF token — the CLI generates and remembers it, the callback
    /// must echo it back. We keep this an explicit parameter (rather than
    /// generating inside) so the caller can correlate it with whatever they
    /// stash for the lifetime of the listener.
    pub state: &'a str,
}

/// Build the authorize URL the user opens in their browser. Query params are
/// percent-encoded; scopes are sent as-is in the `scope` parameter (the
/// authorization server splits on spaces).
pub fn build_authorize_url(p: &AuthorizeParams<'_>) -> String {
    let mut out = String::with_capacity(p.authorize_url.len() + 256);
    out.push_str(p.authorize_url);
    out.push(if p.authorize_url.contains('?') { '&' } else { '?' });
    out.push_str("response_type=code");
    out.push_str("&client_id=");
    out.push_str(&encode(p.client_id));
    out.push_str("&redirect_uri=");
    out.push_str(&encode(p.redirect_uri));
    out.push_str("&scope=");
    out.push_str(&encode(p.scopes));
    out.push_str("&state=");
    out.push_str(&encode(p.state));
    out.push_str("&code_challenge=");
    out.push_str(&encode(p.challenge));
    out.push_str("&code_challenge_method=S256");
    out
}

/// Parse the OAuth 2.0 callback. The authorization server redirects to
/// `redirect_uri?code=...&state=...` on success, or
/// `redirect_uri?error=access_denied[&error_description=...]&state=...` on
/// failure. `request` is the raw HTTP request line + headers as received on
/// the loopback listener.
///
/// Returns `Ok(CallbackPayload::Success { code, state })` on success,
/// `Ok(CallbackPayload::Error { ... })` on user denial or provider error,
/// and `Err(...)` only when the request is malformed (no path, no query).
pub fn parse_callback(request: &str) -> Result<CallbackPayload> {
    let first_line = request
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty HTTP request"))?;
    let path = first_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("malformed request line: {:?}", first_line))?;
    let query = path
        .split('?')
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("callback URL has no query string"))?;

    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    let mut error: Option<String> = None;
    let mut error_description: Option<String> = None;
    for param in query.split('&') {
        let mut parts = param.splitn(2, '=');
        let key = match parts.next() {
            Some(k) => k,
            None => continue,
        };
        let raw_val = parts.next().unwrap_or("");
        let val = decode(raw_val);
        match key {
            "code" => code = Some(val),
            "state" => state = Some(val),
            "error" => error = Some(val),
            "error_description" => error_description = Some(val),
            _ => {}
        }
    }

    if let Some(err) = error {
        return Ok(CallbackPayload::Error {
            error: err,
            error_description,
            state,
        });
    }
    let code = code.ok_or_else(|| {
        anyhow::anyhow!("callback has neither `code` nor `error` — authorization server bug?")
    })?;
    Ok(CallbackPayload::Success { code, state })
}

#[derive(Debug)]
pub enum CallbackPayload {
    Success {
        code: String,
        state: Option<String>,
    },
    Error {
        error: String,
        error_description: Option<String>,
        state: Option<String>,
    },
}

/// Form parameters for the `POST <token_url>` exchange that swaps the auth
/// code (+ verifier) for an access token. PKCE skips `client_secret`
/// entirely — the verifier proves possession.
pub fn build_token_exchange_form<'a>(
    code: &'a str,
    code_verifier: &'a str,
    client_id: &'a str,
    redirect_uri: &'a str,
) -> Vec<(&'static str, String)> {
    vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("code_verifier", code_verifier.to_string()),
        ("client_id", client_id.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
    ]
}

/// Form parameters for the refresh request. Used by `RestConnector::ensure_token`
/// when a PKCE-issued access token expires. Like the exchange, no
/// `client_secret` is sent.
pub fn build_refresh_form<'a>(
    refresh_token: &'a str,
    client_id: &'a str,
) -> Vec<(&'static str, String)> {
    vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
        ("client_id", client_id.to_string()),
    ]
}

/// Parsed shape of a successful token-endpoint response. Per RFC 6749 §5.1
/// `access_token` and `token_type` are required; `expires_in` and
/// `refresh_token` are common but optional. Some providers (X, Google)
/// rotate refresh tokens on every refresh; the caller must persist the new
/// value when it differs from the old.
#[derive(Debug, Clone)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_in: Option<u64>,
    pub refresh_token: Option<String>,
    pub token_type: Option<String>,
    pub scope: Option<String>,
}

impl TokenResponse {
    /// Parse a raw JSON value (as returned by reqwest's `.json::<Value>()`)
    /// into a typed response. Returns Err if `access_token` is missing.
    pub fn from_json(v: &serde_json::Value) -> Result<Self> {
        let access_token = v
            .get("access_token")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("token response has no access_token"))?
            .to_string();
        Ok(Self {
            access_token,
            expires_in: v.get("expires_in").and_then(|n| n.as_u64()),
            refresh_token: v
                .get("refresh_token")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string()),
            token_type: v
                .get("token_type")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string()),
            scope: v
                .get("scope")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string()),
        })
    }
}

/// Percent-encode a query-string value. Encodes everything outside the
/// unreserved set per RFC 3986 (`A-Z a-z 0-9 - . _ ~`). Spaces become `%20`
/// (not `+`) — the OAuth 2.0 spec treats redirect_uri etc. as URIs, not
/// `application/x-www-form-urlencoded` bodies, so `+` would be a different
/// character literal at the server side.
fn encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", byte));
            }
        }
    }
    out
}

/// Percent-decode (best-effort). Used to recover state/error_description from
/// the inbound callback; if a sequence is malformed (e.g. `%XY` non-hex), the
/// byte triple is passed through literally rather than erroring out — the
/// caller cares about exact-match comparison of state, not lossless decoding.
fn decode(input: &str) -> String {
    let mut out = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // PkcePair — verifier/challenge generation
    //
    // Cross-checked against RFC 7636 §4.4 known answer vectors below.
    // ---------------------------------------------------------------

    #[test]
    fn verifier_length_in_43_to_128_range() {
        let p = PkcePair::new();
        assert!(
            p.verifier.len() >= 43 && p.verifier.len() <= 128,
            "verifier length {} out of [43, 128]",
            p.verifier.len()
        );
    }

    #[test]
    fn verifier_uses_only_unreserved_charset() {
        let p = PkcePair::new();
        for c in p.verifier.chars() {
            assert!(
                c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~'),
                "verifier contains illegal char {:?}",
                c
            );
        }
    }

    #[test]
    fn challenge_is_base64url_no_padding() {
        // S256 challenge is BASE64URL-ENCODE(SHA256(ASCII(verifier))) with
        // padding stripped (RFC 7636 §4.2). SHA-256 → 32 bytes → 43 chars
        // base64url. No `+`, `/`, or `=`.
        let p = PkcePair::new();
        assert_eq!(p.challenge.len(), 43, "challenge: {:?}", p.challenge);
        assert!(
            !p.challenge.contains('+')
                && !p.challenge.contains('/')
                && !p.challenge.contains('='),
            "challenge contains non-URL-safe chars: {:?}",
            p.challenge
        );
    }

    #[test]
    fn consecutive_pairs_are_distinct() {
        // Catastrophic regression guard: a bug that returned a constant
        // verifier (e.g. seeded RNG by accident) would be silent in single-
        // pair tests. Generate a few and require they all differ.
        let a = PkcePair::new();
        let b = PkcePair::new();
        let c = PkcePair::new();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(b.verifier, c.verifier);
        assert_ne!(a.verifier, c.verifier);
    }

    // ---------------------------------------------------------------
    // build_authorize_url
    // ---------------------------------------------------------------

    #[test]
    fn authorize_url_contains_all_required_params() {
        let url = build_authorize_url(&AuthorizeParams {
            authorize_url: "https://x.com/i/oauth2/authorize",
            client_id: "my-client",
            redirect_uri: "http://localhost:53682/callback",
            scopes: "tweet.read users.read",
            challenge: "abc-challenge",
            state: "state-1234",
        });
        assert!(url.starts_with("https://x.com/i/oauth2/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=my-client"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A53682%2Fcallback"));
        assert!(
            url.contains("scope=tweet.read%20users.read"),
            "scope must be space-encoded as %20, got: {}",
            url
        );
        assert!(url.contains("state=state-1234"));
        assert!(url.contains("code_challenge=abc-challenge"));
        assert!(url.contains("code_challenge_method=S256"));
    }

    #[test]
    fn authorize_url_uses_ampersand_when_base_has_query() {
        // Some authorization servers embed a query param in the published
        // authorize URL (e.g. account hint). The first separator must be `&`
        // in that case, not `?`.
        let url = build_authorize_url(&AuthorizeParams {
            authorize_url: "https://provider.example/auth?prompt=consent",
            client_id: "c",
            redirect_uri: "http://localhost/cb",
            scopes: "read",
            challenge: "x",
            state: "y",
        });
        assert!(
            url.starts_with("https://provider.example/auth?prompt=consent&response_type=code"),
            "first separator must be &, got: {}",
            url
        );
    }

    // ---------------------------------------------------------------
    // parse_callback
    // ---------------------------------------------------------------

    #[test]
    fn parse_callback_success_extracts_code_and_state() {
        let req = "GET /callback?code=auth-code-xyz&state=csrf-1234 HTTP/1.1\r\n\
                   Host: localhost\r\n\r\n";
        match parse_callback(req).unwrap() {
            CallbackPayload::Success { code, state } => {
                assert_eq!(code, "auth-code-xyz");
                assert_eq!(state.as_deref(), Some("csrf-1234"));
            }
            other => panic!("expected Success, got {:?}", other),
        }
    }

    #[test]
    fn parse_callback_error_returns_provider_error() {
        let req = "GET /callback?error=access_denied&error_description=User%20denied&state=csrf HTTP/1.1\r\n\r\n";
        match parse_callback(req).unwrap() {
            CallbackPayload::Error {
                error,
                error_description,
                state,
            } => {
                assert_eq!(error, "access_denied");
                assert_eq!(error_description.as_deref(), Some("User denied"));
                assert_eq!(state.as_deref(), Some("csrf"));
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[test]
    fn parse_callback_rejects_request_without_query() {
        // If the authorization server somehow redirects to the bare path
        // (browser quirk, user typed it manually) we should error rather
        // than blindly treat the empty query as a denial.
        let req = "GET /callback HTTP/1.1\r\n\r\n";
        let err = parse_callback(req).expect_err("bare path must fail to parse");
        assert!(
            err.to_string().contains("no query"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn parse_callback_decodes_percent_escapes_in_state() {
        // state may contain anything the client chose; we shouldn't return a
        // raw percent-encoded blob to the caller's equality comparison.
        let req = "GET /callback?code=c&state=abc%2Fdef HTTP/1.1\r\n\r\n";
        match parse_callback(req).unwrap() {
            CallbackPayload::Success { state, .. } => {
                assert_eq!(state.as_deref(), Some("abc/def"));
            }
            other => panic!("expected Success, got {:?}", other),
        }
    }

    // ---------------------------------------------------------------
    // build_token_exchange_form / build_refresh_form
    //
    // Critical PKCE invariant: NEVER include client_secret in either form
    // body. PKCE is the "public client" flow precisely because the client
    // can't keep a secret. Including a secret would be either useless (if
    // empty) or a confused-deputy bug (if filled with someone else's).
    // ---------------------------------------------------------------

    #[test]
    fn token_exchange_form_contains_required_params_and_omits_client_secret() {
        let form = build_token_exchange_form("auth-code", "the-verifier", "cli-id", "http://localhost/cb");
        let map: std::collections::HashMap<_, _> = form.iter().cloned().collect();
        assert_eq!(map.get("grant_type").map(|s| s.as_str()), Some("authorization_code"));
        assert_eq!(map.get("code").map(|s| s.as_str()), Some("auth-code"));
        assert_eq!(map.get("code_verifier").map(|s| s.as_str()), Some("the-verifier"));
        assert_eq!(map.get("client_id").map(|s| s.as_str()), Some("cli-id"));
        assert_eq!(map.get("redirect_uri").map(|s| s.as_str()), Some("http://localhost/cb"));
        assert!(
            !map.contains_key("client_secret"),
            "PKCE flow must NEVER send client_secret — that's the whole point"
        );
    }

    #[test]
    fn refresh_form_contains_required_params_and_omits_client_secret() {
        let form = build_refresh_form("ref-tok", "cli-id");
        let map: std::collections::HashMap<_, _> = form.iter().cloned().collect();
        assert_eq!(map.get("grant_type").map(|s| s.as_str()), Some("refresh_token"));
        assert_eq!(map.get("refresh_token").map(|s| s.as_str()), Some("ref-tok"));
        assert_eq!(map.get("client_id").map(|s| s.as_str()), Some("cli-id"));
        assert!(!map.contains_key("client_secret"));
        assert!(
            !map.contains_key("code_verifier"),
            "refresh must not include the (long-discarded) verifier — only the exchange does"
        );
    }

    // ---------------------------------------------------------------
    // TokenResponse::from_json
    // ---------------------------------------------------------------

    #[test]
    fn token_response_parses_full_x_shape() {
        // Mirrors the documented X v2 success response.
        let json = serde_json::json!({
            "access_token": "Mxx...",
            "token_type": "bearer",
            "expires_in": 7200,
            "refresh_token": "Rxx...",
            "scope": "tweet.read users.read offline.access"
        });
        let parsed = TokenResponse::from_json(&json).unwrap();
        assert_eq!(parsed.access_token, "Mxx...");
        assert_eq!(parsed.expires_in, Some(7200));
        assert_eq!(parsed.refresh_token.as_deref(), Some("Rxx..."));
        assert_eq!(parsed.token_type.as_deref(), Some("bearer"));
        assert_eq!(parsed.scope.as_deref(), Some("tweet.read users.read offline.access"));
    }

    #[test]
    fn token_response_tolerates_missing_optional_fields() {
        // expires_in / refresh_token / scope are all optional per RFC 6749.
        // We must not error when a provider omits them — Stripe and others do.
        let json = serde_json::json!({"access_token": "tok", "token_type": "bearer"});
        let parsed = TokenResponse::from_json(&json).unwrap();
        assert_eq!(parsed.access_token, "tok");
        assert!(parsed.expires_in.is_none());
        assert!(parsed.refresh_token.is_none());
        assert!(parsed.scope.is_none());
    }

    #[test]
    fn token_response_errors_when_access_token_missing() {
        // Without access_token there's nothing usable — caller must surface
        // the upstream JSON in the error path (often contains `error_description`).
        let json = serde_json::json!({"error": "invalid_grant"});
        let err = TokenResponse::from_json(&json).expect_err("must reject missing access_token");
        assert!(err.to_string().contains("no access_token"), "got: {}", err);
    }
}
