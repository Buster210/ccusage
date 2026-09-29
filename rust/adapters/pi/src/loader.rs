use std::{
    collections::{HashMap, HashSet},
    io::BufRead,
    path::{Component, Path, PathBuf},
};

use crate::{
    LoadedEntry, PricingMap, Result, cli::SharedArgs, collect_files_with_extension,
    cost_and_missing_for_output, debug_log, fast::FxHashSet, parse_tz, read_files_parallel,
};

use super::{parser, paths};

pub fn load_entries(
    shared: &SharedArgs,
    custom_path: Option<&str>,
    pricing: Option<&PricingMap>,
) -> Result<Vec<LoadedEntry>> {
    crate::progress::track_usage_load(crate::progress::UsageLoadAgent("pi"), shared.json, || {
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

#[doc(hidden)]
pub fn load_entries_for_store_path(
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

pub fn load_entries_for_store_paths(
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
    // Phase 0: collect every session file first. Replay lineage can span
    // roots, so the lineage set below must resolve globally.
    let mut groups: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    for path in paths {
        let mut files = Vec::new();
        collect_files_with_extension(&path, "jsonl", &mut files);
        // pi-subagents writes derived debug transcripts under a
        // `subagent-artifacts/` directory inside the sessions tree. They are
        // pure copies of calls already recorded in the primary session files
        // (no session header, no entry ids), so neither the replay suppression
        // below nor entry-id dedup can collapse them. Skip them here instead.
        // `run-*/` fresh-context child sessions are the primary record for
        // those children and must keep counting.
        files.retain(|file| !is_subagent_artifact_transcript(file));
        groups.push((path, files));
    }
    // Phase 1: session headers only (first line, no row work) to find the
    // files replay matching can consult: files carrying a parent header,
    // plus the parents they reference. Everything else takes the cached
    // entries-only parse below, which is behavior-identical for such files.
    let mut flat: Vec<PathBuf> = Vec::new();
    for (_, files) in &groups {
        flat.extend(files.iter().cloned());
    }
    // One cache unit per ledger namespace: the default scope keeps the
    // historical single `pi` namespace over every root, while named stores
    // keep one `pi:<name>:<path>` namespace per root so distinct store
    // roots never pollute each other's ledger. Built before the header scan
    // so headers can reuse the per-namespace persisted state below.
    let mut units: Vec<PiCacheUnit> = Vec::new();
    let mut unit_by_namespace: HashMap<String, usize> = HashMap::new();
    let mut unit_by_index: Vec<usize> = Vec::with_capacity(flat.len());
    match scope {
        PiLoadScope::Default => {
            units.push(PiCacheUnit {
                namespace: "pi".to_string(),
                root: PathBuf::new(),
                files: flat
                    .iter()
                    .enumerate()
                    .map(|(index, file)| (index, file.clone()))
                    .collect(),
            });
            unit_by_index.resize(flat.len(), 0);
        }
        PiLoadScope::Named { store_name } => {
            let mut index = 0;
            for (root, files) in &groups {
                let namespace = format!("pi:{store_name}:{}", root.display());
                let unit_no = match unit_by_namespace.get(&namespace) {
                    Some(&unit_no) => unit_no,
                    None => {
                        units.push(PiCacheUnit {
                            namespace: namespace.clone(),
                            root: root.clone(),
                            files: Vec::new(),
                        });
                        let unit_no = units.len() - 1;
                        unit_by_namespace.insert(namespace, unit_no);
                        unit_no
                    }
                };
                for file in files {
                    units[unit_no].files.push((index, file.clone()));
                    unit_by_index.push(unit_no);
                    index += 1;
                }
            }
        }
    }
    let namespaces: Vec<&str> = unit_by_index
        .iter()
        .map(|&unit_no| units[unit_no].namespace.as_str())
        .collect();
    let headers = load_session_headers(&flat, &namespaces, shared.single_thread);
    // Replay prep below only matters when some file carries a parent
    // header; otherwise the lineage set stays empty and every downstream
    // replay loop is a proven no-op, so skip both path maps.
    let has_replay = headers
        .iter()
        .flatten()
        .any(|header| header.parent_session.is_some());
    let mut files_by_path = HashMap::new();
    let mut lineage = HashSet::new();
    if has_replay {
        for (index, file) in flat.iter().enumerate() {
            files_by_path.entry(normalize_path(file)).or_insert(index);
        }
        for (index, header) in headers.iter().enumerate() {
            let Some(header) = header else {
                continue;
            };
            let Some(parent) = header.parent_session.as_ref() else {
                continue;
            };
            lineage.insert(index);
            if let Some(parent_index) =
                resolve_parent_index(parent, &flat[index], &files_by_path)
            {
                lineage.insert(parent_index);
            }
        }
    }
    // Lineage files take the full parse fresh every run (replay signatures
    // are parse-only state the entry cache cannot round-trip).
    // Warm-run costs stay current: cached hits are repriced through the
    // same mode/pricing path as fresh parses.
    let reprice = |entry: &mut LoadedEntry| {
        let (cost, missing_pricing_model) =
            cost_and_missing_for_output(&entry.data, shared.mode, pricing);
        entry.cost = cost;
        entry.missing_pricing_model = missing_pricing_model;
    };
    let mut loaded: Vec<Option<parser::PiSessionData>> =
        (0..flat.len()).map(|_| None).collect();
    let mut entries_by_index: HashMap<usize, Vec<LoadedEntry>> = HashMap::new();
    let parse_started = std::env::var_os("CCUSAGE_DEBUG_TIMING").is_some().then(std::time::Instant::now);
    for unit in &units {
        let lineage_paths: Vec<&PathBuf> = unit
            .files
            .iter()
            .filter(|(index, _)| lineage.contains(index))
            .map(|(_, file)| file)
            .collect();
        if !lineage_paths.is_empty() {
            let owned: Vec<PathBuf> = lineage_paths.into_iter().cloned().collect();
            let parsed = read_files_parallel(&owned, shared.single_thread, |file| {
                let result = match scope {
                    PiLoadScope::Default => {
                        parser::read_session_file_data(file, tz.as_ref(), shared.mode, pricing)
                    }
                    PiLoadScope::Named { .. } => {
                        parser::read_session_file_data_for_store(
                            file,
                            &unit.root,
                            tz.as_ref(),
                            shared.mode,
                            pricing,
                        )
                    }
                };
                match result {
                    Ok(data) => Some(data),
                    Err(error) => {
                        match scope {
                            PiLoadScope::Default => debug_log(
                                shared,
                                format!(
                                    "Failed to read pi session file {}: {error}",
                                    file.display()
                                ),
                            ),
                            PiLoadScope::Named { store_name } => debug_log(
                                shared,
                                format!(
                                    "Failed to read pi-format store '{store_name}' session file {}: {error}",
                                    file.display()
                                ),
                            ),
                        }
                        None
                    }
                }
            });
            let index_by_unit_path: HashMap<&Path, usize> = unit
                .files
                .iter()
                .map(|(index, file)| (file.as_path(), *index))
                .collect();
            for (file, data) in owned.iter().zip(parsed) {
                if let Some(index) = index_by_unit_path.get(file.as_path()) {
                    loaded[*index] = data;
                }
            }
        }
        // Non-lineage subset: persistent per-file cache. Files that fail to
        // parse contribute no rows (matching the pre-cache behavior of
        // logging and continuing with an empty file).
        let plain: Vec<(usize, PathBuf)> = unit
            .files
            .iter()
            .filter(|(index, _)| !lineage.contains(index))
            .cloned()
            .collect();
        if plain.is_empty() {
            continue;
        }
        let plain_paths: Vec<PathBuf> =
            plain.iter().map(|(_, file)| file.clone()).collect();
        let parse_cached = |file: &Path| -> Result<Vec<LoadedEntry>> {
            let result = match scope {
                PiLoadScope::Default => parser::read_session_file_data_lean(
                    file,
                    tz.as_ref(),
                    shared.mode,
                    pricing,
                )
                .map(|data| data.entries),
                PiLoadScope::Named { .. } => {
                    parser::read_session_file_data_for_store_lean(
                        file,
                        &unit.root,
                        tz.as_ref(),
                        shared.mode,
                        pricing,
                    )
                    .map(|data| data.entries)
                }
            };
            Ok(result.unwrap_or_else(|error| {
                match scope {
                    PiLoadScope::Default => debug_log(
                        shared,
                        format!("Failed to read pi session file {}: {error}", file.display()),
                    ),
                    PiLoadScope::Named { store_name } => debug_log(
                        shared,
                        format!(
                            "Failed to read pi-format store '{store_name}' session file {}: {error}",
                            file.display()
                        ),
                    ),
                }
                Vec::new()
            }))
        };
        let cached = crate::cache::load_with_cache_grouped(
            &unit.namespace,
            &plain_paths,
            crate::cache::CacheOpts {
                single_thread: shared.single_thread,
                live_only: shared.live_only,
            },
            crate::cache::Freshness::FileStat,
            parse_cached,
            reprice,
        )?;
        for ((index, _), entries) in plain.iter().zip(cached) {
            entries_by_index.insert(*index, entries);
        }
    }

    if let Some(parse_started) = parse_started {
        eprintln!("[timing] pi: parse {:?}", parse_started.elapsed());
    }
    let tail_started = std::env::var_os("CCUSAGE_DEBUG_TIMING").is_some().then(std::time::Instant::now);
    let replay_plan = PiReplayPlan::new(&flat, &loaded, &files_by_path);
    for (index, data) in loaded.into_iter().enumerate() {
        let Some(data) = data else {
            continue;
        };
        let skip = replay_plan.skip_prefix(index);
        let entries = if skip == 0 {
            data.entries
        } else {
            data.entries.into_iter().skip(skip).collect()
        };
        entries_by_index.insert(index, entries);
    }
    // Route entries to their cache unit in original file order without
    // deduping: the ledger below is idempotent to duplicate live keys
    // (primary-key INSERT OR IGNORE plus set-based merge), and the single
    // post-ledger pass removes duplicates with first-wins order intact.
    // Skipping this pass saves one id string per entry with zero output change.
    let mut per_unit: Vec<Vec<LoadedEntry>> = (0..units.len()).map(|_| Vec::new()).collect();
    for (index, unit_no) in unit_by_index.iter().enumerate() {
        let Some(entries) = entries_by_index.remove(&index) else {
            continue;
        };
        per_unit[*unit_no].extend(entries);
    }
    let mut live: Vec<LoadedEntry> = Vec::new();
    let ledger_started = std::env::var_os("CCUSAGE_DEBUG_TIMING").is_some().then(std::time::Instant::now);
    for (unit, entries) in units.iter().zip(per_unit) {
        live.extend(crate::cache::retain_via_ledger(
            &unit.namespace,
            entries,
            shared.live_only,
            None,
        ));
    }
    if let Some(ledger_started) = ledger_started {
        eprintln!("[timing] pi: ledger {:?}", ledger_started.elapsed());
    }
    let mut seen = FxHashSet::with_capacity_and_hasher(live.len(), Default::default());
    let mut entries = Vec::with_capacity(live.len());
    for entry in live {
        let id = match scope {
            PiLoadScope::Default => parser::entry_id(&entry),
            PiLoadScope::Named { .. } => parser::entry_id_for_store(scope.store_name(), &entry),
        };
        if seen.insert(id) {
            entries.push(entry);
        }
    }
    entries.sort_by_key(|entry| entry.timestamp);
    if let Some(tail_started) = tail_started {
        eprintln!("[timing] pi: dedup_ledger_sort {:?}", tail_started.elapsed());
    }
    Ok(entries)
}

struct PiCacheUnit {
    namespace: String,
    root: PathBuf,
    files: Vec<(usize, PathBuf)>,
}

/// The session header is always the first line: read only that, never the
/// whole file. Unreadable files yield no header and fail open downstream.
fn read_first_line_header(file: &Path) -> Option<parser::PiSessionHeader> {
    let handle = std::fs::File::open(file).ok()?;
    let mut reader = std::io::BufReader::new(handle);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).ok()?;
    parser::parse_session_header(&line)
}

/// First-line session header persisted across runs, keyed by file stat.
/// A header only changes with its file, under the same size+mtime trust the
/// entries cache (`Freshness::FileStat`) already relies on.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct StoredSessionHeader {
    mtime_ms: u64,
    size: u64,
    session: bool,
    parent: Option<String>,
    malformed: bool,
    timestamp_ms: Option<i64>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct HeaderSidecar {
    v: u32,
    files: HashMap<String, StoredSessionHeader>,
}

/// `stat` fingerprint without opening the file; mirrors
/// `ccusage_core::cache::file_metadata` (whose fields are crate-private).
fn file_stat_ms(path: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime_ms = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    Some((mtime_ms, metadata.len()))
}

/// Session headers for every file, opening only new/changed ones; failures fall back to reading, never skipping.
fn load_session_headers(
    files: &[PathBuf],
    namespaces: &[&str],
    single_thread: bool,
) -> Vec<Option<parser::PiSessionHeader>> {
    let timed = std::env::var_os("CCUSAGE_DEBUG_TIMING").is_some();
    let started = timed.then(std::time::Instant::now);
    let sidecar_path = crate::cache::cache_dir().map(|dir| dir.join("pi-headers.json"));
    let sidecar: HeaderSidecar = sidecar_path
        .as_ref()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .filter(|sidecar: &HeaderSidecar| sidecar.v == 1)
        .unwrap_or_default();
    let mut headers: Vec<Option<parser::PiSessionHeader>> = Vec::with_capacity(files.len());
    let mut need: Vec<(usize, PathBuf, String)> = Vec::new();
    let mut reused = 0usize;
    for (index, file) in files.iter().enumerate() {
        let key = format!("{}\0{}", namespaces[index], file.to_string_lossy());
        let reuse = file_stat_ms(file).and_then(|(mtime_ms, size)| {
            let stored = sidecar.files.get(&key)?;
            (stored.mtime_ms == mtime_ms && stored.size == size).then(|| {
                stored.session.then(|| parser::PiSessionHeader {
                    parent_session: stored.parent.as_ref().map(PathBuf::from),
                    parent_session_is_malformed: stored.malformed,
                    timestamp: stored.timestamp_ms.map(crate::TimestampMs::from_millis),
                })
            })
        });
        match reuse {
            Some(header) => {
                reused += 1;
                headers.push(header);
            }
            None => {
                headers.push(None);
                need.push((index, file.clone(), key));
            }
        }
    }
    if !need.is_empty() {
        let paths: Vec<PathBuf> = need.iter().map(|(_, file, _)| file.clone()).collect();
        let parsed = read_files_parallel(&paths, single_thread, read_first_line_header);
        let mut inserts: HashMap<String, StoredSessionHeader> = HashMap::new();
        for ((index, _, key), header) in need.into_iter().zip(parsed) {
            // Stat is bound to the content just parsed; a file that vanished
            // mid-read is simply not stored and fails open next run.
            if let Some((mtime_ms, size)) = file_stat_ms(&files[index]) {
                // ponytail: stores the outcome even when the first line is
                // not a session header; lineage only needs parent-bearing
                // files, and this keeps the warm path at zero opens.
                let (session, parent, malformed, timestamp_ms) = header
                    .as_ref()
                    .map(|header| {
                        (
                            true,
                            header
                                .parent_session
                                .as_ref()
                                .map(|p| p.to_string_lossy().into_owned()),
                            header.parent_session_is_malformed,
                            header.timestamp.map(|ts| ts.as_millis()),
                        )
                    })
                    .unwrap_or((false, None, false, None));
                inserts.insert(
                    key,
                    StoredSessionHeader {
                        mtime_ms,
                        size,
                        session,
                        parent,
                        malformed,
                        timestamp_ms,
                    },
                );
            }
            headers[index] = header;
        }
        if !inserts.is_empty() {
            // Reload-and-union: sibling loader calls (other scopes sharing
            // this file) may save concurrently; union converges while
            // last-writer-wins would drop their keys. Same-key races carry
            // the same stat-bound content, so overwrites are idempotent.
            // Stale keys (deleted files) simply never match a stat again;
            // no pruning, so concurrent namespaces cannot wipe each other.
            let mut merged: HeaderSidecar = sidecar_path
                .as_ref()
                .and_then(|path| std::fs::read(path).ok())
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .filter(|sidecar: &HeaderSidecar| sidecar.v == 1)
                .unwrap_or_default();
            merged.files.extend(inserts);
            merged.v = 1;
            if let Some(path) = sidecar_path.as_ref()
                && let Some(parent) = path.parent()
                && std::fs::create_dir_all(parent).is_ok()
                && let Ok(bytes) = serde_json::to_vec(&merged)
            {
                // Atomic replace so a concurrent reader never sees a torn file.
                let tmp = path.with_extension("json.tmp");
                if std::fs::write(&tmp, &bytes).is_ok() {
                    let _ = std::fs::rename(&tmp, path);
                }
            }
        }
    }
    if let Some(started) = started {
        eprintln!(
            "[timing] pi: header_scan {} opened, {} reused, {} total {:?}",
            files.len() - reused,
            reused,
            files.len(),
            started.elapsed()
        );
    }
    headers
}

struct ParentReplay {
    parent_index: usize,
    fork_timestamp: crate::TimestampMs,
}

struct PiReplayPlan {
    skip_by_file: HashMap<usize, usize>,
}

impl PiReplayPlan {
    fn new(
        files: &[PathBuf],
        loaded: &[Option<parser::PiSessionData>],
        files_by_path: &HashMap<PathBuf, usize>,
    ) -> Self {
        // No parsed parent header means no child to suppress; the loops
        // below would leave every set empty, so return that directly.
        if !loaded.iter().flatten().any(|data| {
            data.header
                .as_ref()
                .is_some_and(|header| header.parent_session.is_some())
        }) {
            return Self {
                skip_by_file: HashMap::new(),
            };
        }

        let mut invalid_lineage = HashSet::new();
        for (index, data) in loaded.iter().enumerate() {
            let Some(header) = data.as_ref().and_then(|data| data.header.as_ref()) else {
                invalid_lineage.insert(index);
                continue;
            };
            if header.timestamp.is_none() || header.parent_session_is_malformed {
                invalid_lineage.insert(index);
            }
        }

        let mut parent_by_child = HashMap::new();
        for (child_index, data) in loaded.iter().enumerate() {
            let Some(header) = data.as_ref().and_then(|data| data.header.as_ref()) else {
                continue;
            };
            let Some(parent_path) = header.parent_session.as_ref() else {
                continue;
            };
            let Some(fork_timestamp) = header.timestamp else {
                continue;
            };
            let Some(parent_index) =
                resolve_parent_index(parent_path, &files[child_index], files_by_path)
            else {
                invalid_lineage.insert(child_index);
                continue;
            };
            if parent_index == child_index {
                invalid_lineage.insert(child_index);
                continue;
            }
            parent_by_child.insert(
                child_index,
                ParentReplay {
                    parent_index,
                    fork_timestamp,
                },
            );
        }

        let mut skip_by_file = HashMap::new();
        for (&child_index, replay) in &parent_by_child {
            if !has_valid_lineage(child_index, &parent_by_child, &invalid_lineage) {
                continue;
            }
            let Some(parent) = loaded
                .get(replay.parent_index)
                .and_then(|data| data.as_ref())
            else {
                continue;
            };
            let Some(child) = loaded.get(child_index).and_then(|data| data.as_ref()) else {
                continue;
            };
            let Some(matched) = parent.matching_replay_prefix(child, replay.fork_timestamp) else {
                continue;
            };
            if matched > 0 {
                skip_by_file.insert(child_index, matched);
            }
        }

        Self { skip_by_file }
    }

    fn skip_prefix(&self, file_index: usize) -> usize {
        self.skip_by_file.get(&file_index).copied().unwrap_or(0)
    }
}

fn has_valid_lineage(
    child_index: usize,
    parent_by_child: &HashMap<usize, ParentReplay>,
    invalid_lineage: &HashSet<usize>,
) -> bool {
    let mut visited = HashSet::new();
    let mut current = child_index;
    while let Some(replay) = parent_by_child.get(&current) {
        if invalid_lineage.contains(&current) {
            return false;
        }
        if !visited.insert(current) {
            return false;
        }
        current = replay.parent_index;
    }
    !invalid_lineage.contains(&current)
}

fn resolve_parent_index(
    parent_path: &Path,
    child_path: &Path,
    files_by_path: &HashMap<PathBuf, usize>,
) -> Option<usize> {
    let normalized_parent = normalize_path(parent_path);
    if let Some(parent_index) = files_by_path.get(&normalized_parent).copied() {
        return Some(parent_index);
    }
    if parent_path.is_relative()
        && let Some(parent) = child_path.parent()
    {
        return files_by_path
            .get(&normalize_path(&parent.join(parent_path)))
            .copied();
    }
    None
}

fn is_subagent_artifact_transcript(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str().to_string_lossy() == "subagent-artifacts")
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_default()
    };
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{CostMode, SharedArgs};
    use ccusage_test_support::{CacheEnv, Fixture};
    use serde_json::json;
    use std::path::Path;

    fn session_line(id: &str, timestamp: &str, parent: Option<&Path>) -> String {
        let mut line = json!({
            "type": "session",
            "id": id,
            "timestamp": timestamp,
        });
        if let Some(parent) = parent {
            line["parentSession"] = json!(parent.to_string_lossy().to_string());
        }
        line.to_string()
    }

    fn usage_line_with_model_and_total(
        timestamp: &str,
        model: &str,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        total_tokens: u64,
    ) -> String {
        json!({
            "type": "message",
            "timestamp": timestamp,
            "message": {
                "role": "assistant",
                "model": model,
                "usage": {
                    "input": input,
                    "output": output,
                    "cacheRead": cache_read,
                    "cacheWrite": cache_write,
                    "totalTokens": total_tokens,
                },
            },
        })
        .to_string()
    }

    fn usage_line(
        timestamp: &str,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> String {
        usage_line_with_model_and_total(
            timestamp,
            "gpt-5",
            input,
            output,
            cache_read,
            cache_write,
            input + output + cache_read + cache_write,
        )
    }

    fn linked_usage_line(id: &str, parent_id: Option<&str>, timestamp: &str, input: u64) -> String {
        let mut line = json!({
            "type": "message",
            "id": id,
            "timestamp": timestamp,
            "message": {
                "role": "assistant",
                "model": "gpt-5",
                "usage": {
                    "input": input,
                    "output": 10,
                    "cacheRead": 20,
                    "cacheWrite": 3,
                    "totalTokens": input + 33,
                },
            },
        });
        line["parentId"] = parent_id.map_or(serde_json::Value::Null, |parent_id| json!(parent_id));
        line.to_string()
    }

    fn usage_line_with_display_cost(timestamp: &str, display_cost: f64) -> String {
        json!({
            "type": "message",
            "timestamp": timestamp,
            "message": {
                "role": "assistant",
                "model": "gpt-5",
                "usage": {
                    "input": 10,
                    "output": 20,
                    "cacheRead": 30,
                    "cacheWrite": 40,
                    "totalTokens": 100,
                    "cost": {"total": display_cost},
                },
            },
        })
        .to_string()
    }

    #[test]
    fn skips_replayed_parent_prefix_but_keeps_child_usage() {
        let _cache_env = CacheEnv::new("pi-replay-prefix");
        let fixture = Fixture::new();
        let parent = fixture.write_file(
            "sessions/project-a/root.jsonl",
            [
                session_line("root", "2026-01-01T00:00:00.000Z", None),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
                usage_line("2026-01-03T00:00:00.000Z", 200, 20, 30, 4),
                usage_line("2026-01-04T00:00:00.000Z", 50, 5, 6, 1),
            ]
            .join("\n"),
        );
        let _child_a = fixture.write_file(
            "sessions/project-a/child-a.jsonl",
            [
                session_line("child-a", "2026-01-03T00:00:00.000Z", Some(&parent)),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
                usage_line("2026-01-03T00:00:00.000Z", 200, 20, 30, 4),
                usage_line("2026-01-04T00:00:00.000Z", 50, 5, 6, 1),
            ]
            .join("\n"),
        );
        let _child_b = fixture.write_file(
            "sessions/project-a/child-b.jsonl",
            [
                session_line("child-b", "2026-01-03T00:00:00.000Z", Some(&parent)),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
                usage_line("2026-01-03T00:00:00.000Z", 200, 20, 30, 999),
                usage_line("2026-01-03T01:00:00.000Z", 70, 7, 8, 2),
            ]
            .join("\n"),
        );

        for single_thread in [true, false] {
            let shared = SharedArgs {
                mode: CostMode::Display,
                single_thread,
                ..SharedArgs::default()
            };
            let entries = load_entries_from_paths(
                &shared,
                vec![fixture.path("sessions")],
                None,
                PiLoadScope::Default,
            )
            .unwrap();

            assert_eq!(entries.len(), 6, "single_thread={single_thread}");
            let child_a_entries = entries
                .iter()
                .filter(|entry| entry.session_id.as_ref() == "child-a")
                .collect::<Vec<_>>();
            assert_eq!(child_a_entries.len(), 1, "single_thread={single_thread}");
            assert_eq!(
                child_a_entries[0].data.message.usage.input_tokens, 50,
                "single_thread={single_thread}"
            );
            let child_b_entries = entries
                .iter()
                .filter(|entry| entry.session_id.as_ref() == "child-b")
                .collect::<Vec<_>>();
            assert_eq!(child_b_entries.len(), 2, "single_thread={single_thread}");
            assert_eq!(
                child_b_entries[0]
                    .data
                    .message
                    .usage
                    .cache_creation_input_tokens,
                999,
                "single_thread={single_thread}"
            );
        }
    }

    #[test]
    fn skips_the_copied_active_parent_branch_after_an_abandoned_sibling() {
        let _cache_env = CacheEnv::new("pi-abandoned-sibling");
        let fixture = Fixture::new();
        let parent = fixture.write_file(
            "sessions/project-a/root.jsonl",
            [
                session_line("root", "2026-01-01T00:00:00.000Z", None),
                linked_usage_line("a", None, "2026-01-02T10:00:00.000Z", 100),
                linked_usage_line("x", Some("a"), "2026-01-02T11:00:00.000Z", 200),
                linked_usage_line("y", Some("a"), "2026-01-02T12:00:00.000Z", 300),
                linked_usage_line("z", Some("y"), "2026-01-02T13:00:00.000Z", 400),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/child.jsonl",
            [
                session_line("child", "2026-01-03T00:00:00.000Z", Some(&parent)),
                linked_usage_line("copy-a", None, "2026-01-02T10:00:00.000Z", 100),
                linked_usage_line("copy-y", Some("copy-a"), "2026-01-02T12:00:00.000Z", 300),
                linked_usage_line("copy-z", Some("copy-y"), "2026-01-02T13:00:00.000Z", 400),
                linked_usage_line(
                    "child-only",
                    Some("copy-z"),
                    "2026-01-03T01:00:00.000Z",
                    500,
                ),
            ]
            .join("\n"),
        );

        for single_thread in [true, false] {
            let shared = SharedArgs {
                mode: CostMode::Display,
                single_thread,
                ..SharedArgs::default()
            };
            let entries = load_entries_from_paths(
                &shared,
                vec![fixture.path("sessions")],
                None,
                PiLoadScope::Default,
            )
            .unwrap();

            assert_eq!(entries.len(), 5, "single_thread={single_thread}");
            assert!(entries.iter().any(|entry| {
                entry.session_id.as_ref() == "root" && entry.data.message.usage.input_tokens == 200
            }));
            let child_entries = entries
                .iter()
                .filter(|entry| entry.session_id.as_ref() == "child")
                .collect::<Vec<_>>();
            assert_eq!(child_entries.len(), 1, "single_thread={single_thread}");
            assert_eq!(
                child_entries[0].data.message.usage.input_tokens, 500,
                "single_thread={single_thread}"
            );
        }
    }

    #[test]
    fn skips_copied_branch_when_parent_has_multiple_disconnected_roots() {
        let _cache_env = CacheEnv::new("pi-disconnected-roots");
        let fixture = Fixture::new();
        let parent = fixture.write_file(
            "sessions/project-a/root.jsonl",
            [
                session_line("root", "2026-01-01T00:00:00.000Z", None),
                linked_usage_line("unrelated-root", None, "2026-01-02T09:00:00.000Z", 50),
                linked_usage_line("active-root", None, "2026-01-02T10:00:00.000Z", 100),
                linked_usage_line(
                    "active-leaf",
                    Some("active-root"),
                    "2026-01-02T11:00:00.000Z",
                    200,
                ),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/child.jsonl",
            [
                session_line("child", "2026-01-03T00:00:00.000Z", Some(&parent)),
                linked_usage_line("copy-root", None, "2026-01-02T10:00:00.000Z", 100),
                linked_usage_line(
                    "copy-leaf",
                    Some("copy-root"),
                    "2026-01-02T11:00:00.000Z",
                    200,
                ),
                linked_usage_line(
                    "child-only",
                    Some("copy-leaf"),
                    "2026-01-03T01:00:00.000Z",
                    300,
                ),
            ]
            .join("\n"),
        );

        for single_thread in [true, false] {
            let shared = SharedArgs {
                mode: CostMode::Display,
                single_thread,
                ..SharedArgs::default()
            };
            let entries = load_entries_from_paths(
                &shared,
                vec![fixture.path("sessions")],
                None,
                PiLoadScope::Default,
            )
            .unwrap();

            assert_eq!(entries.len(), 4, "single_thread={single_thread}");
            assert!(entries.iter().any(|entry| {
                entry.session_id.as_ref() == "root" && entry.data.message.usage.input_tokens == 50
            }));
            let child_entries = entries
                .iter()
                .filter(|entry| entry.session_id.as_ref() == "child")
                .collect::<Vec<_>>();
            assert_eq!(child_entries.len(), 1, "single_thread={single_thread}");
            assert_eq!(
                child_entries[0].data.message.usage.input_tokens, 300,
                "single_thread={single_thread}"
            );
        }
    }

    #[test]
    fn fails_open_for_missing_malformed_and_unrelated_same_token_sessions() {
        let _cache_env = CacheEnv::new("pi-fail-open");
        let fixture = Fixture::new();
        let missing_parent = fixture.path("sessions/project-a/missing.jsonl");
        let malformed_parent = fixture.path("sessions/project-a/malformed-parent.jsonl");
        let outside_parent = fixture.write_file(
            "outside/root.jsonl",
            [
                session_line("outside", "2026-01-01T00:00:00.000Z", None),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/missing-child.jsonl",
            [
                session_line(
                    "missing-child",
                    "2026-01-03T00:00:00.000Z",
                    Some(&missing_parent),
                ),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/outside-parent-child.jsonl",
            [
                session_line(
                    "outside-parent-child",
                    "2026-01-03T00:00:00.000Z",
                    Some(&outside_parent),
                ),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/malformed-parent.jsonl",
            [
                "not json".to_string(),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/malformed-parent-child.jsonl",
            [
                session_line(
                    "malformed-parent-child",
                    "2026-01-03T00:00:00.000Z",
                    Some(&malformed_parent),
                ),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/malformed-parent-grandchild.jsonl",
            [
                session_line(
                    "malformed-parent-grandchild",
                    "2026-01-04T00:00:00.000Z",
                    Some(&fixture.path("sessions/project-a/malformed-parent-child.jsonl")),
                ),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/malformed-child.jsonl",
            [
                "not json".to_string(),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/regular-a.jsonl",
            usage_line("2026-01-02T11:00:00.000Z", 100, 10, 20, 3),
        );
        let _ = fixture.write_file(
            "sessions/project-a/regular-b.jsonl",
            usage_line("2026-01-02T11:00:00.000Z", 100, 10, 20, 3),
        );

        for single_thread in [true, false] {
            let shared = SharedArgs {
                mode: CostMode::Display,
                single_thread,
                ..SharedArgs::default()
            };
            let entries = load_entries_from_paths(
                &shared,
                vec![fixture.path("sessions")],
                None,
                PiLoadScope::Default,
            )
            .unwrap();

            assert_eq!(entries.len(), 8, "single_thread={single_thread}");
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| entry.session_id.as_ref() == "missing-child")
                    .count(),
                1,
                "single_thread={single_thread}"
            );
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| entry.session_id.as_ref() == "outside-parent-child")
                    .count(),
                1,
                "single_thread={single_thread}"
            );
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| entry.session_id.as_ref() == "regular-a")
                    .count(),
                1,
                "single_thread={single_thread}"
            );
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| entry.session_id.as_ref() == "regular-b")
                    .count(),
                1,
                "single_thread={single_thread}"
            );
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| entry.session_id.as_ref() == "malformed-parent-grandchild")
                    .count(),
                1,
                "single_thread={single_thread}"
            );
        }
    }

    #[test]
    fn fails_open_for_self_referential_and_cyclic_sessions() {
        let _cache_env = CacheEnv::new("pi-cycles");
        let fixture = Fixture::new();
        let self_path = fixture.path("sessions/project-a/self.jsonl");
        let cycle_a = fixture.path("sessions/project-a/cycle-a.jsonl");
        let cycle_b = fixture.path("sessions/project-a/cycle-b.jsonl");
        let _ = fixture.write_file(
            "sessions/project-a/self.jsonl",
            [
                session_line("self", "2026-01-03T00:00:00.000Z", Some(&self_path)),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/cycle-a.jsonl",
            [
                session_line("cycle-a", "2026-01-03T00:00:00.000Z", Some(&cycle_b)),
                usage_line("2026-01-02T10:00:00.000Z", 200, 20, 30, 4),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/cycle-b.jsonl",
            [
                session_line("cycle-b", "2026-01-03T00:00:00.000Z", Some(&cycle_a)),
                usage_line("2026-01-02T10:00:00.000Z", 200, 20, 30, 4),
            ]
            .join("\n"),
        );

        for single_thread in [true, false] {
            let shared = SharedArgs {
                mode: CostMode::Display,
                single_thread,
                ..SharedArgs::default()
            };
            let entries = load_entries_from_paths(
                &shared,
                vec![fixture.path("sessions")],
                None,
                PiLoadScope::Default,
            )
            .unwrap();

            assert_eq!(entries.len(), 3, "single_thread={single_thread}");
        }
    }

    #[test]
    fn uses_raw_parent_stream_for_nested_forks() {
        let _cache_env = CacheEnv::new("pi-nested-forks");
        let fixture = Fixture::new();
        let root = fixture.write_file(
            "sessions/project-a/root.jsonl",
            [
                session_line("root", "2026-01-01T00:00:00.000Z", None),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
                usage_line("2026-01-02T11:00:00.000Z", 200, 20, 30, 4),
            ]
            .join("\n"),
        );
        let child = fixture.write_file(
            "sessions/project-a/child.jsonl",
            [
                session_line("child", "2026-01-03T00:00:00.000Z", Some(&root)),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
                usage_line("2026-01-02T11:00:00.000Z", 200, 20, 30, 4),
                usage_line("2026-01-03T01:00:00.000Z", 300, 30, 40, 5),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/grandchild.jsonl",
            [
                session_line("grandchild", "2026-01-04T00:00:00.000Z", Some(&child)),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
                usage_line("2026-01-02T11:00:00.000Z", 200, 20, 30, 4),
                usage_line("2026-01-03T01:00:00.000Z", 300, 30, 40, 5),
                usage_line("2026-01-04T01:00:00.000Z", 400, 40, 50, 6),
            ]
            .join("\n"),
        );

        for single_thread in [true, false] {
            let shared = SharedArgs {
                mode: CostMode::Display,
                single_thread,
                ..SharedArgs::default()
            };
            let entries = load_entries_from_paths(
                &shared,
                vec![fixture.path("sessions")],
                None,
                PiLoadScope::Default,
            )
            .unwrap();

            assert_eq!(entries.len(), 4, "single_thread={single_thread}");
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| entry.session_id.as_ref() == "child")
                    .count(),
                1,
                "single_thread={single_thread}"
            );
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| entry.session_id.as_ref() == "grandchild")
                    .count(),
                1,
                "single_thread={single_thread}"
            );
            assert_eq!(
                entries
                    .iter()
                    .find(|entry| entry.session_id.as_ref() == "grandchild")
                    .unwrap()
                    .data
                    .message
                    .usage
                    .input_tokens,
                400,
                "single_thread={single_thread}"
            );
        }
    }

    #[test]
    fn compares_every_effective_usage_field_before_suppressing_replay() {
        let _cache_env = CacheEnv::new("pi-usage-fields");
        let fixture = Fixture::new();
        let parent = fixture.write_file(
            "sessions/project-a/root.jsonl",
            [
                session_line("root", "2026-01-01T00:00:00.000Z", None),
                usage_line_with_model_and_total(
                    "2026-01-02T10:00:00.000Z",
                    "gpt-5",
                    10,
                    20,
                    30,
                    40,
                    100,
                ),
            ]
            .join("\n"),
        );
        let variants = [
            ("input", "gpt-5", 11, 20, 30, 40, 101),
            ("output", "gpt-5", 10, 21, 30, 40, 101),
            ("cache-read", "gpt-5", 10, 20, 31, 40, 101),
            ("cache-write", "gpt-5", 10, 20, 30, 41, 101),
            ("model", "gpt-5-mini", 10, 20, 30, 40, 100),
            ("total-fallback", "gpt-5", 10, 20, 30, 40, 110),
        ];
        for (name, model, input, output, cache_read, cache_write, total_tokens) in variants {
            let _ = fixture.write_file(
                format!("sessions/project-a/child-{name}.jsonl"),
                [
                    session_line(name, "2026-01-03T00:00:00.000Z", Some(&parent)),
                    usage_line_with_model_and_total(
                        "2026-01-02T10:00:00.000Z",
                        model,
                        input,
                        output,
                        cache_read,
                        cache_write,
                        total_tokens,
                    ),
                    usage_line("2026-01-03T01:00:00.000Z", 1, 2, 3, 4),
                ]
                .join("\n"),
            );
        }

        let shared = SharedArgs {
            mode: CostMode::Display,
            single_thread: false,
            ..SharedArgs::default()
        };
        let entries = load_entries_from_paths(
            &shared,
            vec![fixture.path("sessions")],
            None,
            PiLoadScope::Default,
        )
        .unwrap();

        assert_eq!(entries.len(), 1 + variants.len() * 2);
        for (name, ..) in variants {
            let session_id = format!("child-{name}");
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| entry.session_id.as_ref() == session_id)
                    .count(),
                2,
                "variant={name}"
            );
        }
    }

    #[test]
    fn keeps_child_usage_when_display_cost_differs() {
        let _cache_env = CacheEnv::new("pi-display-cost-differs");
        let fixture = Fixture::new();
        let parent = fixture.write_file(
            "sessions/project-a/root.jsonl",
            [
                session_line("root", "2026-01-01T00:00:00.000Z", None),
                usage_line_with_display_cost("2026-01-02T10:00:00.000Z", 1.0),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/child.jsonl",
            [
                session_line("child", "2026-01-03T00:00:00.000Z", Some(&parent)),
                usage_line_with_display_cost("2026-01-02T10:00:00.000Z", 2.0),
            ]
            .join("\n"),
        );

        for mode in [CostMode::Display, CostMode::Auto] {
            let shared = SharedArgs {
                mode,
                ..SharedArgs::default()
            };
            let entries = load_entries_from_paths(
                &shared,
                vec![fixture.path("sessions")],
                None,
                PiLoadScope::Default,
            )
            .unwrap();

            assert_eq!(entries.len(), 2, "mode={mode:?}");
            assert_eq!(
                entries
                    .iter()
                    .find(|entry| entry.session_id.as_ref() == "child")
                    .unwrap()
                    .cost,
                2.0,
                "mode={mode:?}"
            );
        }
    }

    #[test]
    fn ignores_stored_display_cost_when_calculating_replay_identity() {
        let _cache_env = CacheEnv::new("pi-calc-identity");
        let fixture = Fixture::new();
        let parent = fixture.write_file(
            "sessions/project-a/root.jsonl",
            [
                session_line("root", "2026-01-01T00:00:00.000Z", None),
                usage_line_with_display_cost("2026-01-02T10:00:00.000Z", 1.0),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/child.jsonl",
            [
                session_line("child", "2026-01-03T00:00:00.000Z", Some(&parent)),
                usage_line_with_display_cost("2026-01-02T10:00:00.000Z", 2.0),
            ]
            .join("\n"),
        );
        let shared = SharedArgs {
            mode: CostMode::Calculate,
            ..SharedArgs::default()
        };
        let entries = load_entries_from_paths(
            &shared,
            vec![fixture.path("sessions")],
            None,
            PiLoadScope::Default,
        )
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id.as_ref(), "root");
    }

    #[test]
    fn treats_signed_zero_display_costs_as_equal_replay_identity() {
        let _cache_env = CacheEnv::new("pi-signed-zero");
        let fixture = Fixture::new();
        let parent = fixture.write_file(
            "sessions/project-a/root.jsonl",
            [
                session_line("root", "2026-01-01T00:00:00.000Z", None),
                usage_line_with_display_cost("2026-01-02T10:00:00.000Z", 0.0),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/child.jsonl",
            [
                session_line("child", "2026-01-03T00:00:00.000Z", Some(&parent)),
                usage_line_with_display_cost("2026-01-02T10:00:00.000Z", -0.0),
            ]
            .join("\n"),
        );

        for mode in [CostMode::Display, CostMode::Auto] {
            let shared = SharedArgs {
                mode,
                ..SharedArgs::default()
            };
            let entries = load_entries_from_paths(
                &shared,
                vec![fixture.path("sessions")],
                None,
                PiLoadScope::Default,
            )
            .unwrap();

            assert_eq!(entries.len(), 1, "mode={mode:?}");
            assert_eq!(entries[0].session_id.as_ref(), "root", "mode={mode:?}");
        }
    }

    #[test]
    fn treats_missing_and_zero_display_costs_as_equal_replay_identity() {
        let _cache_env = CacheEnv::new("pi-missing-zero");
        let fixture = Fixture::new();
        let parent = fixture.write_file(
            "sessions/project-a/root.jsonl",
            [
                session_line("root", "2026-01-01T00:00:00.000Z", None),
                usage_line_with_model_and_total(
                    "2026-01-02T10:00:00.000Z",
                    "gpt-5",
                    10,
                    20,
                    30,
                    40,
                    100,
                ),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/child.jsonl",
            [
                session_line("child", "2026-01-03T00:00:00.000Z", Some(&parent)),
                usage_line_with_display_cost("2026-01-02T10:00:00.000Z", 0.0),
            ]
            .join("\n"),
        );
        let shared = SharedArgs {
            mode: CostMode::Display,
            ..SharedArgs::default()
        };
        let entries = load_entries_from_paths(
            &shared,
            vec![fixture.path("sessions")],
            None,
            PiLoadScope::Default,
        )
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id.as_ref(), "root");
    }

    #[test]
    fn dedupes_replay_when_raw_total_underreports_billable_usage() {
        let _cache_env = CacheEnv::new("pi-underreports");
        let fixture = Fixture::new();
        let parent = fixture.write_file(
            "sessions/project-a/root.jsonl",
            [
                session_line("root", "2026-01-01T00:00:00.000Z", None),
                usage_line_with_model_and_total(
                    "2026-01-02T10:00:00.000Z",
                    "gpt-5",
                    10,
                    20,
                    30,
                    40,
                    100,
                ),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/child.jsonl",
            [
                session_line("child", "2026-01-03T00:00:00.000Z", Some(&parent)),
                usage_line_with_model_and_total(
                    "2026-01-02T10:00:00.000Z",
                    "gpt-5",
                    10,
                    20,
                    30,
                    40,
                    99,
                ),
                usage_line("2026-01-03T01:00:00.000Z", 1, 2, 3, 4),
            ]
            .join("\n"),
        );

        for single_thread in [true, false] {
            let shared = SharedArgs {
                mode: CostMode::Display,
                single_thread,
                ..SharedArgs::default()
            };
            let entries = load_entries_from_paths(
                &shared,
                vec![fixture.path("sessions")],
                None,
                PiLoadScope::Default,
            )
            .unwrap();

            assert_eq!(entries.len(), 2, "single_thread={single_thread}");
            let child_entries = entries
                .iter()
                .filter(|entry| entry.session_id.as_ref() == "child")
                .collect::<Vec<_>>();
            assert_eq!(child_entries.len(), 1, "single_thread={single_thread}");
            assert_eq!(
                child_entries[0].data.message.usage.input_tokens, 1,
                "single_thread={single_thread}"
            );
        }
    }

    #[test]
    fn matches_parent_files_across_multiple_paths_of_one_named_store() {
        let _cache_env = CacheEnv::new("pi-cross-path");
        let fixture = Fixture::new();
        let first_store = fixture.create_dir_all("first");
        let second_store = fixture.create_dir_all("second");
        let parent = fixture.write_file(
            "first/project-a/root.jsonl",
            [
                session_line("root", "2026-01-01T00:00:00.000Z", None),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "second/project-a/child.jsonl",
            [
                session_line("child", "2026-01-03T00:00:00.000Z", Some(&parent)),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
                usage_line("2026-01-03T01:00:00.000Z", 50, 5, 6, 1),
            ]
            .join("\n"),
        );

        let shared = SharedArgs {
            mode: CostMode::Display,
            single_thread: false,
            ..SharedArgs::default()
        };
        let entries =
            load_entries_for_store_paths(&shared, vec![first_store, second_store], "omp", None)
                .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.session_id.as_ref() == "child")
                .count(),
            1
        );
        assert_eq!(entries[1].model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn skips_subagent_artifact_transcripts_but_keeps_fresh_context_child_sessions() {
        let _cache_env = CacheEnv::new("pi-artifacts");
        let fixture = Fixture::new();
        let _ = fixture.write_file(
            "sessions/project-a/root.jsonl",
            [
                session_line("root", "2026-01-01T00:00:00.000Z", None),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "sessions/project-a/root/subagent-artifacts/run1_agent_0_transcript.jsonl",
            usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
        );
        let _ = fixture.write_file(
            "sessions/project-a/subagent-artifacts/other_transcript.jsonl",
            usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
        );
        let _ = fixture.write_file(
            "sessions/project-a/root/run-abc/run-0/session.jsonl",
            usage_line("2026-01-03T01:00:00.000Z", 50, 5, 6, 1),
        );

        for single_thread in [true, false] {
            let shared = SharedArgs {
                mode: CostMode::Display,
                single_thread,
                ..SharedArgs::default()
            };
            let entries = load_entries_from_paths(
                &shared,
                vec![fixture.path("sessions")],
                None,
                PiLoadScope::Default,
            )
            .unwrap();

            assert_eq!(entries.len(), 2, "single_thread={single_thread}");
            assert!(
                entries
                    .iter()
                    .any(|entry| entry.data.message.usage.input_tokens == 100),
                "single_thread={single_thread}"
            );
            assert!(
                entries
                    .iter()
                    .any(|entry| entry.data.message.usage.input_tokens == 50),
                "single_thread={single_thread}"
            );
            assert!(
                !entries
                    .iter()
                    .any(|entry| entry.session_id.as_ref().contains("transcript")),
                "single_thread={single_thread}"
            );
        }
    }

    #[test]
    fn skips_subagent_artifact_transcripts_for_named_stores() {
        let _cache_env = CacheEnv::new("pi-artifacts-named");
        let fixture = Fixture::new();
        let store = fixture.create_dir_all("store");
        let _ = fixture.write_file(
            "store/project-a/root.jsonl",
            [
                session_line("root", "2026-01-01T00:00:00.000Z", None),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
            ]
            .join("\n"),
        );
        let _ = fixture.write_file(
            "store/project-a/root/subagent-artifacts/run1_agent_0_transcript.jsonl",
            usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
        );

        let shared = SharedArgs {
            mode: CostMode::Display,
            single_thread: false,
            ..SharedArgs::default()
        };
        let entries = load_entries_for_store_paths(&shared, vec![store], "omp", None).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.message.usage.input_tokens, 100);
    }

    #[test]
    fn deleted_file_spend_survives_via_the_ledger() {
        let _cache_env = CacheEnv::new("pi-ledger");
        let fixture = Fixture::new();
        let path = fixture.write_file(
            "sessions/project-a/root.jsonl",
            [
                session_line("root", "2026-01-01T00:00:00.000Z", None),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
            ]
            .join("\n"),
        );
        let shared = SharedArgs {
            mode: CostMode::Display,
            ..SharedArgs::default()
        };
        let cold = load_entries_from_paths(
            &shared,
            vec![fixture.path("sessions")],
            None,
            PiLoadScope::Default,
        )
        .unwrap();
        assert_eq!(cold.len(), 1);

        std::fs::remove_file(&path).unwrap();
        let warm = load_entries_from_paths(
            &shared,
            vec![fixture.path("sessions")],
            None,
            PiLoadScope::Default,
        )
        .unwrap();
        assert_eq!(warm.len(), 1, "deleted spend must be re-emitted");
        assert_eq!(warm[0].data.message.usage.input_tokens, 100);
    }

    #[test]
    fn live_only_suppresses_deleted_file_spend() {
        let _cache_env = CacheEnv::new("pi-live-only");
        let fixture = Fixture::new();
        let path = fixture.write_file(
            "sessions/project-a/root.jsonl",
            [
                session_line("root", "2026-01-01T00:00:00.000Z", None),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
            ]
            .join("\n"),
        );
        let live = SharedArgs {
            mode: CostMode::Display,
            ..SharedArgs::default()
        };
        let cold = load_entries_from_paths(
            &live,
            vec![fixture.path("sessions")],
            None,
            PiLoadScope::Default,
        )
        .unwrap();
        assert_eq!(cold.len(), 1);

        std::fs::remove_file(&path).unwrap();
        let live_only = SharedArgs {
            mode: CostMode::Display,
            live_only: true,
            ..SharedArgs::default()
        };
        let entries = load_entries_from_paths(
            &live_only,
            vec![fixture.path("sessions")],
            None,
            PiLoadScope::Default,
        )
        .unwrap();
        assert!(entries.is_empty(), "live_only must not re-emit deleted spend");
    }

    #[test]
    fn warm_run_reuses_the_persistent_entry_cache() {
        let cache_env = CacheEnv::new("pi-warm");
        let fixture = Fixture::new();
        let _ = fixture.write_file(
            "sessions/project-a/root.jsonl",
            [
                session_line("root", "2026-01-01T00:00:00.000Z", None),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
            ]
            .join("\n"),
        );
        let shared = SharedArgs {
            mode: CostMode::Display,
            ..SharedArgs::default()
        };
        let cold = load_entries_from_paths(
            &shared,
            vec![fixture.path("sessions")],
            None,
            PiLoadScope::Default,
        )
        .unwrap();
        assert_eq!(cold.len(), 1);
        assert!(
            cache_env.dir().join("ccusage/cache.db").exists(),
            "first load must persist the entry cache"
        );

        let warm = load_entries_from_paths(
            &shared,
            vec![fixture.path("sessions")],
            None,
            PiLoadScope::Default,
        )
        .unwrap();
        assert_eq!(warm.len(), 1);
        assert_eq!(warm[0].cost, cold[0].cost);
        assert_eq!(
            warm[0].data.message.usage.input_tokens,
            cold[0].data.message.usage.input_tokens
        );
    }

    #[test]
    fn warm_run_reuses_persisted_session_headers() {
        let cache_env = CacheEnv::new("pi-headers");
        let fixture = Fixture::new();
        let parent = fixture.write_file(
            "sessions/project-a/root.jsonl",
            [
                session_line("root", "2026-01-01T00:00:00.000Z", None),
                usage_line("2026-01-02T10:00:00.000Z", 100, 10, 20, 3),
            ]
            .join("\n"),
        );
        let child = fixture.write_file(
            "sessions/project-a/child.jsonl",
            [
                session_line("child", "2026-01-03T00:00:00.000Z", Some(&parent)),
                usage_line("2026-01-03T01:00:00.000Z", 50, 5, 6, 1),
            ]
            .join("\n"),
        );
        let shared = SharedArgs {
            mode: CostMode::Display,
            ..SharedArgs::default()
        };
        let load = || {
            load_entries_from_paths(
                &shared,
                vec![fixture.path("sessions")],
                None,
                PiLoadScope::Default,
            )
            .unwrap()
        };
        let cold = load();
        assert!(!cold.is_empty());
        assert!(
            cache_env.dir().join("ccusage/pi-headers.json").exists(),
            "first load must persist session headers"
        );
        let warm = load();
        assert_eq!(
            warm.len(),
            cold.len(),
            "reused headers must give identical rows"
        );
        assert_eq!(warm[0].cost, cold[0].cost);

        let mut grown = std::fs::read_to_string(&child).unwrap();
        grown.push('\n');
        grown.push_str(&usage_line("2026-01-03T02:00:00.000Z", 7, 1, 1, 1));
        std::fs::write(&child, grown).unwrap();
        let edited = load();
        assert!(
            edited.len() > cold.len(),
            "edited file must contribute new rows"
        );
    }

    #[test]
    fn detects_subagent_artifact_path_segments() {
        assert!(is_subagent_artifact_transcript(Path::new(
            "sessions/project-a/root/subagent-artifacts/run1_agent_0_transcript.jsonl"
        )));
        assert!(!is_subagent_artifact_transcript(Path::new(
            "sessions/project-a/root/run-abc/run-0/session.jsonl"
        )));
        assert!(!is_subagent_artifact_transcript(Path::new(
            "sessions/project-a/root.jsonl"
        )));
    }
}

#[cfg(test)]
mod local_display_tests {
    use super::load_entries;
    use crate::cli::{CostMode, SharedArgs};
    use ccusage_test_support::{CacheEnv, fs_fixture};

    #[test]
    fn display_mode_surfaces_logged_cost() {
        let _cache_env = CacheEnv::new("pi-display");
        let fixture = fs_fixture!({
            "sessions/project-a/agent_session-a.jsonl": r#"{"type":"message","timestamp":"2026-01-02T00:00:00.000Z","message":{"role":"assistant","model":"gpt-5","usage":{"input":100,"output":200,"cost":{"total":0.05}}}}"#,
        });
        let shared = SharedArgs {
            mode: CostMode::Display,
            offline: true,
            ..SharedArgs::default()
        };
        // Mirror pi::run(): pricing is loaded regardless of mode. Display must
        // still surface the logged costUSD, not reprice it to 0.0.
        let pricing = crate::PricingMap::load_with_overrides(
            shared.offline,
            false,
            shared.pricing_overrides.iter(),
        );

        let entries = load_entries(&shared, fixture.root().to_str(), Some(&pricing)).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].cost, 0.05);
        assert_eq!(entries[0].missing_pricing_model, None);
    }
}
