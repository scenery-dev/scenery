//! Heuristic recommendation engine for SKILL.md authoring.
//!
//! Given a skill name, description, and system prompt, `recommend_from_prompt`
//! applies a set of deterministic heuristics to produce a `Recommendations`
//! struct that covers permissions, model sizing, execution constraints, and
//! suggested trigger phrases.
//!
//! This is intentionally a pure, synchronous function — no I/O, no async.
//! AUTH2e (LLM-backed recommendations) is deferred.

// ── Public types ──────────────────────────────────────────────────────────────

/// Full recommendation output from the heuristic engine.
#[derive(Debug, Clone)]
pub struct Recommendations {
    pub triggers: Vec<String>,
    pub permissions: RecommendedPermissions,
    pub model: RecommendedModel,
    pub execution: RecommendedExecution,
    pub confidence: RecommendationConfidence,
}

/// Recommended permission settings for the skill.
#[derive(Debug, Clone)]
pub struct RecommendedPermissions {
    pub network: &'static str,    // "none" | "restricted" | "full"
    pub filesystem: &'static str, // "none" | "read-only" | "full"
    pub clipboard: &'static str,  // "none" | "read" | "write" | "full"
}

/// Recommended model sizing for the skill.
#[derive(Debug, Clone)]
pub struct RecommendedModel {
    pub min_params_b: f32,
    pub recommended: Vec<String>,
    pub fallback: &'static str, // "mcp_sampling" | "error"
}

/// Recommended execution constraints for the skill.
#[derive(Debug, Clone)]
pub struct RecommendedExecution {
    pub timeout_ms: u32,
    pub memory_mb: u32,
    pub sandbox: &'static str, // "strict" | "relaxed" | "unrestricted"
}

/// Confidence level of the produced recommendations.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RecommendationConfidence {
    High,
    Medium,
    Low,
}

// ── Heuristic keywords ────────────────────────────────────────────────────────

const NETWORK_RESTRICTED_KEYWORDS: &[&str] = &[
    "api", "fetch", "http", "url", "endpoint", "webhook", "rest", "graphql",
];

const NETWORK_FULL_KEYWORDS: &[&str] = &["download", "upload", "send request"];

const FILESYSTEM_READ_KEYWORDS: &[&str] = &[
    "read file",
    "load file",
    "parse file",
    "analyze file",
    "open file",
];

const FILESYSTEM_FULL_KEYWORDS: &[&str] = &[
    "write file",
    "save file",
    "create file",
    "output file",
    "delete file",
];

const CLIPBOARD_KEYWORDS: &[&str] = &["clipboard", "paste", "copy to clipboard"];

const COMPLEX_KEYWORDS: &[&str] = &[
    "code",
    "analysis",
    "reasoning",
    "complex",
    "multi-step",
    "research",
];

const HIGH_COMPLEXITY_KEYWORDS: &[&str] = &["complex", "multi-step", "research"];

// ── Public entry point ────────────────────────────────────────────────────────

/// Produce heuristic recommendations for a skill from its name, description,
/// and system prompt.
///
/// All three string slices are combined for keyword matching; the combined text
/// is lowercased before any check.
pub fn recommend_from_prompt(
    name: &str,
    description: &str,
    system_prompt: &str,
) -> Recommendations {
    let combined = format!("{name} {description} {system_prompt}").to_lowercase();

    let network = infer_network(&combined);
    let filesystem = infer_filesystem(&combined);
    let clipboard = infer_clipboard(&combined);
    let min_params_b = infer_min_params_b(&combined, system_prompt.len());
    let timeout_ms = infer_timeout_ms(&combined, system_prompt.len());
    let memory_mb = infer_memory_mb(min_params_b);
    let sandbox = infer_sandbox(network, filesystem);
    let confidence = infer_confidence(network, filesystem, min_params_b);
    let recommended_models = infer_recommended_models(min_params_b);
    let fallback = infer_fallback(min_params_b);
    let triggers = generate_triggers(name, description);

    Recommendations {
        triggers,
        permissions: RecommendedPermissions {
            network,
            filesystem,
            clipboard,
        },
        model: RecommendedModel {
            min_params_b,
            recommended: recommended_models,
            fallback,
        },
        execution: RecommendedExecution {
            timeout_ms,
            memory_mb,
            sandbox,
        },
        confidence,
    }
}

// ── Heuristic sub-functions ───────────────────────────────────────────────────

fn infer_network(lower: &str) -> &'static str {
    for kw in NETWORK_FULL_KEYWORDS {
        if lower.contains(kw) {
            return "full";
        }
    }
    for kw in NETWORK_RESTRICTED_KEYWORDS {
        if lower.contains(kw) {
            return "restricted";
        }
    }
    "none"
}

fn infer_filesystem(lower: &str) -> &'static str {
    for kw in FILESYSTEM_FULL_KEYWORDS {
        if lower.contains(kw) {
            return "full";
        }
    }
    for kw in FILESYSTEM_READ_KEYWORDS {
        if lower.contains(kw) {
            return "read-only";
        }
    }
    "none"
}

fn infer_clipboard(lower: &str) -> &'static str {
    for kw in CLIPBOARD_KEYWORDS {
        if lower.contains(kw) {
            return "read";
        }
    }
    "none"
}

fn has_complex_keywords(lower: &str) -> bool {
    COMPLEX_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

fn has_high_complexity_keywords(lower: &str) -> bool {
    HIGH_COMPLEXITY_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

fn infer_min_params_b(lower: &str, prompt_len: usize) -> f32 {
    let is_long = prompt_len > 2000;
    let is_complex = has_complex_keywords(lower);
    let is_short_and_simple = prompt_len < 500 && !is_complex;

    if is_short_and_simple {
        return 1.0;
    }

    if is_long || is_complex {
        if has_high_complexity_keywords(lower) {
            return 14.0;
        }
        return 7.0;
    }

    3.0
}

fn infer_timeout_ms(lower: &str, prompt_len: usize) -> u32 {
    if prompt_len < 500 {
        return 30000;
    }
    if prompt_len > 2000 || has_complex_keywords(lower) {
        return 120000;
    }
    60000
}

fn infer_memory_mb(min_params_b: f32) -> u32 {
    if min_params_b <= 1.0 {
        return 256;
    }
    if min_params_b <= 8.0 {
        return 512;
    }
    1024
}

fn infer_sandbox(network: &str, filesystem: &str) -> &'static str {
    if network != "none" || filesystem != "none" {
        "relaxed"
    } else {
        "strict"
    }
}

fn infer_confidence(
    network: &str,
    filesystem: &str,
    min_params_b: f32,
) -> RecommendationConfidence {
    let mut signals: u32 = 0;

    if network != "none" {
        signals += 1;
    }
    if filesystem != "none" {
        signals += 1;
    }
    // Model complexity signal: triggered whenever min_params_b > 1.0 (meaning
    // the heuristic found at least one complexity keyword or a long prompt).
    if min_params_b > 1.0 {
        signals += 1;
    }

    match signals {
        0 => RecommendationConfidence::Low,
        1 => RecommendationConfidence::Medium,
        _ => RecommendationConfidence::High,
    }
}

fn infer_recommended_models(min_params_b: f32) -> Vec<String> {
    if min_params_b <= 1.0 {
        vec!["gemma3:2b".to_string()]
    } else if min_params_b <= 3.0 {
        vec!["gemma3:4b".to_string(), "llama3.2:3b".to_string()]
    } else if min_params_b <= 8.0 {
        vec!["llama3.2:8b".to_string(), "gemma3:4b".to_string()]
    } else {
        vec!["llama3.1:70b".to_string()]
    }
}

fn infer_fallback(min_params_b: f32) -> &'static str {
    if min_params_b > 7.0 {
        "mcp_sampling"
    } else {
        "error"
    }
}

// ── Trigger generation ────────────────────────────────────────────────────────

const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "in", "on", "at", "to", "for", "of", "with", "by", "is", "are",
    "was", "be", "that", "this", "it", "as", "from", "into", "than", "then",
];

fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.contains(&word)
}

/// Generate 3–5 trigger phrases from the skill name and description.
///
/// Strategy:
/// 1. Name phrase: convert hyphens to spaces, possibly reverse word order.
/// 2. Description noun/verb phrases: up to 3 phrases split on punctuation.
/// 3. Synthetic variants: "use {name}", "{first_verb} {first_noun}".
/// 4. Filter: min 3 chars, deduplicate, cap at 5.
fn generate_triggers(name: &str, description: &str) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();

    // 1. Name → trigger phrase.
    let name_phrase = name.to_lowercase().replace(['-', '_'], " ");
    candidates.push(name_phrase.trim().to_string());

    // Reversed word order variant (reads better for 2-word skill names like
    // "contract-compare" → "compare contracts"). Skip 3+ word names where
    // reversal produces nonsense phrases.
    let words: Vec<&str> = name_phrase.split_whitespace().collect();
    if words.len() == 2 {
        let mut reversed = words.clone();
        reversed.reverse();
        candidates.push(reversed.join(" "));
    }

    // 2. Description noun/verb phrases (split on sentence-ending punctuation).
    let phrases: Vec<&str> = description
        .split(|c: char| ".!?,;:".contains(c))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .take(3)
        .collect();

    for phrase in phrases {
        let content_words: Vec<&str> = phrase
            .split_whitespace()
            .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
            .filter(|w| !w.is_empty() && !is_stop_word(&w.to_lowercase()))
            .take(4)
            .collect();
        if !content_words.is_empty() {
            candidates.push(content_words.join(" ").to_lowercase());
        }
    }

    // 3. Synthetic variants.
    let clean_name = name.to_lowercase().replace(['-', '_'], " ");
    candidates.push(format!("use {clean_name}"));

    // Try to extract a (verb, noun) pair from description for a compact trigger.
    if !description.is_empty() {
        let desc_words: Vec<&str> = description
            .split_whitespace()
            .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
            .filter(|w| !w.is_empty())
            .collect();
        if desc_words.len() >= 2 {
            let first = desc_words[0].to_lowercase();
            let second = desc_words[1].to_lowercase();
            if !is_stop_word(&first) && !is_stop_word(&second) {
                candidates.push(format!("{first} {second}"));
            }
        }
    }

    // 4. Filter, deduplicate, cap at 5.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut result: Vec<String> = Vec::new();

    for candidate in candidates {
        let lower = candidate.to_lowercase().trim().to_string();
        if lower.len() < 3 {
            continue;
        }
        if seen.insert(lower.clone()) {
            result.push(lower);
            if result.len() == 5 {
                break;
            }
        }
    }

    result
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn api_prompt_yields_restricted_network() {
        let rec = recommend_from_prompt(
            "api-caller",
            "Calls an external REST API",
            "You are an assistant that calls a REST API endpoint to fetch data.",
        );
        assert_eq!(rec.permissions.network, "restricted");
    }

    #[test]
    fn file_read_prompt_yields_read_only_filesystem() {
        let rec = recommend_from_prompt(
            "file-reader",
            "Reads and summarizes a file",
            "Read file contents and summarize the document for the user.",
        );
        assert_eq!(rec.permissions.filesystem, "read-only");
    }

    #[test]
    fn short_simple_prompt_yields_small_model_and_short_timeout() {
        let rec = recommend_from_prompt("greeter", "Says hello", "Say hello to the user.");
        assert!(
            (rec.model.min_params_b - 1.0).abs() < f32::EPSILON,
            "expected 1.0, got {}",
            rec.model.min_params_b
        );
        assert_eq!(rec.execution.timeout_ms, 30000);
    }

    #[test]
    fn long_complex_prompt_yields_large_model_and_long_timeout() {
        // Construct a prompt that is > 2000 chars and contains "research".
        let long_body = "a".repeat(2100);
        let prompt = format!("Do complex multi-step research analysis. {long_body}");
        let rec = recommend_from_prompt("analyst", "Complex research tool", &prompt);
        assert!(
            rec.model.min_params_b >= 7.0,
            "expected >= 7.0, got {}",
            rec.model.min_params_b
        );
        assert_eq!(rec.execution.timeout_ms, 120000);
    }

    #[test]
    fn network_and_filesystem_yields_relaxed_sandbox() {
        let rec = recommend_from_prompt(
            "data-tool",
            "Fetches and writes data",
            "Call the API endpoint to fetch data, then write file to disk.",
        );
        assert_eq!(rec.execution.sandbox, "relaxed");
    }

    #[test]
    fn clean_prompt_yields_strict_sandbox() {
        let rec = recommend_from_prompt(
            "text-summarizer",
            "Summarizes text",
            "Summarize the provided text into a concise paragraph.",
        );
        assert_eq!(rec.execution.sandbox, "strict");
    }

    #[test]
    fn download_keyword_yields_full_network() {
        let rec = recommend_from_prompt(
            "downloader",
            "Downloads content",
            "Download the resource from the given URL.",
        );
        assert_eq!(rec.permissions.network, "full");
    }

    #[test]
    fn clipboard_keyword_yields_clipboard_read() {
        let rec = recommend_from_prompt(
            "clipboard-tool",
            "Reads clipboard",
            "Read the clipboard and process the pasted content.",
        );
        assert_eq!(rec.permissions.clipboard, "read");
    }

    #[test]
    fn triggers_are_generated_and_bounded() {
        let rec = recommend_from_prompt(
            "contract-compare",
            "Compare two contracts and highlight differences",
            "You are an expert contract reviewer.",
        );
        assert!(!rec.triggers.is_empty(), "triggers should not be empty");
        assert!(
            rec.triggers.len() <= 5,
            "at most 5 triggers, got {}",
            rec.triggers.len()
        );
        for t in &rec.triggers {
            assert!(t.len() >= 3, "trigger '{t}' is too short");
        }
    }

    #[test]
    fn triggers_deduplication() {
        // Name and its reversed form should not produce the same single-word trigger.
        let rec = recommend_from_prompt("fix", "Fix things", "Fix the provided input.");
        // All triggers should be unique.
        let mut seen = std::collections::HashSet::new();
        for t in &rec.triggers {
            assert!(seen.insert(t.clone()), "duplicate trigger found: '{t}'");
        }
    }

    #[test]
    fn confidence_no_signals_is_low() {
        let rec = recommend_from_prompt("greet", "Say hello", "Hello!");
        assert_eq!(rec.confidence, RecommendationConfidence::Low);
    }

    #[test]
    fn confidence_one_signal_is_medium() {
        let rec = recommend_from_prompt("net-tool", "Calls APIs", "Call the API to get results.");
        // network restricted → 1 signal, min_params_b 1.0 (short, no complex) → 0 model signal
        assert_eq!(rec.permissions.network, "restricted");
        assert_eq!(rec.confidence, RecommendationConfidence::Medium);
    }

    #[test]
    fn confidence_two_signals_is_high() {
        let rec = recommend_from_prompt(
            "code-analyst",
            "Analyzes code files",
            "Read file and perform complex code analysis with multi-step reasoning.",
        );
        // filesystem read-only + model complexity → >= 2 signals
        assert!(matches!(rec.confidence, RecommendationConfidence::High));
    }

    #[test]
    fn fallback_large_model_uses_mcp_sampling() {
        let prompt = format!(
            "Do multi-step research analysis. {}",
            "detailed ".repeat(300)
        );
        let rec = recommend_from_prompt("research-tool", "Research assistant", &prompt);
        assert_eq!(rec.model.fallback, "mcp_sampling");
    }

    #[test]
    fn fallback_small_model_uses_error() {
        let rec = recommend_from_prompt("greeter", "Says hi", "Say hello.");
        assert_eq!(rec.model.fallback, "error");
    }
}
