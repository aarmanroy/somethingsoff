//! Guided pattern learning for custom log formats.
//!
//! This module suggests regex patterns from example log lines so agents can
//! bootstrap custom parsers without hand-writing regex from scratch.

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PatternSuggestion {
    pub regex: String,
    pub confidence: f64,
    pub matches: BTreeMap<String, String>,
}

static BRACKET_CAPTURE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[([^\]]+)\]").unwrap());
static TIMESTAMP_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^\d{4}-\d{2}-\d{2}(?:[T\s]\d{2}:\d{2}:\d{2}(?:[.,]\d{3})?(?:Z|[+-]\d{2}:?\d{2})?)?$",
    )
    .unwrap()
});
static LEVEL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(TRACE|DEBUG|INFO|WARN|WARNING|ERROR|CRITICAL|FATAL|PANIC)$").unwrap()
});
static METHOD_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)$").unwrap());
static STATUS_CODE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[1-5]\d{2}$").unwrap());

pub fn suggest_patterns(sample: &str) -> Vec<PatternSuggestion> {
    let sample = sample.trim();
    if sample.is_empty() {
        return Vec::new();
    }

    let mut suggestions = Vec::new();

    if let Some(suggestion) = suggest_bracketed_pattern(sample) {
        suggestions.push(suggestion);
    }

    if let Some(suggestion) = suggest_delimited_pattern(sample) {
        if !suggestions
            .iter()
            .any(|existing| existing.regex == suggestion.regex)
        {
            suggestions.push(suggestion);
        }
    }

    if suggestions.is_empty() {
        suggestions.push(PatternSuggestion {
            regex: "^(?P<message>.*)$".to_string(),
            confidence: 0.25,
            matches: BTreeMap::from([("message".to_string(), sample.to_string())]),
        });
    }

    suggestions
}

fn suggest_bracketed_pattern(sample: &str) -> Option<PatternSuggestion> {
    let captures: Vec<_> = BRACKET_CAPTURE_RE.captures_iter(sample).collect();
    if captures.len() < 2 {
        return None;
    }

    let mut regex = String::from("^");
    let mut matches = BTreeMap::new();
    let mut used_fields = HashSet::new();
    let mut previous_end = 0usize;
    let last_capture = captures.last()?;
    let has_tail_message = !sample[last_capture.get(0)?.end()..].trim().is_empty();

    for (idx, capture) in captures.iter().enumerate() {
        let full = capture.get(0)?;
        let value = capture.get(1)?.as_str().trim();
        regex.push_str(&regex::escape(&sample[previous_end..full.start()]));

        let inferred = infer_field_name(value, idx, idx == captures.len() - 1 && !has_tail_message);
        let field_name = uniquify_field_name(inferred, &mut used_fields);
        regex.push_str(r"\[(?P<");
        regex.push_str(&field_name);
        regex.push('>');
        regex.push_str(capture_pattern_for(&field_name, true));
        regex.push_str(r")\]");

        matches.insert(field_name, value.to_string());
        previous_end = full.end();
    }

    let tail = &sample[previous_end..];
    let leading_ws_len = tail.len() - tail.trim_start().len();
    regex.push_str(&regex::escape(&tail[..leading_ws_len]));
    if !tail.trim().is_empty() {
        regex.push_str("(?P<message>.*)");
        matches.insert("message".to_string(), tail.trim().to_string());
    }
    regex.push('$');

    Some(PatternSuggestion {
        regex,
        confidence: score_confidence(&matches, 0.84),
        matches,
    })
}

fn suggest_delimited_pattern(sample: &str) -> Option<PatternSuggestion> {
    let separators = [" | ", " - ", "\t", " :: ", "::"];

    let separator = separators.iter().find(|sep| sample.contains(**sep))?;
    let parts: Vec<_> = sample.split(separator).map(str::trim).collect();
    if parts.len() < 3 {
        return None;
    }

    let mut regex = String::from("^");
    let mut matches = BTreeMap::new();
    let mut used_fields = HashSet::new();

    for (idx, part) in parts.iter().enumerate() {
        if idx > 0 {
            regex.push_str(&regex::escape(separator));
        }

        let inferred = if idx == parts.len() - 1 {
            "message".to_string()
        } else {
            infer_field_name(part, idx, false)
        };
        let field_name = uniquify_field_name(inferred, &mut used_fields);
        regex.push_str("(?P<");
        regex.push_str(&field_name);
        regex.push('>');
        regex.push_str(capture_pattern_for(&field_name, false));
        regex.push(')');
        matches.insert(field_name, (*part).to_string());
    }

    regex.push('$');

    Some(PatternSuggestion {
        regex,
        confidence: score_confidence(&matches, 0.78),
        matches,
    })
}

fn infer_field_name(value: &str, index: usize, allow_message: bool) -> String {
    let trimmed = value.trim();
    let upper = trimmed.to_uppercase();

    if TIMESTAMP_RE.is_match(trimmed) {
        "timestamp".to_string()
    } else if LEVEL_RE.is_match(&upper) {
        "level".to_string()
    } else if METHOD_RE.is_match(&upper) {
        "method".to_string()
    } else if STATUS_CODE_RE.is_match(trimmed) {
        "status_code".to_string()
    } else if allow_message && trimmed.contains(' ') {
        "message".to_string()
    } else if looks_like_source(trimmed) {
        "source".to_string()
    } else {
        format!("field_{}", index + 1)
    }
}

fn looks_like_source(value: &str) -> bool {
    value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | ':'))
        && !value.is_empty()
}

fn uniquify_field_name(candidate: String, used_fields: &mut HashSet<String>) -> String {
    if used_fields.insert(candidate.clone()) {
        return candidate;
    }

    let mut suffix = 2usize;
    loop {
        let named = format!("{}_{}", candidate, suffix);
        if used_fields.insert(named.clone()) {
            return named;
        }
        suffix += 1;
    }
}

fn capture_pattern_for(field_name: &str, bracketed: bool) -> &'static str {
    match field_name {
        "timestamp" => {
            r"\d{4}-\d{2}-\d{2}(?:[T\s]\d{2}:\d{2}:\d{2}(?:[.,]\d{3})?(?:Z|[+-]\d{2}:?\d{2})?)?"
        }
        "level" => r"(?:TRACE|DEBUG|INFO|WARN|WARNING|ERROR|CRITICAL|FATAL|PANIC)",
        "method" => r"(?:GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)",
        "status_code" => r"[1-5]\d{2}",
        "message" => r".*",
        _ if bracketed => r"[^\]]+",
        _ => r".+?",
    }
}

fn score_confidence(matches: &BTreeMap<String, String>, base: f64) -> f64 {
    let mut confidence = base;
    if matches.contains_key("timestamp") {
        confidence += 0.06;
    }
    if matches.contains_key("level") {
        confidence += 0.05;
    }
    if matches.contains_key("message") {
        confidence += 0.03;
    }
    confidence.min(0.99)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_suggest_patterns_for_bracketed_log() {
        let suggestions = suggest_patterns("[2026-03-29] [DB] [ERROR] Timeout");
        let suggestion = &suggestions[0];

        assert!(suggestion.regex.contains("timestamp"));
        assert!(suggestion.regex.contains("source"));
        assert!(suggestion.regex.contains("level"));
        assert_eq!(suggestion.matches.get("timestamp").unwrap(), "2026-03-29");
        assert_eq!(suggestion.matches.get("source").unwrap(), "DB");
        assert_eq!(suggestion.matches.get("level").unwrap(), "ERROR");
        assert_eq!(suggestion.matches.get("message").unwrap(), "Timeout");
    }

    #[test]
    fn test_suggest_patterns_for_delimited_log() {
        let suggestions = suggest_patterns("2026-03-29 10:00:00,000 | WARN | api | timeout");
        let suggestion = suggestions
            .iter()
            .find(|item| item.matches.get("message") == Some(&"timeout".to_string()))
            .unwrap();

        assert!(suggestion.regex.contains("timestamp"));
        assert!(suggestion.regex.contains("level"));
        assert!(suggestion.regex.contains("source"));
        assert_eq!(suggestion.matches.get("source").unwrap(), "api");
    }

    #[test]
    fn test_suggest_patterns_empty_input() {
        assert!(suggest_patterns("").is_empty());
    }

    #[test]
    fn test_suggest_patterns_fallback_for_freeform_line() {
        let suggestions = suggest_patterns("opaque legacy line");

        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].regex, "^(?P<message>.*)$");
        assert_eq!(
            suggestions[0].matches.get("message"),
            Some(&"opaque legacy line".to_string())
        );
    }
}
