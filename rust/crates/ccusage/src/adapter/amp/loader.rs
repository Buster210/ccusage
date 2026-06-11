use crate::{
    LoadedEntry, PricingMap, Result, adapter::parse_or_log, cli::SharedArgs,
    collect_files_with_extension, parse_tz,
};

use super::{parser, paths};

pub(crate) fn load_entries(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    crate::progress::track_usage_load(crate::progress::UsageLoadAgent::Amp, shared.json, || {
        load_entries_inner(shared, pricing)
    })
}

fn load_entries_inner(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    let tz = parse_tz(shared.timezone.as_deref());
    let mut files = Vec::new();
    for path in paths::paths()? {
        let threads_dir = path.join("threads");
        collect_files_with_extension(&threads_dir, "json", &mut files);
    }
    let mode = shared.mode;
    let mut entries = crate::cache::load_with_cache(
        "amp",
        &files,
        crate::cache::CacheOpts {
            single_thread: shared.single_thread,
            live_only: shared.live_only,
        },
        crate::cache::Freshness::FileStat,
        |file| {
            Ok(parse_or_log(file, shared, "Amp thread file", || {
                parser::read_thread_file(file, tz.as_ref(), mode, Some(pricing))
            }))
        },
        |e| {
            // Amp bills reasoning tokens as output; extra_total_tokens holds them
            // and the logged cost is never used (parse passes cost_usd = None).
            let cost_usage = crate::TokenUsageRaw {
                output_tokens: e
                    .data
                    .message
                    .usage
                    .output_tokens
                    .saturating_add(e.extra_total_tokens),
                cache_creation: None,
                ..e.data.message.usage
            };
            let model = e.data.message.model.as_deref();
            e.cost = crate::calculate_cost_for_usage(model, cost_usage, None, mode, Some(pricing));
            e.missing_pricing_model = crate::missing_pricing_model_for_usage(
                model,
                cost_usage,
                None,
                mode,
                Some(pricing),
            );
        },
    )?;
    entries.sort_by_key(|entry| entry.timestamp);
    Ok(entries)
}
