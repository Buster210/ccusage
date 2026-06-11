use std::{collections::HashSet, path::PathBuf};

use crate::{
    LoadedEntry, PricingMap, Result, cli::SharedArgs, collect_files_with_extension,
    cost_and_missing_for_output, debug_log, parse_tz, read_files_parallel,
};

use super::{parser, paths};

pub(crate) fn load_entries(
    shared: &SharedArgs,
    custom_path: Option<&str>,
    pricing: Option<&PricingMap>,
) -> Result<Vec<LoadedEntry>> {
    crate::progress::track_usage_load(crate::progress::UsageLoadAgent::Pi, shared.json, || {
        load_entries_inner(shared, custom_path, pricing)
    })
}

fn load_entries_inner(
    shared: &SharedArgs,
    custom_path: Option<&str>,
    pricing: Option<&PricingMap>,
) -> Result<Vec<LoadedEntry>> {
    load_entries_from_paths(
        shared,
        paths::paths(custom_path)?,
        pricing,
        PiLoadScope::Default,
    )
}

#[cfg(test)]
pub(crate) fn load_entries_for_store_path(
    shared: &SharedArgs,
    store_path: &str,
    store_name: &str,
    pricing: Option<&PricingMap>,
) -> Result<Vec<LoadedEntry>> {
    load_entries_for_store_paths(
        shared,
        paths::named_store_paths(store_path)?,
        store_name,
        pricing,
    )
}

pub(crate) fn load_entries_for_store_paths(
    shared: &SharedArgs,
    store_paths: Vec<PathBuf>,
    store_name: &str,
    pricing: Option<&PricingMap>,
) -> Result<Vec<LoadedEntry>> {
    load_entries_from_paths(
        shared,
        store_paths,
        pricing,
        PiLoadScope::Named { store_name },
    )
}

#[derive(Clone, Copy)]
enum PiLoadScope<'a> {
    Default,
    Named { store_name: &'a str },
}

impl<'a> PiLoadScope<'a> {
    fn store_name(self) -> &'a str {
        match self {
            Self::Default => "pi",
            Self::Named { store_name } => store_name,
        }
    }
}

fn load_entries_from_paths(
    shared: &SharedArgs,
    paths: Vec<PathBuf>,
    pricing: Option<&PricingMap>,
    scope: PiLoadScope<'_>,
) -> Result<Vec<LoadedEntry>> {
    let tz = parse_tz(shared.timezone.as_deref());

    let parsed: Vec<LoadedEntry> = match scope {
        PiLoadScope::Default => {
            let mut files = Vec::new();
            for path in &paths {
                collect_files_with_extension(path, "jsonl", &mut files);
            }
            crate::cache::load_with_cache(
                "pi",
                &files,
                crate::cache::CacheOpts {
                    single_thread: shared.single_thread,
                    live_only: shared.live_only,
                },
                crate::cache::Freshness::FileStat,
                |file| {
                    Ok(
                        parser::read_session_file(file, tz.as_ref(), shared.mode, pricing)
                            .unwrap_or_else(|error| {
                                debug_log(
                                    shared,
                                    format!(
                                        "Failed to read pi session file {}: {error}",
                                        file.display()
                                    ),
                                );
                                Vec::new()
                            }),
                    )
                },
                |e| {
                    let (cost, missing_pricing_model) =
                        cost_and_missing_for_output(&e.data, shared.mode, pricing);
                    e.cost = cost;
                    e.missing_pricing_model = missing_pricing_model;
                },
            )?
        }
        // ponytail: named stores skip the on-disk cache — extract_project_for_store
        // needs each file's store root, which load_with_cache's flat Fn(&Path) closure
        // can't thread. Named stores are a rare, newly-added path; parse directly.
        PiLoadScope::Named { store_name } => {
            let mut result = Vec::new();
            for path in &paths {
                let mut files = Vec::new();
                collect_files_with_extension(path, "jsonl", &mut files);
                let loaded = read_files_parallel(&files, shared.single_thread, |file| {
                    parser::read_session_file_for_store(
                        file,
                        path,
                        tz.as_ref(),
                        shared.mode,
                        pricing,
                        store_name,
                    )
                    .unwrap_or_else(|error| {
                        debug_log(
                            shared,
                            format!(
                                "Failed to read pi-format store '{store_name}' session file {}: {error}",
                                file.display()
                            ),
                        );
                        Vec::new()
                    })
                });
                for file_entries in loaded {
                    result.extend(file_entries);
                }
            }
            result
        }
    };

    let mut seen = HashSet::new();
    let mut entries = Vec::with_capacity(parsed.len());
    for entry in parsed {
        let id = match scope {
            PiLoadScope::Default => parser::entry_id(&entry),
            PiLoadScope::Named { .. } => parser::entry_id_for_store(scope.store_name(), &entry),
        };
        if seen.insert(id) {
            entries.push(entry);
        }
    }
    entries.sort_by_key(|entry| entry.timestamp);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::tests::CacheEnv;
    use ccusage_test_support::fs_fixture;

    #[test]
    fn display_mode_surfaces_logged_cost() {
        let _cache_env = CacheEnv::new("pi-display");
        let fixture = fs_fixture!({
            "sessions/project-a/agent_session-a.jsonl": r#"{"type":"message","timestamp":"2026-01-02T00:00:00.000Z","message":{"role":"assistant","model":"gpt-5","usage":{"input":100,"output":200,"cost":{"total":0.05}}}}"#,
        });
        let shared = SharedArgs {
            mode: crate::cli::CostMode::Display,
            offline: true,
            ..SharedArgs::default()
        };
        // Mirror pi::run(): pricing is loaded regardless of mode. Display must
        // still surface the logged costUSD, not reprice it to 0.0.
        let pricing =
            PricingMap::load_with_overrides(shared.offline, false, shared.pricing_overrides.iter());

        let entries = load_entries(&shared, fixture.root().to_str(), Some(&pricing)).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].cost, 0.05);
        assert_eq!(entries[0].missing_pricing_model, None);
    }
}
