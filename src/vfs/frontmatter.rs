//! tapfs frontmatter parsing/injection + the in-flight POST sentinel +
//! per-resource idempotency-key generation.
//!
//! All pure functions, no `VirtualFs` reference. Pulled out of `core.rs` to
//! make the file boundary obvious: this module is "the bytes-on-disk format
//! tapfs uses to carry state alongside user content," nothing else.

// ---------------------------------------------------------------------------
// Frontmatter parse / strip / inject
// ---------------------------------------------------------------------------

pub(crate) struct TapfsMeta {
    pub(crate) draft: bool,
    pub(crate) id: Option<String>,
    pub(crate) version: Option<u32>,
}

pub(crate) fn parse_tapfs_meta(data: &[u8]) -> TapfsMeta {
    let text = match std::str::from_utf8(data) {
        Ok(t) => t,
        Err(_) => {
            return TapfsMeta {
                draft: false,
                id: None,
                version: None,
            }
        }
    };

    if !text.starts_with("---") {
        return TapfsMeta {
            draft: false,
            id: None,
            version: None,
        };
    }

    let after_open = &text[3..];
    let fm_text = if let Some(pos) = after_open.find("\n---") {
        &after_open[..pos]
    } else {
        return TapfsMeta {
            draft: false,
            id: None,
            version: None,
        };
    };

    let mut draft = false;
    let mut id: Option<String> = None;
    let mut version: Option<u32> = None;

    for line in fm_text.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("_draft:") {
            let v = val.trim();
            draft = v == "true";
        } else if let Some(val) = line.strip_prefix("_id:") {
            let v = val.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                id = Some(v.to_string());
            }
        } else if let Some(val) = line.strip_prefix("_version:") {
            let v = val.trim();
            if let Ok(n) = v.parse::<u32>() {
                version = Some(n);
            }
        }
    }

    TapfsMeta { draft, id, version }
}

pub(crate) fn strip_tapfs_fields(data: &[u8]) -> Vec<u8> {
    let text = match std::str::from_utf8(data) {
        Ok(t) => t,
        Err(_) => return data.to_vec(),
    };

    if !text.starts_with("---") {
        return data.to_vec();
    }

    let after_open = &text[3..];
    let close_pos = match after_open.find("\n---") {
        Some(p) => p,
        None => return data.to_vec(),
    };

    let fm_text = &after_open[..close_pos];
    let body = &after_open[close_pos + 4..];

    let filtered: Vec<&str> = fm_text
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("_draft:")
                && !trimmed.starts_with("_id:")
                && !trimmed.starts_with("_version:")
        })
        .collect();

    let new_fm = filtered.join("\n");
    if new_fm.trim().is_empty() {
        // All frontmatter was tapfs fields — collapse to just body
        let result = format!("---\n---{}", body);
        result.into_bytes()
    } else {
        let result = format!("---\n{}\n---{}", new_fm, body);
        result.into_bytes()
    }
}

pub(crate) fn inject_tapfs_fields(data: &[u8], id: &str, version: u32) -> Vec<u8> {
    let text = match std::str::from_utf8(data) {
        Ok(t) => t,
        Err(_) => return data.to_vec(),
    };

    if !text.starts_with("---") {
        // No frontmatter — prepend one
        let result = format!("---\n_id: {}\n_version: {}\n---\n{}", id, version, text);
        return result.into_bytes();
    }

    let after_open = &text[3..];
    let close_pos = match after_open.find("\n---") {
        Some(p) => p,
        None => {
            let result = format!("---\n_id: {}\n_version: {}\n---\n{}", id, version, text);
            return result.into_bytes();
        }
    };

    let fm_text = &after_open[..close_pos];
    let body = &after_open[close_pos + 4..];

    // Remove old tapfs fields and _draft, then append updated values
    let mut lines: Vec<&str> = fm_text
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("_draft:")
                && !trimmed.starts_with("_id:")
                && !trimmed.starts_with("_version:")
        })
        .collect();

    lines.push(""); // placeholder to trigger join separator trick
    let mut new_fm = lines[..lines.len() - 1].join("\n");
    if !new_fm.is_empty() && !new_fm.ends_with('\n') {
        new_fm.push('\n');
    }
    new_fm.push_str(&format!("_id: {}\n_version: {}", id, version));

    let result = format!("---\n{}\n---{}", new_fm, body);
    result.into_bytes()
}

// ---------------------------------------------------------------------------
// Idempotency key
// ---------------------------------------------------------------------------

/// Generate a per-resource idempotency key for new drafts.
///
/// The key is sent as an HTTP header (e.g. `Idempotency-Key`) on POST so a
/// retried create after a lost response doesn't produce a duplicate. It needs
/// to be unique per draft and stable across retries — including across daemon
/// restarts, because the key lives in the draft file on disk.
///
/// Process-time-nanos prefix plus a process-local atomic counter. Within a
/// process this is monotonically unique. Across processes the nanos component
/// makes collisions effectively impossible. UUIDv4-grade randomness would be
/// stronger but isn't required: APIs treat the key as an opaque string.
pub(crate) fn generate_idempotency_key() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("tapfs-{:016x}-{:08x}", nanos, counter)
}

// ---------------------------------------------------------------------------
// In-flight POST sentinel
// ---------------------------------------------------------------------------

/// How long a `__creating__` sentinel is considered "in flight" before flush
/// will retry the POST despite seeing it. Tuned for: a normal POST takes
/// hundreds of ms; if it's been minutes, the daemon almost certainly crashed
/// or the upstream is permanently wedged. The retry is safe **only** when
/// the connector has an `idempotency_key_header` configured (otherwise a
/// successful-but-lost-response POST will be duplicated).
pub(crate) const SENTINEL_TTL: std::time::Duration = std::time::Duration::from_secs(300);

const SENTINEL_PREFIX: &str = "__creating__";

/// Build a fresh in-flight sentinel string. Format: `__creating__@<unix_seconds>`.
/// The timestamp lets crash-recovery code distinguish a still-in-flight POST
/// from one whose daemon died midway.
pub(crate) fn make_sentinel() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}@{}", SENTINEL_PREFIX, now)
}

/// Classify an `_id` value:
///   - `Fresh`: a sentinel within TTL — skip flush, another writer is on it.
///   - `Stale`: a sentinel older than TTL — daemon probably crashed; retry.
///   - `Legacy`: bare `__creating__` from before the timestamp format —
///     no way to tell its age, so treat as stale.
///   - `NotSentinel`: not a sentinel at all (real id, empty, etc.).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SentinelState {
    Fresh,
    Stale,
    Legacy,
    NotSentinel,
}

pub(crate) fn classify_sentinel(id_value: &str) -> SentinelState {
    if id_value == SENTINEL_PREFIX {
        return SentinelState::Legacy;
    }
    let Some(ts_str) = id_value.strip_prefix(&format!("{}@", SENTINEL_PREFIX)) else {
        return SentinelState::NotSentinel;
    };
    let Ok(ts_secs) = ts_str.parse::<u64>() else {
        // Malformed timestamp — treat as stale; don't get stuck on a
        // sentinel we can't parse.
        return SentinelState::Stale;
    };
    let sentinel_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(ts_secs);
    let now = std::time::SystemTime::now();
    match now.duration_since(sentinel_time) {
        Ok(age) if age < SENTINEL_TTL => SentinelState::Fresh,
        Ok(_) => SentinelState::Stale,
        // Sentinel timestamp is in the future (clock skew). Treat as fresh
        // to be safe — better to wait an extra TTL than to dup-POST.
        Err(_) => SentinelState::Fresh,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_sentinel_has_expected_prefix_and_numeric_timestamp() {
        let s = make_sentinel();
        assert!(s.starts_with("__creating__@"), "got: {}", s);
        let ts: u64 = s
            .trim_start_matches("__creating__@")
            .parse()
            .expect("timestamp portion must parse as u64");
        // Should be in the modern era (post-2020). If this fails, the
        // SystemTime::now() is wildly off.
        assert!(ts > 1_577_836_800, "ts {} looks pre-2020", ts);
    }

    #[test]
    fn classify_real_id_is_not_sentinel() {
        assert_eq!(classify_sentinel("12345"), SentinelState::NotSentinel);
        assert_eq!(classify_sentinel("abc-def-ghi"), SentinelState::NotSentinel);
        assert_eq!(classify_sentinel(""), SentinelState::NotSentinel);
    }

    #[test]
    fn classify_legacy_bare_sentinel() {
        // From before the timestamp was added — no way to tell its age, so
        // treated as stale (i.e. retry on next flush).
        assert_eq!(classify_sentinel("__creating__"), SentinelState::Legacy);
    }

    #[test]
    fn classify_fresh_sentinel_within_ttl() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let s = format!("__creating__@{}", now);
        assert_eq!(classify_sentinel(&s), SentinelState::Fresh);
    }

    #[test]
    fn classify_stale_sentinel_past_ttl() {
        // 1 hour ago — well past SENTINEL_TTL of 5 minutes.
        let stale_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(3600);
        let s = format!("__creating__@{}", stale_ts);
        assert_eq!(classify_sentinel(&s), SentinelState::Stale);
    }

    #[test]
    fn classify_malformed_sentinel_treated_as_stale() {
        // Non-numeric "timestamp" — can't gauge age, so treat as stale to
        // avoid getting permanently stuck.
        assert_eq!(
            classify_sentinel("__creating__@notanumber"),
            SentinelState::Stale
        );
    }

    #[test]
    fn classify_future_timestamp_treated_as_fresh() {
        // Clock skew can put a sentinel "in the future" — be conservative
        // and skip rather than risk a duplicate POST.
        let future_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let s = format!("__creating__@{}", future_ts);
        assert_eq!(classify_sentinel(&s), SentinelState::Fresh);
    }
}
