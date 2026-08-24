use std::{
    collections::{HashMap, HashSet},
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

use jiff::tz::TimeZone as JiffTimeZone;

use super::{
    parser::{OpenCodeMessage, message_to_entry, reprice},
    paths::paths,
};
use ccusage_core::{
    cache::{self, OpenCodeRow},
    sqlite_util::{open_readonly, read_id_session_data},
};

use crate::{
    LoadedEntry, PricingMap, Result,
    cli::{CostMode, SharedArgs},
    collect_files_with_extension, date_range_bounds_ms, debug_log, parse_tz,
};

pub fn load_entries(shared: &SharedArgs) -> Result<Vec<LoadedEntry>> {
    crate::progress::track_usage_load(
        crate::progress::UsageLoadAgent("OpenCode"),
        shared.json,
        || load_entries_inner(shared),
    )
}

fn load_entries_inner(shared: &SharedArgs) -> Result<Vec<LoadedEntry>> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for path in paths()? {
        for entry in load_entries_from_directory(&path, shared)? {
            if let Some(id) = entry_id(&entry)
                && !seen.insert(id.to_string())
            {
                continue;
            }
            entries.push(entry);
        }
    }
    entries.sort_by_key(|entry| entry.timestamp);
    Ok(entries)
}

pub fn load_entries_from_directory(
    opencode_dir: &Path,
    shared: &SharedArgs,
) -> Result<Vec<LoadedEntry>> {
    let pricing = if shared.mode == CostMode::Display {
        None
    } else {
        Some(PricingMap::load_with_overrides(
            shared.offline,
            crate::log_level() != Some(0),
            shared.pricing_overrides.iter(),
        ))
    };
    let tz = parse_tz(shared.timezone.as_deref());
    let window = DateWindow::from_shared(shared, tz.as_ref());
    let mut entries = Vec::new();
    let mut seen = HashSet::new();

    // Always pass database rows through the ledger under a dedicated namespace
    // so spend is retained even after the whole `opencode.db` is deleted.
    // Skipping the merge on a gone/corrupt db would drop all previously-ledgered
    // spend. `merge_ledger` only re-emits keys absent from the live set, so a
    // present source is never double-counted; `live_only` suppresses the re-emit.
    let db_entries = db_path(opencode_dir)
        .and_then(|db_path| {
            load_entries_from_database(
                &db_path,
                tz.as_ref(),
                shared.mode,
                pricing.as_ref(),
                shared,
                window,
            )
        })
        .unwrap_or_default();
    for entry in cache::retain_via_ledger("opencode-db", db_entries, shared.live_only, None) {
        if !window.is_unbounded() && !window.contains(entry.timestamp.as_millis()) {
            continue;
        }
        if let Some(id) = entry_id(&entry)
            && !seen.insert(id.to_string())
        {
            continue;
        }
        entries.push(entry);
    }

    let messages_dir = opencode_dir.join("storage").join("message");
    let mut files = Vec::new();
    collect_files_with_extension(&messages_dir, "json", &mut files);

    // The DB-covered file skip that used to live here is gone: the cache is keyed
    // by file, so a file dropped from one run's input would be evicted and re-read
    // on the next run with a different db state. The id dedup below still discards
    // those entries, we just pay a cached read instead of a parse.
    let json_entries = cache::load_with_cache(
        "opencode",
        &files,
        cache::CacheOpts {
            single_thread: shared.single_thread,
            live_only: shared.live_only,
        },
        cache::Freshness::FileStat,
        |path| read_message_file(path, tz.as_ref(), shared.mode, pricing.as_ref(), shared),
        |e| reprice(e, shared.mode, pricing.as_ref()),
        None,
    )?;
    // The window is applied here rather than before the parse: caching a
    // window-filtered result would poison the cache for the next run's window.
    for entry in json_entries {
        if !window.is_unbounded() && !window.contains(entry.timestamp.as_millis()) {
            continue;
        }
        if let Some(id) = entry_id(&entry)
            && !seen.insert(id.to_string())
        {
            continue;
        }
        entries.push(entry);
    }
    entries.sort_by_key(|entry| entry.timestamp);
    Ok(entries)
}

/// Reports whether `opencode_dir` holds any usage source at all: the SQLite
/// database, or at least one message file.
///
/// Detection has to ignore `--since`/`--until` because the loader applies the
/// window while reading, so an out-of-range query returns no entries even on an
/// install full of logs. Stops at the first message file rather than collecting
/// them all, so this stays cheap next to a large legacy dump.
pub(crate) fn has_source(opencode_dir: &Path) -> bool {
    if db_path(opencode_dir).is_some() {
        return true;
    }
    has_json_file(&opencode_dir.join("storage").join("message"))
}

// Mirrors `collect_files_with_extension`: judge entries by `file_type()` so
// symlinks are neither followed nor counted. Following them would let detection
// claim files the collection pass then refuses to read, and a symlinked cycle
// would recurse until the stack gives out.
fn has_json_file(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(std::result::Result::ok).any(|entry| {
        let Ok(file_type) = entry.file_type() else {
            return false;
        };
        let path = entry.path();
        if file_type.is_file() {
            path.extension()
                .is_some_and(|extension| extension == "json")
        } else {
            file_type.is_dir() && has_json_file(&path)
        }
    })
}

fn db_path(opencode_dir: &Path) -> Option<PathBuf> {
    let default_path = opencode_dir.join("opencode.db");
    if default_path.is_file() {
        return Some(default_path);
    }
    let mut candidates = fs::read_dir(opencode_dir)
        .ok()?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_channel_db_name)
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.into_iter().next()
}

fn is_channel_db_name(name: &str) -> bool {
    name.starts_with("opencode-")
        && name.ends_with(".db")
        && name["opencode-".len()..name.len() - ".db".len()]
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn fallback_from_cache(
    cached: HashMap<String, OpenCodeRow>,
    mode: CostMode,
    pricing: Option<&PricingMap>,
) -> Option<Vec<LoadedEntry>> {
    if cached.is_empty() {
        return None;
    }
    let mut entries: Vec<_> = cached
        .into_values()
        .map(|row| LoadedEntry::from(row.entry))
        .collect();
    for entry in &mut entries {
        reprice(entry, mode, pricing);
    }
    Some(entries)
}

/// Load OpenCode message-database rows, reusing the per-database row cache so
/// unchanged rows skip the expensive JSON parse.
///
/// V1 stores messages in `message`; V2 (beta) stores them in `session_message`.
/// Both tables share the `(id, session_id, data)` columns this loader selects,
/// so both are scanned when present and deduped by message id downstream.
fn load_entries_from_database(
    db_path: &Path,
    tz: Option<&JiffTimeZone>,
    mode: CostMode,
    pricing: Option<&PricingMap>,
    shared: &SharedArgs,
    window: DateWindow,
) -> Option<Vec<LoadedEntry>> {
    // Load the row cache first: it survives opencode.db corruption.
    let cache_key = cache::cache_key(db_path);
    let cached: HashMap<String, OpenCodeRow> = cache::load_opencode_row_cache(&cache_key)
        .into_iter()
        .flatten()
        .map(|row| (row.id.clone(), row))
        .collect();

    // On open/query failure fall back to the cached rows so spend is preserved
    // rather than zeroed (corruption) or doubled (ledger re-emission of an empty
    // live set).
    let Ok(connection) = open_readonly(db_path) else {
        debug_log(
            shared,
            format!("Failed to open OpenCode database: {}", db_path.display()),
        );
        return fallback_from_cache(cached, mode, pricing);
    };

    let Ok(tables) = message_tables(&connection) else {
        debug_log(
            shared,
            format!(
                "Failed to read OpenCode database schema: {}",
                db_path.display()
            ),
        );
        return fallback_from_cache(cached, mode, pricing);
    };
    if tables.is_empty() {
        debug_log(
            shared,
            format!(
                "OpenCode database has no message or session_message table: {}",
                db_path.display()
            ),
        );
        return fallback_from_cache(cached, mode, pricing);
    }

    let mut entries = Vec::new();
    let mut fresh_rows = Vec::new();
    let mut all_completed = true;
    for table in &tables {
        let (table_entries, table_fresh, completed) = scan_message_table(
            &connection,
            table,
            db_path,
            &cached,
            tz,
            mode,
            pricing,
            shared,
            window,
        );
        if !completed {
            all_completed = false;
        }
        entries.extend(table_entries);
        fresh_rows.extend(table_fresh);
    }

    if !all_completed {
        return fallback_from_cache(cached, mode, pricing);
    }

    // A full scan sees every live row, so the cache is rebuilt from exactly what
    // this run saw and rows deleted from the database drop out. A windowed scan
    // only saw part of the table, so the rows it did see are merged over the
    // existing cache instead of replacing it.
    if window.is_unbounded() {
        cache::save_opencode_row_cache(&cache_key, &fresh_rows);
    } else {
        let mut merged = cached;
        for row in fresh_rows {
            merged.insert(row.id.clone(), row);
        }
        cache::save_opencode_row_cache(&cache_key, &merged.into_values().collect::<Vec<_>>());
    }

    // Reprice every entry (cache hits carry cost 0 from `CachedEntry`; fresh
    // parses are repriced idempotently) so cost reflects current pricing/mode
    // without reparsing. Ledger-frozen entries are merged later and untouched.
    for entry in &mut entries {
        reprice(entry, mode, pricing);
    }

    Some(entries)
}

/// Which message tables a database actually has, `message` (V1) before
/// `session_message` (V2) so a duplicated id resolves to the V1 row first.
fn message_tables(connection: &sqlite::Connection) -> sqlite::Result<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name IN \
         ('message', 'session_message') \
         ORDER BY CASE name WHEN 'message' THEN 0 ELSE 1 END",
    )?;
    let mut tables = Vec::new();
    while let sqlite::State::Row = statement.next()? {
        if let Ok(name) = statement.read::<String, _>(0) {
            tables.push(name);
        }
    }
    Ok(tables)
}

/// Scan one message table with the per-row content-hash cache. Returns the
/// parsed entries, the fresh rows for the cache rebuild, and whether the scan
/// ran to completion (a mid-scan error must not clobber a good row cache).
#[allow(clippy::too_many_arguments)]
fn scan_message_table(
    connection: &sqlite::Connection,
    table: &str,
    db_path: &Path,
    cached: &HashMap<String, OpenCodeRow>,
    tz: Option<&JiffTimeZone>,
    mode: CostMode,
    pricing: Option<&PricingMap>,
    shared: &SharedArgs,
    window: DateWindow,
) -> (Vec<LoadedEntry>, Vec<OpenCodeRow>, bool) {
    // Push the window into SQL only while a sample of `time_created` still looks
    // millisecond-scaled. The sample cannot prove the whole column is, which is
    // why the payload check in the loop stays authoritative either way.
    let pushdown = if window.is_unbounded() || time_created_looks_like_millis(connection, table) {
        window.widened_for_pushdown()
    } else {
        debug_log(
            shared,
            format!(
                "OpenCode {table}.time_created is not millisecond-scale; scanning unfiltered: {}",
                db_path.display()
            ),
        );
        DateWindow::UNBOUNDED
    };
    let statement = prepare_message_query(connection, table, pushdown).or_else(|| {
        // A pre-SQLite-era schema has no `time_created` column, so the filtered
        // query cannot prepare. Scan unfiltered rather than return nothing.
        debug_log(
            shared,
            format!(
                "Failed to prepare filtered OpenCode query; scanning unfiltered: {}",
                db_path.display()
            ),
        );
        prepare_message_query(connection, table, DateWindow::UNBOUNDED)
    });
    let Some(mut statement) = statement else {
        debug_log(
            shared,
            format!("Failed to read OpenCode database: {}", db_path.display()),
        );
        return (Vec::new(), Vec::new(), false);
    };
    let mut completed = false;
    let mut entries = Vec::new();
    let mut fresh_rows = Vec::new();
    loop {
        match statement.next() {
            Ok(sqlite::State::Row) => {
                let Some((id, session_id, data)) = read_id_session_data(&statement) else {
                    continue;
                };
                if !window.is_unbounded()
                    && let Some(millis) = extract_message_timestamp(&data)
                    && !window.contains(millis)
                {
                    continue;
                }

                let mut hasher = crate::fast::FxHasher::default();
                data.hash(&mut hasher);
                let content_hash = hasher.finish();

                // Reuse the cached entry on exact (id, content_hash) match.
                if let Some(row) = cached.get(&id)
                    && row.content_hash == content_hash
                {
                    entries.push(LoadedEntry::from(row.entry.clone()));
                    fresh_rows.push(row.clone());
                    continue;
                }

                let msg = match serde_json::from_str::<OpenCodeMessage>(&data) {
                    Ok(msg) => msg,
                    Err(error) => {
                        debug_log(
                            shared,
                            format!(
                                "Failed to read OpenCode database message {}: {error}",
                                db_path.display()
                            ),
                        );
                        continue;
                    }
                };
                if let Some(entry) =
                    message_to_entry(&msg, Some(id.clone()), Some(session_id), tz, mode, pricing)
                {
                    fresh_rows.push(OpenCodeRow {
                        id,
                        content_hash,
                        entry: cache::CachedEntry::from(&entry),
                    });
                    entries.push(entry);
                }
            }
            Ok(sqlite::State::Done) => {
                completed = true;
                break;
            }
            Err(_) => {
                debug_log(
                    shared,
                    format!("Failed to query OpenCode database: {}", db_path.display()),
                );
                break;
            }
        }
    }

    (entries, fresh_rows, completed)
}

fn read_message_file(
    path: &Path,
    tz: Option<&JiffTimeZone>,
    mode: CostMode,
    pricing: Option<&PricingMap>,
    shared: &SharedArgs,
) -> Result<Vec<LoadedEntry>> {
    let content = match fs::read(path) {
        Ok(content) => content,
        Err(error) => {
            debug_log(
                shared,
                format!(
                    "Failed to read OpenCode message file {}: {error}",
                    path.display()
                ),
            );
            return Ok(Vec::new());
        }
    };
    let Ok(msg) = serde_json::from_slice::<OpenCodeMessage>(&content) else {
        return Ok(Vec::new());
    };
    Ok(message_to_entry(&msg, None, None, tz, mode, pricing)
        .into_iter()
        .collect())
}

fn entry_id(entry: &LoadedEntry) -> Option<&str> {
    entry.data.message.id.as_deref().filter(|id| !id.is_empty())
}

/// Pulls `time.created` millis from raw JSON to skip rows before a full parse.
///
/// Only the canonical `"time": { ... "created": <digits> ... }` shape is
/// recognized, and the search for `created` never leaves that object, so a
/// `time` object belonging to something else in the payload cannot contribute a
/// number. Everything else returns `None` and the caller falls back to a full
/// parse: a scan that gives up costs time, whereas a scan that guesses wrong
/// would silently drop an in-range entry.
fn extract_message_timestamp(data: &str) -> Option<i64> {
    const TIME_KEY: &str = "\"time\":";
    const CREATED_KEY: &str = "\"created\":";

    let time_object = data[data.find(TIME_KEY)? + TIME_KEY.len()..]
        .trim_start()
        .strip_prefix('{')?;
    let time_object = &time_object[..time_object.find('}')?];
    let after_key = time_object[time_object.find(CREATED_KEY)? + CREATED_KEY.len()..].trim_start();
    let end = after_key
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(after_key.len());
    after_key[..end].parse::<i64>().ok()
}

/// Half-open millisecond window derived from `--since`/`--until`, used to skip
/// rows and files before they are parsed.
///
/// A `None` bound is not narrowed, either because the option was absent or
/// because it is not a full date. The window is deliberately equivalent to the
/// authoritative `date_within_range` check applied to loaded entries: both
/// resolve the bounds in the reporting timezone, so pre-filtering can never drop
/// an entry the report would have kept.
#[derive(Clone, Copy, PartialEq, Eq)]
struct DateWindow {
    start: Option<i64>,
    end: Option<i64>,
}

impl DateWindow {
    const UNBOUNDED: Self = Self {
        start: None,
        end: None,
    };

    fn from_shared(shared: &SharedArgs, tz: Option<&JiffTimeZone>) -> Self {
        let (start, end) =
            date_range_bounds_ms(shared.since.as_deref(), shared.until.as_deref(), tz);
        Self { start, end }
    }

    fn is_unbounded(self) -> bool {
        self == Self::UNBOUNDED
    }

    /// Same window, widened by a day on each side for the SQL push-down.
    ///
    /// SQL filters on `message.time_created` while the report filters on the
    /// payload's `time.created`. They hold the same value on every OpenCode
    /// build checked here, but the column is only a proxy for the payload, so
    /// the pushed-down window is kept loose: a column drifting from its payload
    /// by up to a day costs a few extra rows to scan instead of excluding a row
    /// the report wanted. Drift past a day is still excluded — ruling that out
    /// would take the full-column scan this push-down exists to avoid. The exact
    /// window is applied per row either way.
    fn widened_for_pushdown(self) -> Self {
        Self {
            start: self.start.map(|start| start - crate::MILLIS_PER_DAY),
            end: self.end.map(|end| end + crate::MILLIS_PER_DAY),
        }
    }

    fn contains(self, millis: i64) -> bool {
        self.start.is_none_or(|start| millis >= start) && self.end.is_none_or(|end| millis < end)
    }
}

/// Prepares the `table` scan, narrowed to `window` where its bounds are set.
///
/// Returns `None` when the statement cannot be prepared, which is what a schema
/// without a `time_created` column looks like.
///
/// The bounds are applied through a subquery that selects only `id`. OpenCode's
/// index is `(session_id, time_created, id)`, so a bare `time_created` range
/// cannot seek it and scans the table, reading every `data` blob on the way. The
/// subquery is answered from that index alone, leaving only in-range rows to be
/// fetched by primary key — the difference is what keeps a narrow window off the
/// gigabytes of payload it does not need.
fn prepare_message_query<'c>(
    connection: &'c sqlite::Connection,
    table: &str,
    window: DateWindow,
) -> Option<sqlite::Statement<'c>> {
    let sql = match (window.start, window.end) {
        (Some(_), Some(_)) => format!(
            "SELECT id, session_id, data FROM {table} WHERE id IN \
             (SELECT id FROM {table} WHERE time_created >= ?1 AND time_created < ?2)"
        ),
        (Some(_), None) => format!(
            "SELECT id, session_id, data FROM {table} WHERE id IN \
             (SELECT id FROM {table} WHERE time_created >= ?1)"
        ),
        (None, Some(_)) => format!(
            "SELECT id, session_id, data FROM {table} WHERE id IN \
             (SELECT id FROM {table} WHERE time_created < ?1)"
        ),
        (None, None) => format!("SELECT id, session_id, data FROM {table}"),
    };
    let mut statement = connection.prepare(sql).ok()?;
    for (index, bound) in [window.start, window.end].into_iter().flatten().enumerate() {
        statement.bind((index + 1, bound)).ok()?;
    }
    Some(statement)
}

/// Smallest value treated as millisecond scale: any Unix timestamp after 1973
/// needs at least 12 digits, while second-scale values stay far below it.
const MIN_MILLIS_SCALE: i64 = 100_000_000_000;

/// Reports whether a sample of `<table>.time_created` looks like Unix
/// milliseconds, the scale the payload's `time.created` uses.
///
/// Sampling keeps this cheap on databases tens of gigabytes in size, and proves
/// nothing about the rows it did not read: a column with mixed scales can still
/// slip through, and matching scales say nothing about matching values. What it
/// does catch is a build that stored seconds, or left the column at zero, where
/// millisecond bounds would otherwise exclude every row — those disable the
/// push-down and leave the payload check to filter.
fn time_created_looks_like_millis(connection: &sqlite::Connection, table: &str) -> bool {
    let Ok(mut statement) = connection.prepare(format!(
        "SELECT max(time_created) FROM (SELECT time_created FROM {table} LIMIT 8)"
    )) else {
        return false;
    };
    match statement.next() {
        Ok(sqlite::State::Row) => statement
            .read::<i64, _>(0)
            .is_ok_and(|max| max >= MIN_MILLIS_SCALE),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::load_entries_from_directory;
    use crate::cli::{CostMode, SharedArgs};
    use ccusage_test_support::{CacheEnv, fs_fixture};

    // Unbounded window: the cache and ledger tests are about what survives a
    // reload, so a date filter would only mask which rows came back.
    fn display_shared() -> SharedArgs {
        SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        }
    }

    // Mirrors the real OpenCode schema, where `time_created` repeats the
    // payload's `time.created`, so tests exercise the range push-down.
    /// Rows a plain scan can still read, for tests that corrupt a DB and need to
    /// prove the live path actually broke rather than assuming it did.
    fn raw_message_count(path: &Path) -> usize {
        let Ok(db) = sqlite::open(path) else { return 0 };
        let Ok(mut statement) = db.prepare("SELECT id FROM message") else {
            return 0;
        };
        let mut rows = 0;
        while let Ok(sqlite::State::Row) = statement.next() {
            rows += 1;
        }
        rows
    }

    fn create_db_message(path: &Path, id: &str, session_id: &str, data: &str) {
        let created = serde_json::from_str::<serde_json::Value>(data)
            .ok()
            .and_then(|value| value["time"]["created"].as_i64())
            .expect("test message payload needs time.created");
        create_db_message_with_time(path, id, session_id, created, data);
    }

    // Schema with `time_created` set independently of the payload, for the cases
    // where the column and the payload have to disagree.
    fn create_db_message_with_time(
        path: &Path,
        id: &str,
        session_id: &str,
        time_created_ms: i64,
        data: &str,
    ) {
        let db = sqlite::open(path).unwrap();
        db.execute(
            "CREATE TABLE IF NOT EXISTS message \
             (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER NOT NULL DEFAULT 0, data TEXT)",
        )
        .unwrap();
        let mut statement = db
            .prepare(
                "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4)",
            )
            .unwrap();
        statement.bind((1, id)).unwrap();
        statement.bind((2, session_id)).unwrap();
        statement.bind((3, time_created_ms)).unwrap();
        statement.bind((4, data)).unwrap();
        statement.next().unwrap();
    }

    // Pre-SQLite-era layout: no `time_created` column, so the filtered query
    // cannot prepare and the loader has to fall back to an unfiltered scan.
    fn create_db_message_legacy_schema(path: &Path, id: &str, session_id: &str, data: &str) {
        let db = sqlite::open(path).unwrap();
        db.execute("CREATE TABLE IF NOT EXISTS message (id TEXT, session_id TEXT, data TEXT)")
            .unwrap();
        let mut statement = db
            .prepare("INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)")
            .unwrap();
        statement.bind((1, id)).unwrap();
        statement.bind((2, session_id)).unwrap();
        statement.bind((3, data)).unwrap();
        statement.next().unwrap();
    }

    /// Real V2 (beta) schema: messages live in `session_message` with `type` /
    /// `seq` columns and V2-shaped payloads (nested `model`), while `message`
    /// either does not exist or stays empty.
    fn create_db_session_message(path: &Path, id: &str, session_id: &str, data: &str) {
        let created = serde_json::from_str::<serde_json::Value>(data)
            .ok()
            .and_then(|value| value["time"]["created"].as_i64())
            .expect("test message payload needs time.created");
        let db = sqlite::open(path).unwrap();
        db.execute(
            "CREATE TABLE IF NOT EXISTS session_message \
             (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER NOT NULL, \
              time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL)",
        )
        .unwrap();
        let mut statement = db
            .prepare(
                "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .unwrap();
        statement.bind((1, id)).unwrap();
        statement.bind((2, session_id)).unwrap();
        statement.bind((3, "assistant")).unwrap();
        statement.bind((4, 1_i64)).unwrap();
        statement.bind((5, created)).unwrap();
        statement.bind((6, created)).unwrap();
        statement.bind((7, data)).unwrap();
        statement.next().unwrap();
    }

    #[test]
    fn loads_v2_session_message_table() {
        let _cache_env = CacheEnv::new("opencode-loads-v2-session-message-table");
        let fixture = fs_fixture!({});
        create_db_session_message(
            &fixture.path("opencode.db"),
            "msg-v2-1",
            "ses-v2-a",
            r#"{"time":{"created":1767312000000,"completed":1767312001000},"agent":"build","model":{"id":"deepseek-v4-flash-free","providerID":"opencode","variant":"max"},"cost":0,"tokens":{"input":120,"output":60,"cache":{"read":12,"write":24}}}"#,
        );

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].date, "2026-01-02");
        assert_eq!(entries[0].session_id.as_ref(), "ses-v2-a");
        assert_eq!(entries[0].data.message.id.as_deref(), Some("msg-v2-1"));
        assert_eq!(entries[0].model.as_deref(), Some("deepseek-v4-flash-free"));
        assert_eq!(entries[0].data.message.usage.input_tokens, 120);
        assert_eq!(entries[0].data.message.usage.output_tokens, 60);
        assert_eq!(
            entries[0].data.message.usage.cache_creation_input_tokens,
            24
        );
        assert_eq!(entries[0].data.message.usage.cache_read_input_tokens, 12);
    }

    #[test]
    fn loads_v2_channel_database() {
        let _cache_env = CacheEnv::new("opencode-loads-v2-channel-database");
        let fixture = fs_fixture!({});
        create_db_session_message(
            &fixture.path("opencode-next.db"),
            "msg-v2-1",
            "ses-v2-a",
            r#"{"time":{"created":1767312000000},"model":{"id":"mimo-v2.5-free","providerID":"opencode"},"tokens":{"input":80,"output":40}}"#,
        );

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].model.as_deref(), Some("mimo-v2.5-free"));
        assert_eq!(entries[0].data.message.usage.input_tokens, 80);
    }

    #[test]
    fn loads_v1_and_v2_tables_with_dedup() {
        let _cache_env = CacheEnv::new("opencode-loads-v1-and-v2-tables-with-dedup");
        // V1 row wins for a duplicated id; V2-only rows are still loaded.
        let fixture = fs_fixture!({});
        let db = fixture.path("opencode.db");
        create_db_message(
            &db,
            "msg-shared",
            "ses-v1",
            r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":111,"output":1},"cost":0.03}"#,
        );
        create_db_session_message(
            &db,
            "msg-shared",
            "ses-v2",
            r#"{"time":{"created":1767312000000},"model":{"id":"mimo-v2.5-free","providerID":"opencode"},"tokens":{"input":999,"output":9}}"#,
        );
        create_db_session_message(
            &db,
            "msg-v2-only",
            "ses-v2-b",
            r#"{"time":{"created":1767312000001},"model":{"id":"mimo-v2.5-free","providerID":"opencode"},"tokens":{"input":222,"output":2}}"#,
        );

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();

        assert_eq!(entries.len(), 2);
        let shared_entry = entries
            .iter()
            .find(|e| e.data.message.id.as_deref() == Some("msg-shared"))
            .expect("shared id present");
        assert_eq!(shared_entry.session_id.as_ref(), "ses-v1");
        assert_eq!(shared_entry.data.message.usage.input_tokens, 111);
        let v2_only = entries
            .iter()
            .find(|e| e.data.message.id.as_deref() == Some("msg-v2-only"))
            .expect("v2-only row present");
        assert_eq!(v2_only.session_id.as_ref(), "ses-v2-b");
        assert_eq!(v2_only.data.message.usage.input_tokens, 222);
    }

    #[test]
    fn loads_message_json_files() {
        let _cache_env = CacheEnv::new("opencode-loads-message-json-files");
        let fixture = fs_fixture!({
            "storage/message/message.json": r#"{"id":"msg-1","sessionID":"session-a","providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":100,"output":50,"cache":{"read":10,"write":20}},"cost":0.02}"#,
        });

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].date, "2026-01-02");
        assert_eq!(entries[0].session_id.as_ref(), "session-a");
        assert_eq!(
            entries[0].model.as_deref(),
            Some("claude-sonnet-4-20250514")
        );
        assert_eq!(entries[0].data.message.usage.input_tokens, 100);
        assert_eq!(entries[0].data.message.usage.output_tokens, 50);
        assert_eq!(
            entries[0].data.message.usage.cache_creation_input_tokens,
            20
        );
        assert_eq!(entries[0].data.message.usage.cache_read_input_tokens, 10);
        assert_eq!(entries[0].cost, 0.02);
    }

    #[test]
    fn loads_messages_from_sqlite_database() {
        let _cache_env = CacheEnv::new("opencode-loads-messages-from-sqlite-database");
        let fixture = fs_fixture!({});
        create_db_message(
            &fixture.path("opencode.db"),
            "db-msg-1",
            "db-session-a",
            r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":120,"output":60,"cache":{"read":12,"write":24}},"cost":0.03}"#,
        );

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].date, "2026-01-02");
        assert_eq!(entries[0].session_id.as_ref(), "db-session-a");
        assert_eq!(entries[0].data.message.id.as_deref(), Some("db-msg-1"));
        assert_eq!(entries[0].data.message.usage.input_tokens, 120);
        assert_eq!(entries[0].data.message.usage.output_tokens, 60);
        assert_eq!(
            entries[0].data.message.usage.cache_creation_input_tokens,
            24
        );
        assert_eq!(entries[0].data.message.usage.cache_read_input_tokens, 12);
        assert_eq!(entries[0].cost, 0.03);
    }

    #[test]
    fn loads_channel_sqlite_database() {
        let _cache_env = CacheEnv::new("opencode-loads-channel-sqlite-database");
        let fixture = fs_fixture!({});
        create_db_message(
            &fixture.path("opencode-beta.db"),
            "channel-msg-1",
            "channel-session-a",
            r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":80,"output":40}}"#,
        );

        let entries = load_entries_from_directory(fixture.root(), &SharedArgs::default()).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id.as_ref(), "channel-session-a");
        assert_eq!(entries[0].data.message.usage.input_tokens, 80);
    }

    #[test]
    fn prefers_database_messages_over_duplicate_json_files() {
        let _cache_env =
            CacheEnv::new("opencode-prefers-database-messages-over-duplicate-json-files");
        let fixture = fs_fixture!({
            "storage/message/message.json": r#"{"id":"msg-1","sessionID":"json-session-a","providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":999,"output":999},"cost":0.99}"#,
        });
        create_db_message(
            &fixture.path("opencode.db"),
            "msg-1",
            "db-session-a",
            r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":120,"output":60},"cost":0.03}"#,
        );

        let shared = SharedArgs {
            mode: CostMode::Display,
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id.as_ref(), "db-session-a");
        assert_eq!(entries[0].data.message.usage.input_tokens, 120);
        assert_eq!(entries[0].cost, 0.03);
    }

    #[test]
    fn skips_message_files_already_covered_by_database() {
        let _cache_env = CacheEnv::new("opencode-skips-message-files-already-covered-by-database");
        // Real OpenCode message files live at
        // `storage/message/<sessionID>/<messageID>.json`, so the file stem is
        // the message id. The DB pass contributes `msg-db`, so the matching
        // file must be dropped (DB wins) while the file that the DB does not
        // cover is still loaded.
        let fixture = fs_fixture!({
            "storage/message/ses_a/msg-db.json": r#"{"id":"msg-db","sessionID":"json-session","providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":999,"output":999},"cost":0.99}"#,
            "storage/message/ses_a/msg-file.json": r#"{"id":"msg-file","sessionID":"file-session","providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000001},"tokens":{"input":50,"output":25},"cost":0.01}"#,
        });
        create_db_message(
            &fixture.path("opencode.db"),
            "msg-db",
            "db-session",
            r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":120,"output":60},"cost":0.03}"#,
        );

        let shared = SharedArgs {
            mode: CostMode::Display,
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();

        assert_eq!(entries.len(), 2);
        // The DB-covered id keeps the DB row, not the file's inflated tokens.
        let db_entry = entries
            .iter()
            .find(|entry| entry.data.message.id.as_deref() == Some("msg-db"))
            .expect("db-covered message present");
        assert_eq!(db_entry.session_id.as_ref(), "db-session");
        assert_eq!(db_entry.data.message.usage.input_tokens, 120);
        // The file the DB does not cover is still read and parsed.
        let file_entry = entries
            .iter()
            .find(|entry| entry.data.message.id.as_deref() == Some("msg-file"))
            .expect("db-uncovered message present");
        assert_eq!(file_entry.session_id.as_ref(), "file-session");
        assert_eq!(file_entry.data.message.usage.input_tokens, 50);
    }

    #[test]
    fn dedup_is_stable_across_thread_counts() {
        let _cache_env = CacheEnv::new("opencode-dedup-is-stable-across-thread-counts");
        // Build a directory with many files spread over several sessions, some
        // sharing ids with each other and with the DB, so the file pass has to
        // dedup. Parallel reads must not change which duplicate survives or the
        // final ordering compared to the single-threaded read.
        let fixture = ccusage_test_support::Fixture::new();
        for session in 0..4 {
            for message in 0..15 {
                let id = format!("msg-{session}-{message}");
                let created = 1_767_312_000_000_i64 + i64::from(session * 100 + message);
                let path = format!("storage/message/ses_{session}/{id}.json");
                let data = format!(
                    r#"{{"id":"{id}","sessionID":"ses_{session}","providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{{"created":{created}}},"tokens":{{"input":{input},"output":10}}}}"#,
                    input = 100 + message,
                );
                let _ = fixture.write_file(path, data);
            }
        }
        // A duplicate file (same id, later timestamp) to force the file-vs-file
        // dedup path under both thread counts.
        let _ = fixture.write_file(
            "storage/message/ses_dup/msg-0-0.json",
            r#"{"id":"msg-0-0","sessionID":"ses_dup","providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312999999},"tokens":{"input":7777,"output":10}}"#,
        );

        create_db_message(
            &fixture.path("opencode.db"),
            "msg-1-1",
            "db-session",
            r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":120,"output":60}}"#,
        );

        let single = SharedArgs {
            mode: CostMode::Display,
            single_thread: true,
            ..SharedArgs::default()
        };
        let multi = SharedArgs {
            mode: CostMode::Display,
            single_thread: false,
            ..SharedArgs::default()
        };

        let single_entries = load_entries_from_directory(fixture.root(), &single).unwrap();
        let multi_entries = load_entries_from_directory(fixture.root(), &multi).unwrap();

        let project = |entries: &[crate::LoadedEntry]| {
            entries
                .iter()
                .map(|entry| {
                    (
                        entry.timestamp.as_millis(),
                        entry.data.message.id.clone(),
                        entry.session_id.to_string(),
                        entry.data.message.usage.input_tokens,
                    )
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(project(&single_entries), project(&multi_entries));
    }

    #[test]
    fn since_filter_drops_db_rows_older_than_lower_bound() {
        let _cache_env =
            CacheEnv::new("opencode-since-filter-drops-db-rows-older-than-lower-bound");
        let fixture = fs_fixture!({});
        // 2025-12-31 00:00 UTC
        create_db_message_with_time(
            &fixture.path("opencode.db"),
            "msg-old",
            "session-old",
            1_767_139_200_000,
            r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767139200000},"tokens":{"input":1,"output":1}}"#,
        );
        // 2026-01-04 00:00 UTC, in range for since=20260103
        create_db_message_with_time(
            &fixture.path("opencode.db"),
            "msg-new",
            "session-new",
            1_767_484_800_000,
            r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767484800000},"tokens":{"input":2,"output":2}}"#,
        );

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            since: Some("20260103".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.message.id.as_deref(), Some("msg-new"));
    }

    #[test]
    fn until_filter_drops_db_rows_at_or_after_upper_bound() {
        let _cache_env =
            CacheEnv::new("opencode-until-filter-drops-db-rows-at-or-after-upper-bound");
        let fixture = fs_fixture!({});
        // 2026-01-02 00:00 UTC, in range for until=20260105
        create_db_message_with_time(
            &fixture.path("opencode.db"),
            "msg-early",
            "session-early",
            1_767_312_000_000,
            r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":1,"output":1}}"#,
        );
        // 2026-01-11 00:00 UTC, out of range for until=20260105
        create_db_message_with_time(
            &fixture.path("opencode.db"),
            "msg-late",
            "session-late",
            1_768_089_600_000,
            r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1768089600000},"tokens":{"input":2,"output":2}}"#,
        );

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            until: Some("20260105".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.message.id.as_deref(), Some("msg-early"));
    }

    #[test]
    fn legacy_schema_without_time_created_still_returns_in_range_rows() {
        let _cache_env = CacheEnv::new(
            "opencode-legacy-schema-without-time-created-still-returns-in-range-rows",
        );
        let fixture = fs_fixture!({});
        create_db_message_legacy_schema(
            &fixture.path("opencode.db"),
            "msg-in-range",
            "session-a",
            // payload date 2026-01-05, inside the requested window
            r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767571200000},"tokens":{"input":3,"output":3}}"#,
        );

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            since: Some("20260103".to_string()),
            until: Some("20260107".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.message.id.as_deref(), Some("msg-in-range"));
    }

    #[test]
    fn legacy_schema_without_time_created_still_drops_out_of_range_rows() {
        let _cache_env = CacheEnv::new(
            "opencode-legacy-schema-without-time-created-still-drops-out-of-range-rows",
        );
        let fixture = fs_fixture!({});
        create_db_message_legacy_schema(
            &fixture.path("opencode.db"),
            "msg-out-of-range",
            "session-a",
            // payload date 2026-01-02, before since=20260103
            r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":3,"output":3}}"#,
        );

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            since: Some("20260103".to_string()),
            until: Some("20260107".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();

        assert!(
            entries.is_empty(),
            "fallback scan must still exclude out-of-range rows via the in-loop check"
        );
    }

    #[test]
    fn filters_json_file_entries_by_until() {
        let _cache_env = CacheEnv::new("opencode-filters-json-file-entries-by-until");
        let fixture = fs_fixture!({
            "storage/message/message.json": r#"{"id":"msg-1","sessionID":"session-a","providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":100,"output":50},"cost":0.02}"#,
        });

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            until: Some("20260101".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();
        assert!(
            entries.is_empty(),
            "message on 2026-01-02 should be excluded by until=20260101"
        );
    }

    #[test]
    fn filters_json_file_entries_by_since() {
        let _cache_env = CacheEnv::new("opencode-filters-json-file-entries-by-since");
        let fixture = fs_fixture!({
            "storage/message/message.json": r#"{"id":"msg-1","sessionID":"session-a","providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":100,"output":50},"cost":0.02}"#,
        });

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            since: Some("20260103".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();
        assert!(
            entries.is_empty(),
            "message on 2026-01-02 should be excluded by since=20260103"
        );
    }

    #[test]
    fn includes_entries_when_since_until_bracket_date() {
        let _cache_env = CacheEnv::new("opencode-includes-entries-when-since-until-bracket-date");
        let fixture = fs_fixture!({
            "storage/message/message.json": r#"{"id":"msg-1","sessionID":"session-a","providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":100,"output":50},"cost":0.02}"#,
        });

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            since: Some("20260101".to_string()),
            until: Some("20260103".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();
        assert_eq!(
            entries.len(),
            1,
            "message on 2026-01-02 should be included when since=20260101 and until=20260103"
        );
    }

    #[test]
    fn includes_entries_when_since_exact_match() {
        let _cache_env = CacheEnv::new("opencode-includes-entries-when-since-exact-match");
        let fixture = fs_fixture!({
            "storage/message/message.json": r#"{"id":"msg-1","sessionID":"session-a","providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":100,"output":50},"cost":0.02}"#,
        });

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            since: Some("20260102".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();
        assert_eq!(
            entries.len(),
            1,
            "message on 2026-01-02 should be included when since=20260102"
        );
    }

    #[test]
    fn includes_entries_when_until_exact_match() {
        let _cache_env = CacheEnv::new("opencode-includes-entries-when-until-exact-match");
        let fixture = fs_fixture!({
            "storage/message/message.json": r#"{"id":"msg-1","sessionID":"session-a","providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":100,"output":50},"cost":0.02}"#,
        });

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            until: Some("20260102".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();
        assert_eq!(
            entries.len(),
            1,
            "message on 2026-01-02 should be included when until=20260102"
        );
    }

    // Real OpenCode message files are pretty-printed (newlines + indentation),
    // unlike the minified fixtures above. Pins `extract_message_timestamp`
    // against the real on-disk shape.
    const PRETTY_PRINTED_MESSAGE: &str = r#"{
  "id": "msg-pretty",
  "sessionID": "session-a",
  "role": "assistant",
  "providerID": "anthropic",
  "modelID": "claude-sonnet-4-20250514",
  "time": {
    "created": 1767312000000,
    "completed": 1767312001000
  },
  "tokens": {
    "input": 100,
    "output": 50
  },
  "cost": 0.02
}"#;

    #[test]
    fn extracts_timestamp_from_pretty_printed_file_when_out_of_range() {
        let _cache_env =
            CacheEnv::new("opencode-extracts-timestamp-from-pretty-printed-file-when-out-of-range");
        let fixture = fs_fixture!({
            "storage/message/message.json": PRETTY_PRINTED_MESSAGE,
        });

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            until: Some("20260101".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();
        assert!(
            entries.is_empty(),
            "pretty-printed message on 2026-01-02 must be excluded by until=20260101"
        );
    }

    #[test]
    fn extracts_timestamp_from_pretty_printed_file_when_in_range() {
        let _cache_env =
            CacheEnv::new("opencode-extracts-timestamp-from-pretty-printed-file-when-in-range");
        let fixture = fs_fixture!({
            "storage/message/message.json": PRETTY_PRINTED_MESSAGE,
        });

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            since: Some("20260101".to_string()),
            until: Some("20260103".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.message.id.as_deref(), Some("msg-pretty"));
    }

    // 2026-01-01 12:00 UTC, which is 2026-01-02 in UTC+14 and therefore only
    // kept when the lower bound is resolved in the reporting timezone.
    const NOON_UTC_2026_01_01: &str = r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767268800000},"tokens":{"input":1,"output":1}}"#;

    // 2026-01-02 06:00 UTC, which is still 2026-01-01 in UTC-12.
    const EARLY_UTC_2026_01_02: &str = r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767333600000},"tokens":{"input":1,"output":1}}"#;

    #[test]
    fn since_bound_follows_local_midnight_in_the_reporting_timezone() {
        let _cache_env =
            CacheEnv::new("opencode-since-bound-follows-local-midnight-in-the-reporting-timezone");
        let fixture = fs_fixture!({});
        create_db_message(
            &fixture.path("opencode.db"),
            "msg-utc-plus-14",
            "session-a",
            NOON_UTC_2026_01_01,
        );

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("Pacific/Kiritimati".to_string()),
            since: Some("20260102".to_string()),
            until: Some("20260102".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();

        assert_eq!(
            entries.len(),
            1,
            "a UTC+14 local date of 2026-01-02 must survive since=until=20260102"
        );
        assert_eq!(entries[0].date, "2026-01-02");
    }

    #[test]
    fn until_bound_follows_local_midnight_in_the_reporting_timezone() {
        let _cache_env =
            CacheEnv::new("opencode-until-bound-follows-local-midnight-in-the-reporting-timezone");
        let fixture = fs_fixture!({});
        create_db_message(
            &fixture.path("opencode.db"),
            "msg-utc-minus-12",
            "session-a",
            EARLY_UTC_2026_01_02,
        );

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("Etc/GMT+12".to_string()),
            until: Some("20260101".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();

        assert_eq!(
            entries.len(),
            1,
            "a UTC-12 local date of 2026-01-01 must survive until=20260101"
        );
        assert_eq!(entries[0].date, "2026-01-01");
    }

    #[test]
    fn pushdown_margin_keeps_rows_whose_column_drifts_from_the_payload() {
        let _cache_env = CacheEnv::new(
            "opencode-pushdown-margin-keeps-rows-whose-column-drifts-from-the-payload",
        );
        let fixture = fs_fixture!({});
        // Payload lands on 2026-01-02, but the column sits 26 hours later, which
        // an exact SQL window would push past its upper bound.
        create_db_message_with_time(
            &fixture.path("opencode.db"),
            "msg-drifted",
            "session-a",
            1_767_312_000_000 + 26 * 60 * 60 * 1000,
            r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":1,"output":1}}"#,
        );

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            since: Some("20260102".to_string()),
            until: Some("20260102".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();

        assert_eq!(
            entries.len(),
            1,
            "the payload decides the window, so a drifting column must not exclude the row"
        );
        assert_eq!(entries[0].date, "2026-01-02");
    }

    #[test]
    fn second_scale_time_created_disables_the_range_pushdown() {
        let _cache_env =
            CacheEnv::new("opencode-second-scale-time-created-disables-the-range-pushdown");
        let fixture = fs_fixture!({});
        // The payload is on 2026-01-02, but the column holds seconds. Comparing
        // it against millisecond bounds would exclude the row outright.
        create_db_message_with_time(
            &fixture.path("opencode.db"),
            "msg-seconds",
            "session-a",
            1_767_312_000,
            r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":1,"output":1}}"#,
        );

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            since: Some("20260101".to_string()),
            until: Some("20260103".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();

        assert_eq!(
            entries.len(),
            1,
            "an unrecognized time_created scale must fall back to scanning, not drop rows"
        );
        assert_eq!(entries[0].data.message.id.as_deref(), Some("msg-seconds"));
    }

    #[test]
    fn non_ascii_date_bounds_leave_filtering_to_the_report() {
        let _cache_env =
            CacheEnv::new("opencode-non-ascii-date-bounds-leave-filtering-to-the-report");
        let fixture = fs_fixture!({});
        create_db_message(
            &fixture.path("opencode.db"),
            "msg-1",
            "session-a",
            r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":1,"output":1}}"#,
        );

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            // Multi-byte bound: 8 bytes long, but not 8 ASCII digits.
            since: Some("abあcde".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries_from_directory(fixture.root(), &shared).unwrap();

        assert_eq!(
            entries.len(),
            1,
            "a bound with no instant must not pre-filter; the report's string filter decides"
        );
        assert!(!crate::date_within_range(
            &entries[0].date,
            shared.since.as_deref(),
            None
        ));
    }

    #[test]
    fn detects_sources_regardless_of_the_date_window() {
        let _cache_env = CacheEnv::new("opencode-detects-sources-regardless-of-the-date-window");
        let db_only = fs_fixture!({});
        create_db_message(
            &db_only.path("opencode.db"),
            "msg-1",
            "session-a",
            r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":1,"output":1}}"#,
        );
        assert!(super::has_source(db_only.root()));

        let files_only = fs_fixture!({
            "storage/message/session-a/msg-1.json": r#"{"id":"msg-1"}"#,
        });
        assert!(super::has_source(files_only.root()));

        let empty = fs_fixture!({});
        assert!(!super::has_source(empty.root()));

        // A window that excludes every entry must not make the source vanish;
        // that is what keeps OpenCode in the aggregate report's detected list.
        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            since: Some("20200101".to_string()),
            until: Some("20200102".to_string()),
            ..SharedArgs::default()
        };
        assert!(
            load_entries_from_directory(db_only.root(), &shared)
                .unwrap()
                .is_empty()
        );
        assert!(super::has_source(db_only.root()));
    }

    #[test]
    fn extracts_timestamp_from_minified_and_pretty_printed_payloads() {
        assert_eq!(
            super::extract_message_timestamp(r#"{"time":{"created":1767312000000}}"#),
            Some(1_767_312_000_000)
        );
        assert_eq!(
            super::extract_message_timestamp(PRETTY_PRINTED_MESSAGE),
            Some(1_767_312_000_000)
        );
        // Digits running to the end of the object, without a trailing comma.
        assert_eq!(
            super::extract_message_timestamp(r#"{"time":{"created": 42}}"#),
            Some(42)
        );
    }

    #[test]
    fn ignores_a_time_object_that_is_not_the_messages_own() {
        // The scan must not reach past the first `time` object for a `created`
        // key: guessing wrong here would drop an in-range message, while giving
        // up only costs a full parse.
        assert_eq!(
            super::extract_message_timestamp(
                r#"{"parts":[{"time":{"start":1}}],"time":{"created":1767312000000}}"#
            ),
            None
        );
        // A quoted `"time"` that is a value rather than a key is not followed by
        // a colon, so it cannot start a match either.
        assert_eq!(
            super::extract_message_timestamp(r#"{"unit":"time","created":1767312000000}"#),
            None
        );
    }

    #[test]
    fn declines_to_extract_timestamps_it_cannot_trust() {
        // Quoted numbers, negative values and missing keys all fail open so the
        // full parse decides.
        assert_eq!(
            super::extract_message_timestamp(r#"{"time":{"created":"1767312000000"}}"#),
            None
        );
        assert_eq!(
            super::extract_message_timestamp(r#"{"time":{"created":-5}}"#),
            None
        );
        assert_eq!(
            super::extract_message_timestamp(r#"{"time":{"completed":1767312000000}}"#),
            None
        );
        assert_eq!(super::extract_message_timestamp(r#"{"id":"msg-1"}"#), None);
    }

    #[test]
    fn serves_unchanged_row_from_cache_on_second_load() {
        let env = CacheEnv::new("opencode-row-cache-hit");
        let fixture = fs_fixture!({});
        let db = fixture.path("opencode.db");
        create_db_message(
            &db,
            "msg-1",
            "session-a",
            r#"{"providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":100,"output":50},"cost":0.02}"#,
        );

        let cold = load_entries_from_directory(fixture.root(), &display_shared()).unwrap();
        assert!(
            env.dir().join("ccusage").join("cache.db").exists(),
            "row cache should be written to the cache database"
        );
        // Assert the row cache itself, not just that some cache file appeared:
        // the ledger writes the same cache.db, and cold == warm holds trivially
        // on a plain reparse, so neither one can detect a cache miss.
        let cached = super::cache::load_opencode_row_cache(&super::cache::cache_key(&db))
            .expect("row cache must hold the scanned row");
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].id, "msg-1");

        let warm = load_entries_from_directory(fixture.root(), &display_shared()).unwrap();
        assert_eq!(cold.len(), warm.len());
        assert_eq!(cold[0].cost, warm[0].cost);
        assert_eq!(
            cold[0].data.message.usage.input_tokens,
            warm[0].data.message.usage.input_tokens
        );
    }

    #[test]
    fn changed_row_content_invalidates_cache() {
        let _env = CacheEnv::new("opencode-row-content-change");
        let fixture = fs_fixture!({});
        let db = fixture.path("opencode.db");
        create_db_message(
            &db,
            "msg-1",
            "session-a",
            r#"{"providerID":"anthropic","modelID":"m","time":{"created":1767312000000},"tokens":{"input":100}}"#,
        );

        let cold = load_entries_from_directory(fixture.root(), &display_shared()).unwrap();
        assert_eq!(cold[0].data.message.usage.input_tokens, 100);

        // Rewrite the row's `data` in place (same id). Only the content hash
        // catches this; the old `(id, time_updated)` key would serve a stale 100.
        let conn = sqlite::open(&db).unwrap();
        conn.execute(
            r#"UPDATE message SET data = '{"providerID":"anthropic","modelID":"m","time":{"created":1767312000000},"tokens":{"input":999}}' WHERE id = 'msg-1'"#,
        )
        .unwrap();
        drop(conn);

        let warm = load_entries_from_directory(fixture.root(), &display_shared()).unwrap();
        assert_eq!(
            warm[0].data.message.usage.input_tokens, 999,
            "changed row content must invalidate the cache and reparse"
        );
    }

    #[test]
    fn deleted_database_retains_spend_via_ledger() {
        let _env = CacheEnv::new("opencode-db-retain");
        let fixture = fs_fixture!({});
        let db = fixture.path("opencode.db");
        create_db_message(
            &db,
            "msg-1",
            "session-a",
            r#"{"providerID":"anthropic","modelID":"m","time":{"created":1767312000000},"tokens":{"input":100}}"#,
        );

        // Cold load records the DB row's spend in the ledger.
        let cold = load_entries_from_directory(fixture.root(), &display_shared()).unwrap();
        assert_eq!(cold.len(), 1);
        assert_eq!(cold[0].data.message.usage.input_tokens, 100);

        // Delete the entire database file — simulates a removed opencode store.
        fs::remove_file(&db).unwrap();

        // The spend must still be reported, re-emitted from the ledger.
        let warm = load_entries_from_directory(fixture.root(), &display_shared()).unwrap();
        assert_eq!(
            warm.len(),
            1,
            "deleted DB spend must be retained via the ledger"
        );
        assert_eq!(warm[0].data.message.usage.input_tokens, 100);
    }

    #[test]
    fn deleted_row_spend_retained_via_ledger() {
        let _env = CacheEnv::new("opencode-row-cache-delete");
        let fixture = fs_fixture!({});
        let db = fixture.path("opencode.db");
        create_db_message(
            &db,
            "msg-1",
            "session-a",
            r#"{"providerID":"anthropic","modelID":"m","time":{"created":1767312000000},"tokens":{"input":100}}"#,
        );
        create_db_message(
            &db,
            "msg-2",
            "session-a",
            r#"{"providerID":"anthropic","modelID":"m","time":{"created":1767312000000},"tokens":{"input":200}}"#,
        );

        let first = load_entries_from_directory(fixture.root(), &display_shared()).unwrap();
        assert_eq!(first.len(), 2);

        // Delete one row, then reload: its spend must persist via the ledger so
        // a removed chat never erases the tokens/cost already incurred.
        let conn = sqlite::open(&db).unwrap();
        conn.execute("DELETE FROM message WHERE id = 'msg-2'")
            .unwrap();
        drop(conn);

        let second = load_entries_from_directory(fixture.root(), &display_shared()).unwrap();
        assert_eq!(second.len(), 2, "deleted row's spend must be retained");
        let deleted = second
            .iter()
            .find(|e| e.data.message.id.as_deref() == Some("msg-2"))
            .expect("deleted row retained from ledger");
        assert_eq!(deleted.data.message.usage.input_tokens, 200);
    }

    #[test]
    fn loads_distinct_rows_with_same_timestamp() {
        let _env = CacheEnv::new("opencode-row-same-tick");
        let cached_ids = |db: &Path| {
            let mut ids: Vec<_> =
                super::cache::load_opencode_row_cache(&super::cache::cache_key(db))
                    .expect("row cache must hold both rows")
                    .into_iter()
                    .map(|row| row.id)
                    .collect();
            ids.sort();
            ids
        };
        let fixture = fs_fixture!({});
        let db = fixture.path("opencode.db");
        // Two distinct rows sharing an identical time_created value: content-hash
        // keying must keep both, never collapse them on the shared timestamp.
        create_db_message_with_time(
            &db,
            "msg-1",
            "session-a",
            1767312000000,
            r#"{"providerID":"anthropic","modelID":"m","time":{"created":1767312000000},"tokens":{"input":100}}"#,
        );
        create_db_message_with_time(
            &db,
            "msg-2",
            "session-a",
            1767312000000,
            r#"{"providerID":"anthropic","modelID":"m","time":{"created":1767312000000},"tokens":{"input":200}}"#,
        );

        let first = load_entries_from_directory(fixture.root(), &display_shared()).unwrap();
        assert_eq!(first.len(), 2);
        // The cache is keyed by row id, so assert both ids survived it. Counting
        // returned entries alone passes with the cache switched off entirely.
        assert_eq!(cached_ids(&db), ["msg-1", "msg-2"]);

        let second = load_entries_from_directory(fixture.root(), &display_shared()).unwrap();
        assert_eq!(second.len(), 2);
        assert_eq!(cached_ids(&db), ["msg-1", "msg-2"]);
    }

    /// Corruption must leave spend untouched — not zeroed, not doubled. The row
    /// cache and the ledger both re-emit here and produce identical output, so
    /// this pins the guarantee, not which of the two delivered it.
    #[test]
    fn garbage_db_neither_zeroes_nor_doubles_spend() {
        let _env = CacheEnv::new("opencode-garbage-db");
        let fixture = fs_fixture!({});
        let db = fixture.path("opencode.db");
        create_db_message(
            &db,
            "msg-1",
            "session-a",
            r#"{"providerID":"anthropic","modelID":"m","time":{"created":1767312000000},"tokens":{"input":100,"output":50}}"#,
        );

        let cold = load_entries_from_directory(fixture.root(), &display_shared()).unwrap();
        assert_eq!(cold.len(), 1);
        assert_eq!(cold[0].data.message.usage.input_tokens, 100);

        // Replace the DB with garbage — simulates corruption.
        fs::write(&db, b"not a sqlite database at all").unwrap();

        let warm = load_entries_from_directory(fixture.root(), &display_shared()).unwrap();
        assert_eq!(warm.len(), 1, "garbage DB must not zero or double spend");
        assert_eq!(warm[0].data.message.usage.input_tokens, 100);
    }

    /// `live_only` suppresses the ledger's re-emission (`merge_ledger`), leaving
    /// `fallback_from_cache` as the only thing between a corrupt DB and zeroed
    /// spend. Every other test here runs with both mechanisms live, so each one
    /// alone can be disabled without a single failure and neither is really
    /// covered; this is the case that pins the fallback by itself.
    #[test]
    fn live_only_corrupt_db_preserves_spend_without_the_ledger() {
        let _env = CacheEnv::new("opencode-live-only-corrupt");
        let fixture = fs_fixture!({});
        let db = fixture.path("opencode.db");
        create_db_message(
            &db,
            "msg-1",
            "session-a",
            r#"{"providerID":"anthropic","modelID":"m","time":{"created":1767312000000},"tokens":{"input":100,"output":50}}"#,
        );

        let shared = SharedArgs {
            live_only: true,
            ..display_shared()
        };
        let cold = load_entries_from_directory(fixture.root(), &shared).unwrap();
        assert_eq!(cold.len(), 1);
        assert_eq!(cold[0].data.message.usage.input_tokens, 100);

        fs::write(&db, b"not a sqlite database at all").unwrap();

        let warm = load_entries_from_directory(fixture.root(), &shared).unwrap();
        assert_eq!(
            warm.len(),
            1,
            "ledger suppressed, so the row-cache fallback alone must hold spend"
        );
        assert_eq!(warm[0].data.message.usage.input_tokens, 100);
    }

    /// A DB that opens and prepares but fails during iteration takes the
    /// `!completed` path, which skips the cache save and re-emits what was
    /// already stored. Like the garbage-DB case, the cache and the ledger
    /// produce the same output, so this pins the full set coming back rather
    /// than a truncated one — not which mechanism returned it.
    /// SQLite is resilient to page-level damage and may return Done instead of
    /// Err on a truncated file, so the corruption here is aggressive (zeroed
    /// data pages) to actually reach the step error.
    #[test]
    fn mid_scan_error_neither_zeroes_nor_truncates_spend() {
        let _env = CacheEnv::new("opencode-mid-scan-error");
        let fixture = fs_fixture!({});
        let db = fixture.path("opencode.db");

        // Two messages spanning separate data pages.
        create_db_message(
            &db,
            "msg-1",
            "session-a",
            r#"{"providerID":"anthropic","modelID":"m","time":{"created":1767312000000},"tokens":{"input":100,"output":50}}"#,
        );
        create_db_message(
            &db,
            "msg-2",
            "session-b",
            r#"{"providerID":"anthropic","modelID":"m","time":{"created":1767312000001},"tokens":{"input":200,"output":100}}"#,
        );

        let cold = load_entries_from_directory(fixture.root(), &display_shared()).unwrap();
        assert_eq!(cold.len(), 2, "cold run must return both messages");

        // Corrupt the DB: keep header + first page (schema), zero remaining
        // pages so iteration hits I/O or corruption errors.
        let mut contents = fs::read(&db).unwrap();
        let page_size = 4096;
        assert!(
            contents.len() > page_size,
            "fixture DB is {} bytes, so zeroing past the first page corrupts \
             nothing and the warm assertion below would prove nothing",
            contents.len()
        );
        for byte in &mut contents[page_size..] {
            *byte = 0;
        }
        fs::write(&db, &contents).unwrap();

        // SQLite tolerates a lot of page damage. Without this the corruption may
        // leave the DB fully readable, the warm load is then a plain reparse,
        // and `warm.len() == 2` passes with the cache and ledger both switched
        // off — exactly the vacuous test this one is meant not to be.
        let live_rows = raw_message_count(&db);
        assert!(
            live_rows < 2,
            "corruption left {live_rows} rows readable, so the live scan never \
             failed and the warm assertion is vacuous"
        );

        let warm = load_entries_from_directory(fixture.root(), &display_shared()).unwrap();
        // Must return the full cached set, not a truncated subset.
        assert_eq!(
            warm.len(),
            2,
            "mid-scan error must not clobber cache or truncate return"
        );
    }
}
