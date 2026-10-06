use std::sync::Arc;

use jiff::tz::TimeZone as JiffTimeZone;
use serde::Deserialize;

use crate::{
    LoadedEntry, PricingMap, TokenUsageRaw, UsageEntry, UsageMessage, apply_total_token_fallback,
    calculate_cost_for_usage_at, cli::CostMode, format_date_tz,
    missing_pricing_model_for_candidates,
};
use ccusage_adapter_common::jsonl;

/// A single parsed OpenCode message. Only the fields ccusage consumes are
/// declared; serde skips everything else.
#[derive(Debug, Default, Deserialize)]
pub struct OpenCodeMessage {
    #[serde(default, deserialize_with = "jsonl::lenient_object")]
    tokens: Option<OpenCodeTokens>,
    #[serde(
        rename = "modelID",
        default,
        deserialize_with = "jsonl::non_empty_string"
    )]
    model_id: Option<String>,
    #[serde(
        rename = "providerID",
        default,
        deserialize_with = "jsonl::non_empty_string"
    )]
    provider_id: Option<String>,
    /// V2 records nest model identity (`"model": {"id", "providerID"}`) instead
    /// of the V1 top-level `modelID` / `providerID` fields. `None` for V1 rows.
    #[serde(default, deserialize_with = "jsonl::lenient_object")]
    model: Option<OpenCodeModel>,
    #[serde(default, deserialize_with = "jsonl::lenient_object")]
    time: Option<OpenCodeTime>,
    #[serde(default, deserialize_with = "jsonl::non_empty_string")]
    id: Option<String>,
    #[serde(
        rename = "sessionID",
        default,
        deserialize_with = "jsonl::non_empty_string"
    )]
    session_id: Option<String>,
    #[serde(default, deserialize_with = "jsonl::lenient_f64")]
    cost: Option<f64>,
}

/// Nested model identity carried by OpenCode V2 `session_message` records.
#[derive(Debug, Default, Deserialize)]
struct OpenCodeModel {
    #[serde(default, deserialize_with = "jsonl::non_empty_string")]
    id: Option<String>,
    #[serde(
        default,
        alias = "providerID",
        deserialize_with = "jsonl::non_empty_string"
    )]
    provider_id: Option<String>,
}

/// Token usage block carried by OpenCode messages.
#[derive(Debug, Default, Deserialize)]
struct OpenCodeTokens {
    #[serde(default, deserialize_with = "jsonl::lenient_u64")]
    input: u64,
    #[serde(default, deserialize_with = "jsonl::lenient_u64")]
    output: u64,
    #[serde(default, deserialize_with = "jsonl::lenient_u64")]
    reasoning: u64,
    #[serde(default, deserialize_with = "jsonl::lenient_object")]
    cache: Option<OpenCodeCache>,
    #[serde(default, deserialize_with = "jsonl::lenient_u64")]
    total: u64,
}

/// Cache read/write counts nested under OpenCode token usage.
#[derive(Debug, Default, Deserialize)]
struct OpenCodeCache {
    #[serde(default, deserialize_with = "jsonl::lenient_u64")]
    read: u64,
    #[serde(default, deserialize_with = "jsonl::lenient_u64")]
    write: u64,
}

/// Creation timestamp block carried by OpenCode messages.
#[derive(Debug, Default, Deserialize)]
struct OpenCodeTime {
    #[serde(default, deserialize_with = "jsonl::lenient_i64")]
    created: Option<i64>,
}

/// Trim a string and discard it if it is empty, matching the original
/// `Value`-based parser's `non_empty_json_string` handling.
fn non_empty(value: Option<&String>) -> Option<String> {
    let trimmed = value?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub fn message_to_entry(
    msg: &OpenCodeMessage,
    id: Option<String>,
    session_id: Option<String>,
    tz: Option<&JiffTimeZone>,
    mode: CostMode,
    pricing: Option<&PricingMap>,
) -> Option<LoadedEntry> {
    let tokens = msg.tokens.as_ref()?;
    let usage = TokenUsageRaw {
        input_tokens: tokens.input,
        output_tokens: tokens.output,
        cache_creation_input_tokens: tokens.cache.as_ref().map_or(0, |c| c.write),
        cache_read_input_tokens: tokens.cache.as_ref().map_or(0, |c| c.read),
        speed: None,
        cache_creation: None,
    };
    let (usage, extra_total_tokens) =
        apply_total_token_fallback(usage, tokens.reasoning, tokens.total);
    if usage.input_tokens == 0
        && usage.output_tokens == 0
        && usage.cache_creation_input_tokens == 0
        && usage.cache_read_input_tokens == 0
        && extra_total_tokens == 0
    {
        return None;
    }
    let model = non_empty(msg.model_id.as_ref())
        .or_else(|| msg.model.as_ref().and_then(|m| non_empty(m.id.as_ref())))?;
    let provider = non_empty(msg.provider_id.as_ref()).or_else(|| {
        msg.model
            .as_ref()
            .and_then(|m| non_empty(m.provider_id.as_ref()))
    })?;
    let millis = msg.time.as_ref().and_then(|t| t.created).unwrap_or(0);
    let timestamp = crate::TimestampMs::from_millis(millis);
    let timestamp_text = crate::format_rfc3339_millis(timestamp);
    let message_id = id.or_else(|| non_empty(msg.id.as_ref()));
    let session_id = session_id.or_else(|| non_empty(msg.session_id.as_ref()));
    let data = UsageEntry {
        session_id: session_id.clone(),
        timestamp: timestamp_text,
        version: None,
        message: UsageMessage {
            usage,
            model: Some(model.clone()),
            id: message_id,
            provider: Some(provider.clone()),
        },
        cost_usd: msg.cost,
        request_id: None,
        is_api_error_message: None,
        is_sidechain: None,
    };
    let cost_usage = TokenUsageRaw {
        output_tokens: usage.output_tokens.saturating_add(extra_total_tokens),
        cache_creation: None,
        ..usage
    };
    let (cost, missing_pricing_model) = open_code_cost_and_missing(
        &model,
        &provider,
        cost_usage,
        data.cost_usd,
        open_code_timestamp(msg),
        mode,
        pricing,
    );
    let loaded_session_id = data
        .session_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    Some(LoadedEntry {
        date: format_date_tz(timestamp, tz),
        timestamp,
        project: Arc::from("opencode"),
        session_id: Arc::from(loaded_session_id),
        project_path: Arc::from("OpenCode"),
        cost,
        extra_total_tokens,
        credits: None,
        message_count: None,
        model: Some(model),
        usage_limit_reset_time: None,
        missing_pricing_model,
        data,
    })
}

struct MessageEntryOptions {
    allow_cost_only: bool,
    pricing_timestamp: Option<crate::TimestampMs>,
}

fn message_value_to_entry_inner(
    value: &OpenCodeMessage,
    id: Option<String>,
    session_id: Option<String>,
    tz: Option<&JiffTimeZone>,
    mode: CostMode,
    pricing: Option<&PricingMap>,
    options: MessageEntryOptions,
) -> Option<LoadedEntry> {
    let tokens = value.tokens.as_ref()?;
    let cache = tokens.cache.as_ref();
    let usage = TokenUsageRaw {
        input_tokens: tokens.input,
        output_tokens: tokens.output,
        cache_creation_input_tokens: cache.map_or(0, |cache| cache.write),
        cache_read_input_tokens: cache.map_or(0, |cache| cache.read),
        speed: None,
        cache_creation: None,
    };
    let total_tokens = tokens.total;
    let (usage, extra_total_tokens) =
        apply_total_token_fallback(usage, tokens.reasoning, total_tokens);
    if usage.input_tokens == 0
        && usage.output_tokens == 0
        && usage.cache_creation_input_tokens == 0
        && usage.cache_read_input_tokens == 0
        && extra_total_tokens == 0
        && !(options.allow_cost_only && value.cost.is_some_and(|cost| cost > 0.0))
    {
        return None;
    }
    let model = value.model_id.clone()?;
    let provider = value.provider_id.clone()?;
    let timestamp = open_code_timestamp(value).unwrap_or(crate::TimestampMs::UNIX_EPOCH);
    let timestamp_text = crate::format_rfc3339_millis(timestamp);
    let message_id = id.or_else(|| value.id.clone());
    let session_id = session_id.or_else(|| value.session_id.clone());
    let data = UsageEntry {
        session_id: session_id.clone(),
        timestamp: timestamp_text,
        version: None,
        message: UsageMessage {
            usage,
            model: Some(model.clone()),
            id: message_id,
            provider: Some(provider.clone()),
        },
        cost_usd: value.cost,
        request_id: None,
        is_api_error_message: None,
        is_sidechain: None,
    };
    let cost_usage = TokenUsageRaw {
        output_tokens: usage.output_tokens.saturating_add(extra_total_tokens),
        cache_creation: None,
        ..usage
    };
    let (cost, missing_pricing_model) = open_code_cost_and_missing(
        &model,
        &provider,
        cost_usage,
        data.cost_usd,
        options.pricing_timestamp,
        mode,
        pricing,
    );
    let loaded_session_id = data
        .session_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    Some(LoadedEntry {
        date: format_date_tz(timestamp, tz),
        timestamp,
        project: Arc::from("opencode"),
        session_id: Arc::from(loaded_session_id),
        project_path: Arc::from("OpenCode"),
        cost,
        extra_total_tokens,
        credits: None,
        message_count: None,
        model: Some(model),
        usage_limit_reset_time: None,
        missing_pricing_model,
        data,
    })
}

/// Reprice a cached entry from stored tokens (fallback rules match [`open_code_cost_and_missing`]).
pub(crate) fn reprice(entry: &mut LoadedEntry, mode: CostMode, pricing: Option<&PricingMap>) {
    let model = entry.data.message.model.clone().unwrap_or_default();
    let provider = entry.data.message.provider.clone().unwrap_or_default();
    let cost_usage = TokenUsageRaw {
        output_tokens: entry
            .data
            .message
            .usage
            .output_tokens
            .saturating_add(entry.extra_total_tokens),
        cache_creation: None,
        ..entry.data.message.usage
    };
    let (cost, missing_pricing_model) = open_code_cost_and_missing(
        &model,
        &provider,
        cost_usage,
        entry.data.cost_usd,
        Some(entry.timestamp),
        mode,
        pricing,
    );
    entry.cost = cost;
    entry.missing_pricing_model = missing_pricing_model;
}

pub(crate) struct OpenCodeSessionAggregate {
    pub(crate) session_id: String,
    pub(crate) created: i64,
    pub(crate) model: String,
    pub(crate) provider: String,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) reasoning_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_write_tokens: u64,
    pub(crate) cost: Option<f64>,
}

pub(crate) fn session_value_to_entry(
    aggregate: OpenCodeSessionAggregate,
    tz: Option<&JiffTimeZone>,
    mode: CostMode,
    pricing: Option<&PricingMap>,
) -> Option<LoadedEntry> {
    if aggregate.input_tokens == 0
        && aggregate.output_tokens == 0
        && aggregate.reasoning_tokens == 0
        && aggregate.cache_read_tokens == 0
        && aggregate.cache_write_tokens == 0
        && aggregate.cost.unwrap_or(0.0) <= 0.0
    {
        return None;
    }
    let value = OpenCodeMessage {
        tokens: Some(OpenCodeTokens {
            input: aggregate.input_tokens,
            output: aggregate.output_tokens,
            reasoning: aggregate.reasoning_tokens,
            cache: Some(OpenCodeCache {
                read: aggregate.cache_read_tokens,
                write: aggregate.cache_write_tokens,
            }),
            total: 0,
        }),
        model_id: Some(aggregate.model),
        provider_id: Some(aggregate.provider),
        model: None,
        time: Some(OpenCodeTime {
            created: Some(aggregate.created),
        }),
        id: Some(format!("session:{}", aggregate.session_id)),
        session_id: Some(aggregate.session_id),
        cost: aggregate.cost,
    };
    message_value_to_entry_inner(
        &value,
        None,
        None,
        tz,
        mode,
        pricing,
        MessageEntryOptions {
            allow_cost_only: true,
            pricing_timestamp: None,
        },
    )
}

fn open_code_timestamp(value: &OpenCodeMessage) -> Option<crate::TimestampMs> {
    value
        .time
        .as_ref()
        .and_then(|time| time.created)
        .filter(|millis| *millis > 0)
        .map(crate::TimestampMs::from_millis)
}

/// Decide cost and the missing-pricing flag together so they stay coherent.
///
/// Cost is always computed from tokens first; the logged `costUSD` is used as
/// a fallback only when the computation yields `0.0` because the model can't
/// be priced — in `Auto` (never lose logged spend for unmapped models) or
/// offline (`pricing` is `None`) in any recomputing mode. When that fallback
/// fires the row is not flagged as missing pricing. Free-tier models never
/// take the fallback: their exact 0 stands in every mode.
fn open_code_cost_and_missing(
    model: &str,
    provider: &str,
    usage: TokenUsageRaw,
    cost_usd: Option<f64>,
    timestamp: Option<crate::TimestampMs>,
    mode: CostMode,
    pricing: Option<&PricingMap>,
) -> (f64, Option<String>) {
    let computed = calculate_open_code_cost(model, provider, usage, timestamp, mode, pricing);
    let allow_fallback = mode == CostMode::Auto || pricing.is_none();
    if allow_fallback
        && computed == 0.0
        && !model.to_ascii_lowercase().ends_with("free")
        && let Some(cost) = cost_usd.filter(|c| *c > 0.0)
    {
        return (cost, None);
    }
    (
        computed,
        missing_open_code_pricing(model, provider, usage, mode, pricing),
    )
}

fn calculate_open_code_cost(
    model: &str,
    provider: &str,
    usage: TokenUsageRaw,
    timestamp: Option<crate::TimestampMs>,
    _mode: CostMode,
    pricing: Option<&PricingMap>,
) -> f64 {
    // Free-tier models must never be billed: they are published with
    // per-token rates of 0 in models.dev, but the generic `> 0.0` skip
    // would discard that 0 and fuzzy-match a paid sibling (e.g.
    // `mimo-v2.5-free` -> `mimo-v2.5`). Short-circuit on the model id
    // itself so the exact 0 is honoured and no sibling is consulted.
    // `free` is always a suffix (`-free` or `:free`) — verified 358/358
    // in opencode DB and 96/96 in pricing all end with `free`.
    if model.to_ascii_lowercase().ends_with("free") {
        return 0.0;
    }
    for candidate in open_code_model_candidates(model, provider) {
        let cost = calculate_cost_for_usage_at(
            Some(&candidate),
            usage,
            None,
            timestamp,
            CostMode::Calculate,
            pricing,
        );
        if cost > 0.0 {
            return cost;
        }
    }
    0.0
}

fn missing_open_code_pricing(
    model: &str,
    provider: &str,
    usage: TokenUsageRaw,
    mode: CostMode,
    pricing: Option<&PricingMap>,
) -> Option<String> {
    if model.to_ascii_lowercase().ends_with("free") {
        return None;
    }
    if mode == CostMode::Display {
        return None;
    }
    missing_pricing_model_for_candidates(
        model,
        open_code_model_candidates(model, provider),
        crate::total_usage_tokens(usage),
        pricing,
    )
}

fn open_code_model_candidates(model: &str, provider: &str) -> Vec<String> {
    let resolved = resolve_open_code_model_name(model);
    let normalized = normalize_open_code_model_name(&resolved);
    let mut base = vec![resolved];
    if normalized != base[0] {
        base.push(normalized);
    }
    let mut candidates = base.clone();
    if provider != "unknown" {
        let provider = provider.replace('-', "_");
        candidates.extend(base.into_iter().map(|model| format!("{provider}/{model}")));
    }
    candidates.dedup();
    candidates
}

fn resolve_open_code_model_name(model: &str) -> String {
    match model {
        "gemini-3-pro-high" => "gemini-3-pro-preview".to_string(),
        "k2p6" => "kimi-k2.6".to_string(),
        _ => model.to_string(),
    }
}

fn normalize_open_code_model_name(model: &str) -> String {
    for family in ["claude-haiku-", "claude-opus-", "claude-sonnet-"] {
        if let Some(rest) = model.strip_prefix(family) {
            if let Some((major, minor_and_suffix)) = rest.split_once('.')
                && major.chars().all(|ch| ch.is_ascii_digit())
                && minor_and_suffix
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_digit())
            {
                return format!("{family}{major}-{minor_and_suffix}");
            }
            let mut chars = rest.chars();
            if let (Some(major), Some(minor)) = (chars.next(), chars.next())
                && major.is_ascii_digit()
                && minor.is_ascii_digit()
            {
                return format!("{family}{major}-{minor}{}", chars.collect::<String>());
            }
        }
    }
    model.to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        MessageEntryOptions, OpenCodeCache, OpenCodeMessage, OpenCodeSessionAggregate,
        OpenCodeTime, OpenCodeTokens, message_to_entry, message_value_to_entry_inner,
        open_code_model_candidates, open_code_timestamp, session_value_to_entry,
    };
    use crate::{LoadedEntry, PricingMap, cli::CostMode};

    fn test_message(tokens: OpenCodeTokens) -> OpenCodeMessage {
        OpenCodeMessage {
            id: Some("message-a".to_string()),
            session_id: Some("session-a".to_string()),
            provider_id: Some("openai".to_string()),
            model_id: Some("gpt-test".to_string()),
            model: None,
            time: Some(OpenCodeTime { created: Some(0) }),
            tokens: Some(tokens),
            cost: Some(0.0),
        }
    }

    fn message(value: serde_json::Value) -> OpenCodeMessage {
        serde_json::from_value(value).unwrap()
    }

    fn entry_snapshot(entry: &LoadedEntry) -> serde_json::Value {
        json!({
            "date": entry.date,
            "timestamp": entry.timestamp.as_millis(),
            "sessionId": entry.session_id.as_ref(),
            "project": entry.project.as_ref(),
            "projectPath": entry.project_path.as_ref(),
            "cost": entry.cost,
            "extraTotalTokens": entry.extra_total_tokens,
            "model": entry.model.as_deref(),
            "data": {
                "sessionId": entry.data.session_id.as_deref(),
                "timestamp": entry.data.timestamp,
                "version": entry.data.version.as_deref(),
                "message": {
                    "id": entry.data.message.id.as_deref(),
                    "model": entry.data.message.model.as_deref(),
                    "usage": {
                        "inputTokens": entry.data.message.usage.input_tokens,
                        "outputTokens": entry.data.message.usage.output_tokens,
                        "cacheCreationInputTokens": entry.data.message.usage.cache_creation_input_tokens,
                        "cacheReadInputTokens": entry.data.message.usage.cache_read_input_tokens,
                    },
                },
                "costUSD": entry.cost,
            },
        })
    }

    #[test]
    fn calculates_cost_when_opencode_stores_zero_cost() {
        let mut pricing = PricingMap::default();
        pricing.load_json(
            r#"{
                "gpt-test": {
                    "input_cost_per_token": 0.000001,
                    "output_cost_per_token": 0.000010,
                    "cache_read_input_token_cost": 0.0000001
                }
            }"#,
        );
        let entry = message_to_entry(
            &test_message(OpenCodeTokens {
                input: 100,
                output: 10,
                reasoning: 0,
                total: 0,
                cache: Some(OpenCodeCache { read: 50, write: 0 }),
            }),
            None,
            None,
            None,
            CostMode::Auto,
            Some(&pricing),
        )
        .unwrap();

        assert_eq!(entry.cost, 0.000205);
    }

    #[test]
    fn keeps_positive_opencode_cost() {
        let entry = message_to_entry(
            &OpenCodeMessage {
                cost: Some(0.02),
                ..test_message(OpenCodeTokens {
                    input: 100,
                    output: 0,
                    reasoning: 0,
                    total: 0,
                    cache: None,
                })
            },
            None,
            None,
            None,
            CostMode::Auto,
            None,
        )
        .unwrap();

        assert_eq!(entry.cost, 0.02);
    }

    #[test]
    fn calculate_ignores_logged_cost_for_unmapped_model() {
        let pricing = PricingMap::default();
        let entry = message_to_entry(
            &OpenCodeMessage {
                cost: Some(0.02),
                ..test_message(OpenCodeTokens {
                    input: 100,
                    output: 10,
                    reasoning: 0,
                    total: 0,
                    cache: None,
                })
            },
            None,
            None,
            None,
            CostMode::Calculate,
            Some(&pricing),
        )
        .unwrap();

        assert_eq!(entry.cost, 0.0);
        assert_eq!(entry.missing_pricing_model.as_deref(), Some("gpt-test"));
    }

    #[test]
    fn free_model_never_bills_logged_cost() {
        let pricing = PricingMap::default();
        for mode in [CostMode::Display, CostMode::Auto, CostMode::Calculate] {
            let entry = message_to_entry(
                &OpenCodeMessage {
                    model_id: Some("mimo-v2.5-free".to_string()),
                    cost: Some(0.05),
                    ..test_message(OpenCodeTokens {
                        input: 100,
                        output: 10,
                        reasoning: 0,
                        total: 0,
                        cache: None,
                    })
                },
                None,
                None,
                None,
                mode,
                Some(&pricing),
            )
            .unwrap();

            assert_eq!(entry.cost, 0.0, "mode={mode:?}");
            assert_eq!(entry.missing_pricing_model, None, "mode={mode:?}");
        }
    }

    #[test]
    fn keeps_opencode_record_when_cache_field_is_not_an_object() {
        let entry = message_to_entry(
            &message(json!({
                "id": "message-a",
                "sessionID": "session-a",
                "providerID": "openai",
                "modelID": "gpt-test",
                "time": { "created": 0 },
                "tokens": {
                    "input": 100,
                    "output": 10,
                    "cache": 0
                },
                "cost": 0.02
            })),
            None,
            None,
            None,
            CostMode::Auto,
            None,
        )
        .unwrap();

        assert_eq!(entry.data.message.usage.input_tokens, 100);
        assert_eq!(entry.data.message.usage.output_tokens, 10);
        assert_eq!(entry.data.message.usage.cache_creation_input_tokens, 0);
        assert_eq!(entry.data.message.usage.cache_read_input_tokens, 0);
        assert_eq!(entry.cost, 0.02);
    }
    #[test]
    fn coerces_string_typed_numeric_fields_without_dropping_record() {
        let entry = message_to_entry(
            &message(json!({
                "id": "message-a",
                "sessionID": "session-a",
                "providerID": "openai",
                "modelID": "gpt-test",
                "time": { "created": 0 },
                "tokens": {
                    "input": "100",
                    "output": 10
                },
                "cost": 0.02
            })),
            None,
            None,
            None,
            CostMode::Auto,
            None,
        )
        .expect("record with a mistyped token field must still be recorded");

        assert_eq!(entry.data.message.usage.input_tokens, 100);
        assert_eq!(entry.data.message.usage.output_tokens, 10);
        assert_eq!(entry.cost, 0.02);
    }

    #[test]
    fn falls_back_to_total_tokens_when_opencode_token_parts_are_missing() {
        let entry = message_to_entry(
            &test_message(OpenCodeTokens {
                input: 0,
                output: 0,
                reasoning: 0,
                total: 123,
                cache: None,
            }),
            None,
            None,
            None,
            CostMode::Auto,
            None,
        )
        .unwrap();

        assert_eq!(entry.data.message.usage.output_tokens, 123);
        assert_eq!(entry.extra_total_tokens, 0);
    }

    #[test]
    fn resolves_v2_nested_model_shape() {
        let entry = message_to_entry(
            &message(json!({
                "id": "message-v2",
                "sessionID": "session-v2",
                "time": { "created": 1767312000000_i64 },
                "model": {
                    "id": "deepseek-v4-flash-free",
                    "providerID": "opencode",
                    "variant": "max"
                },
                "tokens": {
                    "input": 100,
                    "output": 10,
                    "cache": { "read": 50, "write": 25 }
                },
                "cost": 0
            })),
            None,
            None,
            None,
            CostMode::Auto,
            None,
        )
        .unwrap();

        assert_eq!(entry.data.message.usage.input_tokens, 100);
        assert_eq!(entry.data.message.usage.output_tokens, 10);
        assert_eq!(entry.data.message.usage.cache_creation_input_tokens, 25);
        assert_eq!(entry.data.message.usage.cache_read_input_tokens, 50);
        assert_eq!(entry.model.as_deref(), Some("deepseek-v4-flash-free"));
        assert_eq!(entry.data.message.provider.as_deref(), Some("opencode"));
        assert_eq!(entry.data.message.id.as_deref(), Some("message-v2"));
    }

    #[test]
    fn v1_top_level_model_fields_win_over_nested_model() {
        let entry = message_to_entry(
            &message(json!({
                "id": "message-both",
                "providerID": "anthropic",
                "modelID": "claude-sonnet-4-20250514",
                "model": { "id": "nested-model", "providerID": "nested-provider" },
                "time": { "created": 0 },
                "tokens": { "input": 10, "output": 5 },
                "cost": 0.01
            })),
            None,
            None,
            None,
            CostMode::Auto,
            None,
        )
        .unwrap();

        assert_eq!(entry.model.as_deref(), Some("claude-sonnet-4-20250514"));
        assert_eq!(entry.data.message.provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn drops_v2_record_without_model_identity() {
        let entry = message_to_entry(
            &message(json!({
                "id": "message-nomodel",
                "sessionID": "session-v2",
                "time": { "created": 0 },
                "tokens": { "input": 10, "output": 5 },
                "cost": 0
            })),
            None,
            None,
            None,
            CostMode::Auto,
            None,
        );
        assert!(entry.is_none(), "record without model identity is dropped");
    }

    #[test]
    fn creates_open_code_provider_and_normalized_model_candidates() {
        assert_eq!(
            open_code_model_candidates("claude-sonnet-4.5", "github-copilot"),
            vec![
                "claude-sonnet-4.5",
                "claude-sonnet-4-5",
                "github_copilot/claude-sonnet-4.5",
                "github_copilot/claude-sonnet-4-5",
            ]
        );
    }

    #[test]
    fn calculates_cost_for_k2p6_when_opencode_stores_zero_cost() {
        let pricing = PricingMap::load_embedded();
        let entry = message_to_entry(
            &OpenCodeMessage {
                provider_id: Some("kimi-for-coding".to_string()),
                model_id: Some("k2p6".to_string()),
                ..test_message(OpenCodeTokens {
                    input: 100,
                    output: 10,
                    reasoning: 0,
                    total: 0,
                    cache: Some(OpenCodeCache { read: 50, write: 0 }),
                })
            },
            None,
            None,
            None,
            CostMode::Auto,
            Some(&pricing),
        )
        .unwrap();

        assert_eq!(entry.cost, 0.000143);
    }

    #[test]
    fn snapshots_message_to_entry_variants_and_model_candidates() {
        let tz = crate::parse_tz(Some("UTC"));
        let mut pricing = PricingMap::default();
        pricing.load_json(
            r#"{
                "github_copilot/claude-sonnet-4-5": {
                    "input_cost_per_token": 0.125,
                    "output_cost_per_token": 0.25,
                    "cache_read_input_token_cost": 0.0625
                }
            }"#,
        );
        let calculated = message_to_entry(
            &OpenCodeMessage {
                id: Some("message-a".to_string()),
                session_id: Some("session-a".to_string()),
                provider_id: Some("github-copilot".to_string()),
                model_id: Some("claude-sonnet-4.5".to_string()),
                model: None,
                time: Some(OpenCodeTime {
                    created: Some(1767312000000),
                }),
                tokens: Some(OpenCodeTokens {
                    input: 100,
                    output: 10,
                    reasoning: 0,
                    total: 185,
                    cache: Some(OpenCodeCache {
                        read: 50,
                        write: 25,
                    }),
                }),
                cost: Some(0.0),
            },
            None,
            None,
            tz.as_ref(),
            CostMode::Auto,
            Some(&pricing),
        )
        .unwrap();
        let display_cost = message_to_entry(
            &OpenCodeMessage {
                id: Some("message-b".to_string()),
                session_id: None,
                provider_id: Some("openai".to_string()),
                model_id: Some("gpt-test".to_string()),
                model: None,
                time: Some(OpenCodeTime { created: Some(0) }),
                tokens: Some(OpenCodeTokens {
                    input: 0,
                    output: 0,
                    reasoning: 0,
                    total: 123,
                    cache: None,
                }),
                cost: Some(0.02),
            },
            None,
            Some("explicit-session".to_string()),
            tz.as_ref(),
            CostMode::Display,
            None,
        )
        .unwrap();

        insta::assert_json_snapshot!(json!({
            "calculated": entry_snapshot(&calculated),
            "displayCost": entry_snapshot(&display_cost),
            "candidates": {
                "anthropic": open_code_model_candidates("claude-sonnet-4.5", "anthropic"),
                "copilot": open_code_model_candidates("claude-sonnet-4.5", "github-copilot"),
                "geminiAlias": open_code_model_candidates("gemini-3-pro-high", "google"),
                "unknownProvider": open_code_model_candidates("gpt-test", "unknown"),
            }
        }));
    }

    fn deepseek_pricing(input: f64) -> PricingMap {
        let mut pricing = PricingMap::default();
        pricing.load_json(&format!(
            "{{\"deepseek-v4-flash\":{{\"input_cost_per_token\":{input},\"output_cost_per_token\":0.00000028}}}}"
        ));
        pricing
    }

    #[test]
    fn keeps_epoch_for_display_but_omits_missing_message_timestamp_for_pricing() {
        let pricing = deepseek_pricing(0.000009);
        let message = message(json!({
            "id": "message-a",
            "sessionID": "session-a",
            "providerID": "deepseek",
            "modelID": "deepseek-v4-flash",
            "tokens": { "input": 1_000_000 },
            "cost": 0
        }));
        let entry = message_value_to_entry_inner(
            &message,
            None,
            None,
            None,
            CostMode::Calculate,
            Some(&pricing),
            MessageEntryOptions {
                allow_cost_only: false,
                pricing_timestamp: open_code_timestamp(&message),
            },
        )
        .unwrap();

        assert_eq!(entry.timestamp.as_millis(), 0);
        assert_eq!(entry.cost, 9.0);
    }

    #[test]
    fn prices_cumulative_session_aggregates_with_static_rates() {
        let pricing = deepseek_pricing(0.00000014);
        let created = crate::parse_ts_timestamp("2026-08-17T01:00:00Z")
            .unwrap()
            .as_millis();
        let entry = session_value_to_entry(
            OpenCodeSessionAggregate {
                session_id: "session-a".to_string(),
                created,
                model: "deepseek-v4-flash".to_string(),
                provider: "deepseek".to_string(),
                input_tokens: 1_000_000,
                output_tokens: 0,
                reasoning_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost: None,
            },
            None,
            CostMode::Calculate,
            Some(&pricing),
        )
        .unwrap();

        assert_eq!(entry.timestamp.as_millis(), created);
        assert_eq!(entry.cost, 0.14);
    }
}
