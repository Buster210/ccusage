//! On-disk caching of parsed usage entries.
//!
//! Parsing agent log files is the dominant cost of a ccusage run, so this module
//! persists the parsed [`LoadedEntry`] values per source file; unchanged files
//! are never re-parsed.
//!
//! # Layout
//!
//! One SQLite database, `cache.db`, under [`cache_dir`] (`$XDG_CACHE_HOME/ccusage`,
//! falling back to `~/.cache/ccusage`; on Windows `$LOCALAPPDATA/ccusage`, falling
//! back to the home profile's `.cache/ccusage`). Three tables:
//!
//! - `files` — one row per source file: freshness fingerprint (mtime, size, cost
//!   fingerprint) and a postcard-encoded `Vec<CachedEntry>` blob.
//! - `ledger` — billable entries retained from deleted source files, keyed by
//!   `(namespace, dedup_key)`. The primary key makes a duplicate append a no-op,
//!   so spend is counted at most once and survives source-file deletion.
//! - `opencode` — one row per OpenCode message database, keyed by its cache key.
//!
//! # Validity
//!
//! A `files` row is trusted only while the source's mtime, size, and cost
//! fingerprint all match what was recorded; any drift (or a missing file) demotes
//! the path to "fresh" and the adapter re-parses it.
//!
//! # Concurrency
//!
//! Writes run inside one `BEGIN IMMEDIATE` transaction; WAL lets readers proceed
//! while a writer commits, and a busy timeout makes competing writers wait rather
//! than fail. Every database error is non-fatal: the run degrades to parsing
//! without the cache.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Instant, UNIX_EPOCH},
};

static PRICING_V2_MIGRATED: AtomicBool = AtomicBool::new(false);
static WAL_DONE: AtomicBool = AtomicBool::new(false);
static EMPTY_CACHE: std::sync::OnceLock<std::sync::Mutex<HashSet<String>>> =
    std::sync::OnceLock::new();

fn is_known_empty(namespace: &str) -> bool {
    EMPTY_CACHE
        .get()
        .and_then(|m| m.lock().ok())
        .is_some_and(|set| set.contains(namespace))
}

fn mark_known_empty(namespace: &str) {
    let set = EMPTY_CACHE.get_or_init(|| std::sync::Mutex::new(HashSet::new()));
    if let Ok(mut s) = set.lock() {
        s.insert(namespace.to_string());
    }
}

fn clear_known_empty(namespace: &str) {
    if let Some(m) = EMPTY_CACHE.get()
        && let Ok(mut s) = m.lock()
    {
        s.remove(namespace);
    }
}

#[inline]
fn timing_enabled() -> bool {
    std::env::var_os("CCUSAGE_DEBUG_TIMING").is_some()
}

macro_rules! timed {
    ($label:expr, $block:expr) => {{
        let __start = if timing_enabled() {
            Some(Instant::now())
        } else {
            None
        };
        let __res = $block;
        if let Some(s) = __start {
            eprintln!("[timing] {} took {:?}", $label, s.elapsed());
        }
        __res
    }};
}

use rustc_hash::{FxHashSet, FxHasher};
use serde::{Deserialize, Serialize};

use crate::{
    LoadedEntry, chunk_file_indexes_by_size,
    types::{TokenUsageRaw, UsageEntry, UsageMessage},
};

/// Compact on-disk form of [`LoadedEntry`]. Only fields needed post-cache are
/// persisted; parse-only fields are stripped. `cost_usd` is kept as a fallback
/// for the no-pricing path (reprice uses it when pricing is unavailable).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedEntry {
    timestamp: crate::date_utils::TimestampMs,
    /// Original `data.timestamp` string from the source entry, persisted so the
    /// warm round-trip reproduces the cold value exactly — the `TimestampMs`
    /// field above cannot losslessly reproduce every original rfc3339 variant.
    data_timestamp: String,
    date: String,
    project: Arc<str>,
    session_id: Arc<str>,
    project_path: Arc<str>,
    cost_usd: Option<f64>,
    extra_total_tokens: u64,
    credits: Option<f64>,
    message_count: Option<u64>,
    model: Option<String>,
    usage_limit_reset_time: Option<crate::date_utils::TimestampMs>,
    // Flattened from UsageEntry — only what is used post-parse:
    usage: TokenUsageRaw,
    version: Option<String>,
    message_id: Option<String>,
    request_id: Option<String>,
    is_sidechain: Option<bool>,
    message_model: Option<String>,
    message_provider: Option<String>,
}

impl From<&LoadedEntry> for CachedEntry {
    fn from(e: &LoadedEntry) -> Self {
        CachedEntry {
            timestamp: e.timestamp,
            data_timestamp: e.data.timestamp.clone(),
            date: e.date.clone(),
            project: Arc::clone(&e.project),
            session_id: Arc::clone(&e.session_id),
            project_path: Arc::clone(&e.project_path),
            cost_usd: e.data.cost_usd,
            extra_total_tokens: e.extra_total_tokens,
            credits: e.credits,
            message_count: e.message_count,
            model: e.model.clone(),
            usage_limit_reset_time: e.usage_limit_reset_time,
            usage: e.data.message.usage,
            version: e.data.version.clone(),
            message_id: e.data.message.id.clone(),
            request_id: e.data.request_id.clone(),
            is_sidechain: e.data.is_sidechain,
            message_model: e.data.message.model.clone(),
            message_provider: e.data.message.provider.clone(),
        }
    }
}

impl From<CachedEntry> for LoadedEntry {
    fn from(c: CachedEntry) -> Self {
        LoadedEntry {
            data: UsageEntry {
                session_id: Some(c.session_id.to_string()),
                timestamp: c.data_timestamp,
                version: c.version,
                message: UsageMessage {
                    usage: c.usage,
                    model: c.message_model.map(|m| strip_model_prefix(&m).to_string()),
                    id: c.message_id,
                    provider: c.message_provider,
                },
                cost_usd: c.cost_usd,
                request_id: c.request_id,
                is_api_error_message: None,
                is_sidechain: c.is_sidechain,
            },
            timestamp: c.timestamp,
            date: c.date,
            project: c.project,
            session_id: c.session_id,
            project_path: c.project_path,
            cost: 0.0,
            extra_total_tokens: c.extra_total_tokens,
            credits: c.credits,
            message_count: c.message_count,
            model: c.model.map(|m| strip_model_prefix(&m).to_string()),
            usage_limit_reset_time: c.usage_limit_reset_time,
            missing_pricing_model: None,
        }
    }
}

/// Spend projection retained for a *deleted* source file. Wraps a
/// [`CachedEntry`] with the two fields it lacks (`cost`, `missing_pricing_model`).
/// On the way in, the six dedup-identity fields that only matter for a live
/// source (`data_timestamp`, `message_id`, `request_id`, `is_sidechain`,
/// `message_provider`, `cost_usd`) are cleared, since the source file is gone.
/// Reappearance on resume is resolved by the `dedup_key` primary key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    base: CachedEntry,
    /// Frozen at the cost computed when the source was last live. Deliberately
    /// NOT repriced on later runs (see `ledger_entry_cost_immutable_after_pricing_change`):
    /// a ledger records observed spend, not a recomputed estimate. This means a
    /// later upstream *pricing-bug fix* will not retroactively correct a
    /// deleted-source entry.
    /// FUTURE: an explicit, opt-in `cache reprice --model <id>` could recompute
    /// these from the stored `usage` tokens when a real pricing correction lands.
    cost: f64,
    missing_pricing_model: Option<String>,
}

impl From<&LoadedEntry> for LedgerEntry {
    fn from(e: &LoadedEntry) -> Self {
        let mut base = CachedEntry::from(e);
        // Dedup-identity fields are dropped since the source is gone; they are
        // never read off a deleted-source record, so clearing them here keeps a
        // stored ledger row from leaking them back on reconstruction.
        base.data_timestamp = String::new();
        base.message_id = None;
        base.request_id = None;
        base.is_sidechain = None;
        base.message_provider = None;
        base.cost_usd = None;
        LedgerEntry {
            base,
            cost: e.cost,
            missing_pricing_model: e.missing_pricing_model.clone(),
        }
    }
}

impl LedgerEntry {
    /// Rebuild a [`LoadedEntry`] from a ledger row. `dedup_key` is the row's
    /// primary-key column; it is restored as the message id so the entry keeps
    /// the identity it had when stored (the natural id, or the synthetic key for
    /// adapters without one). The remaining dedup-only fields are never read off
    /// a deleted-source record, so they reconstruct as `None`.
    fn into_loaded(self, dedup_key: String) -> LoadedEntry {
        let mut loaded: LoadedEntry = self.base.into();
        loaded.data.message.id = Some(dedup_key);
        loaded.cost = self.cost;
        loaded.missing_pricing_model = self.missing_pricing_model;
        loaded
    }
}

/// Freshness fingerprint for a single source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadata {
    pub(crate) mtime_epoch_millis: u64,
    pub(crate) size: u64,
    /// Content fingerprint for SQLite adapters (0 for FileStat strategy).
    pub(crate) fingerprint: u64,
}

/// Strategy for determining whether a cached file is still fresh.
pub enum Freshness {
    /// Compare file mtime + size (original behavior for JSONL adapters).
    FileStat,
    /// Use a content fingerprint (for SQLite/WAL adapters).
    Fingerprint(fn(&Path) -> Option<u64>),
}

/// Parsed entries recovered from the cache for one source file.
struct CachedEntries {
    entries: Vec<LoadedEntry>,
}

/// Root directory for all ccusage cache files.
pub fn cache_dir() -> Option<PathBuf> {
    Some(dirs_cache_dir()?.join("ccusage"))
}

fn dirs_cache_dir() -> Option<PathBuf> {
    // `LOCALAPPDATA` is only honored on Windows (see `is_windows` below), so
    // reading it unconditionally is harmless and keeps this call cfg-free.
    dirs_cache_dir_from_env(
        std::env::var_os("XDG_CACHE_HOME"),
        std::env::var_os("LOCALAPPDATA"),
        crate::home::home_dir(),
        cfg!(windows),
    )
}

/// Pure cache-base resolver behind [`dirs_cache_dir`], so the Windows
/// `LOCALAPPDATA` branch is unit-testable on any host. `XDG_CACHE_HOME` wins
/// everywhere; on Windows `LOCALAPPDATA` is next and the home profile's
/// `.cache` is the last resort, matching the Unix fallback.
fn dirs_cache_dir_from_env(
    xdg_cache_home: Option<std::ffi::OsString>,
    local_app_data: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
    is_windows: bool,
) -> Option<PathBuf> {
    if let Some(dir) = xdg_cache_home {
        return Some(PathBuf::from(dir));
    }
    if is_windows
        && let Some(dir) = local_app_data
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    Some(home?.join(".cache"))
}

/// Stable cache key derived from the source file path. Used as a compact
/// identifier for the OpenCode adapter's per-database row cache.
pub fn cache_key(path: &Path) -> String {
    let mut hasher = FxHasher::default();
    path.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Capture the current freshness fingerprint of a source file, if it exists.
pub fn file_metadata(path: &Path) -> Option<FileMetadata> {
    let metadata = fs::metadata(path).ok()?;
    let mtime = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    Some(FileMetadata {
        mtime_epoch_millis: mtime,
        size: metadata.len(),
        fingerprint: 0,
    })
}

// ---------------------------------------------------------------------------
// SQLite store
// ---------------------------------------------------------------------------

/// File name of the cache database within [`cache_dir`].
const DB_FILE: &str = "cache.db";
/// Schema version for the `files` table, tracked in `schema_meta`.
const FILES_SCHEMA_VERSION: i64 = 1;
/// Schema version for the `ledger` table, tracked in `schema_meta`. Bump on
/// every `LedgerEntry` layout change so the mismatch warning below fires.
/// v2 adds `date TEXT` column (denormalized from `LedgerEntry.base.date`) and
/// index `ledger(namespace,date)` for windowed queries.
const LEDGER_SCHEMA_VERSION: i64 = 2;
/// Schema version for the `opencode` table, tracked in `schema_meta`.
const OPENCODE_SCHEMA_VERSION: i64 = 1;
/// Schema version for the `pricing` table, tracked in `schema_meta`.
const PRICING_SCHEMA_VERSION: i64 = 2;

/// Encode parsed entries into the compact blob stored in the `files` table.
fn encode_entries(entries: &[LoadedEntry]) -> Option<Vec<u8>> {
    let cached: Vec<CachedEntry> = entries.iter().map(CachedEntry::from).collect();
    postcard::to_allocvec(&cached).ok()
}

/// Decode a `files` blob back into reconstructed entries.
fn decode_entries(bytes: &[u8]) -> Option<Vec<LoadedEntry>> {
    let cached: Vec<CachedEntry> = postcard::from_bytes(bytes).ok()?;
    Some(cached.into_iter().map(LoadedEntry::from).collect())
}

/// Concurrent opens pile onto SQLite's file locks (0.2ms -> 24ms under 8 loader
/// threads), so serialize just the open+migrate phase. Leaf-level: nothing under
/// it takes another lock.
static OPEN_DB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Open (creating if needed) the cache database, apply pragmas, and migrate the
/// schema. Returns `None` on any failure so the caller degrades to parsing
/// without a cache, consistent with this module's non-fatal philosophy.
fn open_db() -> Option<sqlite::Connection> {
    // `None` degrades to parsing without a cache, matching this function's
    // contract; a poisoned lock would mean a loader thread panicked mid-open.
    let _guard = OPEN_DB_LOCK.lock().ok()?;
    timed!("open_db", {
        let dir = cache_dir()?;
        fs::create_dir_all(&dir).ok()?;
        let conn = sqlite::open(dir.join(DB_FILE)).ok()?;
        // Set the busy timeout first so ordinary lock waits — BEGIN IMMEDIATE and the
        // schema migration — block and retry instead of failing outright.
        conn.execute("PRAGMA busy_timeout=5000;").ok()?;
        // SQLite treats mmap_size as a hint; if it fails we still have a usable
        // connection, so ignore the error rather than discarding the DB.
        let _ = conn.execute("PRAGMA mmap_size=268435456;");
        // Switch to WAL so readers run while one writer commits (NORMAL stays durable
        // under WAL). The switch needs a lock upgrade that busy_timeout cannot cover,
        // so it carries its own bounded retry and never discards the connection.
        if !WAL_DONE.load(Ordering::Relaxed) {
            let wal_ok = set_wal_mode(&conn);
            if wal_ok && !cfg!(test) {
                WAL_DONE.store(true, Ordering::Relaxed);
            }
        }
        migrate(&conn)?;
        Some(conn)
    })
}

/// Switch `conn` to WAL journaling, retrying past the cold-open upgrade deadlock.
///
/// Changing journal mode needs a SHARED->EXCLUSIVE lock upgrade. When several
/// processes open a fresh database at once they can each hold SHARED while all
/// want EXCLUSIVE; SQLite returns `SQLITE_BUSY` immediately for that upgrade
/// instead of invoking the busy handler (calling it would only deadlock), so
/// `busy_timeout` cannot cover this case. A short bounded backoff resolves it:
/// once any opener wins the switch the file is WAL for everyone, so a loser's
/// retry is a no-op success. If every attempt loses — pathologically unlikely —
/// the connection stays in its default rollback journal; writes still serialize
/// through `busy_timeout`, so no row is lost, only cross-process read concurrency
/// is reduced until a later uncontended run flips it to WAL.
fn set_wal_mode(conn: &sqlite::Connection) -> bool {
    for attempt in 0..8u64 {
        if conn
            .execute("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .is_ok()
        {
            return true;
        }
        if attempt < 7 {
            std::thread::sleep(std::time::Duration::from_millis(attempt + 1));
        }
    }
    false
}
/// Remove the entire cache database and its sidecars. Non-fatal: all IO
/// errors are ignored and there is no output. If the cache directory does not
/// resolve, this is a no-op.
pub fn clear_cache() {
    let Some(dir) = cache_dir() else {
        return;
    };
    for name in &[DB_FILE, "cache.db-wal", "cache.db-shm"] {
        let _ = fs::remove_file(dir.join(name));
    }
}

/// Remove the `files` and `ledger` rows for `agent`'s namespaces, plus the
/// `opencode` table when the agent is "opencode". Returns whether `agent` is
/// cacheable, so callers can report a no-op for agents with no on-disk cache.
/// Non-fatal: all IO/SQL errors are ignored.
pub fn clear_cache_namespaces(agent: &str) -> bool {
    let mut cacheable = !agent_namespaces(agent).is_empty();
    let Some(conn) = open_db() else {
        // report cacheable even if DB unavailable
        return cacheable
            || agent == "pi"
            || agent.contains(':')
            || (!agent.is_empty() && agent != "all" && agent_namespaces(agent).is_empty());
    };
    for ns in agent_namespaces(agent) {
        cacheable = true;
        if let Ok(mut st) = conn.prepare("DELETE FROM files WHERE namespace = ?") {
            let _ = st.bind((1, *ns));
            let _ = st.next();
        }
        if let Ok(mut st) = conn.prepare("DELETE FROM ledger WHERE namespace = ?") {
            let _ = st.bind((1, *ns));
            let _ = st.next();
        }
    }
    // pi named stores use `pi:<name>` (e.g. `pi:omp`); clear them when
    // the store name or its parent `pi` is requested.
    if agent == "pi" {
        cacheable = true;
        let _ = conn.execute("DELETE FROM files WHERE namespace LIKE 'pi:%'");
        let _ = conn.execute("DELETE FROM ledger WHERE namespace LIKE 'pi:%'");
    } else if agent.contains(':') {
        cacheable = true;
        if let Ok(mut st) = conn.prepare("DELETE FROM files WHERE namespace = ?") {
            let _ = st.bind((1, agent));
            let _ = st.next();
        }
        if let Ok(mut st) = conn.prepare("DELETE FROM ledger WHERE namespace = ?") {
            let _ = st.bind((1, agent));
            let _ = st.next();
        }
    } else if !agent.is_empty() && agent != "all" && agent_namespaces(agent).is_empty() {
        // unknown agent likely a pi named store (e.g. `omp` -> `pi:omp` and `pi:omp:<path>`)
        let mut escaped = String::with_capacity(agent.len());
        for c in agent.chars() {
            if matches!(c, '\\' | '%' | '_') {
                escaped.push('\\');
            }
            escaped.push(c);
        }
        let ns = format!("pi:{agent}");
        let ns_prefix = format!("pi:{escaped}:%");
        cacheable = true;
        if let Ok(mut st) =
            conn.prepare("DELETE FROM files WHERE namespace = ? OR namespace LIKE ? ESCAPE '\\'")
        {
            let _ = st.bind((1, ns.as_str()));
            let _ = st.bind((2, ns_prefix.as_str()));
            let _ = st.next();
        }
        if let Ok(mut st) =
            conn.prepare("DELETE FROM ledger WHERE namespace = ? OR namespace LIKE ? ESCAPE '\\'")
        {
            let _ = st.bind((1, ns.as_str()));
            let _ = st.bind((2, ns_prefix.as_str()));
            let _ = st.next();
        }
    }
    if agent == "opencode" {
        let _ = conn.execute("DELETE FROM opencode");
    }
    cacheable
}

fn agent_namespaces(agent: &str) -> &'static [&'static str] {
    match agent {
        "claude" => &["claude"],
        "opencode" => &["opencode", "opencode-db"],
        "amp" => &["amp"],
        "codebuff" => &["codebuff"],
        "copilot" => &["copilot"],
        "droid" => &["droid"],
        "gemini" => &["gemini"],
        "goose" => &["goose"],
        "hermes" => &["hermes"],
        "kilo" => &["kilo"],
        "openclaw" => &["openclaw"],
        "pi" => &["pi"],
        "qwen" => &["qwen"],
        _ => &[],
    }
}

/// Create the schema if absent and stamp each table's version into `schema_meta`.
///
/// Only `ledger` is irreplaceable; the regenerable tables self-heal via their
/// freshness fingerprints. A stale ledger version may no longer decode, so it is
/// surfaced rather than dropped silently — see the `FUTURE` marker for migration.
fn migrate(conn: &sqlite::Connection) -> Option<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS files (\
             path TEXT PRIMARY KEY,\
             namespace TEXT NOT NULL,\
             mtime INTEGER NOT NULL,\
             size INTEGER NOT NULL,\
             cost_fingerprint INTEGER NOT NULL,\
             entries BLOB NOT NULL\
         );\
         CREATE INDEX IF NOT EXISTS files_namespace ON files(namespace);\
         CREATE TABLE IF NOT EXISTS ledger (\
             namespace TEXT NOT NULL,\
             dedup_key TEXT NOT NULL,\
             date TEXT,\
             entry BLOB NOT NULL,\
             PRIMARY KEY (namespace, dedup_key)\
         );\
         CREATE TABLE IF NOT EXISTS opencode (\
             db_key TEXT PRIMARY KEY,\
             cost_fingerprint INTEGER NOT NULL,\
             rows BLOB NOT NULL\
         );\
         CREATE TABLE IF NOT EXISTS pricing (\
             url TEXT PRIMARY KEY,\
             etag TEXT,\
             last_modified TEXT,\
             body TEXT NOT NULL,\
             updated_at INTEGER\
        );\
        CREATE TABLE IF NOT EXISTS schema_meta (\
             name TEXT PRIMARY KEY,\
             version INTEGER NOT NULL\
         );",
    )
    .ok()?;
    {
        let mut st = conn
            .prepare(
                "INSERT OR IGNORE INTO schema_meta(name, version) VALUES\
                     (?, ?), (?, ?), (?, ?), (?, ?)",
            )
            .ok()?;
        st.bind((1, "files")).ok()?;
        st.bind((2, FILES_SCHEMA_VERSION)).ok()?;
        st.bind((3, "ledger")).ok()?;
        st.bind((4, LEDGER_SCHEMA_VERSION)).ok()?;
        st.bind((5, "opencode")).ok()?;
        st.bind((6, OPENCODE_SCHEMA_VERSION)).ok()?;
        st.bind((7, "pricing")).ok()?;
        st.bind((8, PRICING_SCHEMA_VERSION)).ok()?;
        st.next().ok()?;
    }
    // pricing v2: `updated_at` drives the freshness window. Memoize the
    // probe in prod - CREATE TABLE already includes the column for new DBs,
    // so after the first successful check we skip the prepare on every
    // open_db (called per adapter, ~15x per run). In tests each CacheEnv is a
    // fresh temp DB, and one test deliberately creates a legacy DB without the
    // column, so we must not memoize there.
    let should_check = if cfg!(test) {
        true
    } else {
        !PRICING_V2_MIGRATED.load(Ordering::Relaxed)
    };
    if should_check
        && conn
            .prepare("SELECT updated_at FROM pricing LIMIT 1")
            .is_err()
        && let Err(e) = conn.execute("ALTER TABLE pricing ADD COLUMN updated_at INTEGER")
    {
        eprintln!(
            "WARN  Failed to upgrade pricing cache schema ({e}); refreshing without the cache."
        );
        return None;
    }
    if !cfg!(test) {
        PRICING_V2_MIGRATED.store(true, Ordering::Relaxed);
    }
    if let Ok(mut st) = conn.prepare("UPDATE schema_meta SET version = ? WHERE name = 'pricing'") {
        let _ = st.bind((1, PRICING_SCHEMA_VERSION));
        let _ = st.next();
    }
    // ledger v2: `date` denormalized from `LedgerEntry.base.date` for windowed queries.
    // Legacy DBs add column+index; existing NULL rows stay NULL until re-appended.
    if conn.prepare("SELECT date FROM ledger LIMIT 1").is_err() {
        let _ = conn.execute("ALTER TABLE ledger ADD COLUMN date TEXT");
    }
    let _ =
        conn.execute("CREATE INDEX IF NOT EXISTS ledger_namespace_date ON ledger(namespace, date)");
    if let Some(stored) = read_schema_version(conn, "ledger")
        && stored != LEDGER_SCHEMA_VERSION
    {
        eprintln!(
            "WARN  Ledger cache schema v{stored} differs from expected v{LEDGER_SCHEMA_VERSION}; \
             retained spend may be dropped. Run `ccusage clear-cache` if totals look wrong."
        );
        // Stamp the new version so the warning fires once per upgrade, not every run.
        if let Ok(mut st) = conn.prepare("UPDATE schema_meta SET version = ? WHERE name = 'ledger'")
        {
            let _ = st.bind((1, LEDGER_SCHEMA_VERSION));
            let _ = st.next();
        }
    }
    // FUTURE: migration logic — when a table's on-disk layout changes
    // incompatibly, bump its constant above and add per-table migration here.
    Some(())
}

/// Read a table's stored schema version, or `None` if unstamped/unreadable.
fn read_schema_version(conn: &sqlite::Connection, name: &str) -> Option<i64> {
    let mut st = conn
        .prepare("SELECT version FROM schema_meta WHERE name = ?")
        .ok()?;
    st.bind((1, name)).ok()?;
    match st.next() {
        Ok(sqlite::State::Row) => st.read::<i64, _>(0).ok(),
        _ => None,
    }
}

/// A `files` row's freshness fingerprint and its (still encoded) entries blob.
struct StoredFile {
    mtime: u64,
    size: u64,
    /// Content fingerprint from the `cost_fingerprint` column.
    fingerprint: u64,
    entries: Vec<u8>,
}

/// Load every `files` row for `namespace`, keyed by source path. The entries
/// blob is decoded lazily by [`partition_files`] only on a confirmed cache hit.
/// Blob transfer dominates warm runs, so large namespaces shard the scan across
/// workers (connections are not shareable); failed shards read as fresh, never loss.
fn load_namespace_files(conn: &sqlite::Connection, namespace: &str) -> HashMap<String, StoredFile> {
    if row_count(conn, namespace) >= 128 {
        let workers = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .clamp(2, 8);
        let mut map = HashMap::new();
        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(workers);
            for shard in 0..workers {
                handles.push(scope.spawn(move || load_namespace_shard(namespace, workers, shard)));
            }
            for handle in handles {
                map.extend(handle.join().unwrap_or_default());
            }
        });
        if !map.is_empty() {
            return map;
        }
        // Sharded load came back empty (connections refused?); fall through
        // to the single-connection scan rather than dropping the cache.
    }
    load_namespace_files_single(conn, namespace)
}

fn row_count(conn: &sqlite::Connection, namespace: &str) -> usize {
    let Ok(mut st) = conn.prepare("SELECT COUNT(*) FROM files WHERE namespace = ?") else {
        return 0;
    };
    if st.bind((1, namespace)).is_err() {
        return 0;
    }
    match st.next() {
        Ok(sqlite::State::Row) => st.read::<i64, _>(0).unwrap_or(0).max(0) as usize,
        _ => 0,
    }
}

fn load_namespace_shard(
    namespace: &str,
    workers: usize,
    shard: usize,
) -> HashMap<String, StoredFile> {
    let mut map = HashMap::new();
    let Some(conn) = open_db() else {
        return map;
    };
    let Ok(mut st) = conn.prepare(
        "SELECT path, mtime, size, cost_fingerprint, entries FROM files \
         WHERE namespace = ? AND (rowid % ?2) = ?3",
    ) else {
        return map;
    };
    if st.bind((1, namespace)).is_err()
        || st.bind((2, workers as i64)).is_err()
        || st.bind((3, shard as i64)).is_err()
    {
        return map;
    }
    while let Ok(sqlite::State::Row) = st.next() {
        let (Ok(path), Ok(mtime), Ok(size), Ok(cost), Ok(entries)) = (
            st.read::<String, _>(0),
            st.read::<i64, _>(1),
            st.read::<i64, _>(2),
            st.read::<i64, _>(3),
            st.read::<Vec<u8>, _>(4),
        ) else {
            continue;
        };
        map.insert(
            path,
            StoredFile {
                mtime: mtime as u64,
                size: size as u64,
                fingerprint: cost as u64,
                entries,
            },
        );
    }
    map
}

fn load_namespace_files_single(
    conn: &sqlite::Connection,
    namespace: &str,
) -> HashMap<String, StoredFile> {
    let mut map = HashMap::new();
    let Ok(mut st) = conn.prepare(
        "SELECT path, mtime, size, cost_fingerprint, entries FROM files WHERE namespace = ?",
    ) else {
        return map;
    };
    if st.bind((1, namespace)).is_err() {
        return map;
    }
    while let Ok(sqlite::State::Row) = st.next() {
        let (Ok(path), Ok(mtime), Ok(size), Ok(cost), Ok(entries)) = (
            st.read::<String, _>(0),
            st.read::<i64, _>(1),
            st.read::<i64, _>(2),
            st.read::<i64, _>(3),
            st.read::<Vec<u8>, _>(4),
        ) else {
            continue;
        };
        map.insert(
            path,
            StoredFile {
                mtime: mtime as u64,
                size: size as u64,
                fingerprint: cost as u64,
                entries,
            },
        );
    }
    map
}

/// A source file that must be re-parsed, paired with the freshness fingerprint
/// captured *before* the parse. Caching against this pre-parse metadata (rather
/// than re-statting afterwards) guarantees that a concurrent append during the
/// parse can never be recorded as already-cached: the post-append file will
/// mismatch the stored size/mtime on the next run and be re-parsed.
struct FreshFile {
    path: PathBuf,
    /// `None` when the file vanished or could not be statted; such a file is
    /// parsed but never cached.
    metadata: Option<FileMetadata>,
}

/// The split of an input file list into cache hits and files needing parsing.
struct FilePartition {
    cached: Vec<(usize, CachedEntries)>,
    fresh: Vec<(usize, FreshFile)>,
}

/// Partition source files into cached (still valid) and fresh (must re-parse).
fn partition_files(
    files: &[PathBuf],
    stored: &HashMap<String, StoredFile>,
    freshness: &Freshness,
    single_thread: bool,
) -> FilePartition {
    let worker_count = if single_thread {
        1
    } else {
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .min(files.len())
    };
    if worker_count <= 1 {
        let mut cached: Vec<(usize, CachedEntries)> = Vec::new();
        let mut fresh: Vec<(usize, FreshFile)> = Vec::new();

        for (index, path) in files.iter().enumerate() {
            let Some(current) = file_metadata(path) else {
                fresh.push((
                    index,
                    FreshFile {
                        path: path.clone(),
                        metadata: None,
                    },
                ));
                continue;
            };

            let mut current = current;
            let fingerprint_value = match freshness {
                Freshness::FileStat => 0,
                Freshness::Fingerprint(f) => f(path).unwrap_or(u64::MAX),
            };
            current.fingerprint = fingerprint_value;

            let path_str = path.to_string_lossy().to_string();
            if let Some(entry) = stored.get(&path_str) {
                let unchanged = match freshness {
                    Freshness::FileStat => {
                        entry.mtime == current.mtime_epoch_millis && entry.size == current.size
                    }
                    Freshness::Fingerprint(_) => {
                        entry.fingerprint == current.fingerprint && current.fingerprint != u64::MAX
                    }
                };
                if unchanged && let Some(entries) = decode_entries(&entry.entries) {
                    cached.push((index, CachedEntries { entries }));
                    continue;
                }
            }

            fresh.push((
                index,
                FreshFile {
                    path: path.clone(),
                    metadata: Some(current),
                },
            ));
        }

        return FilePartition { cached, fresh };
    }

    // Round-robin avoids double `stat` via `chunk_file_indexes_by_size` (which re-stats for size-balance).
    // Partition `stat` is uniform, so even distribution is enough; `parse_fresh_files` keeps size-balance.
    let mut chunks: Vec<Vec<usize>> = (0..worker_count).map(|_| Vec::new()).collect();
    for (idx, _) in files.iter().enumerate() {
        chunks[idx % worker_count].push(idx);
    }
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            handles.push(scope.spawn(move || {
                let mut cached: Vec<(usize, CachedEntries)> = Vec::new();
                let mut fresh: Vec<(usize, FreshFile)> = Vec::new();
                for index in chunk {
                    let path = &files[index];
                    let Some(mut current) = file_metadata(path) else {
                        fresh.push((
                            index,
                            FreshFile {
                                path: path.clone(),
                                metadata: None,
                            },
                        ));
                        continue;
                    };

                    let fingerprint_value = match freshness {
                        Freshness::FileStat => 0,
                        Freshness::Fingerprint(f) => f(path).unwrap_or(u64::MAX),
                    };
                    current.fingerprint = fingerprint_value;

                    let path_str = path.to_string_lossy().to_string();
                    if let Some(entry) = stored.get(&path_str) {
                        let unchanged = match freshness {
                            Freshness::FileStat => {
                                entry.mtime == current.mtime_epoch_millis
                                    && entry.size == current.size
                            }
                            Freshness::Fingerprint(_) => {
                                entry.fingerprint == current.fingerprint
                                    && current.fingerprint != u64::MAX
                            }
                        };
                        if unchanged && let Some(entries) = decode_entries(&entry.entries) {
                            cached.push((index, CachedEntries { entries }));
                            continue;
                        }
                    }

                    fresh.push((
                        index,
                        FreshFile {
                            path: path.clone(),
                            metadata: Some(current),
                        },
                    ));
                }
                (cached, fresh)
            }));
        }
        let mut all_cached: Vec<(usize, CachedEntries)> = Vec::new();
        let mut all_fresh: Vec<(usize, FreshFile)> = Vec::new();
        for handle in handles {
            // Fail open like load_namespace_shard: a panicked worker
            // contributes nothing instead of aborting the whole load.
            let (cached, fresh) = handle.join().unwrap_or_default();
            all_cached.extend(cached);
            all_fresh.extend(fresh);
        }
        all_cached.sort_by_key(|(index, _)| *index);
        all_fresh.sort_by_key(|(index, _)| *index);
        FilePartition {
            cached: all_cached,
            fresh: all_fresh,
        }
    })
}

/// Options for [`load_with_cache`], grouped so the two flags can't be
/// transposed at a call site the way adjacent positional bools can.
#[derive(Clone, Copy)]
pub struct CacheOpts {
    pub single_thread: bool,
    pub live_only: bool,
}

/// Load parsed entries for `files`, reusing the cache for unchanged files and
/// parsing only the fresh ones with `parse_file`. `namespace` identifies the
/// calling adapter (e.g. `"claude"`, `"amp"`).
///
/// Every billable live entry is recorded in the ledger so its spend persists
/// after its source file is deleted; ledger entries whose source is gone are
/// re-emitted, while those still on disk are skipped (the live copy wins via the
/// adapter's dedup). When `live_only` is set the re-emission is suppressed, so
/// the result holds only entries whose source file still exists on disk.
///
/// `parse_file` returns `Err` when a file cannot be read or parsed; the error
/// propagates and a failed run is never persisted.
pub fn load_with_cache<F, R>(
    namespace: &str,
    files: &[PathBuf],
    opts: CacheOpts,
    freshness: Freshness,
    parse_file: F,
    reprice: R,
    date_filter: Option<(&str, &str)>,
) -> crate::Result<Vec<LoadedEntry>>
where
    F: Fn(&Path) -> crate::Result<Vec<LoadedEntry>> + Sync,
    R: Fn(&mut LoadedEntry) + Sync,
{
    let __lwc_start = timing_enabled().then(Instant::now);
    if files.is_empty() {
        if is_known_empty(namespace) {
            if let Some(s) = __lwc_start {
                eprintln!(
                    "[timing] load_with_cache:{namespace}: empty known-empty fast path {:?}",
                    s.elapsed()
                );
            }
            return Ok(Vec::new());
        }
        let conn = open_db();
        if let Some(s) = __lwc_start {
            eprintln!(
                "[timing] load_with_cache:{namespace}: open_db {:?}",
                s.elapsed()
            );
        }
        // Quick probe for truly empty (no files, no ledger) to cache the
        // negative and skip DB work on subsequent warm runs. This is the
        // common case for adapters with no data (e.g., amp, goose on a
        // Claude-only machine).
        let mut has_files = false;
        let mut has_ledger = false;
        if let Some(c) = conn.as_ref() {
            if let Ok(mut st) = c.prepare("SELECT 1 FROM files WHERE namespace = ? LIMIT 1") {
                let _ = st.bind((1, namespace));
                has_files = matches!(st.next(), Ok(sqlite::State::Row));
            }
            if let Ok(mut st) = c.prepare("SELECT 1 FROM ledger WHERE namespace = ? LIMIT 1") {
                let _ = st.bind((1, namespace));
                has_ledger = matches!(st.next(), Ok(sqlite::State::Row));
            }
        }
        if !has_files && !has_ledger {
            mark_known_empty(namespace);
            if let Some(total) = __lwc_start {
                eprintln!(
                    "[timing] load_with_cache:{namespace}: TOTAL (truly empty) {:?}",
                    total.elapsed()
                );
            }
            return Ok(Vec::new());
        }
        // Even with no live files we must prune deleted `files` rows and
        // re-emit ledger entries, otherwise the ledger test fails. This is
        // what `write_back` does when `fresh` is empty. Only prune if we
        // know there are files rows to prune.
        if has_files
            && let Some(c) = conn.as_ref()
            && c.execute("BEGIN IMMEDIATE").is_ok()
        {
            let ok = prune_deleted(c, namespace, files).is_some();
            let _ = c.execute(if ok { "COMMIT" } else { "ROLLBACK" });
        }
        let __merge_start = timing_enabled().then(Instant::now);
        let res = Ok(match conn {
            Some(conn) => {
                if has_ledger {
                    merge_ledger(&conn, namespace, Vec::new(), opts.live_only, date_filter)
                } else {
                    Vec::new()
                }
            }
            None => Vec::new(),
        });
        if let Some(s) = __merge_start {
            eprintln!(
                "[timing] load_with_cache:{namespace}: merge_ledger {:?}",
                s.elapsed()
            );
        }
        if let Some(total) = __lwc_start {
            eprintln!(
                "[timing] load_with_cache:{namespace}: TOTAL {:?}",
                total.elapsed()
            );
        }
        return res;
    }
    clear_known_empty(namespace);
    let conn = open_db();
    if let Some(s) = __lwc_start {
        eprintln!(
            "[timing] load_with_cache:{namespace}: open_db {:?}",
            s.elapsed()
        );
    }
    let __stored_start = timing_enabled().then(Instant::now);
    // Partition against the stored namespace snapshot (empty when the cache is
    // unavailable, which forces every file to be parsed fresh).
    let stored = conn
        .as_ref()
        .map(|conn| load_namespace_files(conn, namespace))
        .unwrap_or_default();
    if let Some(s) = __stored_start {
        eprintln!(
            "[timing] load_with_cache:{namespace}: load_namespace_files {:?} (files={})",
            s.elapsed(),
            files.len()
        );
    }
    let __part_start = timing_enabled().then(Instant::now);
    let partition = partition_files(files, &stored, &freshness, opts.single_thread);
    if let Some(s) = __part_start {
        eprintln!(
            "[timing] load_with_cache:{namespace}: partition_files {:?} cached={} fresh={}",
            s.elapsed(),
            partition.cached.len(),
            partition.fresh.len()
        );
    }

    let fresh_paths: Vec<PathBuf> = partition
        .fresh
        .iter()
        .map(|(_, f)| f.path.clone())
        .collect();
    let __parse_start = timing_enabled().then(Instant::now);
    let mut parsed = parse_fresh_files(&fresh_paths, opts.single_thread, &parse_file)?;
    if let Some(s) = __parse_start {
        eprintln!(
            "[timing] load_with_cache:{namespace}: parse_fresh_files {:?} fresh_count={}",
            s.elapsed(),
            fresh_paths.len()
        );
    }

    // Reprice fresh entries before persisting so cache and ledger store current cost.
    for entry in parsed.iter_mut().flatten() {
        reprice(entry);
    }

    // Persist fresh entries, append them to the ledger, and prune deleted sources
    // in one transaction. Skipped when the cache is unavailable.
    if let Some(conn) = conn.as_ref() {
        let __wb_start = timing_enabled().then(Instant::now);
        write_back(conn, namespace, &partition.fresh, &parsed, files, true);
        if let Some(s) = __wb_start {
            eprintln!(
                "[timing] load_with_cache:{namespace}: write_back {:?}",
                s.elapsed()
            );
        }
    }

    // Assemble live entries (cached hits first, then freshly parsed).
    let cached_total: usize = partition.cached.iter().map(|(_, c)| c.entries.len()).sum();
    let fresh_total: usize = parsed.iter().map(Vec::len).sum();
    let mut live = Vec::with_capacity(cached_total + fresh_total);
    for (_, cached) in partition.cached {
        live.extend(cached.entries);
    }
    for entries in parsed {
        live.extend(entries);
    }

    // Reprice cached hits; fresh entries were already repriced above.
    for entry in &mut live[..cached_total] {
        reprice(entry);
    }

    // Merge the ledger: record new billable entries and re-emit entries whose
    // source file has since been deleted. Without a cache there is nothing to
    // merge, so the live entries pass through unchanged.
    let __merge_start = timing_enabled().then(Instant::now);
    let res = Ok(match conn {
        Some(conn) => merge_ledger(&conn, namespace, live, opts.live_only, date_filter),
        None => live,
    });
    if let Some(s) = __merge_start {
        eprintln!(
            "[timing] load_with_cache:{namespace}: merge_ledger {:?}",
            s.elapsed()
        );
    }
    if let Some(total) = __lwc_start {
        eprintln!(
            "[timing] load_with_cache:{namespace}: TOTAL {:?}",
            total.elapsed()
        );
    }
    res
}

/// [`load_with_cache`] variant returning per-file entry vectors 1:1 with `files`
/// (pi's replay suppression matches parent/child sessions across files).
///
/// This does NOT touch the ledger: the caller suppresses first and passes only
/// the surviving live set to [`retain_via_ledger`], or suppressed duplicates
/// would be re-emitted as deleted spend on the next run.
pub fn load_with_cache_grouped<F, R>(
    namespace: &str,
    files: &[PathBuf],
    opts: CacheOpts,
    freshness: Freshness,
    parse_file: F,
    reprice: R,
) -> crate::Result<Vec<Vec<LoadedEntry>>>
where
    F: Fn(&Path) -> crate::Result<Vec<LoadedEntry>> + Sync,
    R: Fn(&mut LoadedEntry) + Sync,
{
    if files.is_empty() {
        if let Some(conn) = open_db()
            && conn.execute("BEGIN IMMEDIATE").is_ok()
        {
            let ok = prune_deleted(&conn, namespace, files).is_some();
            let _ = conn.execute(if ok { "COMMIT" } else { "ROLLBACK" });
        }
        return Ok(Vec::new());
    }
    clear_known_empty(namespace);
    let conn = open_db();
    let stored = conn
        .as_ref()
        .map(|conn| load_namespace_files(conn, namespace))
        .unwrap_or_default();
    let partition = partition_files(files, &stored, &freshness, opts.single_thread);
    let fresh_paths: Vec<PathBuf> = partition
        .fresh
        .iter()
        .map(|(_, fresh)| fresh.path.clone())
        .collect();
    let mut parsed = parse_fresh_files(&fresh_paths, opts.single_thread, &parse_file)?;
    for entry in parsed.iter_mut().flatten() {
        reprice(entry);
    }
    if let Some(conn) = conn.as_ref() {
        write_back(conn, namespace, &partition.fresh, &parsed, files, false);
    }
    let mut groups: Vec<Vec<LoadedEntry>> = vec![Vec::new(); files.len()];
    for ((index, _), entries) in partition.fresh.iter().zip(parsed) {
        groups[*index] = entries;
    }
    for (index, mut cached) in partition.cached {
        for entry in &mut cached.entries {
            reprice(entry);
        }
        groups[index] = cached.entries;
    }
    Ok(groups)
}

/// Upsert fresh entries, optionally append them to the ledger, and prune
/// deleted sources in one transaction (rolls back, so failures re-parse next run).
///
/// `append_ledger` is false when the caller suppresses cross-file duplicates
/// after loading: only the surviving live set may reach the ledger.
fn write_back(
    conn: &sqlite::Connection,
    namespace: &str,
    fresh: &[(usize, FreshFile)],
    parsed: &[Vec<LoadedEntry>],
    files: &[PathBuf],
    append_ledger: bool,
) {
    if conn.execute("BEGIN IMMEDIATE").is_err() {
        return;
    }
    let committed = (|| -> Option<()> {
        for ((_, fresh), entries) in fresh.iter().zip(parsed.iter()) {
            // Cache against the pre-parse fingerprint so a concurrent append is
            // re-parsed next run instead of being silently trusted as cached. A
            // file that vanished mid-run has no metadata and is left uncached.
            if let Some(meta) = &fresh.metadata {
                upsert_file(conn, namespace, &fresh.path, meta, entries)?;
            }
            if append_ledger {
                append_entries_to_ledger(conn, namespace, entries)?;
            }
        }
        prune_deleted(conn, namespace, files)?;
        Some(())
    })();
    let _ = conn.execute(if committed.is_some() {
        "COMMIT"
    } else {
        "ROLLBACK"
    });
}

/// Insert or replace the `files` row for one source file.
fn upsert_file(
    conn: &sqlite::Connection,
    namespace: &str,
    path: &Path,
    meta: &FileMetadata,
    entries: &[LoadedEntry],
) -> Option<()> {
    let blob = encode_entries(entries)?;
    let mut st = conn
        .prepare(
            "INSERT OR REPLACE INTO files(path, namespace, mtime, size, cost_fingerprint, entries) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .ok()?;
    let path_str = path.to_string_lossy();
    st.bind((1, path_str.as_ref())).ok()?;
    st.bind((2, namespace)).ok()?;
    st.bind((3, meta.mtime_epoch_millis as i64)).ok()?;
    st.bind((4, meta.size as i64)).ok()?;
    st.bind((5, meta.fingerprint as i64)).ok()?;
    st.bind((6, &blob[..])).ok()?;
    st.next().ok()?;
    Some(())
}

/// Drop `files` rows for `namespace` whose source file no longer exists on disk.
/// Their spend is preserved in the ledger (appended while the file was last
/// live), so the cached entries for a deleted file are dead weight; removing
/// them keeps the cache bounded.
fn prune_deleted(conn: &sqlite::Connection, namespace: &str, files: &[PathBuf]) -> Option<()> {
    // Build a set of live file paths for O(1) membership test, avoiding a
    // per-row `stat` syscall. `files` is the full on-disk set for this
    // namespace, so any stored path absent from it is a deleted source.
    let live: HashSet<String> = files
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let mut gone = Vec::new();
    {
        let mut st = conn
            .prepare("SELECT path FROM files WHERE namespace = ?")
            .ok()?;
        st.bind((1, namespace)).ok()?;
        while let Ok(sqlite::State::Row) = st.next() {
            if let Ok(path) = st.read::<String, _>(0)
                && !live.contains(&path)
            {
                gone.push(path);
            }
        }
    }
    for path in gone {
        let mut st = conn
            .prepare("DELETE FROM files WHERE namespace = ? AND path = ?")
            .ok()?;
        st.bind((1, namespace)).ok()?;
        st.bind((2, path.as_str())).ok()?;
        st.next().ok()?;
    }
    Some(())
}

/// One cached OpenCode message-database row: its id, a content hash for
/// freshness validation, and the parsed entry. The content hash is computed
/// from the `data` string so changes to the row content (without time_updated
/// changing) correctly invalidate the cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenCodeRow {
    pub id: String,
    /// Hash of the `data` field for cache validity checking.
    pub content_hash: u64,
    pub entry: CachedEntry,
}

/// Load the cached rows for an OpenCode database, keyed by [`file_metadata`]'s
/// `cache_key`. Returns `None` on any missing/corrupt cache or fingerprint
/// mismatch, mirroring the rest of this module's tolerance for stale or
/// unreadable cache state.
pub fn load_opencode_row_cache(cache_key: &str) -> Option<Vec<OpenCodeRow>> {
    let conn = open_db()?;
    let mut st = conn
        .prepare("SELECT rows FROM opencode WHERE db_key = ?")
        .ok()?;
    st.bind((1, cache_key)).ok()?;
    match st.next().ok()? {
        sqlite::State::Row => {
            let blob = st.read::<Vec<u8>, _>(0).ok()?;
            postcard::from_bytes::<Vec<OpenCodeRow>>(&blob).ok()
        }
        sqlite::State::Done => None,
    }
}

/// Persist the cached rows for an OpenCode database. Failures are silently
/// ignored: a missing cache simply forces a full re-parse next run.
// The unconditional INSERT OR REPLACE rewrites every row even on a 100%
// cache-hit run. It is load-bearing, not wasteful: the caller rebuilds from *this*
// run's rows only, so the full rewrite is what prunes rows deleted from the DB
// since the last run. Any future dirty-skip must preserve that delete semantics.
pub fn save_opencode_row_cache(cache_key: &str, rows: &[OpenCodeRow]) {
    let Some(conn) = open_db() else { return };
    let Ok(blob) = postcard::to_allocvec(rows) else {
        return;
    };
    // `cost_fingerprint` is retained as a NOT NULL schema column to avoid a
    // migration, but cost is now derived at load, so it is always written as 0
    // and never consulted for validity.
    let Ok(mut st) = conn.prepare(
        "INSERT OR REPLACE INTO opencode(db_key, cost_fingerprint, rows) VALUES (?, 0, ?)",
    ) else {
        return;
    };
    let _ = st.bind((1, cache_key));
    let _ = st.bind((2, &blob[..]));
    let _ = st.next();
}
/// Cached conditional-fetch state for a remote pricing JSON document.
pub struct CachedPricing {
    pub body: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Unix seconds of the last store; None for pre-migration rows (stale, refetch once).
    pub updated_at: Option<i64>,
}

/// Load cached pricing metadata for `url`. Returns `None` on any miss or
/// error — a missing cache simply forces an unconditional fetch.
pub fn load_pricing(url: &str) -> Option<CachedPricing> {
    let conn = open_db()?;
    let mut st = conn
        .prepare("SELECT etag, last_modified, body, updated_at FROM pricing WHERE url = ?")
        .ok()?;
    st.bind((1, url)).ok()?;
    match st.next().ok()? {
        sqlite::State::Row => Some(CachedPricing {
            etag: st.read::<Option<String>, _>(0).ok().flatten(),
            last_modified: st.read::<Option<String>, _>(1).ok().flatten(),
            body: st.read::<String, _>(2).ok()?,
            updated_at: st.read::<Option<i64>, _>(3).ok().flatten(),
        }),
        sqlite::State::Done => None,
    }
}

/// Persist pricing metadata for `url`. Best-effort: all IO errors are
/// silently ignored (the cache is an optimization, not correctness). `updated_at`
/// stamps the write so callers can serve warm copies without revalidating.
pub fn store_pricing(
    url: &str,
    body: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
    updated_at: i64,
) {
    let Some(conn) = open_db() else { return };
    let Ok(mut st) = conn.prepare(
        "INSERT OR REPLACE INTO pricing(url, etag, last_modified, body, updated_at) \
         VALUES (?, ?, ?, ?, ?)",
    ) else {
        return;
    };
    let _ = st.bind((1, url));
    let _ = st.bind((2, etag));
    let _ = st.bind((3, last_modified));
    let _ = st.bind((4, body));
    let _ = st.bind((5, updated_at));
    let _ = st.next();
}

/// Update only the freshness timestamp when `cached` is still the stored row.
/// Best-effort: all IO errors are silently ignored, like `store_pricing`.
pub fn refresh_pricing_if_unchanged(url: &str, cached: &CachedPricing, updated_at: i64) {
    let Some(conn) = open_db() else { return };
    let Ok(mut st) = conn.prepare(
        "UPDATE pricing SET updated_at = ? \
         WHERE url = ? AND body IS ? AND etag IS ? AND last_modified IS ? AND updated_at IS ?",
    ) else {
        return;
    };
    let _ = st.bind((1, updated_at));
    let _ = st.bind((2, url));
    let _ = st.bind((3, cached.body.as_str()));
    let _ = st.bind((4, cached.etag.as_deref()));
    let _ = st.bind((5, cached.last_modified.as_deref()));
    let _ = st.bind((6, cached.updated_at));
    let _ = st.next();
}

/// Stable dedup key for ledger writes. Prefers `message_id`; adapters without
/// one (Pi, Qwen) get a synthetic key spanning every dedup dimension so
/// distinct calls aren't collapsed. Uses token counts, not cost, for
/// repricing invariance.
fn entry_ledger_key(e: &LoadedEntry) -> String {
    if let Some(id) = &e.data.message.id {
        return id.clone();
    }
    let usage = &e.data.message.usage;
    // 0x1F (ASCII Unit Separator) joins the fields so a literal ':' in a
    // project/session value can't shift field boundaries and collide two distinct
    // keys. NUL ('\0') is unusable — it truncates SQLite TEXT keys. 0x1F never
    // appears in these values (project names, UUID session ids, model ids, integers).
    // Strip legacy `[pi] ` / `[openclaw] ` prefix so old ledger keys (prefixed) and
    // new raw keys deduplicate — preserves deleted-file spend without double-count.
    let model = e
        .model
        .as_deref()
        .map(strip_model_prefix)
        .unwrap_or_default();
    format!(
        "synth\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
        e.timestamp.as_millis(),
        e.project.as_ref(),
        e.session_id.as_ref(),
        model,
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_creation_input_tokens,
        usage.cache_read_input_tokens,
        e.extra_total_tokens,
    )
}

fn strip_model_prefix(model: &str) -> &str {
    if let Some(rest) = model.strip_prefix('[')
        && let Some(idx) = rest.find(']')
    {
        let after = &rest[idx + 1..];
        return after.strip_prefix(' ').unwrap_or(after);
    }
    model
}

fn normalize_ledger_key(key: &str) -> Cow<'_, str> {
    if !key.starts_with("synth\u{1f}") {
        return Cow::Borrowed(key);
    }
    let mut parts: Vec<&str> = key.split('\u{1f}').collect();
    if parts.len() >= 5 {
        let orig = parts[4];
        parts[4] = strip_model_prefix(orig);
        if parts[4].len() != orig.len() {
            return Cow::Owned(parts.join("\u{1f}"));
        }
    }
    Cow::Borrowed(key)
}
/// recorded at write-back time, not here.
///
/// `live_only` skips the re-emission, so deleted sources contribute nothing to
/// the result while their spend stays retained in the ledger for a later run.
fn merge_ledger(
    conn: &sqlite::Connection,
    namespace: &str,
    live: Vec<LoadedEntry>,
    live_only: bool,
    date_filter: Option<(&str, &str)>,
) -> Vec<LoadedEntry> {
    merge_ledger_with(conn, namespace, live, live_only, date_filter, None)
}

/// [`merge_ledger`] with optionally precomputed key sets. `Some` sets must be the
/// full unwindowed scan; `None` behaves exactly like [`merge_ledger`].
fn merge_ledger_with(
    conn: &sqlite::Connection,
    namespace: &str,
    live: Vec<LoadedEntry>,
    live_only: bool,
    date_filter: Option<(&str, &str)>,
    precomputed: Option<(FxHashSet<String>, Vec<String>)>,
) -> Vec<LoadedEntry> {
    // Blobs load under raw keys, so normalization stays at comparison time.
    let mut seen: FxHashSet<String>;
    let owned_raw: Vec<String>;
    match precomputed {
        Some((live_keys, raw)) => {
            seen = live_keys;
            owned_raw = raw;
        }
        None => {
            seen = FxHashSet::with_capacity_and_hasher(live.len(), Default::default());
            seen.extend(live.iter().map(entry_ledger_key));
            owned_raw = Vec::new();
        }
    }
    let stored_raw: &[String] = &owned_raw;
    let mut out = live;

    if !live_only {
        // Fast-exit: skip full ledger scan when namespace has no retained rows.
        // This is the common case for empty adapters (amp, goose etc) and costs ~µs
        // vs scanning 0 rows via full index walk. Lossless: SELECT 1 LIMIT 1 is exact.
        // When a window is set, check only that window (including NULL legacy rows).
        let has_rows = if let Some((since, until)) = date_filter {
            conn.prepare(
                "SELECT 1 FROM ledger WHERE namespace = ? AND (date BETWEEN ? AND ? OR date IS NULL) LIMIT 1",
            )
            .ok()
            .and_then(|mut st| {
                st.bind((1, namespace)).ok()?;
                st.bind((2, since)).ok()?;
                st.bind((3, until)).ok()?;
                Some(matches!(st.next(), Ok(sqlite::State::Row)))
            })
            .unwrap_or(true)
        } else if let Ok(mut st) = conn.prepare("SELECT 1 FROM ledger WHERE namespace = ? LIMIT 1")
        {
            let _ = st.bind((1, namespace));
            matches!(st.next(), Ok(sqlite::State::Row))
        } else {
            true
        };
        if !has_rows {
            return out;
        }
        // Read keys before blobs: a key missing from the live set marks a deleted
        // source whose blob must be decoded, while a key already covered by `out`
        // is skipped — so a warm run with no deletions decodes no ledger blobs.
        // Windowed: SQL already filtered by date (plus NULL), but NULL rows need
        // Rust-side date check via the blob so legacy rows don't leak outside the window.
        // A windowed filter always re-scans so date/NULL semantics stay in SQL.
        let scanned;
        let stored_keys: &[String] =
            if date_filter.is_none() && !stored_raw.is_empty() {
                stored_raw
            } else {
                scanned = load_ledger_keys(conn, namespace, date_filter);
                &scanned
            };
        let deleted_keys: Vec<String> = stored_keys
            .iter()
            .filter(|k| !seen.contains(normalize_ledger_key(k).as_ref()))
            .cloned()
            .collect();
        for key in deleted_keys {
            if let Some(blob) = load_ledger_blob(conn, namespace, &key)
                && let Ok(entry) = postcard::from_bytes::<LedgerEntry>(&blob)
            {
                if let Some((since, until)) = date_filter {
                    let d = entry.base.date.as_str();
                    if d < since || d > until {
                        continue;
                    }
                }
                let mut loaded = entry.into_loaded(key.clone());
                if let Some(m) = loaded.model.as_deref() {
                    let stripped = strip_model_prefix(m).to_string();
                    if stripped != m {
                        loaded.model = Some(stripped);
                        if let Some(dm) = loaded.data.message.model.as_mut() {
                            *dm = strip_model_prefix(dm).to_string();
                        }
                    }
                }
                seen.insert(normalize_ledger_key(&key).into_owned());
                out.push(loaded);
            }
        }
    }
    out
}

/// Append `entries` to the ledger in the caller's transaction. The primary key
/// makes inserts idempotent, so spend is recorded at most once.
fn append_entries_to_ledger(
    conn: &sqlite::Connection,
    namespace: &str,
    entries: &[LoadedEntry],
) -> Option<()> {
    let mut st = conn
        .prepare(
            "INSERT OR IGNORE INTO ledger(namespace, dedup_key, date, entry) \
             VALUES (?, ?, ?, ?)",
        )
        .ok()?;
    for entry in entries {
        append_ledger_row(&mut st, namespace, entry)?;
    }
    Some(())
}

fn append_ledger_row(
    st: &mut sqlite::Statement<'_>,
    namespace: &str,
    entry: &LoadedEntry,
) -> Option<()> {
    let key = entry_ledger_key(entry);
    // The dedup key is the row's primary-key column, so the blob omits every
    // dedup-identity field; the id is restored from the column on read.
    let blob = postcard::to_allocvec(&LedgerEntry::from(entry)).ok()?;
    st.reset().ok()?;
    st.bind((1, namespace)).ok()?;
    st.bind((2, key.as_str())).ok()?;
    st.bind((3, entry.date.as_str())).ok()?;
    st.bind((4, &blob[..])).ok()?;
    st.next().ok()?;
    Some(())
}

/// Load every ledger dedup key for `namespace` without decoding the entry blobs.
/// When `date_filter` is `Some((since, until))`, only keys with `date` in that
/// inclusive range are returned; `NULL` dates (legacy rows) are included and
/// filtered in Rust via the blob's `LedgerEntry.base.date` so windowed queries
/// don't miss pre-migration rows. `None` is the full-scan path for `daily` etc.
fn load_ledger_keys(
    conn: &sqlite::Connection,
    namespace: &str,
    date_filter: Option<(&str, &str)>,
) -> Vec<String> {
    let mut out = Vec::new();
    let sql = if date_filter.is_some() {
        "SELECT dedup_key, date FROM ledger WHERE namespace = ? AND (date BETWEEN ? AND ? OR date IS NULL)"
    } else {
        "SELECT dedup_key FROM ledger WHERE namespace = ?"
    };
    let Ok(mut st) = conn.prepare(sql) else {
        return out;
    };
    if st.bind((1, namespace)).is_err() {
        return out;
    }
    if let Some((since, until)) = date_filter
        && (st.bind((2, since)).is_err() || st.bind((3, until)).is_err())
    {
        return out;
    }
    while let Ok(sqlite::State::Row) = st.next() {
        if let Ok(key) = st.read::<String, _>(0) {
            out.push(key);
        }
    }
    out
}

/// Fetch the postcard-encoded entry blob for one `(namespace, dedup_key)`, or
/// `None` if the row is missing.
fn load_ledger_blob(
    conn: &sqlite::Connection,
    namespace: &str,
    dedup_key: &str,
) -> Option<Vec<u8>> {
    let mut st = conn
        .prepare("SELECT entry FROM ledger WHERE namespace = ? AND dedup_key = ?")
        .ok()?;
    st.bind((1, namespace)).ok()?;
    st.bind((2, dedup_key)).ok()?;
    if let Ok(sqlite::State::Row) = st.next() {
        return st.read::<Vec<u8>, _>(0).ok();
    }
    None
}

/// Retain `live` entries for `namespace` through the ledger for sources that do
/// not flow through [`load_with_cache`] (e.g. the OpenCode SQLite database,
/// which is read directly rather than from per-file caches).
///
/// Pass the *full* current live set for the namespace — empty when the source is
/// gone — so deleted spend is re-emitted from the ledger exactly once. Use a
/// namespace distinct from any [`load_with_cache`] caller so the two live sets
/// never appear to "delete" each other. `live_only` carries the same meaning as
/// in [`load_with_cache`]: when set, retained spend for gone sources is suppressed.
pub fn retain_via_ledger(
    namespace: &str,
    live: Vec<LoadedEntry>,
    live_only: bool,
    date_filter: Option<(&str, &str)>,
) -> Vec<LoadedEntry> {
    match open_db() {
        Some(conn) => {
            let cover_started =
                std::env::var_os("CCUSAGE_DEBUG_TIMING").is_some().then(std::time::Instant::now);
            let sets = load_ledger_key_sets(&conn, namespace);
            let mut seen =
                FxHashSet::with_capacity_and_hasher(live.len(), Default::default());
            let mut missing = Vec::new();
            for (index, entry) in live.iter().enumerate() {
                let key = entry_ledger_key(entry);
                if !sets.stored_normalized.contains(&key) {
                    missing.push(index);
                }
                seen.insert(key);
            }
            if let Some(cover_started) = cover_started {
                eprintln!(
                    "[timing] ledger:{namespace}: cover_check {:?} missing={}",
                    cover_started.elapsed(),
                    missing.len()
                );
            }
            let append_started =
                std::env::var_os("CCUSAGE_DEBUG_TIMING").is_some().then(std::time::Instant::now);
            if !missing.is_empty() && conn.execute("BEGIN IMMEDIATE").is_ok() {
                // No per-file cache here, so append in our own transaction.
                let ok = append_missing_ledger_entries(&conn, namespace, &live, &missing)
                    .is_some();
                let _ = conn.execute(if ok { "COMMIT" } else { "ROLLBACK" });
            }
            if let Some(append_started) = append_started {
                eprintln!(
                    "[timing] ledger:{namespace}: append {:?} appended={}",
                    append_started.elapsed(),
                    !missing.is_empty()
                );
            }
            merge_ledger_with(
                &conn,
                namespace,
                live,
                live_only,
                date_filter,
                Some((seen, sets.stored_raw)),
            )
        }
        None => live,
    }
}

/// Ledger keys for one namespace, scanned once per retain and shared by the
/// append filter and the merge below.
struct LedgerKeySets {
    stored_raw: Vec<String>,
    stored_normalized: FxHashSet<String>,
}

/// Scan the unwindowed stored keys once; a windowed subset cannot prove coverage
/// because the append covers all dates. Failures surface as empty sets (fail-open).
fn load_ledger_key_sets(conn: &sqlite::Connection, namespace: &str) -> LedgerKeySets {
    let stored_raw = load_ledger_keys(conn, namespace, None);
    let mut stored_normalized =
        FxHashSet::with_capacity_and_hasher(stored_raw.len(), Default::default());
    stored_normalized
        .extend(stored_raw.iter().map(|key| normalize_ledger_key(key).into_owned()));
    LedgerKeySets {
        stored_raw,
        stored_normalized,
    }
}

fn append_missing_ledger_entries(
    conn: &sqlite::Connection,
    namespace: &str,
    live: &[LoadedEntry],
    missing: &[usize],
) -> Option<()> {
    let mut st = conn
        .prepare(
            "INSERT OR IGNORE INTO ledger(namespace, dedup_key, date, entry) \
             VALUES (?, ?, ?, ?)",
        )
        .ok()?;
    for &index in missing {
        append_ledger_row(&mut st, namespace, &live[index])?;
    }
    Some(())
}

/// Parse each fresh path with `parse_file`, returning results aligned 1:1 with
/// `fresh`. Uses size-balanced parallel chunks when worthwhile.
fn parse_fresh_files<F>(
    fresh: &[PathBuf],
    single_thread: bool,
    parse_file: &F,
) -> crate::Result<Vec<Vec<LoadedEntry>>>
where
    F: Fn(&Path) -> crate::Result<Vec<LoadedEntry>> + Sync,
{
    let worker_count = if single_thread {
        1
    } else {
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .min(fresh.len())
    };
    if worker_count <= 1 {
        return fresh.iter().map(|path| parse_file(path)).collect();
    }

    let chunks = chunk_file_indexes_by_size(fresh, worker_count);
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            handles.push(scope.spawn(move || {
                chunk
                    .into_iter()
                    .map(|index| (index, parse_file(&fresh[index])))
                    .collect::<Vec<_>>()
            }));
        }
        // Index-tagged results need no "was every slot filled" bookkeeping:
        // `chunk_file_indexes_by_size` partitions `0..fresh.len()` exhaustively,
        // so sorting the tagged results back into index order restores the
        // original file order regardless of which worker finished first.
        let mut results: Vec<(usize, crate::Result<Vec<LoadedEntry>>)> = handles
            .into_iter()
            // Fail open like load_namespace_shard: a panicked worker's
            // chunk reads as missing instead of aborting the whole load.
            .flat_map(|handle| handle.join().unwrap_or_default())
            .collect();
        results.sort_by_key(|(index, _)| *index);
        results
            .into_iter()
            .map(|(_, entries)| entries)
            .collect::<crate::Result<Vec<_>>>()
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use ccusage_test_support::CacheEnv;

    use super::*;
    use crate::date_utils::TimestampMs;
    use crate::types::{TokenUsageRaw, UsageEntry, UsageMessage};

    fn sample_entry() -> LoadedEntry {
        LoadedEntry {
            data: UsageEntry {
                session_id: Some("session-a".to_string()),
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                version: None,
                message: UsageMessage {
                    usage: TokenUsageRaw::default(),
                    model: Some("claude-test".to_string()),
                    id: Some("msg-a".to_string()),

                    provider: None,
                },
                cost_usd: None,
                request_id: Some("req-a".to_string()),
                is_api_error_message: None,
                is_sidechain: None,
            },
            timestamp: TimestampMs::from_millis(1_000),
            date: "2026-01-01".to_string(),
            project: Arc::from("proj"),
            session_id: Arc::from("session-a"),
            project_path: Arc::from("/tmp/proj"),
            cost: 0.0,
            extra_total_tokens: 0,
            credits: None,
            message_count: None,
            model: Some("claude-test".to_string()),
            usage_limit_reset_time: None,
            missing_pricing_model: None,
        }
    }

    #[test]
    fn cache_roundtrip_preserves_data_timestamp() {
        // A cache hit must reproduce `data.timestamp` byte-for-byte: openclaw/pi/qwen
        // build their dedup `entry_id` from it, so a warm load that blanked or
        // reformatted it would diverge from a cold load and silently undercount.
        let cold = sample_entry();
        let warm: LoadedEntry = CachedEntry::from(&cold).into();
        assert_eq!(warm.data.timestamp, cold.data.timestamp);
    }

    #[test]
    fn prefers_xdg_cache_home_on_any_platform() {
        let dir = dirs_cache_dir_from_env(
            Some(std::ffi::OsString::from("/tmp/xdg")),
            Some(std::ffi::OsString::from("C:\\Users\\u\\AppData\\Local")),
            Some(PathBuf::from("/home/u")),
            true,
        );
        assert_eq!(dir, Some(PathBuf::from("/tmp/xdg")));
    }

    #[test]
    fn windows_uses_localappdata_without_xdg() {
        let dir = dirs_cache_dir_from_env(
            None,
            Some(std::ffi::OsString::from("C:\\Users\\u\\AppData\\Local")),
            Some(PathBuf::from("C:\\Users\\u")),
            true,
        );
        assert_eq!(dir, Some(PathBuf::from("C:\\Users\\u\\AppData\\Local")));
    }

    #[test]
    fn windows_falls_back_to_home_dot_cache_without_localappdata() {
        let home = PathBuf::from("C:\\Users\\u");
        let dir = dirs_cache_dir_from_env(None, None, Some(home.clone()), true);
        assert_eq!(dir, Some(home.join(".cache")));
    }

    #[test]
    fn unix_ignores_localappdata() {
        let dir = dirs_cache_dir_from_env(
            None,
            Some(std::ffi::OsString::from("C:\\Users\\u\\AppData\\Local")),
            Some(PathBuf::from("/home/u")),
            false,
        );
        assert_eq!(dir, Some(PathBuf::from("/home/u/.cache")));
    }

    #[test]
    fn returns_none_without_any_cache_base() {
        assert_eq!(dirs_cache_dir_from_env(None, None, None, false), None);
        assert_eq!(dirs_cache_dir_from_env(None, None, None, true), None);
    }

    fn write_source(name: &str, contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("ccusage-src-{name}.jsonl"));
        fs::write(&path, contents).unwrap();
        path
    }

    /// Build a one-file stored snapshot mirroring what a cold run would persist,
    /// so [`partition_files`] can be exercised directly.
    fn stored_snapshot(
        path: &Path,
        meta: &FileMetadata,
        entries: &[LoadedEntry],
    ) -> HashMap<String, StoredFile> {
        let mut map = HashMap::new();
        map.insert(
            path.to_string_lossy().to_string(),
            StoredFile {
                mtime: meta.mtime_epoch_millis,
                size: meta.size,
                fingerprint: meta.fingerprint,
                entries: encode_entries(entries).unwrap(),
            },
        );
        map
    }

    /// Source paths recorded in the `files` table for `namespace`.
    fn file_paths(namespace: &str) -> HashSet<String> {
        let conn = open_db().unwrap();
        let mut st = conn
            .prepare("SELECT path FROM files WHERE namespace = ?")
            .unwrap();
        st.bind((1, namespace)).unwrap();
        let mut out = HashSet::new();
        while let Ok(sqlite::State::Row) = st.next() {
            out.insert(st.read::<String, _>(0).unwrap());
        }
        out
    }

    /// Insert a raw ledger row, used to simulate duplicate records that the
    /// production path would never write itself.
    fn insert_ledger_row(namespace: &str, key: &str, entry: &LoadedEntry) {
        let conn = open_db().unwrap();
        let blob = postcard::to_allocvec(&LedgerEntry::from(entry)).unwrap();
        // Try with date column (v2 schema); fallback to legacy schema without date.
        if let Ok(mut st) = conn.prepare(
            "INSERT OR IGNORE INTO ledger(namespace, dedup_key, date, entry) VALUES (?, ?, ?, ?)",
        ) {
            st.bind((1, namespace)).unwrap();
            st.bind((2, key)).unwrap();
            st.bind((3, entry.date.as_str())).unwrap();
            st.bind((4, &blob[..])).unwrap();
            st.next().unwrap();
        } else if let Ok(mut st) = conn
            .prepare("INSERT OR IGNORE INTO ledger(namespace, dedup_key, entry) VALUES (?, ?, ?)")
        {
            st.bind((1, namespace)).unwrap();
            st.bind((2, key)).unwrap();
            st.bind((3, &blob[..])).unwrap();
            st.next().unwrap();
        }
    }

    /// A v1 ledger (no `date` column) is migrated in place: the column
    /// appears and the index is created, while pre-existing rows keep NULL date.
    #[test]
    fn migrate_adds_date_to_legacy_ledger_table() {
        let _env = CacheEnv::new("ledger-migrate-legacy");
        let dir = cache_dir().expect("cache dir");
        std::fs::create_dir_all(&dir).expect("create cache dir");
        let legacy = sqlite::open(dir.join(DB_FILE)).expect("open legacy db");
        legacy
            .execute(
                "CREATE TABLE files (                     path TEXT PRIMARY KEY,                     namespace TEXT NOT NULL,                     mtime INTEGER NOT NULL,                     size INTEGER NOT NULL,                     cost_fingerprint INTEGER NOT NULL,                     entries BLOB NOT NULL                 );                 CREATE TABLE ledger (                     namespace TEXT NOT NULL,                     dedup_key TEXT NOT NULL,                     entry BLOB NOT NULL,                     PRIMARY KEY (namespace, dedup_key)                 );                 CREATE TABLE opencode (                     db_key TEXT PRIMARY KEY,                     cost_fingerprint INTEGER NOT NULL,                     rows BLOB NOT NULL                 );                 CREATE TABLE pricing (                     url TEXT PRIMARY KEY,                     etag TEXT,                     last_modified TEXT,                     body TEXT NOT NULL,                     updated_at INTEGER                 );                 CREATE TABLE schema_meta (                     name TEXT PRIMARY KEY,                     version INTEGER NOT NULL                 );                 INSERT INTO schema_meta(name, version) VALUES ('files', 1), ('ledger', 1), ('opencode', 1), ('pricing', 2);                 INSERT INTO ledger(namespace, dedup_key, entry)                      VALUES ('claude', 'k1', randomblob(16));",
            )
            .expect("seed legacy ledger");
        drop(legacy);

        let conn = open_db().expect("open_db should migrate the legacy ledger");
        // Column must exist now.
        conn.prepare("SELECT date FROM ledger LIMIT 1")
            .expect("date column must exist after migration");
        // Index must exist.
        let mut st = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name='ledger_namespace_date'")
            .unwrap();
        assert_eq!(
            st.next().unwrap(),
            sqlite::State::Row,
            "ledger_namespace_date index must exist"
        );
        // Existing row preserved with NULL date.
        let mut st = conn
            .prepare("SELECT date FROM ledger WHERE namespace='claude'")
            .unwrap();
        st.next().unwrap();
        assert!(
            st.read::<Option<String>, _>(0).unwrap().is_none(),
            "legacy row date stays NULL"
        );
        assert_eq!(
            read_schema_version(&conn, "ledger"),
            Some(LEDGER_SCHEMA_VERSION),
        );
    }

    #[test]
    fn round_trips_cached_entries_for_unchanged_file() {
        let _env = CacheEnv::new("roundtrip");
        let src = write_source("roundtrip", "line\n");
        let meta = file_metadata(&src).expect("metadata");
        let stored = stored_snapshot(&src, &meta, &[sample_entry()]);

        let part = partition_files(
            std::slice::from_ref(&src),
            &stored,
            &Freshness::FileStat,
            false,
        );
        assert_eq!(part.cached.len(), 1);
        assert!(part.fresh.is_empty());
        assert_eq!(part.cached[0].1.entries[0].session_id.as_ref(), "session-a");
        let _ = fs::remove_file(&src);
    }

    #[test]
    fn invalidates_cache_when_file_changes() {
        let _env = CacheEnv::new("invalidate");
        let src = write_source("invalidate", "line\n");
        let meta = file_metadata(&src).expect("metadata");
        let stored = stored_snapshot(&src, &meta, &[sample_entry()]);

        // Mutate the source so its size no longer matches the fingerprint.
        fs::write(&src, "line\nlonger\n").unwrap();

        let part = partition_files(
            std::slice::from_ref(&src),
            &stored,
            &Freshness::FileStat,
            false,
        );
        assert!(part.cached.is_empty());
        assert_eq!(part.fresh.len(), 1);
        assert_eq!(part.fresh[0].1.path, src);
        let _ = fs::remove_file(&src);
    }

    #[test]
    fn load_with_cache_serves_unchanged_files_from_cache() {
        let _env = CacheEnv::new("load-with-cache-hit");
        let src = write_source("load-with-cache-hit", "line\n");

        let calls = std::sync::atomic::AtomicUsize::new(0);
        let cold = load_with_cache(
            "test",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_path| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(vec![sample_entry()])
            },
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(cold.len(), 1);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        let warm = load_with_cache(
            "test",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_path| {
                panic!("parse_file should not run for a cached file");
            },
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(warm.len(), 1);
        assert_eq!(warm[0].session_id.as_ref(), "session-a");

        let _ = fs::remove_file(&src);
    }

    #[test]
    fn load_with_cache_does_not_cache_errored_files() {
        let _env = CacheEnv::new("load-with-cache-error");
        let src = write_source("load-with-cache-error", "line\n");

        // First run: parse_file errors for an unchanged file. load_with_cache
        // propagates the error and caches nothing, so a later run re-parses
        // instead of serving a poisoned empty entry forever.
        let errored = load_with_cache(
            "test",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |path| Err(crate::cli_error(format!("boom: {}", path.display()))),
            |_| {},
            None,
        );
        assert!(errored.is_err());

        // Second run on the same unchanged file must re-parse, not serve a
        // poisoned empty cache entry.
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let recovered = load_with_cache(
            "test",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_path| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(vec![sample_entry()])
            },
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(recovered.len(), 1);

        let _ = fs::remove_file(&src);
    }

    #[test]
    fn partitions_into_cached_and_fresh() {
        let _env = CacheEnv::new("partition");

        let cached_src = write_source("partition-cached", "a\n");
        let cached_meta = file_metadata(&cached_src).unwrap();
        let stored = stored_snapshot(&cached_src, &cached_meta, &[sample_entry()]);

        let fresh_src = write_source("partition-fresh", "b\n");

        let part = partition_files(
            &[cached_src.clone(), fresh_src.clone()],
            &stored,
            &Freshness::FileStat,
            false,
        );

        assert_eq!(part.cached.len(), 1);
        assert_eq!(part.fresh.len(), 1);
        assert_eq!(part.fresh[0].1.path, fresh_src);

        let _ = fs::remove_file(&cached_src);
        let _ = fs::remove_file(&fresh_src);
    }

    // -------------------------------------------------------------------------
    // Ledger retention tests
    // -------------------------------------------------------------------------

    /// Retain-on-delete: cold-load a fixture file (cached); delete the source
    /// file; the warm `load_with_cache` still returns its entries (now from the
    /// ledger) and the deleted file's `files` row is pruned.
    #[test]
    fn retain_on_delete_ledger_emits_entries_after_source_deleted() {
        let _env = CacheEnv::new("retain-on-delete");
        let src = write_source("retain-on-delete", "line\n");

        let cold = load_with_cache(
            "test-ns",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(vec![sample_entry()]),
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(cold.len(), 1);

        // Delete the source file — simulates "deleted chat".
        fs::remove_file(&src).unwrap();

        let warm = load_with_cache(
            "test-ns",
            &[],
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(Vec::new()),
            |_| {},
            None,
        )
        .unwrap();

        assert_eq!(warm.len(), 1, "ledger entry must be emitted after deletion");
        assert_eq!(warm[0].session_id.as_ref(), "session-a");

        assert!(
            !file_paths("test-ns").contains(&src.to_string_lossy().to_string()),
            "files row must be removed after deletion"
        );
    }

    /// Retention for entries without a natural `message_id`.
    ///
    /// Adapters such as Pi and Qwen emit entries with `message_id = None`.
    /// A synthetic key must be assigned on ledger write so spend survives
    /// source-file deletion — previously these entries were silently dropped.
    #[test]
    fn retain_on_delete_no_message_id_entry() {
        let _env = CacheEnv::new("retain-no-id");
        let src = write_source("retain-no-id", "line\n");

        let mut entry = sample_entry();
        entry.data.message.id = None;
        entry.cost = 0.05;

        let cold = load_with_cache(
            "test-ns-noid",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(vec![entry.clone()]),
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(cold.len(), 1);

        // Delete the source file — simulates adapter log removal.
        fs::remove_file(&src).unwrap();

        let warm = load_with_cache(
            "test-ns-noid",
            &[],
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(Vec::new()),
            |_| {},
            None,
        )
        .unwrap();

        assert_eq!(
            warm.len(),
            1,
            "entry with no message_id must be re-emitted via synthetic ledger key after deletion"
        );
        assert_eq!(warm[0].session_id.as_ref(), "session-a");
        assert!(
            (warm[0].cost - 0.05).abs() < 1e-9,
            "cost must be preserved through synthetic-key ledger round-trip"
        );
    }

    #[test]
    fn covered_retain_skips_append_without_losing_ledger() {
        let _env = CacheEnv::new("covered-skip");
        let live = vec![sample_entry()];
        let first = retain_via_ledger("test-ns-covered", live.clone(), false, None);
        assert_eq!(first.len(), 1);
        let sets = load_ledger_key_sets(
            &open_db().expect("cache db must open"),
            "test-ns-covered",
        );
        assert_eq!(sets.stored_raw.len(), 1);
        let second = retain_via_ledger("test-ns-covered", live, false, None);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].session_id.as_ref(), "session-a");
        let reemitted = retain_via_ledger("test-ns-covered", Vec::new(), false, None);
        assert_eq!(
            reemitted.len(),
            1,
            "skipped append must not lose the ledgered entry"
        );
    }

    /// Two entries that differ only by a colon-boundary shift in project and
    /// session must produce DISTINCT synthetic ledger keys. Before the `\u{1f}`
    /// separator, `project="a:b"` + `session="c"` and `project="a"` +
    /// `session="b:c"` both formatted to `synth:a:b:c:...` — a silent spend
    /// loss when the second entry was INSERT-OR-IGNOREd as a duplicate.
    #[test]
    fn synthetic_key_no_colon_boundary_collision() {
        let mut a = sample_entry();
        a.data.message.id = None;
        a.project = Arc::from("a:b");
        a.session_id = Arc::from("c");

        let mut b = sample_entry();
        b.data.message.id = None;
        b.project = Arc::from("a");
        b.session_id = Arc::from("b:c");

        let key_a = entry_ledger_key(&a);
        let key_b = entry_ledger_key(&b);
        assert_ne!(
            key_a, key_b,
            "colon-boundary shift must not produce the same synthetic key"
        );
    }

    /// Two synthetic-keyed entries that share session, timestamp, and
    /// input/output counts but differ in model must NOT collapse in the ledger.
    /// The widened synthetic key spans model and every token bucket, so both
    /// survive source deletion; a coarser key would silently drop one's spend.
    #[test]
    fn synthetic_key_separates_distinct_models() {
        let _env = CacheEnv::new("synth-distinct-models");
        let src = write_source("synth-distinct-models", "line\n");

        let mut a = sample_entry();
        a.data.message.id = None;
        a.model = Some("model-a".to_string());
        a.data.message.model = Some("model-a".to_string());
        a.cost = 0.01;

        let mut b = sample_entry();
        b.data.message.id = None;
        b.model = Some("model-b".to_string());
        b.data.message.model = Some("model-b".to_string());
        b.cost = 0.02;

        let cold = load_with_cache(
            "ns-synth",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(vec![a.clone(), b.clone()]),
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(cold.len(), 2);

        // Source removed — both must re-emit from the ledger, not collapse.
        fs::remove_file(&src).unwrap();
        let warm = load_with_cache(
            "ns-synth",
            &[],
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(Vec::new()),
            |_| {},
            None,
        )
        .unwrap();

        assert_eq!(
            warm.len(),
            2,
            "distinct-model entries must not share a synthetic ledger key"
        );
        let total: f64 = warm.iter().map(|e| e.cost).sum();
        assert!(
            (total - 0.03).abs() < 1e-9,
            "both entries' spend must survive deletion"
        );
    }

    /// Two synthetic-keyed entries that share every field except project must
    /// not collapse in the ledger — pi emits entries in multiple projects under
    /// one namespace, and its live dedup key is project-scoped.
    #[test]
    fn synthetic_key_separates_distinct_projects() {
        let _env = CacheEnv::new("synth-distinct-projects");
        let src = write_source("synth-distinct-projects", "line\n");

        let mut a = sample_entry();
        a.data.message.id = None;
        a.project = Arc::from("project-a");
        a.cost = 0.01;

        let mut b = sample_entry();
        b.data.message.id = None;
        b.project = Arc::from("project-b");
        b.cost = 0.02;

        let cold = load_with_cache(
            "ns-proj",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(vec![a.clone(), b.clone()]),
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(cold.len(), 2);

        fs::remove_file(&src).unwrap();
        let warm = load_with_cache(
            "ns-proj",
            &[],
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(Vec::new()),
            |_| {},
            None,
        )
        .unwrap();

        assert_eq!(
            warm.len(),
            2,
            "distinct-project entries must not share a synthetic ledger key"
        );
        let total: f64 = warm.iter().map(|e| e.cost).sum();
        assert!(
            (total - 0.03).abs() < 1e-9,
            "both entries' spend must survive"
        );
    }

    /// Property: the synthetic ledger key is injective across every billable
    /// dimension it encodes. Mutating any single dimension in isolation must
    /// change the key, and no two single-field mutations may collide. This pins
    /// the full field set — if a future edit drops a dimension from
    /// [`entry_ledger_key`], the row for that dimension fails here, rather than
    /// silently collapsing two distinct calls' spend in the ledger.
    #[test]
    fn synthetic_key_is_injective_per_billable_dimension() {
        type Mutation = (&'static str, fn(&mut LoadedEntry));
        let mutations: &[Mutation] = &[
            ("timestamp", |e| {
                e.timestamp = TimestampMs::from_millis(2_000)
            }),
            ("project", |e| e.project = Arc::from("other-project")),
            ("session_id", |e| e.session_id = Arc::from("other-session")),
            ("model", |e| e.model = Some("other-model".to_string())),
            ("input_tokens", |e| e.data.message.usage.input_tokens = 7),
            ("output_tokens", |e| e.data.message.usage.output_tokens = 7),
            ("cache_creation", |e| {
                e.data.message.usage.cache_creation_input_tokens = 7
            }),
            ("cache_read", |e| {
                e.data.message.usage.cache_read_input_tokens = 7
            }),
            ("extra_total_tokens", |e| e.extra_total_tokens = 7),
        ];

        let mut base = sample_entry();
        base.data.message.id = None; // force the synthetic path
        let base_key = entry_ledger_key(&base);

        let mut seen = HashSet::new();
        seen.insert(base_key.clone());
        for (dimension, mutate) in mutations {
            let mut variant = base.clone();
            mutate(&mut variant);
            let key = entry_ledger_key(&variant);
            assert_ne!(
                key, base_key,
                "mutating {dimension} must change the synthetic ledger key"
            );
            assert!(
                seen.insert(key),
                "{dimension} produced a key already seen — two dimensions collide"
            );
        }
    }

    /// Resume / no double-count: file deleted (spend retained in the ledger),
    /// then recreated and re-parsed; the next `load_with_cache` returns the live
    /// entry exactly once — the live copy wins over the ledger record sharing its
    /// dedup key.
    #[test]
    fn resume_no_double_count() {
        let _env = CacheEnv::new("resume-evict");
        let src = write_source("resume-evict", "original\n");

        let _ = load_with_cache(
            "ns",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(vec![sample_entry()]),
            |_| {},
            None,
        );

        // Delete: spend is retained in the ledger.
        fs::remove_file(&src).unwrap();
        let _ = load_with_cache(
            "ns",
            &[],
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(Vec::new()),
            |_| {},
            None,
        );

        fs::write(&src, "new content\n").unwrap();
        let mut new_entry = sample_entry();
        new_entry.session_id = Arc::from("session-new");

        // Live run: exactly 1 entry (live), not 2 (live + ledger).
        let live = load_with_cache(
            "ns",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(vec![new_entry.clone()]),
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(live.len(), 1, "must not double-count live + ledger");
        assert_eq!(live[0].session_id.as_ref(), "session-new");

        let _ = fs::remove_file(&src);
    }

    /// Deleted-source tracking is lossless through the slim ledger: an entry
    /// carrying dedup-only identity (`request_id`/`is_sidechain`) is re-emitted
    /// after its source file is gone with every *tracking* field intact, even
    /// though those identity fields are no longer persisted.
    #[test]
    fn deleted_source_tracking_survives_without_identity() {
        let _env = CacheEnv::new("slim-ledger");
        let src = write_source("slim-ledger", "original\n");

        let mut entry = sample_entry();
        entry.cost = 1.25;
        entry.extra_total_tokens = 7;
        entry.credits = Some(3.0);
        entry.message_count = Some(2);
        entry.model = Some("claude-track".to_string());
        entry.data.message.usage = TokenUsageRaw {
            input_tokens: 11,
            output_tokens: 22,
            cache_creation_input_tokens: 5,
            cache_read_input_tokens: 9,
            speed: None,
            cache_creation: None,
        };
        // Dedup-only identity that the slim ledger intentionally drops.
        entry.data.request_id = Some("req-track".to_string());
        entry.data.is_sidechain = Some(true);

        let _ = load_with_cache(
            "ns",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(vec![entry.clone()]),
            |_| {},
            None,
        );
        fs::remove_file(&src).unwrap();

        let warm = load_with_cache(
            "ns",
            &[],
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(Vec::new()),
            |_| {},
            None,
        )
        .unwrap();

        assert_eq!(
            warm.len(),
            1,
            "deleted spend must be re-emitted exactly once"
        );
        let got = &warm[0];
        assert!((got.cost - 1.25).abs() < 1e-9, "cost must survive");
        assert_eq!(got.extra_total_tokens, 7);
        assert_eq!(got.credits, Some(3.0));
        assert_eq!(got.message_count, Some(2));
        assert_eq!(got.model.as_deref(), Some("claude-track"));
        assert_eq!(got.data.message.usage.input_tokens, 11);
        assert_eq!(got.data.message.usage.output_tokens, 22);
        assert_eq!(got.data.message.usage.cache_creation_input_tokens, 5);
        assert_eq!(got.data.message.usage.cache_read_input_tokens, 9);
        // Identity restored from the key column; dedup-only fields are gone.
        assert_eq!(got.data.message.id.as_deref(), Some("msg-a"));
        assert_eq!(got.data.request_id, None);
        assert_eq!(got.data.is_sidechain, None);

        let _ = fs::remove_file(&src);
    }

    /// Two-phase ledger read: a warm run with a deleted source re-emits its
    /// spend, while a warm run with ALL sources present returns only the live
    /// copies (zero blob reads from the ledger).
    #[test]
    fn two_phase_ledger_deleted_source_reemitted_all_present_unchanged() {
        let _env = CacheEnv::new("two-phase-ledger");
        let src = write_source("two-phase-ledger", "line\n");

        let mut entry_a = sample_entry();
        entry_a.cost = 0.42;
        entry_a.model = Some("model-a".to_string());
        let cold = load_with_cache(
            "ns-two-phase",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(vec![entry_a.clone()]),
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(cold.len(), 1);

        // Delete source -> spend retained in ledger only.
        fs::remove_file(&src).unwrap();
        let warm_deleted = load_with_cache(
            "ns-two-phase",
            &[],
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(Vec::new()),
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(
            warm_deleted.len(),
            1,
            "deleted source must re-emit from ledger"
        );
        assert!((warm_deleted[0].cost - 0.42).abs() < 1e-9);

        let src2 = write_source("two-phase-ledger", "line2\n");
        let mut entry_b = sample_entry();
        entry_b.session_id = Arc::from("session-b");
        entry_b.cost = 0.99;
        entry_b.model = Some("model-b".to_string());
        // entry_a is still in the ledger; live run provides both A and B.
        let warm_present = load_with_cache(
            "ns-two-phase",
            std::slice::from_ref(&src2),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(vec![entry_a.clone(), entry_b.clone()]),
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(
            warm_present.len(),
            2,
            "all sources present: must return live copies only, no ledger duplicates"
        );
        let total: f64 = warm_present.iter().map(|e| e.cost).sum();
        assert!(
            (total - 1.41).abs() < 1e-9,
            "live entries must win over ledger records"
        );

        let _ = fs::remove_file(&src2);
    }

    /// Namespace isolation: a ledger entry recorded under namespace "a" is NOT
    /// emitted by a `load_with_cache` call for namespace "b".
    #[test]
    fn namespace_isolation_ledger_entries_are_scoped() {
        let _env = CacheEnv::new("ns-isolation");

        // Create file under namespace "a" and cache it.
        let src_a = write_source("ns-isolation-a", "a content\n");
        let _ = load_with_cache(
            "ns-a",
            std::slice::from_ref(&src_a),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(vec![sample_entry()]),
            |_| {},
            None,
        );

        // Delete the "a" file so its spend lives only in the ledger under "ns-a".
        fs::remove_file(&src_a).unwrap();
        let _ = load_with_cache(
            "ns-a",
            &[],
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(Vec::new()),
            |_| {},
            None,
        );

        // Now run namespace "b" with no files — must NOT emit "a"'s ledger entries.
        let result_b = load_with_cache(
            "ns-b",
            &[],
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(Vec::new()),
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(result_b.len(), 0, "ns-b must not see ns-a's ledger entries");

        // Create a separate "b" file that still exists on disk, then run "ns-b".
        // The "a" ledger entry stays scoped to "ns-a" and never leaks into "ns-b".
        let src_b = write_source("ns-isolation-b", "b content\n");
        let result_b2 = load_with_cache(
            "ns-b",
            std::slice::from_ref(&src_b),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(vec![sample_entry()]),
            |_| {},
            None,
        )
        .unwrap();
        // ns-b gets its own live entry, still not the ns-a ledger entry.
        assert_eq!(result_b2.len(), 1);
        assert_eq!(result_b2[0].session_id.as_ref(), "session-a"); // from sample_entry

        let _ = fs::remove_file(&src_b);
    }

    /// A duplicate ledger record (e.g. from a concurrent double-append) must
    /// collapse to a single emitted entry. The `(namespace, dedup_key)` primary
    /// key makes the second write a no-op, so spend is never double-counted.
    #[test]
    fn duplicate_ledger_records_counted_once() {
        let _env = CacheEnv::new("ledger-dup");
        let src = write_source("ledger-dup", "line\n");

        let _ = load_with_cache(
            "dup-ns",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(vec![sample_entry()]),
            |_| {},
            None,
        );

        // Attempt to insert the same key again, simulating a concurrent append.
        // The primary key collapses it to a no-op.
        insert_ledger_row("dup-ns", "msg-a", &sample_entry());

        // Delete the source: the key must emit exactly once.
        fs::remove_file(&src).unwrap();
        let warm = load_with_cache(
            "dup-ns",
            &[],
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(Vec::new()),
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(
            warm.len(),
            1,
            "duplicate ledger records must collapse to one"
        );
        assert_eq!(warm[0].session_id.as_ref(), "session-a");
    }

    /// Pricing change does NOT reparse: a warm `load_with_cache` with a different
    /// reprice closure must not invoke `parse_file` (cache hit on mtime+size), and
    /// the returned entry's cost reflects the new reprice logic.
    #[test]
    fn pricing_change_does_not_reparse() {
        let _env = CacheEnv::new("pricing-no-reparse");
        let src = write_source("pricing-no-reparse", "line\n");

        let cold = load_with_cache(
            "test",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_path| Ok(vec![sample_entry()]),
            |_| {}, // no repricing on cold run
            None,
        )
        .unwrap();
        assert_eq!(cold.len(), 1);
        assert_eq!(cold[0].cost, 0.0);

        let warm = load_with_cache(
            "test",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_path| {
                panic!("parse_file must not run when only pricing changes");
            },
            |e| {
                e.cost = 1.23; // simulates new pricing
            },
            None,
        )
        .unwrap();
        assert_eq!(warm.len(), 1);
        assert!(
            (warm[0].cost - 1.23).abs() < 1e-9,
            "cost must reflect new reprice closure, got {}",
            warm[0].cost
        );

        let _ = fs::remove_file(&src);
    }

    /// Mode change does NOT reparse but cost updates: switching CostMode between
    /// runs must be handled by the reprice closure, not by invalidating the cache.
    #[test]
    fn mode_change_does_not_reparse_but_cost_updates() {
        let _env = CacheEnv::new("mode-no-reparse");
        let src = write_source("mode-no-reparse", "line\n");

        let cold = load_with_cache(
            "test",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_path| Ok(vec![sample_entry()]),
            |e| {
                e.cost = 0.5;
            },
            None,
        )
        .unwrap();
        assert_eq!(cold.len(), 1);
        assert!((cold[0].cost - 0.5).abs() < 1e-9);

        // parse_file must NOT be invoked.
        let warm = load_with_cache(
            "test",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_path| {
                panic!("parse_file must not run when only mode changes");
            },
            |e| {
                e.cost = 0.0;
            }, // display mode: no cost_usd → 0
            None,
        )
        .unwrap();
        assert_eq!(warm.len(), 1);
        assert!(
            (warm[0].cost - 0.0).abs() < 1e-9,
            "cost must reflect mode-change reprice"
        );

        let _ = fs::remove_file(&src);
    }

    /// Ledger immutability: a retained (deleted-source) entry keeps its original
    /// cost after a pricing change — ledger entries are NOT repriced by the
    /// reprice closure (only live entries are).
    #[test]
    fn ledger_entry_cost_immutable_after_pricing_change() {
        let _env = CacheEnv::new("ledger-immutable");
        let src = write_source("ledger-immutable", "line\n");

        let mut entry = sample_entry();
        entry.cost = 0.42;
        let _ = load_with_cache(
            "ledger-immut-ns",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(vec![entry.clone()]),
            |_| {},
            None,
        );

        // Delete source — spend retained in ledger.
        fs::remove_file(&src).unwrap();

        // Ledger entries must NOT be repriced — they keep the cost from when they
        // were first recorded.
        let warm = load_with_cache(
            "ledger-immut-ns",
            &[],
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(Vec::new()),
            |e| {
                e.cost = 9.99;
            },
            None,
        )
        .unwrap();

        assert_eq!(warm.len(), 1, "ledger entry must be re-emitted");
        assert!(
            (warm[0].cost - 0.42).abs() < 1e-9,
            "ledger entry cost must be immutable (was {}, expected 0.42)",
            warm[0].cost
        );
    }

    /// Concurrent write-backs sharing the cache database must each preserve the
    /// others' `files` rows. Every thread caches a file under its own namespace;
    /// afterward every row must be present. Each round resets the database to a
    /// fresh, non-WAL file so the openers re-race the `journal_mode=WAL` switch —
    /// the lock upgrade where they used to deadlock and silently drop a
    /// connection along with its row. `set_wal_mode`'s bounded retry plus
    /// `busy_timeout` now serialize the writers so no update is lost.
    #[test]
    fn concurrent_writers_preserve_all_file_rows() {
        let _env = CacheEnv::new("concurrent-files");
        const N: usize = 8;
        // The original single-shot version passed or failed on luck. Re-racing the
        // cold WAL switch over many rounds turns an intermittent drop into a
        // near-certain failure, so a regression cannot sneak through green CI.
        const ROUNDS: usize = 40;
        let srcs: Vec<PathBuf> = (0..N)
            .map(|i| write_source(&format!("concurrent-{i}"), "line\n"))
            .collect();

        for round in 0..ROUNDS {
            // Drop the db and its WAL sidecars so the next opens start from a
            // non-WAL file and contend on the journal_mode switch again.
            clear_cache();
            // Release all threads into the write-back together to maximize contention.
            let barrier = Arc::new(std::sync::Barrier::new(N));
            std::thread::scope(|scope| {
                for (i, src) in srcs.iter().enumerate() {
                    let barrier = Arc::clone(&barrier);
                    scope.spawn(move || {
                        let namespace = format!("ns-{i}");
                        barrier.wait();
                        // single_thread = true so each worker does not spawn its
                        // own parse pool; the one file per thread is parsed inline.
                        let _ = load_with_cache(
                            &namespace,
                            std::slice::from_ref(src),
                            CacheOpts {
                                single_thread: true,
                                live_only: false,
                            },
                            Freshness::FileStat,
                            |_| Ok(vec![sample_entry()]),
                            |_| {},
                            None,
                        );
                    });
                }
            });

            for (i, src) in srcs.iter().enumerate() {
                let key = src.to_string_lossy().to_string();
                assert!(
                    file_paths(&format!("ns-{i}")).contains(&key),
                    "round {round}: files table lost the row for {key} under concurrent writes"
                );
            }
        }

        for src in &srcs {
            let _ = fs::remove_file(src);
        }
    }

    #[test]
    fn live_only_excludes_retained_entries() {
        let _env = CacheEnv::new("live-only-exclude");
        let src = write_source("live-only-exclude", "line\n");

        let entry = sample_entry();
        let _ = load_with_cache(
            "test-live",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(vec![entry.clone()]),
            |_| {},
            None,
        )
        .unwrap();

        // Delete source -> spend retained in ledger only.
        fs::remove_file(&src).unwrap();

        // Default run: re-emits the retained entry.
        let default_result = load_with_cache(
            "test-live",
            &[],
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(Vec::new()),
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(
            default_result.len(),
            1,
            "default must include retained entries"
        );

        // live_only run: excludes the retained entry.
        let live_only_result = load_with_cache(
            "test-live",
            &[],
            CacheOpts {
                single_thread: false,
                live_only: true,
            },
            Freshness::FileStat,
            |_| Ok(Vec::new()),
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(
            live_only_result.len(),
            0,
            "live_only must exclude retained entries"
        );
    }

    #[test]
    fn live_only_still_appends_to_ledger() {
        let _env = CacheEnv::new("live-only-append");
        let src = write_source("live-only-append", "line\n");

        // Run with live_only=true: entries should still be appended to the ledger.
        let entry = sample_entry();
        let _ = load_with_cache(
            "test-live",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: true,
            },
            Freshness::FileStat,
            |_| Ok(vec![entry.clone()]),
            |_| {},
            None,
        )
        .unwrap();

        // Delete source and run with live_only=false: should re-emit from ledger.
        fs::remove_file(&src).unwrap();
        let result = load_with_cache(
            "test-live",
            &[],
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(Vec::new()),
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(
            result.len(),
            1,
            "ledger must still contain entries appended under live_only"
        );
    }

    #[test]
    fn pricing_store_load_round_trip() {
        let _env = CacheEnv::new("pricing-roundtrip");
        let url = "https://example.com/pricing.json";
        let body = r#"{"models":{}}"#;
        let etag = Some(r#""abc123""#);
        let last_modified = Some("Wed, 09 Apr 2025 12:00:00 GMT");

        store_pricing(url, body, etag, last_modified, 1_800_000_000);
        let cached = load_pricing(url).expect("cache miss after store");

        assert_eq!(cached.body, body);
        assert_eq!(cached.etag.as_deref(), etag);
        assert_eq!(cached.last_modified.as_deref(), last_modified);
        assert_eq!(cached.updated_at, Some(1_800_000_000));
    }

    #[test]
    fn pricing_load_returns_none_for_unknown_url() {
        let _env = CacheEnv::new("pricing-miss");
        assert!(load_pricing("https://no-such-url.example.com").is_none());
    }

    #[test]
    fn pricing_upsert_replaces_existing_row() {
        let _env = CacheEnv::new("pricing-upsert");
        let url = "https://example.com/pricing.json";

        store_pricing(url, "v1", Some("old-etag"), Some("old-date"), 1_800_000_000);
        store_pricing(url, "v2", Some("new-etag"), Some("new-date"), 1_800_000_100);

        let cached = load_pricing(url).expect("cache miss after upsert");
        assert_eq!(cached.body, "v2");
        assert_eq!(cached.etag.as_deref(), Some("new-etag"));
        assert_eq!(cached.last_modified.as_deref(), Some("new-date"));
        assert_eq!(cached.updated_at, Some(1_800_000_100));
    }

    #[test]
    fn pricing_none_etag_and_last_modified_round_trip() {
        let _env = CacheEnv::new("pricing-none-headers");
        let url = "https://example.com/pricing.json";

        store_pricing(url, "body", None, None, 1_800_000_000);
        let cached = load_pricing(url).expect("cache miss after store");

        assert_eq!(cached.body, "body");
        assert!(cached.etag.is_none());
        assert!(cached.last_modified.is_none());
        assert_eq!(cached.updated_at, Some(1_800_000_000));
    }

    /// A v1 database (no `updated_at` column) is migrated in place: the column
    /// appears, the schema version is stamped 2, and pre-existing rows read back
    /// with `updated_at: None` so they refetch once instead of being served warm.
    #[test]
    fn migrate_adds_updated_at_to_legacy_pricing_table() {
        let _env = CacheEnv::new("pricing-migrate-legacy");
        let dir = cache_dir().expect("cache dir");
        fs::create_dir_all(&dir).expect("create cache dir");
        let legacy = sqlite::open(dir.join(DB_FILE)).expect("open legacy db");
        legacy
            .execute(
                "CREATE TABLE pricing (\
                     url TEXT PRIMARY KEY,\
                     etag TEXT,\
                     last_modified TEXT,\
                     body TEXT NOT NULL\
                 );\
                 CREATE TABLE schema_meta (\
                     name TEXT PRIMARY KEY,\
                     version INTEGER NOT NULL\
                 );\
                 INSERT INTO schema_meta(name, version) VALUES ('pricing', 1);\
                 INSERT INTO pricing(url, etag, last_modified, body) \
                     VALUES ('test://legacy', 'e', 'd', 'BODY');",
            )
            .expect("seed legacy schema");
        drop(legacy);

        let conn = open_db().expect("open_db should migrate the legacy schema");
        assert_eq!(
            read_schema_version(&conn, "pricing"),
            Some(PRICING_SCHEMA_VERSION),
            "migration must stamp the new version"
        );
        let cached = load_pricing("test://legacy").expect("legacy row survives");
        assert_eq!(cached.body, "BODY");
        assert_eq!(cached.etag.as_deref(), Some("e"));
        assert_eq!(
            cached.updated_at, None,
            "legacy rows are stale until a refresh stores a timestamp"
        );
    }

    #[test]
    fn migrate_populates_schema_meta_with_four_rows_at_v1() {
        let _env = CacheEnv::new("schema-meta");
        let conn = open_db().expect("open_db should succeed");
        let mut st = conn
            .prepare("SELECT name, version FROM schema_meta ORDER BY name")
            .unwrap();
        let mut rows: Vec<(String, i64)> = Vec::new();
        while let Ok(sqlite::State::Row) = st.next() {
            rows.push((
                st.read::<String, _>(0).unwrap(),
                st.read::<i64, _>(1).unwrap(),
            ));
        }
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0], ("files".to_string(), FILES_SCHEMA_VERSION));
        assert_eq!(rows[1], ("ledger".to_string(), LEDGER_SCHEMA_VERSION));
        assert_eq!(rows[2], ("opencode".to_string(), OPENCODE_SCHEMA_VERSION));
        assert_eq!(rows[3], ("pricing".to_string(), PRICING_SCHEMA_VERSION));
    }

    /// A second `open_db()` on the same database must be idempotent — no
    /// duplicate rows or errors.
    #[test]
    fn migrate_is_idempotent_on_second_open_db() {
        let _env = CacheEnv::new("schema-meta-idempotent");
        let _ = open_db().expect("first open_db should succeed");
        let conn = open_db().expect("second open_db should succeed");
        let mut st = conn.prepare("SELECT COUNT(*) FROM schema_meta").unwrap();
        st.next().unwrap();
        let count = st.read::<i64, _>(0).unwrap();
        assert_eq!(count, 4, "idempotent open must not duplicate rows");
    }

    /// A stale `ledger` version must trigger the mismatch warning and get
    /// restamped to the current version, so the warning fires once per
    /// upgrade rather than on every subsequent run.
    #[test]
    fn migrate_restamps_stale_ledger_version() {
        let _env = CacheEnv::new("schema-meta-stale-ledger");
        let conn = open_db().expect("open_db should succeed");
        conn.execute("UPDATE schema_meta SET version = 999999 WHERE name = 'ledger'")
            .unwrap();

        migrate(&conn).expect("migrate should succeed on an existing db");

        let stored = read_schema_version(&conn, "ledger").expect("ledger row must still exist");
        assert_eq!(
            stored, LEDGER_SCHEMA_VERSION,
            "migrate must restamp a stale ledger version to the current one"
        );
    }

    /// `clear_cache()` must remove `cache.db` (and sidecars if present).
    #[test]
    fn clear_cache_removes_cache_db() {
        let _env = CacheEnv::new("clear-cache");
        {
            let _conn = open_db().expect("open_db should succeed");
            let dir = cache_dir().unwrap();
            assert!(
                dir.join(DB_FILE).exists(),
                "cache.db should exist after open"
            );
        }
        clear_cache();
        let dir = cache_dir().unwrap();
        assert!(
            !dir.join(DB_FILE).exists(),
            "cache.db must be removed after clear_cache"
        );
        assert!(
            !dir.join("cache.db-wal").exists(),
            "cache.db-wal must be removed"
        );
        assert!(
            !dir.join("cache.db-shm").exists(),
            "cache.db-shm must be removed"
        );
    }

    /// `clear_cache_namespaces` must delete only the target namespace rows
    /// while preserving rows in other namespaces.
    #[test]
    fn clear_cache_namespaces_preserves_other_namespaces() {
        let _env = CacheEnv::new("clear-ns");
        let src_a = write_source("clear-ns-a", "a\n");
        let src_b = write_source("clear-ns-b", "b\n");

        let _ = load_with_cache(
            "claude",
            std::slice::from_ref(&src_a),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(vec![sample_entry()]),
            |_| {},
            None,
        );
        let _ = load_with_cache(
            "opencode",
            std::slice::from_ref(&src_b),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::FileStat,
            |_| Ok(vec![sample_entry()]),
            |_| {},
            None,
        );

        clear_cache_namespaces("claude");

        let conn = open_db().expect("open after clear");
        let mut st = conn
            .prepare("SELECT COUNT(*) FROM files WHERE namespace = 'claude'")
            .unwrap();
        st.next().unwrap();
        let count_claude = st.read::<i64, _>(0).unwrap();
        assert_eq!(count_claude, 0, "claude files must be deleted");

        let mut st = conn
            .prepare("SELECT COUNT(*) FROM files WHERE namespace = 'opencode'")
            .unwrap();
        st.next().unwrap();
        let count_opencode = st.read::<i64, _>(0).unwrap();
        assert_eq!(count_opencode, 1, "opencode files must be preserved");

        let _ = fs::remove_file(&src_a);
        let _ = fs::remove_file(&src_b);
    }
    // -------------------------------------------------------------------------
    // Fingerprint freshness strategy tests
    // -------------------------------------------------------------------------

    #[test]
    fn fingerprint_same_value_is_cache_hit() {
        let _env = CacheEnv::new("fp-hit");
        let src = write_source("fp-hit", "line\n");
        let mut meta = file_metadata(&src).unwrap();
        meta.fingerprint = 42;
        let stored = stored_snapshot(&src, &meta, &[sample_entry()]);

        // Current fingerprint matches stored → cache hit.
        let fp_fn = |_: &std::path::Path| Some(42u64);
        let part = partition_files(
            std::slice::from_ref(&src),
            &stored,
            &Freshness::Fingerprint(fp_fn),
            false,
        );
        assert_eq!(part.cached.len(), 1, "matching fingerprint must hit cache");
        assert!(part.fresh.is_empty());
        let _ = fs::remove_file(&src);
    }

    #[test]
    fn fingerprint_different_value_is_cache_miss() {
        let _env = CacheEnv::new("fp-miss");
        let src = write_source("fp-miss", "line\n");
        let mut meta = file_metadata(&src).unwrap();
        meta.fingerprint = 42;
        let stored = stored_snapshot(&src, &meta, &[sample_entry()]);

        // Current fingerprint differs → cache miss.
        let fp_fn = |_: &std::path::Path| Some(99u64);
        let part = partition_files(
            std::slice::from_ref(&src),
            &stored,
            &Freshness::Fingerprint(fp_fn),
            false,
        );
        assert!(part.cached.is_empty(), "different fingerprint must miss");
        assert_eq!(part.fresh.len(), 1);
        let _ = fs::remove_file(&src);
    }

    #[test]
    fn fingerprint_none_forces_reparse() {
        let _env = CacheEnv::new("fp-none");
        let src = write_source("fp-none", "line\n");
        let mut meta = file_metadata(&src).unwrap();
        meta.fingerprint = 42;
        let stored = stored_snapshot(&src, &meta, &[sample_entry()]);

        // Fingerprint function returns None → must reparse.
        let fp_fn = |_: &std::path::Path| None;
        let part = partition_files(
            std::slice::from_ref(&src),
            &stored,
            &Freshness::Fingerprint(fp_fn),
            false,
        );
        assert!(
            part.cached.is_empty(),
            "None fingerprint must force reparse"
        );
        assert_eq!(part.fresh.len(), 1);
        let _ = fs::remove_file(&src);
    }

    #[test]
    fn fingerprint_ignores_mtime_size_drift() {
        let _env = CacheEnv::new("fp-ignores-mtime");
        let src = write_source("fp-ignores-mtime", "line\n");
        let mut meta = file_metadata(&src).unwrap();
        meta.fingerprint = 42;
        let stored = stored_snapshot(&src, &meta, &[sample_entry()]);

        // Mutate file so mtime/size change, but fingerprint stays the same.
        fs::write(&src, "longer content\n").unwrap();

        let fp_fn = |_: &std::path::Path| Some(42u64);
        let part = partition_files(
            std::slice::from_ref(&src),
            &stored,
            &Freshness::Fingerprint(fp_fn),
            false,
        );
        assert_eq!(
            part.cached.len(),
            1,
            "matching fingerprint must hit even when mtime/size drift"
        );
        assert!(part.fresh.is_empty());
        let _ = fs::remove_file(&src);
    }

    #[test]
    fn fingerprint_load_with_cache_end_to_end() {
        let _env = CacheEnv::new("fp-e2e");
        let src = write_source("fp-e2e", "line\n");
        let calls = std::sync::atomic::AtomicUsize::new(0);

        // `Freshness::Fingerprint` now takes a plain `fn` pointer, which cannot
        // capture, so the toggle it reads lives in a function-local static
        // instead of a captured `AtomicU64`.
        static FP_VALUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(100);
        fn fp_fn(_: &Path) -> Option<u64> {
            Some(FP_VALUE.load(std::sync::atomic::Ordering::SeqCst))
        }

        let cold = load_with_cache(
            "fp-e2e",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::Fingerprint(fp_fn),
            |_path| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(vec![sample_entry()])
            },
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(cold.len(), 1);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        let warm = load_with_cache(
            "fp-e2e",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::Fingerprint(fp_fn),
            |_path| {
                panic!("parse_file must not run for a fingerprint cache hit");
            },
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(warm.len(), 1);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Fingerprint changes → must reparse.
        FP_VALUE.store(200, std::sync::atomic::Ordering::SeqCst);
        let reparsed = load_with_cache(
            "fp-e2e",
            std::slice::from_ref(&src),
            CacheOpts {
                single_thread: false,
                live_only: false,
            },
            Freshness::Fingerprint(fp_fn),
            |_path| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(vec![sample_entry()])
            },
            |_| {},
            None,
        )
        .unwrap();
        assert_eq!(reparsed.len(), 1);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        let _ = fs::remove_file(&src);
    }
}
