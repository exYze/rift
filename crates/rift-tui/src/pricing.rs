//! Cost estimation for metered providers.
//!
//! Built-in USD-per-million-token rates for Anthropic models (from the
//! Anthropic pricing docs, 2026-06); anything else — OpenAI, OpenRouter,
//! custom endpoints — comes from the config's `pricing` map:
//!
//! ```json
//! { "pricing": { "gpt-5": {"input": 1.25, "output": 10.0} } }
//! ```
//!
//! Keys are lowercase substrings matched against the model name; the
//! longest match wins so `claude-opus-4-8` can override a broader `claude`
//! entry. Local models match nothing → no cost is ever shown for them.

use std::collections::HashMap;

use rift_core::config::Pricing;

/// Built-in rates: (model-name substring, $/MTok input, $/MTok output).
/// Only models with rates verified against Anthropic's published pricing —
/// unknown models show no cost rather than a wrong one.
const BUILTIN: &[(&str, f64, f64)] = &[
    ("claude-fable-5", 10.0, 50.0),
    ("claude-mythos-5", 10.0, 50.0),
    ("claude-opus-4-8", 5.0, 25.0),
    ("claude-opus-4-7", 5.0, 25.0),
    ("claude-opus-4-6", 5.0, 25.0),
    ("claude-sonnet-4-6", 3.0, 15.0),
    ("claude-haiku-4-5", 1.0, 5.0),
];

/// Resolve ($/MTok input, $/MTok output) for a model: config entries first
/// (user-supplied rates beat built-ins), then the built-in table; among
/// matches the longest (most specific) substring wins.
pub fn lookup(model: &str, user: &HashMap<String, Pricing>) -> Option<(f64, f64)> {
    let m = model.to_lowercase();
    let from_user = user
        .iter()
        .filter(|(k, _)| m.contains(&k.to_lowercase()))
        .max_by_key(|(k, _)| k.len())
        .map(|(_, p)| (p.input, p.output));
    if from_user.is_some() {
        return from_user;
    }
    BUILTIN
        .iter()
        .filter(|(k, _, _)| m.contains(k))
        .max_by_key(|(k, _, _)| k.len())
        .map(|(_, i, o)| (*i, *o))
}

/// Estimated cost in USD. `billed_input` must be the summed per-call prompt
/// tokens (every iteration re-sends the conversation), not the last call's.
pub fn cost(billed_input: u64, output: u64, (in_rate, out_rate): (f64, f64)) -> f64 {
    billed_input as f64 / 1e6 * in_rate + output as f64 / 1e6 * out_rate
}

/// `$0.0123`-style display; sub-cent costs keep 4 decimals so short local
/// sessions on cheap models don't render as $0.00.
pub fn format_cost(usd: f64) -> String {
    if usd >= 0.1 {
        format!("${usd:.2}")
    } else {
        format!("${usd:.4}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_rates_resolve_by_substring() {
        let none = HashMap::new();
        assert_eq!(lookup("anthropic/claude-opus-4-8", &none), Some((5.0, 25.0)));
        assert_eq!(lookup("claude-sonnet-4-6", &none), Some((3.0, 15.0)));
        assert_eq!(lookup("gemma4:26b", &none), None);
        assert_eq!(lookup("gpt-5", &none), None);
    }

    #[test]
    fn config_pricing_beats_builtin_and_longest_match_wins() {
        let mut user = HashMap::new();
        user.insert("claude-opus-4-8".into(), Pricing { input: 4.0, output: 20.0 });
        user.insert("qwen".into(), Pricing { input: 0.2, output: 0.6 });
        user.insert("qwen3.6:35b".into(), Pricing { input: 0.5, output: 1.5 });
        assert_eq!(lookup("claude-opus-4-8", &user), Some((4.0, 20.0)));
        assert_eq!(lookup("openrouter/qwen3.6:35b", &user), Some((0.5, 1.5)));
        assert_eq!(lookup("qwen3.6:27b", &user), Some((0.2, 0.6)));
    }

    #[test]
    fn cost_math_and_formatting() {
        // 100k in @ $5 + 10k out @ $25 = $0.50 + $0.25
        assert!((cost(100_000, 10_000, (5.0, 25.0)) - 0.75).abs() < 1e-9);
        assert_eq!(format_cost(0.75), "$0.75");
        assert_eq!(format_cost(0.0123), "$0.0123");
    }
}
