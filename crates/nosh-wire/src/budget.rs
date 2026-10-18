//! Budgeted model tokens, as `packages/wire/src/budget.ts`.

use serde_json::Value;

/// Budgeted tokens weigh cache reads at 10%, close to what providers bill for them. Without a
/// breakdown, the provider total counts.
pub fn budget_tokens(
    total: f64,
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
) -> f64 {
    if input + output + cache_read + cache_write > 0.0 {
        input + output + cache_write + (cache_read / 10.0).ceil()
    } else {
        total
    }
}

/// Budget tokens of one usage event payload, recomputed from its raw counts so every budget uses
/// one formula (including events recorded before weighting).
pub fn event_budget_tokens(payload: &Value) -> f64 {
    let count = |key: &str| match payload.get(key).and_then(Value::as_f64) {
        Some(raw) if raw.is_finite() => raw.max(0.0),
        _ => 0.0,
    };
    // `count("providerTotalTokens") || count("modelTokens")`: 0 falls through.
    let total = match count("providerTotalTokens") {
        0.0 => count("modelTokens"),
        total => total,
    };
    budget_tokens(
        total,
        count("inputTokens"),
        count("outputTokens"),
        count("cacheReadTokens"),
        count("cacheWriteTokens"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_the_typescript_vectors() {
        assert_eq!(
            event_budget_tokens(
                &json!({ "modelTokens": 100, "inputTokens": 20, "outputTokens": 10, "cacheReadTokens": 60, "cacheWriteTokens": 10 })
            ),
            46.0
        );
        assert_eq!(
            event_budget_tokens(
                &json!({ "modelTokens": 46, "providerTotalTokens": 100, "inputTokens": 20, "outputTokens": 10, "cacheReadTokens": 60, "cacheWriteTokens": 10 })
            ),
            46.0
        );
        assert_eq!(event_budget_tokens(&json!({ "modelTokens": 42 })), 42.0);
        assert_eq!(event_budget_tokens(&Value::Null), 0.0);
        assert_eq!(
            event_budget_tokens(
                &json!({ "providerTotalTokens": 0, "modelTokens": 7, "cacheReadTokens": -5 })
            ),
            7.0
        );
        assert_eq!(event_budget_tokens(&json!({ "cacheReadTokens": 1 })), 1.0);
    }
}
