//! Reciprocal Rank Fusion for combining heterogeneous provider results.
//!
//! Why RRF: providers' raw scores aren't comparable. BM25 in [0, ∞), cosine in
//! [-1, 1], an upstream API's `relevance` field on whatever scale it picks.
//! RRF discards score magnitude and works in rank space, where 1st-rank from
//! provider A and 1st-rank from provider B reinforce each other cleanly.
//!
//! Per provider, each hit at rank `r` (0-indexed) contributes
//! `weight / (RRF_K + r + 1)` to the document's fused score.

use std::collections::HashMap;

use crate::search::traits::SearchHit;

/// Standard RRF damping constant (Cormack et al., SIGIR 2009).
const RRF_K: f32 = 60.0;

/// Fuse per-provider hit lists, dedupe by `tap_path`, return the top `top_k`.
///
/// Each input is `(weight, hits)` where `hits` is already ranked best-first.
pub fn rrf_fuse(per_provider: Vec<(f32, Vec<SearchHit>)>, top_k: usize) -> Vec<SearchHit> {
    let mut accumulated: HashMap<String, SearchHit> = HashMap::new();
    let mut scores: HashMap<String, f32> = HashMap::new();
    let mut providers_seen: HashMap<String, Vec<String>> = HashMap::new();

    for (weight, hits) in per_provider {
        for (rank, hit) in hits.into_iter().enumerate() {
            let rrf = weight / (RRF_K + rank as f32 + 1.0);
            let key = hit.tap_path.clone();
            *scores.entry(key.clone()).or_insert(0.0) += rrf;
            providers_seen
                .entry(key.clone())
                .or_default()
                .push(hit.provider.clone());

            accumulated
                .entry(key)
                .and_modify(|existing| {
                    // Keep the best per-provider snippet/title.
                    if hit.score > existing.score {
                        if hit.snippet.is_some() {
                            existing.snippet = hit.snippet.clone();
                        }
                        if hit.title.is_some() {
                            existing.title = hit.title.clone();
                        }
                    }
                })
                .or_insert(hit);
        }
    }

    let mut fused: Vec<SearchHit> = accumulated.into_values().collect();
    for hit in &mut fused {
        hit.score = scores[&hit.tap_path];
        if let Some(provs) = providers_seen.get(&hit.tap_path) {
            if provs.len() > 1 {
                let mut uniq: Vec<String> = provs.clone();
                uniq.sort();
                uniq.dedup();
                hit.provider = uniq.join("+");
            }
        }
    }
    // Sort descending by fused score; tie-break by tap_path for determinism.
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.tap_path.cmp(&b.tap_path))
    });
    fused.truncate(top_k);
    fused
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(path: &str, provider: &str, score: f32) -> SearchHit {
        SearchHit {
            tap_path: path.into(),
            connector: "x".into(),
            collection: "y".into(),
            resource_id: path.into(),
            title: None,
            snippet: None,
            score,
            provider: provider.into(),
        }
    }

    #[test]
    fn fuses_disjoint_results() {
        let a = vec![h("/a", "p1", 1.0), h("/b", "p1", 0.9)];
        let b = vec![h("/c", "p2", 5.0), h("/d", "p2", 4.0)];
        let fused = rrf_fuse(vec![(1.0, a), (1.0, b)], 10);
        assert_eq!(fused.len(), 4);
    }

    #[test]
    fn dedups_overlapping_hits() {
        // Same /a appears in both — should dedupe to one row with combined score.
        let a = vec![h("/a", "p1", 1.0), h("/b", "p1", 0.9)];
        let b = vec![h("/a", "p2", 5.0), h("/c", "p2", 4.0)];
        let fused = rrf_fuse(vec![(1.0, a), (1.0, b)], 10);
        assert_eq!(fused.len(), 3, "/a should be deduplicated");
        let top = fused.iter().find(|h| h.tap_path == "/a").unwrap();
        // Top-rank from both providers — should outrank singleton hits.
        assert_eq!(fused[0].tap_path, "/a");
        // Provider field should reflect both sources.
        assert!(top.provider.contains("p1") && top.provider.contains("p2"));
    }

    #[test]
    fn weights_tilt_results() {
        // Same content, different provider weights — heavier provider's
        // ranking wins on tie position.
        let a = vec![h("/a", "p1", 1.0), h("/b", "p1", 0.9)];
        let b = vec![h("/b", "p2", 1.0), h("/a", "p2", 0.9)];
        // p2 is weighted higher → /b should win.
        let fused = rrf_fuse(vec![(1.0, a), (3.0, b)], 10);
        assert_eq!(fused[0].tap_path, "/b");
    }

    #[test]
    fn truncates_to_top_k() {
        let many: Vec<SearchHit> = (0..50).map(|i| h(&format!("/{}", i), "p", 1.0)).collect();
        let fused = rrf_fuse(vec![(1.0, many)], 5);
        assert_eq!(fused.len(), 5);
    }

    #[test]
    fn empty_input() {
        let fused = rrf_fuse(vec![], 10);
        assert!(fused.is_empty());
    }
}
