//! A complexity auto-router, ported from ``LiteLLM``'s `complexity_router`.
//!
//! A client requests one virtual model name (`auto_router.model`); each request
//! is scored by a sub-millisecond lexical heuristic and that name is replaced by
//! the model configured for the resulting tier. Cheap asks go to a small model,
//! hard ones to a large one, without the client choosing per request.
//!
//! The scorer, keyword lists, weights and tier boundaries are ``LiteLLM``'s
//! defaults; only the tier→model map is configurable (the tuning knobs do not
//! change the decision often enough to be worth a config surface).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Complexity tier a request is classified into, cheapest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Short, everyday asks.
    Simple,
    /// Typical coding work.
    Medium,
    /// Multi-part or technical work.
    Complex,
    /// Requests that ask for explicit reasoning.
    Reasoning,
}

impl Tier {
    /// ``LiteLLM``'s default boundaries: `simple_medium`, `medium_complex`,
    /// `complex_reasoning`.
    fn from_score(score: f32) -> Self {
        if score < 0.15 {
            Self::Simple
        } else if score < 0.35 {
            Self::Medium
        } else if score < 0.60 {
            Self::Complex
        } else {
            Self::Reasoning
        }
    }

    /// Tier name as it appears in the decision log.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Simple => "SIMPLE",
            Self::Medium => "MEDIUM",
            Self::Complex => "COMPLEX",
            Self::Reasoning => "REASONING",
        }
    }
}

/// Model per tier. A tier left unset uses [`AutoRouter::fallback`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Tiers {
    /// Model for short, everyday asks.
    pub simple: Option<String>,
    /// Model for typical coding work.
    pub medium: Option<String>,
    /// Model for multi-part or technical work.
    pub complex: Option<String>,
    /// Model for requests that ask for explicit reasoning.
    pub reasoning: Option<String>,
}

impl Tiers {
    fn get(&self, tier: Tier) -> Option<&str> {
        match tier {
            Tier::Simple => self.simple.as_deref(),
            Tier::Medium => self.medium.as_deref(),
            Tier::Complex => self.complex.as_deref(),
            Tier::Reasoning => self.reasoning.as_deref(),
        }
    }
}

/// The auto-router config block. Disabled while `model` is empty.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AutoRouter {
    /// Virtual model name clients request to get auto-routed, e.g.
    /// `claude-auto`. Must contain `claude`/`anthropic` to survive Claude Code's
    /// `/model` gateway filter.
    pub model: String,
    /// Model per tier.
    pub tiers: Tiers,
    /// Model for an unset tier.
    pub fallback: Option<String>,
}

/// The model chosen for a request, plus why.
#[derive(Debug)]
pub struct Choice {
    /// Model to resolve through the normal router.
    pub model: String,
    /// Tier the request was classified into.
    pub tier: Tier,
    /// Weighted complexity score (0.0 is trivial, 1.0 is heavy).
    pub score: f32,
    /// Dimensions that fired, for the decision log.
    pub signals: Vec<String>,
}

impl AutoRouter {
    /// Whether `requested` is the auto-router's virtual model name.
    #[must_use]
    pub fn handles(&self, requested: &str) -> bool {
        !self.model.is_empty() && self.model == requested
    }

    /// The tier model to serve `body` with, when `requested` is the virtual
    /// model name and a model is configured for the chosen tier (or as
    /// fallback).
    #[must_use]
    pub fn choice(&self, requested: &str, body: &Value) -> Option<Choice> {
        if !self.handles(requested) {
            return None;
        }
        let (tier, score, signals) = classify(body);
        let model = self.tiers.get(tier).or(self.fallback.as_deref())?;
        Some(Choice {
            model: model.to_string(),
            tier,
            score,
            signals,
        })
    }
}

// LiteLLM's `DEFAULT_*_KEYWORDS`. Single-word entries match whole words only
// (so `api` does not match `capital`); multi-word entries match as phrases.
const CODE_KEYWORDS: &[&str] = &[
    "function", "class", "def", "const", "let", "var", "import", "export", "return", "async",
    "await", "try", "catch", "exception", "error", "debug", "api", "endpoint", "request",
    "response", "database", "sql", "query", "schema", "algorithm", "implement", "refactor",
    "optimize", "python", "javascript", "typescript", "java", "rust", "golang", "react", "vue",
    "angular", "node", "docker", "kubernetes", "git", "commit", "merge", "branch", "pull request",
];
const REASONING_KEYWORDS: &[&str] = &[
    "step by step", "think through", "let's think", "reason through", "analyze this",
    "break down", "explain your reasoning", "show your work", "chain of thought",
    "think carefully", "consider all", "evaluate", "pros and cons", "compare and contrast",
    "weigh the options", "logical", "deduce", "infer", "conclude",
];
const TECHNICAL_KEYWORDS: &[&str] = &[
    "architecture", "distributed", "scalable", "microservice", "machine learning",
    "neural network", "deep learning", "encryption", "authentication", "authorization",
    "performance", "latency", "throughput", "benchmark", "concurrency", "parallel", "threading",
    "memory", "cpu", "gpu", "optimization", "protocol", "tcp", "http", "grpc", "websocket",
    "container", "orchestration",
];
const SIMPLE_KEYWORDS: &[&str] = &[
    "what is", "what's", "define", "definition of", "who is", "who was", "when did", "when was",
    "where is", "where was", "how many", "how much", "yes or no", "true or false", "simple",
    "brief", "short", "quick", "hello", "hi", "hey", "thanks", "thank you", "goodbye", "bye",
    "okay",
];

/// Score `body`'s current ask and map it to a tier.
#[must_use]
pub fn classify(body: &Value) -> (Tier, f32, Vec<String>) {
    let ask = current_ask(body);
    let text = ask.to_lowercase();
    let words: HashSet<&str> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '-'))
        .filter(|w| !w.is_empty())
        .collect();
    let tokens = ask.chars().count() / 4;

    let mut signals = Vec::new();
    let token_score = if tokens < 15 {
        -1.0
    } else if tokens > 400 {
        1.0
    } else {
        0.0
    };

    let (code, _) = keyword_score(&text, &words, CODE_KEYWORDS, (1, 2), (0.0, 0.5, 1.0), "code", &mut signals);
    let (reasoning, reasoning_hits) =
        keyword_score(&text, &words, REASONING_KEYWORDS, (1, 2), (0.0, 0.7, 1.0), "reasoning", &mut signals);
    let (technical, _) =
        keyword_score(&text, &words, TECHNICAL_KEYWORDS, (2, 4), (0.0, 0.5, 1.0), "technical", &mut signals);
    let (simple, _) =
        keyword_score(&text, &words, SIMPLE_KEYWORDS, (1, 2), (0.0, -1.0, -1.0), "simple", &mut signals);
    let multi_step = if is_multi_step(&text) {
        signals.push("multi_step".to_string());
        0.5
    } else {
        0.0
    };
    let questions = text.matches('?').count();
    let question = if questions > 3 {
        signals.push(format!("questions={questions}"));
        0.5
    } else {
        0.0
    };

    // LiteLLM's `DEFAULT_DIMENSION_WEIGHTS`.
    let dimensions = [
        (token_score, 0.10),
        (code, 0.30),
        (reasoning, 0.25),
        (technical, 0.25),
        (simple, 0.05),
        (multi_step, 0.03),
        (question, 0.02),
    ];
    let score = dimensions.iter().map(|(score, weight)| score * weight).sum();

    // Two reasoning markers promote the request unless the mapping would call
    // it SIMPLE anyway.
    let tier = if reasoning_hits >= 2 && score >= 0.15 {
        Tier::Reasoning
    } else {
        Tier::from_score(score)
    };
    (tier, score, signals)
}

/// Score one keyword dimension: `(none, low, high)` at the `(low, high)` match
/// thresholds, pushing `name=count` into `signals` when it fires.
fn keyword_score(
    text: &str,
    words: &HashSet<&str>,
    keywords: &[&str],
    (low, high): (usize, usize),
    (none, low_score, high_score): (f32, f32, f32),
    name: &str,
    signals: &mut Vec<String>,
) -> (f32, usize) {
    let hits = keywords
        .iter()
        .filter(|kw| {
            if kw.contains(' ') {
                text.contains(**kw)
            } else {
                words.contains(**kw)
            }
        })
        .count();
    if hits < low {
        return (none, hits);
    }
    signals.push(format!("{name}={hits}"));
    (if hits >= high { high_score } else { low_score }, hits)
}

/// ``LiteLLM``'s multi-step patterns: `first…then`, `step N`, `N. `, `a) `.
fn is_multi_step(text: &str) -> bool {
    if text
        .find("first")
        .is_some_and(|i| text[i..].contains("then"))
    {
        return true;
    }
    let chars: Vec<char> = text.chars().collect();
    (0..chars.len()).any(|i| {
        // `step` then optional whitespace then a digit
        if chars[i..].starts_with(&['s', 't', 'e', 'p']) {
            let mut j = i + 4;
            while chars.get(j).is_some_and(|c| c.is_whitespace()) {
                j += 1;
            }
            if chars.get(j).is_some_and(char::is_ascii_digit) {
                return true;
            }
        }
        // a digit run then `. ` or a letter then `) `
        match chars[i] {
            c if c.is_ascii_digit() => {
                let mut j = i + 1;
                while chars.get(j).is_some_and(char::is_ascii_digit) {
                    j += 1;
                }
                chars.get(j) == Some(&'.') && chars.get(j + 1).is_some_and(|c| c.is_whitespace())
            }
            c if c.is_ascii_lowercase() => {
                chars.get(i + 1) == Some(&')') && chars.get(i + 2).is_some_and(|c| c.is_whitespace())
            }
            _ => false,
        }
    })
}

/// Text of the newest user message that carries prose: `<system-reminder>`
/// blocks (client-owned boilerplate) are stripped and tool results ignored, so
/// only the caller's own ask is scored.
fn current_ask(body: &Value) -> String {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return String::new();
    };
    for message in messages.iter().rev() {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let text = match message.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        let text = strip_reminders(&text);
        if !text.trim().is_empty() {
            return text;
        }
    }
    String::new()
}

/// Drop every `<system-reminder>…</system-reminder>` span, including an
/// unterminated one.
fn strip_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        rest = &rest[start + OPEN.len()..];
        rest = rest.find(CLOSE).map_or("", |end| &rest[end + CLOSE.len()..]);
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn router() -> AutoRouter {
        AutoRouter {
            model: "claude-auto".to_string(),
            tiers: Tiers {
                simple: Some("small".to_string()),
                medium: Some("mid".to_string()),
                complex: Some("big".to_string()),
                reasoning: None,
            },
            fallback: Some("mid".to_string()),
        }
    }

    fn ask(text: &str) -> Value {
        json!({"model": "claude-auto", "messages": [{"role": "user", "content": text}]})
    }

    #[test]
    fn trivial_ask_is_simple() {
        let (tier, score, _) = classify(&ask("hi"));
        assert_eq!(tier, Tier::Simple, "score={score}");
    }

    #[test]
    fn technical_coding_ask_is_complex() {
        let (tier, score, signals) = classify(&ask(
            "implement the database schema and the API endpoint function for a scalable \
             architecture with low latency and high throughput",
        ));
        assert_eq!(tier, Tier::Complex, "score={score} signals={signals:?}");
    }

    #[test]
    fn two_reasoning_markers_force_reasoning_tier() {
        let (tier, _, _) = classify(&ask("think step by step and analyze this carefully please"));
        assert_eq!(tier, Tier::Reasoning);
    }

    #[test]
    fn system_prompt_does_not_escalate_the_tier() {
        // Claude Code's identity/permission prompt would otherwise trip the code
        // and technical keyword thresholds on every single turn.
        let mut body = ask("hi");
        body["system"] = json!([{
            "type": "text",
            "text": "You are Claude Code. Implement functions, refactor the database \
                     architecture, and benchmark latency and throughput step by step."
        }]);
        let (tier, _, _) = classify(&body);
        assert_eq!(tier, Tier::Simple);
    }

    #[test]
    fn reminder_only_turns_are_skipped() {
        let body = json!({"messages": [
            {"role": "user", "content": "explain step by step and analyze this deeply"},
            {"role": "assistant", "content": "ok"},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "done"},
                {"type": "text", "text": "<system-reminder>carry on</system-reminder>"}
            ]}
        ]});
        let (tier, _, _) = classify(&body);
        assert_eq!(tier, Tier::Reasoning, "falls back to the last real ask");
    }

    #[test]
    fn unset_tier_uses_fallback_and_other_models_are_ignored() {
        let router = router();
        let reasoning = router
            .choice("claude-auto", &ask("think step by step and analyze this carefully please"))
            .expect("reasoning tier has no model, fallback applies");
        assert_eq!(reasoning.model, "mid");
        assert_eq!(reasoning.tier, Tier::Reasoning);

        assert!(router.choice("claude-sonnet-4-5", &ask("hi")).is_none());
    }

    #[test]
    fn disabled_router_never_handles_a_request() {
        let router = AutoRouter::default();
        assert!(!router.handles("claude-auto"));
        assert!(router.choice("claude-auto", &ask("hi")).is_none());
    }
}
