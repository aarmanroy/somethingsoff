//! Native Drain log-template mining (He et al., ICWS 2017).
//!
//! Clusters pre-masked log messages into templates using a fixed-depth
//! parse tree: messages are routed by token count, then by their first
//! few tokens (digit-bearing tokens wildcarded at branch time), landing
//! in a leaf whose clusters are compared by positional token similarity.
//! Matching clusters absorb the message, generalizing differing
//! positions to `<*>`; otherwise a new cluster is born.
//!
//! State is per-query and in-memory only — templates are recomputed from
//! the index on every `patterns` invocation (the repo's disposable-state
//! invariant). Cluster identity is therefore corpus- and order-dependent:
//! `template_id` is stable for a given template *string*, but which
//! template a message lands in can differ between windows. This is why
//! the `errors` fingerprint (a pure per-message function) must never be
//! replaced by Drain cluster identity.

use std::collections::HashMap;

use sha2::Digest;

/// Tree depth counted like the paper: root + length layer + (depth - 2)
/// token layers. Depth 4 ⇒ branch on the first two tokens.
pub const DEFAULT_DEPTH: usize = 4;
/// Maximum distinct token branches per interior node before new tokens
/// fall through to the `<*>` catch-all branch.
pub const DEFAULT_MAX_CHILDREN: usize = 100;
/// Minimum fraction of positionally-equal tokens for a message to merge
/// into an existing cluster.
pub const DEFAULT_SIMILARITY: f64 = 0.4;
/// Placeholder for a generalized (variable) token position.
pub const WILDCARD: &str = "<*>";

/// Token cap per message: coalesced multiline events can reach 8 KB, but
/// templates beyond this length carry no extra grouping signal.
const MAX_TOKENS: usize = 200;

#[derive(Debug, Clone)]
pub struct DrainConfig {
    pub depth: usize,
    pub max_children: usize,
    pub similarity: f64,
}

impl Default for DrainConfig {
    fn default() -> Self {
        Self {
            depth: DEFAULT_DEPTH,
            max_children: DEFAULT_MAX_CHILDREN,
            similarity: DEFAULT_SIMILARITY,
        }
    }
}

/// Per-message metadata folded into the owning cluster.
pub struct Observation<'a> {
    /// ISO timestamp string (lexicographic order == chronological).
    pub timestamp: &'a str,
    pub level: &'a str,
    pub log_id: &'a str,
    /// Pre-mask first line, kept as the cluster's sample.
    pub raw_message: &'a str,
}

/// A mined template plus aggregate stats over the messages it absorbed.
#[derive(Debug)]
pub struct Cluster {
    /// Template tokens; `WILDCARD` marks generalized positions.
    pub tokens: Vec<String>,
    pub count: usize,
    pub first_seen: String,
    pub last_seen: String,
    /// First raw message observed for this cluster.
    pub sample_message: String,
    /// Up to 3 log IDs for follow-up `get` calls.
    pub sample_log_ids: Vec<String>,
    /// Occurrences per log level.
    pub level_counts: HashMap<String, usize>,
}

impl Cluster {
    fn new(tokens: Vec<String>, obs: &Observation<'_>) -> Self {
        let mut cluster = Self {
            tokens,
            count: 0,
            first_seen: String::new(),
            last_seen: String::new(),
            sample_message: obs.raw_message.to_string(),
            sample_log_ids: Vec::new(),
            level_counts: HashMap::new(),
        };
        cluster.absorb(obs);
        cluster
    }

    fn absorb(&mut self, obs: &Observation<'_>) {
        self.count += 1;
        if !obs.timestamp.is_empty() {
            if self.first_seen.is_empty() || obs.timestamp < self.first_seen.as_str() {
                self.first_seen = obs.timestamp.to_string();
            }
            if obs.timestamp > self.last_seen.as_str() {
                self.last_seen = obs.timestamp.to_string();
            }
        }
        if self.sample_log_ids.len() < 3 && !obs.log_id.is_empty() {
            self.sample_log_ids.push(obs.log_id.to_string());
        }
        if !obs.level.is_empty() {
            *self.level_counts.entry(obs.level.to_string()).or_insert(0) += 1;
        }
    }

    /// The human-readable template ("GET <path> <num> in <dur>").
    pub fn template(&self) -> String {
        self.tokens.join(" ")
    }

    /// Stable, versioned ID for this template string:
    /// `"v1:" + hex(sha256(template)[..8])`. Tokens are joined with
    /// single spaces, so the hashed form is already whitespace-normal.
    /// Stable across runs for identical templates; a different window
    /// can still mine a different template for the same message.
    pub fn template_id(&self) -> String {
        let mut hasher = sha2::Sha256::new();
        sha2::Digest::update(&mut hasher, self.template().as_bytes());
        let result = sha2::Digest::finalize(hasher);
        format!("v1:{}", hex::encode(&result[..8]))
    }
}

#[derive(Default)]
struct Node {
    children: HashMap<String, Node>,
    /// Leaf payload: indexes into `DrainTree::clusters`, insertion order.
    clusters: Vec<usize>,
}

/// Fixed-depth Drain parse tree. Feed messages in chronological order via
/// [`observe`](Self::observe); harvest with [`into_clusters`](Self::into_clusters).
pub struct DrainTree {
    config: DrainConfig,
    /// First routing layer: token count → subtree.
    length_layer: HashMap<usize, Node>,
    clusters: Vec<Cluster>,
}

impl DrainTree {
    pub fn new(config: DrainConfig) -> Self {
        Self {
            config,
            length_layer: HashMap::new(),
            clusters: Vec::new(),
        }
    }

    /// Route one pre-masked message (first line only) into the tree,
    /// merging it into the best-matching cluster or creating a new one.
    pub fn observe(&mut self, masked_first_line: &str, obs: &Observation<'_>) {
        let tokens: Vec<&str> = masked_first_line
            .split_whitespace()
            .take(MAX_TOKENS)
            .collect();
        if tokens.is_empty() {
            return;
        }

        let max_children = self.config.max_children.max(1);
        let token_layers = self.config.depth.saturating_sub(2).min(tokens.len());
        let mut node = self.length_layer.entry(tokens.len()).or_default();
        for token in &tokens[..token_layers] {
            // High-cardinality guard: digit-bearing tokens the masks
            // missed all share the wildcard branch instead of fanning out.
            let mut key = if token.bytes().any(|b| b.is_ascii_digit()) {
                WILDCARD
            } else {
                *token
            };
            // The wildcard branch is always creatable, so a full node
            // degrades gracefully rather than rejecting new tokens.
            if !node.children.contains_key(key)
                && node.children.len() >= max_children
                && key != WILDCARD
            {
                key = WILDCARD;
            }
            node = node.children.entry(key.to_string()).or_default();
        }

        // All clusters in a leaf share the same token count (routed by
        // the length layer), so similarity is positional-match ratio.
        let mut best: Option<(usize, f64)> = None;
        for &idx in &node.clusters {
            let sim = seq_sim(&self.clusters[idx].tokens, &tokens);
            if best.is_none_or(|(_, b)| sim > b) {
                best = Some((idx, sim));
            }
        }

        match best {
            Some((idx, sim)) if sim >= self.config.similarity => {
                let cluster = &mut self.clusters[idx];
                for (slot, token) in cluster.tokens.iter_mut().zip(&tokens) {
                    if slot != token && slot != WILDCARD {
                        *slot = WILDCARD.to_string();
                    }
                }
                cluster.absorb(obs);
            }
            _ => {
                let idx = self.clusters.len();
                self.clusters.push(Cluster::new(
                    tokens.iter().map(|t| t.to_string()).collect(),
                    obs,
                ));
                node.clusters.push(idx);
            }
        }
    }

    /// All mined clusters in creation order (callers sort for display).
    pub fn into_clusters(self) -> Vec<Cluster> {
        self.clusters
    }
}

/// Fraction of positions where the cluster template matches the incoming
/// tokens. A template `WILDCARD` counts as a match: similarity must stay
/// monotone as a cluster generalizes, or clusters would repel the very
/// messages that shaped them.
fn seq_sim(template: &[String], tokens: &[&str]) -> f64 {
    debug_assert_eq!(template.len(), tokens.len());
    if template.is_empty() {
        return 0.0;
    }
    let matches = template
        .iter()
        .zip(tokens)
        .filter(|(slot, token)| slot.as_str() == **token || slot.as_str() == WILDCARD)
        .count();
    matches as f64 / template.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(ts: &'static str, level: &'static str, id: &'static str) -> Observation<'static> {
        Observation {
            timestamp: ts,
            level,
            log_id: id,
            raw_message: "raw",
        }
    }

    fn observe_all(tree: &mut DrainTree, lines: &[&str]) {
        for (i, line) in lines.iter().enumerate() {
            let ids = ["a", "b", "c", "d", "e", "f", "g", "h"];
            tree.observe(
                line,
                &Observation {
                    timestamp: "2026-07-20T00:00:00Z",
                    level: "info",
                    log_id: ids[i % ids.len()],
                    raw_message: line,
                },
            );
        }
    }

    #[test]
    fn test_near_identical_messages_merge_with_wildcard() {
        let mut tree = DrainTree::new(DrainConfig::default());
        observe_all(
            &mut tree,
            &[
                "Connection timeout to alpha after retry",
                "Connection timeout to beta after retry",
            ],
        );
        let clusters = tree.into_clusters();
        assert_eq!(clusters.len(), 1);
        assert_eq!(
            clusters[0].template(),
            "Connection timeout to <*> after retry"
        );
        assert_eq!(clusters[0].count, 2);
    }

    #[test]
    fn test_digit_tokens_share_a_branch() {
        // Digit-bearing second token would otherwise split the tree at
        // branch level; the wildcard rule keeps both in one leaf.
        let mut tree = DrainTree::new(DrainConfig::default());
        observe_all(&mut tree, &["worker 17 started ok", "worker 42 started ok"]);
        let clusters = tree.into_clusters();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].template(), "worker <*> started ok");
    }

    #[test]
    fn test_different_token_counts_never_merge() {
        let mut tree = DrainTree::new(DrainConfig::default());
        observe_all(&mut tree, &["cache flushed", "cache flushed by operator"]);
        assert_eq!(tree.into_clusters().len(), 2);
    }

    #[test]
    fn test_similarity_threshold_boundary() {
        // 5 tokens, 2 matching = 0.4 → merges at the default threshold.
        let mut tree = DrainTree::new(DrainConfig::default());
        observe_all(&mut tree, &["alpha beta one two three", "alpha beta x y z"]);
        assert_eq!(tree.into_clusters().len(), 1);

        // 5 tokens, 1 matching (after the two branch tokens diverge the
        // leaf differs, so compare within one leaf: same first two) —
        // "alpha beta a b c" vs "alpha gamma ..." routes elsewhere; use
        // sub-threshold within the same leaf instead: 1/5 shared tail.
        let mut tree = DrainTree::new(DrainConfig::default());
        observe_all(&mut tree, &["alpha beta one two three", "alpha beta x y three"]);
        // 3/5 = 0.6 merges; then "alpha beta p q r" vs "<*> generalized"
        // template "alpha beta <*> <*> three": 2/5 literal + wildcards.
        assert_eq!(tree.into_clusters().len(), 1);
    }

    #[test]
    fn test_below_threshold_creates_new_cluster() {
        let cfg = DrainConfig {
            similarity: 0.75,
            ..Default::default()
        };
        let mut tree = DrainTree::new(cfg);
        observe_all(&mut tree, &["alpha beta one two three", "alpha beta x y z"]);
        // 2/5 = 0.4 < 0.75 → two clusters in the same leaf.
        assert_eq!(tree.into_clusters().len(), 2);
    }

    #[test]
    fn test_wildcard_counts_as_match() {
        // Two merges generalize positions 2 and 3; the last message then
        // matches only 3/5 literally (0.6 < 0.75) and merges anyway
        // because both wildcard slots count as matches (5/5).
        let mut tree = DrainTree::new(DrainConfig {
            similarity: 0.75,
            ..Default::default()
        });
        observe_all(
            &mut tree,
            &[
                "job run alpha done queue",
                "job run beta done queue",
                "job run beta finished queue",
                "job run gamma polled queue",
            ],
        );
        let clusters = tree.into_clusters();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].count, 4);
        assert_eq!(clusters[0].template(), "job run <*> <*> queue");
    }

    #[test]
    fn test_max_children_overflow_uses_catch_all() {
        let cfg = DrainConfig {
            max_children: 2,
            ..Default::default()
        };
        let mut tree = DrainTree::new(cfg);
        // Four distinct first tokens with only 2 child slots: the
        // overflowing ones must land in the <*> branch without panicking.
        observe_all(
            &mut tree,
            &[
                "alpha connected ok",
                "beta connected ok",
                "gamma connected ok",
                "delta connected ok",
            ],
        );
        let clusters = tree.into_clusters();
        let total: usize = clusters.iter().map(|c| c.count).sum();
        assert_eq!(total, 4);
        // gamma and delta share the catch-all leaf and merge there.
        assert!(clusters.len() <= 3);
    }

    #[test]
    fn test_template_id_stable_and_versioned() {
        let mut tree = DrainTree::new(DrainConfig::default());
        observe_all(&mut tree, &["cache flushed ok"]);
        let clusters = tree.into_clusters();
        let id = clusters[0].template_id();
        assert!(id.starts_with("v1:"));
        assert_eq!(id.len(), 3 + 16, "v1: + 16 hex chars");
        assert_eq!(id, clusters[0].template_id(), "deterministic");
    }

    #[test]
    fn test_metadata_aggregation() {
        let mut tree = DrainTree::new(DrainConfig::default());
        tree.observe("disk sync failed", &obs("2026-07-20T02:00:00Z", "error", "id2"));
        tree.observe("disk sync failed", &obs("2026-07-20T01:00:00Z", "warn", "id1"));
        tree.observe("disk sync failed", &obs("2026-07-20T03:00:00Z", "error", "id3"));
        tree.observe("disk sync failed", &obs("2026-07-20T04:00:00Z", "error", "id4"));
        let clusters = tree.into_clusters();
        assert_eq!(clusters.len(), 1);
        let c = &clusters[0];
        assert_eq!(c.first_seen, "2026-07-20T01:00:00Z");
        assert_eq!(c.last_seen, "2026-07-20T04:00:00Z");
        assert_eq!(c.sample_log_ids, vec!["id2", "id1", "id3"]);
        assert_eq!(c.level_counts.get("error"), Some(&3));
        assert_eq!(c.level_counts.get("warn"), Some(&1));
        assert_eq!(c.sample_message, "raw");
    }

    #[test]
    fn test_empty_and_whitespace_lines_skipped() {
        let mut tree = DrainTree::new(DrainConfig::default());
        observe_all(&mut tree, &["", "   ", "\t"]);
        assert!(tree.into_clusters().is_empty());
    }

    #[cfg(test)]
    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn proptest_observe_never_panics(lines in proptest::collection::vec(".*", 0..40)) {
                let mut tree = DrainTree::new(DrainConfig::default());
                for line in &lines {
                    tree.observe(line, &Observation {
                        timestamp: "2026-01-01T00:00:00Z",
                        level: "info",
                        log_id: "x",
                        raw_message: line,
                    });
                }
                let clusters = tree.into_clusters();
                let total: usize = clusters.iter().map(|c| c.count).sum();
                let non_empty = lines.iter()
                    .filter(|l| l.split_whitespace().next().is_some())
                    .count();
                prop_assert_eq!(total, non_empty);
            }
        }
    }
}
