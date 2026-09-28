use crate::{
    cli::CostMode,
    pricing::{Pricing, PricingMap},
    types::{Speed, UsageEntry},
};

const CACHE_CREATE_1H_INPUT_MULTIPLIER: f64 = 2.0;

/// `-free` suffix match without allocating: byte compare suffices because
/// `to_ascii_lowercase` only affects ASCII bytes, and this runs up to 3x per row.
fn is_free_tier(model: &str) -> bool {
    model.len() >= 4 && model.as_bytes()[model.len() - 4..].eq_ignore_ascii_case(b"free")
}

pub fn calculate_cost(data: &UsageEntry, mode: CostMode, pricing: Option<&PricingMap>) -> f64 {
    calculate_cost_for_usage_at(
        data.message.model.as_deref(),
        data.message.usage,
        data.cost_usd,
        crate::parse_ts_timestamp(&data.timestamp),
        mode,
        pricing,
    )
}

/// Cost and missing-pricing flag for output paths, honoring the cost mode.
///
/// Cost and the missing-pricing flag are decided together so they stay
/// coherent: a row that surfaces a logged `costUSD` is never also flagged as
/// missing pricing.
///
/// `Display` surfaces the agent's logged `costUSD` verbatim (or `0.0` when
/// absent) and never reprices — "display" means show what the agent recorded.
///
/// `Auto` uses the logged `costUSD` when present; only falls back to token
/// computation when the entry has no logged cost. This keeps warm-run output
/// faithful to the agent's own accounting for mapped models, while still
/// computing cost for unmapped models that carry no logged spend.
///
/// `Calculate` always derives cost from tokens + current pricing, ignoring the
/// logged `costUSD` entirely.
pub fn cost_and_missing_for_output(
    data: &UsageEntry,
    mode: CostMode,
    pricing: Option<&PricingMap>,
) -> (f64, Option<String>) {
    let model = data.message.model.as_deref();
    let usage = data.message.usage;
    // Only DeepSeek V4 ids have time-dependent schedules; parsing timestamps for
    // everything else would regress warm runs ~2x on this hot reprice path.
    let cost = if model.is_some_and(crate::pricing::has_time_dependent_pricing) {
        calculate_cost(data, mode, pricing)
    } else {
        calculate_cost_for_usage(model, usage, data.cost_usd, mode, pricing)
    };
    let missing = missing_pricing_model_for_usage(model, usage, data.cost_usd, mode, pricing);
    (cost, missing)
}

pub fn calculate_cost_for_usage(
    model: Option<&str>,
    usage: crate::TokenUsageRaw,
    cost_usd: Option<f64>,
    mode: CostMode,
    pricing: Option<&PricingMap>,
) -> f64 {
    if let Some(model) = model
        && is_free_tier(model)
    {
        return 0.0;
    }
    calculate_cost_for_usage_at(model, usage, cost_usd, None, mode, pricing)
}

pub fn calculate_cost_for_usage_at(
    model: Option<&str>,
    usage: crate::TokenUsageRaw,
    cost_usd: Option<f64>,
    timestamp: Option<crate::TimestampMs>,
    mode: CostMode,
    pricing: Option<&PricingMap>,
) -> f64 {
    // Free-tier guard lives here so the timestamp-aware wrapper bills $0.00 in
    // every mode, including Display/Auto with a logged cost.
    if let Some(model) = model
        && is_free_tier(model)
    {
        return 0.0;
    }
    match mode {
        CostMode::Display => cost_usd.unwrap_or(0.0),
        CostMode::Auto => {
            cost_usd.unwrap_or_else(|| calculate_cost_from_tokens(model, usage, timestamp, pricing))
        }
        CostMode::Calculate => calculate_cost_from_tokens(model, usage, timestamp, pricing),
    }
}

pub fn missing_pricing_model_for_usage(
    model: Option<&str>,
    usage: crate::TokenUsageRaw,
    cost_usd: Option<f64>,
    mode: CostMode,
    pricing: Option<&PricingMap>,
) -> Option<String> {
    if let Some(model) = model
        && is_free_tier(model)
    {
        return None;
    }
    if mode == CostMode::Display || (mode == CostMode::Auto && cost_usd.is_some()) {
        return None;
    }
    missing_pricing_model_for_token_total(model, crate::total_usage_tokens(usage), pricing)
}

pub fn missing_pricing_model_for_token_total(
    model: Option<&str>,
    total_tokens: u64,
    pricing: Option<&PricingMap>,
) -> Option<String> {
    if let Some(model) = model
        && is_free_tier(model)
    {
        return None;
    }
    if total_tokens == 0 {
        return None;
    }
    let model = model?;
    let pricing = pricing?;
    pricing
        .find(model)
        .is_none()
        .then(|| crate::model_aliases::resolve_model_name(model).into_owned())
}

pub fn missing_pricing_model_for_candidates(
    model: &str,
    candidates: impl IntoIterator<Item = String>,
    total_tokens: u64,
    pricing: Option<&PricingMap>,
) -> Option<String> {
    if is_free_tier(model) {
        return None;
    }
    if total_tokens == 0 {
        return None;
    }
    let pricing = pricing?;
    candidates
        .into_iter()
        .all(|candidate| pricing.find(&candidate).is_none())
        .then(|| crate::model_aliases::resolve_model_name(model).into_owned())
}

fn calculate_cost_from_tokens(
    model: Option<&str>,
    usage: crate::TokenUsageRaw,
    timestamp: Option<crate::TimestampMs>,
    pricing: Option<&PricingMap>,
) -> f64 {
    let Some(model) = model else {
        return 0.0;
    };
    if is_free_tier(model) {
        return 0.0;
    }
    let Some(pricing) = pricing.and_then(|pricing| {
        timestamp.map_or_else(
            || pricing.find(model),
            |timestamp| pricing.find_at(model, timestamp),
        )
    }) else {
        return 0.0;
    };
    let multiplier = if matches!(usage.speed, Some(Speed::Fast)) {
        pricing.fast_multiplier
    } else {
        1.0
    };
    calculate_cost_from_pricing(usage, pricing) * multiplier
}

pub fn calculate_cost_from_pricing(usage: crate::TokenUsageRaw, pricing: Pricing) -> f64 {
    let (cache_create_5m_tokens, cache_create_1h_tokens) =
        if let Some(breakdown) = usage.cache_creation {
            (
                breakdown.ephemeral_5m_input_tokens,
                breakdown.ephemeral_1h_input_tokens,
            )
        } else {
            (usage.cache_creation_input_tokens, 0)
        };
    let cache_create_1h_cost = pricing.input * CACHE_CREATE_1H_INPUT_MULTIPLIER;
    let cache_create_1h_cost_above_200k = pricing
        .input_above_200k
        .map(|c| c * CACHE_CREATE_1H_INPUT_MULTIPLIER);

    // Two-stage pricing: a per-model `long_context_threshold` means the
    // request's input size selects the tier and every bucket is billed entirely
    // at that tier's rate. The whole request switches once input exceeds the
    // threshold, so this is not a marginal breakpoint. This mirrors the Codex
    // per-request tiering in `calculate_codex_model_cost`.
    //
    // `input_tokens` here is the uncached remainder - adapters normalize usage
    // into the Claude shape - but the vendor's tier is chosen by the request's
    // whole context, cached or not: a Grok turn re-reading 8M cached tokens
    // with 10K fresh ones is a long-context request, not a short one.
    if let Some(threshold) = pricing.long_context_threshold {
        // Saturating: the counters come from lenient JSONL parsing, and a
        // corrupt line must not wrap the sum below the threshold or abort under
        // overflow checks.
        let context_tokens = usage
            .input_tokens
            .saturating_add(usage.cache_read_input_tokens)
            .saturating_add(cache_create_5m_tokens)
            .saturating_add(cache_create_1h_tokens);
        let long_context = context_tokens > threshold;
        let rate = |base: f64, above: Option<f64>| {
            if long_context {
                above.unwrap_or(base)
            } else {
                base
            }
        };
        return usage.input_tokens as f64 * rate(pricing.input, pricing.input_above_200k)
            + usage.output_tokens as f64 * rate(pricing.output, pricing.output_above_200k)
            + cache_create_5m_tokens as f64
                * rate(pricing.cache_create, pricing.cache_create_above_200k)
            + cache_create_1h_tokens as f64
                * rate(cache_create_1h_cost, cache_create_1h_cost_above_200k)
            + usage.cache_read_input_tokens as f64
                * rate(pricing.cache_read, pricing.cache_read_above_200k);
    }

    // LiteLLM `*_above_200k_tokens` data keeps its marginal above-threshold
    // semantics at the default 200K boundary.
    let threshold = crate::pricing::DEFAULT_LONG_CONTEXT_THRESHOLD_TOKENS;
    tiered_cost(
        usage.input_tokens,
        pricing.input,
        pricing.input_above_200k,
        threshold,
    ) + tiered_cost(
        usage.output_tokens,
        pricing.output,
        pricing.output_above_200k,
        threshold,
    ) + tiered_cost(
        cache_create_5m_tokens,
        pricing.cache_create,
        pricing.cache_create_above_200k,
        threshold,
    ) + tiered_cost(
        cache_create_1h_tokens,
        cache_create_1h_cost,
        cache_create_1h_cost_above_200k,
        threshold,
    ) + tiered_cost(
        usage.cache_read_input_tokens,
        pricing.cache_read,
        pricing.cache_read_above_200k,
        threshold,
    )
}

pub fn tiered_cost(tokens: u64, base: f64, above: Option<f64>, threshold: u64) -> f64 {
    if tokens == 0 {
        return 0.0;
    }
    if let Some(above) = above
        && tokens > threshold
    {
        return (threshold as f64 * base) + ((tokens - threshold) as f64 * above);
    }
    tokens as f64 * base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_tier_suffix_is_zero_across_modes_with_timestamp_lookup() {
        // A logged cost on a `-free` model is never billed in any mode.
        let pricing = pricing();
        for mode in [CostMode::Display, CostMode::Auto, CostMode::Calculate] {
            let data = entry("test-model-free", Some(42.0));
            assert_eq!(
                calculate_cost(&data, mode, Some(&pricing)),
                0.0,
                "mode={mode:?} must not bill free-tier models"
            );
            assert_eq!(
                calculate_cost_for_usage_at(
                    Some("test-model-free"),
                    data.message.usage,
                    Some(42.0),
                    crate::parse_ts_timestamp(&data.timestamp),
                    mode,
                    Some(&pricing),
                ),
                0.0,
                "mode={mode:?} must not bill free-tier models"
            );
        }
    }

    #[test]
    fn zero_rate_pricing_is_not_missing_pricing() {
        let mut pricing = PricingMap::default();
        pricing.load_json(
            r#"{ "free-model": { "input_cost_per_token": 0, "output_cost_per_token": 0 } }"#,
        );

        assert_eq!(
            missing_pricing_model_for_token_total(Some("free-model"), 100, Some(&pricing)),
            None
        );
        assert_eq!(
            missing_pricing_model_for_token_total(Some("unknown-model"), 100, Some(&pricing)),
            Some("unknown-model".to_string())
        );
        assert_eq!(
            missing_pricing_model_for_token_total(Some("unknown-model"), 0, Some(&pricing)),
            None,
            "usage without tokens is not reported"
        );
    }

    #[test]
    fn stored_costs_suppress_missing_pricing_detection() {
        let pricing = PricingMap::default();
        let usage = crate::TokenUsageRaw {
            input_tokens: 100,
            output_tokens: 10,
            ..crate::TokenUsageRaw::default()
        };

        assert_eq!(
            missing_pricing_model_for_usage(
                Some("unknown-model"),
                usage,
                None,
                CostMode::Auto,
                Some(&pricing)
            ),
            Some("unknown-model".to_string())
        );
        assert_eq!(
            missing_pricing_model_for_usage(
                Some("unknown-model"),
                usage,
                Some(1.5),
                CostMode::Auto,
                Some(&pricing)
            ),
            None,
            "auto mode trusts a stored cost"
        );
        assert_eq!(
            missing_pricing_model_for_usage(
                Some("unknown-model"),
                usage,
                None,
                CostMode::Display,
                Some(&pricing)
            ),
            None,
            "display mode never looks prices up"
        );
        assert_eq!(
            missing_pricing_model_for_usage(
                Some("unknown-model"),
                usage,
                None,
                CostMode::Calculate,
                Some(&pricing)
            ),
            Some("unknown-model".to_string())
        );
    }

    use crate::{
        cli::CostMode,
        pricing::PricingMap,
        types::{CacheCreationRaw, TokenUsageRaw, UsageEntry, UsageMessage},
    };

    use super::{
        calculate_cost, calculate_cost_for_usage, calculate_cost_for_usage_at,
        cost_and_missing_for_output,
    };

    fn entry(model: &str, cost_usd: Option<f64>) -> UsageEntry {
        UsageEntry {
            session_id: Some("session".to_string()),
            timestamp: "2026-01-02T00:00:00.000Z".to_string(),
            version: None,
            message: UsageMessage {
                usage: TokenUsageRaw {
                    input_tokens: 100,
                    output_tokens: 200,
                    ..TokenUsageRaw::default()
                },
                model: Some(model.to_string()),
                id: None,
                provider: None,
            },
            cost_usd,
            request_id: None,
            is_api_error_message: None,
            is_sidechain: None,
        }
    }

    #[test]
    fn auto_falls_back_to_logged_cost_for_unmapped_model() {
        // Model absent from the pricing map: Auto must surface the logged
        // costUSD rather than zeroing it, and must not flag missing pricing.
        let data = entry("model-not-in-map", Some(0.42));
        let (cost, missing) = cost_and_missing_for_output(&data, CostMode::Auto, Some(&pricing()));
        assert_eq!(cost, 0.42);
        assert_eq!(missing, None);
    }

    #[test]
    fn calculate_stays_strict_zero_for_unmapped_model() {
        // Calculate with pricing available keeps its always-recompute contract:
        // an unmapped model is $0.00 and is flagged as missing pricing.
        let data = entry("model-not-in-map", Some(0.42));
        let (cost, missing) =
            cost_and_missing_for_output(&data, CostMode::Calculate, Some(&pricing()));
        assert_eq!(cost, 0.0);
        assert_eq!(missing.as_deref(), Some("model-not-in-map"));
    }

    #[test]
    fn auto_uses_logged_cost_for_mapped_model() {
        // Mapped model in Auto with logged costUSD: the logged value is used
        // verbatim (999.0), not recomputed from tokens. No missing flag.
        let data = entry("test-model", Some(999.0));
        let (cost, missing) = cost_and_missing_for_output(&data, CostMode::Auto, Some(&pricing()));
        assert!((cost - 999.0).abs() < f64::EPSILON);
        assert_eq!(missing, None);
    }

    #[test]
    fn auto_falls_back_to_compute_when_no_logged_cost() {
        // Auto with no logged costUSD and a priceable model: computes from tokens.
        let data = entry("test-model", None);
        let (cost, missing) = cost_and_missing_for_output(&data, CostMode::Auto, Some(&pricing()));
        assert!((cost - 2100.0).abs() < f64::EPSILON);
        assert_eq!(missing, None);
    }

    #[test]
    fn auto_flags_missing_when_no_logged_cost_and_unmapped() {
        // Auto with no logged costUSD and an unmapped model: flags missing pricing.
        let data = entry("model-not-in-map", None);
        let (cost, missing) = cost_and_missing_for_output(&data, CostMode::Auto, Some(&pricing()));
        assert_eq!(cost, 0.0);
        assert_eq!(missing.as_deref(), Some("model-not-in-map"));
    }

    fn pricing() -> PricingMap {
        let mut pricing = PricingMap::default();
        pricing.load_json(
            r#"{
                "test-model": {
                    "input_cost_per_token": 1.0,
                    "output_cost_per_token": 10.0,
                    "cache_creation_input_token_cost": 1.25,
                    "cache_read_input_token_cost": 0.1,
                    "input_cost_per_token_above_200k_tokens": 2.0,
                    "cache_creation_input_token_cost_above_200k_tokens": 1.5
                }
            }"#,
        );
        pricing
    }

    #[test]
    fn prices_cache_creation_breakdown_by_duration() {
        let usage = TokenUsageRaw {
            cache_creation_input_tokens: 999,
            cache_read_input_tokens: 30,
            cache_creation: Some(CacheCreationRaw {
                ephemeral_5m_input_tokens: 10,
                ephemeral_1h_input_tokens: 20,
            }),
            ..TokenUsageRaw::default()
        };

        let cost = calculate_cost_for_usage(
            Some("test-model"),
            usage,
            None,
            CostMode::Calculate,
            Some(&pricing()),
        );

        assert!((cost - 55.5).abs() < f64::EPSILON);
    }

    #[test]
    fn falls_back_to_flat_cache_creation_rate_without_breakdown() {
        let usage = TokenUsageRaw {
            cache_creation_input_tokens: 10,
            ..TokenUsageRaw::default()
        };

        let cost = calculate_cost_for_usage(
            Some("test-model"),
            usage,
            None,
            CostMode::Calculate,
            Some(&pricing()),
        );

        assert!((cost - 12.5).abs() < f64::EPSILON);
    }

    #[test]
    fn prices_two_stage_model_as_whole_request_at_long_context_rates() {
        let mut pricing = PricingMap::default();
        assert_eq!(
            pricing.load_models_dev_json_for_tests(
                r#"{
                    "two-stage-test": {
                        "cost": {
                            "input": 1,
                            "output": 10,
                            "cache_read": 0.1,
                            "tiers": [{
                                "input": 2,
                                "output": 20,
                                "cache_read": 0.2,
                                "tier": { "type": "context", "size": 100 }
                            }]
                        }
                    }
                }"#,
            ),
            Some(1)
        );

        // Keep cache reads non-zero so tier selection covers the whole context
        // rather than only freshly processed input.
        let short = TokenUsageRaw {
            input_tokens: 59,
            output_tokens: 3,
            cache_read_input_tokens: 40,
            ..TokenUsageRaw::default()
        };
        let cost = calculate_cost_for_usage(
            Some("two-stage-test"),
            short,
            None,
            CostMode::Calculate,
            Some(&pricing),
        );
        assert!(
            (cost - 93e-6).abs() < 1e-12,
            "short-context cost was {cost}"
        );

        let boundary = TokenUsageRaw {
            input_tokens: 60,
            output_tokens: 3,
            cache_read_input_tokens: 40,
            ..TokenUsageRaw::default()
        };
        let cost = calculate_cost_for_usage(
            Some("two-stage-test"),
            boundary,
            None,
            CostMode::Calculate,
            Some(&pricing),
        );
        assert!((cost - 94e-6).abs() < 1e-12, "boundary cost was {cost}");

        let long = TokenUsageRaw {
            input_tokens: 61,
            output_tokens: 3,
            cache_read_input_tokens: 40,
            ..TokenUsageRaw::default()
        };
        let cost = calculate_cost_for_usage(
            Some("two-stage-test"),
            long,
            None,
            CostMode::Calculate,
            Some(&pricing),
        );
        assert!(
            (cost - 190e-6).abs() < 1e-12,
            "long-context cost was {cost}"
        );
    }

    #[test]
    fn cached_context_selects_the_long_context_tier() {
        // The tier is chosen by the whole context the request carried, and on
        // agents with aggressive prompt caching almost all of it is cache
        // reads: a Grok turn re-reading megabytes of cached context with a few
        // thousand fresh tokens is a long-context request. Judging by uncached
        // input alone billed such turns at the short-context rate.
        let pricing = PricingMap::load_embedded();

        // grok-4.5: base $2/$6, cache read $0.3; above 200K context $4/$12/$0.6.
        let cached_heavy = TokenUsageRaw {
            input_tokens: 10_000,
            output_tokens: 1_000,
            cache_read_input_tokens: 500_000,
            ..TokenUsageRaw::default()
        };
        let cost = calculate_cost_for_usage(
            Some("grok-4.5"),
            cached_heavy,
            None,
            CostMode::Calculate,
            Some(&pricing),
        );
        // 0.01M * 4 + 0.001M * 12 + 0.5M * 0.6
        assert!((cost - 0.352).abs() < 1e-9, "cached-heavy cost was {cost}");

        // The same shape below the boundary stays on the base rates.
        let short = TokenUsageRaw {
            input_tokens: 10_000,
            output_tokens: 1_000,
            cache_read_input_tokens: 100_000,
            ..TokenUsageRaw::default()
        };
        let cost = calculate_cost_for_usage(
            Some("grok-4.5"),
            short,
            None,
            CostMode::Calculate,
            Some(&pricing),
        );
        // 0.01M * 2 + 0.001M * 6 + 0.1M * 0.3
        assert!((cost - 0.056).abs() < 1e-9, "short-context cost was {cost}");
    }

    #[test]
    fn parses_cache_creation_breakdown_from_usage_json() {
        let usage = serde_json::from_str::<TokenUsageRaw>(
            r#"{
                "input_tokens": 1,
                "output_tokens": 2,
                "cache_creation_input_tokens": 300,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 100,
                    "ephemeral_1h_input_tokens": 200
                }
            }"#,
        )
        .unwrap();

        assert_eq!(usage.cache_creation_token_count(), 300);
    }

    fn deepseek_pricing() -> PricingMap {
        let mut pricing = PricingMap::default();
        pricing.load_json(
            r#"{
                "deepseek-v4-flash": {
                    "input_cost_per_token": 0.00000014,
                    "output_cost_per_token": 0.00000028,
                    "cache_creation_input_token_cost": 0.000000123,
                    "cache_read_input_token_cost": 0.0000000028
                },
                "deepseek-v4-pro": {
                    "input_cost_per_token": 0.000000435,
                    "output_cost_per_token": 0.00000087,
                    "cache_creation_input_token_cost": 0.000000456,
                    "cache_read_input_token_cost": 0.000000003625
                }
            }"#,
        );
        pricing
    }

    fn timestamp(value: &str) -> crate::TimestampMs {
        crate::parse_ts_timestamp(value).unwrap()
    }

    #[test]
    fn calculates_all_deepseek_v4_token_buckets_at_each_scheduled_rate() {
        let pricing = deepseek_pricing();
        let usage = TokenUsageRaw {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_creation_input_tokens: 1_000_000,
            cache_read_input_tokens: 1_000_000,
            ..TokenUsageRaw::default()
        };
        let cases = [
            (
                "2026-08-16T15:59:59Z",
                [("deepseek-v4-flash", 0.5628), ("deepseek-v4-pro", 1.743625)],
            ),
            (
                "2026-08-17T12:00:00Z",
                [("deepseek-v4-flash", 1.107), ("deepseek-v4-pro", 3.322)],
            ),
            (
                "2026-08-17T01:00:00Z",
                [("deepseek-v4-flash", 2.214), ("deepseek-v4-pro", 6.644)],
            ),
        ];

        for (timestamp_text, models) in cases {
            for (model, expected) in models {
                let cost = calculate_cost_for_usage_at(
                    Some(model),
                    usage,
                    None,
                    Some(timestamp(timestamp_text)),
                    CostMode::Calculate,
                    Some(&pricing),
                );
                assert!((cost - expected).abs() < 1e-12, "{model} cost was {cost}");
            }
        }
    }

    #[test]
    fn scheduled_rates_preserve_display_and_auto_cost_semantics() {
        let pricing = deepseek_pricing();
        let usage = TokenUsageRaw {
            input_tokens: 1_000_000,
            ..TokenUsageRaw::default()
        };
        let timestamp = Some(timestamp("2026-08-17T01:00:00Z"));

        assert_eq!(
            calculate_cost_for_usage_at(
                Some("deepseek-v4-flash"),
                usage,
                Some(42.0),
                timestamp,
                CostMode::Display,
                Some(&pricing),
            ),
            42.0
        );
        assert_eq!(
            calculate_cost_for_usage_at(
                Some("deepseek-v4-flash"),
                usage,
                Some(42.0),
                timestamp,
                CostMode::Auto,
                Some(&pricing),
            ),
            42.0
        );
        let calculated = calculate_cost_for_usage_at(
            Some("deepseek-v4-flash"),
            usage,
            Some(42.0),
            timestamp,
            CostMode::Calculate,
            Some(&pricing),
        );
        assert!((calculated - 0.44).abs() < 1e-12);
        let automatic = calculate_cost_for_usage_at(
            Some("deepseek-v4-flash"),
            usage,
            None,
            timestamp,
            CostMode::Auto,
            Some(&pricing),
        );
        assert!((automatic - 0.44).abs() < 1e-12);
    }

    #[test]
    fn legacy_cost_api_keeps_static_lookup_without_a_timestamp() {
        let pricing = deepseek_pricing();
        let usage = TokenUsageRaw {
            input_tokens: 1_000_000,
            ..TokenUsageRaw::default()
        };

        assert_eq!(
            calculate_cost_for_usage(
                Some("deepseek-v4-flash"),
                usage,
                None,
                CostMode::Calculate,
                Some(&pricing),
            ),
            0.14
        );
    }

    #[test]
    fn usage_entry_cost_wrapper_uses_event_timestamp() {
        let pricing = deepseek_pricing();
        let mut entry = UsageEntry {
            session_id: None,
            timestamp: "2026-08-17T01:00:00Z".to_string(),
            version: None,
            message: UsageMessage {
                usage: TokenUsageRaw {
                    input_tokens: 1_000_000,
                    ..TokenUsageRaw::default()
                },
                model: Some("deepseek-v4-flash".to_string()),
                id: None,
                provider: None,
            },
            cost_usd: None,
            request_id: None,
            is_api_error_message: None,
            is_sidechain: None,
        };

        let cost = calculate_cost(&entry, CostMode::Calculate, Some(&pricing));
        assert!((cost - 0.44).abs() < 1e-12);

        entry.timestamp = "not-a-timestamp".to_string();
        assert_eq!(
            calculate_cost(&entry, CostMode::Calculate, Some(&pricing)),
            0.14
        );
    }
}
