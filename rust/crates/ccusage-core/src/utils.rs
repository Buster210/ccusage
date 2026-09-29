use std::path::PathBuf;

use serde_json::Value;

use crate::TokenUsageRaw;

pub fn json_value_u64(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or_default()
}

pub fn non_empty_json_string(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

pub fn total_usage_tokens(usage: TokenUsageRaw) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.output_tokens)
        .saturating_add(usage.cache_creation_token_count())
        .saturating_add(usage.cache_read_input_tokens)
}

pub fn apply_total_token_fallback(
    mut usage: TokenUsageRaw,
    mut extra_total_tokens: u64,
    total_tokens: u64,
) -> (TokenUsageRaw, u64) {
    let known_tokens = total_usage_tokens(usage).saturating_add(extra_total_tokens);
    let missing_tokens = total_tokens.saturating_sub(known_tokens);
    if missing_tokens == 0 {
        return (usage, extra_total_tokens);
    }
    if usage.output_tokens == 0 {
        usage.output_tokens = missing_tokens;
    } else {
        extra_total_tokens = extra_total_tokens.saturating_add(missing_tokens);
    }
    (usage, extra_total_tokens)
}

pub fn chunk_file_indexes_by_size(files: &[PathBuf], chunk_count: usize) -> Vec<Vec<usize>> {
    // Callers derive the count from available_parallelism, which can be 0 for a
    // caller that clamps against an empty file list; one chunk still returns
    // every index rather than indexing an empty vector.
    let chunk_count = chunk_count.max(1);
    let mut weighted_indexes = Vec::with_capacity(files.len());
    for (index, file) in files.iter().enumerate() {
        let size = std::fs::metadata(file).map_or(0, |metadata| metadata.len());
        weighted_indexes.push((index, size));
    }
    weighted_indexes.sort_unstable_by(|a, b| match b.1.cmp(&a.1) {
        std::cmp::Ordering::Equal => a.0.cmp(&b.0),
        order => order,
    });

    let mut chunks = vec![Vec::new(); chunk_count];
    let mut chunk_sizes = vec![0_u64; chunk_count];
    for (index, size) in weighted_indexes {
        let mut target = 0;
        for candidate in 1..chunk_sizes.len() {
            if chunk_sizes[candidate] < chunk_sizes[target] {
                target = candidate;
            }
        }
        chunks[target].push(index);
        chunk_sizes[target] = chunk_sizes[target].saturating_add(size);
    }

    chunks
        .into_iter()
        .filter(|chunk| !chunk.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_total_token_fallback_to_missing_output_tokens() {
        let (usage, extra_total_tokens) = apply_total_token_fallback(
            TokenUsageRaw {
                input_tokens: 100,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 25,
                speed: None,
                cache_creation: None,
            },
            0,
            175,
        );

        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.cache_read_input_tokens, 25);
        assert_eq!(extra_total_tokens, 0);
    }

    #[test]
    fn keeps_total_fallback_as_extra_when_output_is_known() {
        let (usage, extra_total_tokens) = apply_total_token_fallback(
            TokenUsageRaw {
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 25,
                speed: None,
                cache_creation: None,
            },
            0,
            200,
        );

        assert_eq!(usage.output_tokens, 50);
        assert_eq!(extra_total_tokens, 25);
    }

    #[test]
    fn saturates_total_token_fallback_for_huge_counters() {
        let (usage, extra_total_tokens) = apply_total_token_fallback(
            TokenUsageRaw {
                input_tokens: u64::MAX,
                output_tokens: u64::MAX,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: u64::MAX,
                speed: None,
                cache_creation: Some(crate::CacheCreationRaw {
                    ephemeral_5m_input_tokens: u64::MAX,
                    ephemeral_1h_input_tokens: u64::MAX,
                }),
            },
            u64::MAX,
            u64::MAX,
        );

        assert_eq!(total_usage_tokens(usage), u64::MAX);
        assert_eq!(extra_total_tokens, u64::MAX);
    }
}
