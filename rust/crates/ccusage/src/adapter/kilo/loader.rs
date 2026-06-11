use std::{
    collections::HashSet,
    hash::{Hash, Hasher},
    path::Path,
};

use jiff::tz::TimeZone as JiffTimeZone;

use crate::{
    LoadedEntry, PricingMap, Result,
    adapter::sqlite_util::{open_readonly, read_id_session_data},
    cli::SharedArgs,
    debug_log, parse_tz,
};

use super::{
    parser::{KiloMessage, message_value_to_entry, reprice},
    paths::{db_path, paths},
};

pub(crate) fn load_entries(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    crate::progress::track_usage_load(crate::progress::UsageLoadAgent::Kilo, shared.json, || {
        load_entries_inner(shared, pricing)
    })
}

fn load_entries_inner(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    let tz = parse_tz(shared.timezone.as_deref());
    let db_paths: Vec<_> = paths()?.into_iter().filter_map(|p| db_path(&p)).collect();
    let all = crate::cache::load_with_cache(
        "kilo",
        &db_paths,
        crate::cache::CacheOpts {
            single_thread: shared.single_thread,
            live_only: shared.live_only,
        },
        crate::cache::Freshness::Fingerprint(fingerprint_kilo_db),
        |db| load_entries_from_database(db, tz.as_ref(), shared, pricing),
        |e| reprice(e, shared.mode, pricing),
    )?;
    // Deduplicate message ids across databases (same id can appear in multiple synced dbs).
    let mut seen: HashSet<String> = HashSet::new();
    let mut entries: Vec<LoadedEntry> = all
        .into_iter()
        .filter(|e| {
            if let Some(id) = e.data.message.id.as_deref() {
                seen.insert(id.to_string())
            } else {
                true
            }
        })
        .collect();
    entries.sort_by_key(|e| e.timestamp);
    Ok(entries)
}
/// Content fingerprint for a Kilo SQLite database.
///
/// Detects whether the `time_updated` column exists by inspecting the
/// CREATE TABLE statement in `sqlite_master` and hashes accordingly:
/// - Full schema (with `time_updated`): hashes count(*), max(rowid), and
///   max(time_updated), so in-place edits during streaming are detected.
/// - Fallback (no `time_updated`): hashes each row's `(id, data)` so in-place
///   edits are detected even when row count and max rowid are unchanged.
///
/// A discriminant byte (0 = full, 1 = fallback) is mixed in so the two
/// paths can never produce the same fingerprint for the same row state.
fn fingerprint_kilo_db(path: &Path) -> Option<u64> {
    let conn = open_readonly(path).ok()?;

    // Check whether the message table exists.
    {
        let mut st = conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='message'")
            .ok()?;
        if !matches!(st.next().ok()?, sqlite::State::Row) {
            return None;
        }
    }
    // Detect time_updated column presence.
    // The `sqlite` crate does not surface PRAGMA results via prepare/next,
    // so we parse the CREATE TABLE statement from sqlite_master instead.
    let has_time_updated = {
        let mut st = conn
            .prepare("SELECT sql FROM sqlite_master WHERE type='table' AND name='message'")
            .ok()?;
        if let Ok(sqlite::State::Row) = st.next() {
            let create_sql: String = st.read(0).ok()?;
            create_sql.contains("time_updated")
        } else {
            false
        }
    };
    let mut hasher = rustc_hash::FxHasher::default();

    if has_time_updated {
        // Full path: include time_updated for in-place-edit detection.
        let mut st = conn
            .prepare(
                "SELECT count(*), coalesce(max(rowid), 0), coalesce(max(time_updated), 0) \
                 FROM message",
            )
            .ok()?;
        st.next().ok()?;
        let count: i64 = st.read(0).ok()?;
        let max_rowid: i64 = st.read(1).ok()?;
        let max_time: i64 = st.read(2).ok()?;
        0u8.hash(&mut hasher); // discriminant: full schema
        count.hash(&mut hasher);
        max_rowid.hash(&mut hasher);
        max_time.hash(&mut hasher);
    } else {
        let mut st = conn
            .prepare("SELECT id, data FROM message ORDER BY rowid")
            .ok()?;
        1u8.hash(&mut hasher); // discriminant: fallback
        while let Ok(sqlite::State::Row) = st.next() {
            let id: String = st.read(0).ok()?;
            let data: String = st.read(1).ok()?;
            id.hash(&mut hasher);
            data.hash(&mut hasher);
        }
    }

    Some(hasher.finish())
}

fn load_entries_from_database(
    db_path: &Path,
    tz: Option<&JiffTimeZone>,
    shared: &SharedArgs,
    pricing: &PricingMap,
) -> crate::Result<Vec<LoadedEntry>> {
    let connection = open_readonly(db_path).map_err(|e| {
        crate::cli_error(format!(
            "Failed to open Kilo database {}: {e}",
            db_path.display()
        ))
    })?;
    let mut statement = connection
        .prepare("SELECT id, session_id, data FROM message")
        .map_err(|e| {
            crate::cli_error(format!(
                "Failed to read Kilo database {}: {e}",
                db_path.display()
            ))
        })?;
    let mut entries = Vec::new();
    loop {
        match statement.next() {
            Ok(sqlite::State::Row) => {
                let Some((row_id, row_session_id, data)) = read_id_session_data(&statement) else {
                    continue;
                };
                let value = match serde_json::from_str::<KiloMessage>(&data) {
                    Ok(value) => value,
                    Err(error) => {
                        debug_log(
                            shared,
                            format!(
                                "Failed to read Kilo database {}: {error}",
                                db_path.display()
                            ),
                        );
                        continue;
                    }
                };
                if let Some(entry) = message_value_to_entry(
                    &value,
                    &row_id,
                    &row_session_id,
                    db_path,
                    tz,
                    shared.mode,
                    pricing,
                ) {
                    entries.push(entry);
                }
            }
            Ok(sqlite::State::Done) => break,
            Err(e) => {
                return Err(crate::cli_error(format!(
                    "Failed to query Kilo database {}: {e}",
                    db_path.display()
                )));
            }
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Mutex};

    use super::*;
    use crate::{PricingMap, cache::tests::CacheEnv, cli::CostMode};
    use ccusage_test_support::{EnvVarGuard, fs_fixture};

    static KILO_DATA_DIR_LOCK: Mutex<()> = Mutex::new(());

    fn create_db_message(path: &Path, id: &str, session_id: &str, data: &str) {
        let db = sqlite::open(path).unwrap();
        db.execute("CREATE TABLE message (id TEXT, session_id TEXT, data TEXT)")
            .unwrap();
        let mut statement = db
            .prepare("INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)")
            .unwrap();
        statement.bind((1, id)).unwrap();
        statement.bind((2, session_id)).unwrap();
        statement.bind((3, data)).unwrap();
        statement.next().unwrap();
    }

    #[test]
    fn loads_kilo_messages_from_sqlite() {
        let _cache_env = CacheEnv::new("kilo-loads-sqlite");
        let _guard = KILO_DATA_DIR_LOCK.lock().unwrap();
        let fixture = fs_fixture!({});
        create_db_message(
            &fixture.path(super::super::paths::KILO_DB_FILE_NAME),
            "row-1",
            "session-a",
            r#"{"id":"msg-1","role":"assistant","providerID":"anthropic","modelID":"claude-sonnet-4-20250514","time":{"created":1767312000000},"tokens":{"input":100,"output":50,"reasoning":5,"cache":{"read":10,"write":20}},"cost":0.02,"agent":"build"}"#,
        );
        let _cleanup = EnvVarGuard::set(super::super::paths::KILO_DATA_DIR_ENV, fixture.root());
        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();

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
        assert_eq!(entries[0].extra_total_tokens, 5);
        // Display mode surfaces the agent's logged cost verbatim (0.02), never
        // reprices from tokens.
        assert_eq!(entries[0].cost, 0.02);
    }

    #[test]
    fn ignores_kilo_messages_without_timestamps() {
        let _cache_env = CacheEnv::new("kilo-no-timestamps");
        let _guard = KILO_DATA_DIR_LOCK.lock().unwrap();
        let fixture = fs_fixture!({});
        create_db_message(
            &fixture.path(super::super::paths::KILO_DB_FILE_NAME),
            "row-1",
            "session-a",
            r#"{"role":"assistant","providerID":"openai","modelID":"gpt-5","tokens":{"input":1,"output":1,"cache":{"read":0,"write":0}}}"#,
        );
        let _cleanup = EnvVarGuard::set(super::super::paths::KILO_DATA_DIR_ENV, fixture.root());
        let shared = SharedArgs::default();
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();

        assert!(entries.is_empty());
    }

    #[test]
    fn deduplicates_kilo_messages_across_data_dirs() {
        let _cache_env = CacheEnv::new("kilo-dedup-dirs");
        let _guard = KILO_DATA_DIR_LOCK.lock().unwrap();
        let first = fs_fixture!({});
        let second = fs_fixture!({});
        for (fixture, input) in [(&first, 10), (&second, 20)] {
            create_db_message(
                &fixture.path(super::super::paths::KILO_DB_FILE_NAME),
                "row-1",
                "session-a",
                &format!(
                    r#"{{"id":"embedded-msg-1","role":"assistant","providerID":"openai","modelID":"gpt-5","time":{{"created":1767312000000}},"tokens":{{"input":{input},"output":1,"cache":{{"read":0,"write":0}}}}}}"#
                ),
            );
        }
        let _cleanup = EnvVarGuard::set(
            super::super::paths::KILO_DATA_DIR_ENV,
            format!("{},{}", first.root().display(), second.root().display()),
        );
        let shared = SharedArgs::default();
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.message.usage.input_tokens, 10);
    }
    #[test]
    fn missing_message_table_returns_error() {
        let _cache_env = CacheEnv::new("kilo-missing-table");
        let _guard = KILO_DATA_DIR_LOCK.lock().unwrap();
        let fixture = fs_fixture!({});
        let db = sqlite::open(fixture.path(super::super::paths::KILO_DB_FILE_NAME)).unwrap();
        db.execute("CREATE TABLE other (id TEXT)").unwrap();
        drop(db);
        let _cleanup = EnvVarGuard::set(super::super::paths::KILO_DATA_DIR_ENV, fixture.root());
        let shared = SharedArgs::default();
        let result = load_entries(&shared, &PricingMap::load_embedded());
        assert!(result.is_err(), "missing message table must return Err");
    }

    #[test]
    fn malformed_json_row_skipped_good_row_returned() {
        let _cache_env = CacheEnv::new("kilo-bad-row");
        let _guard = KILO_DATA_DIR_LOCK.lock().unwrap();
        let fixture = fs_fixture!({});
        let db_path = fixture.path(super::super::paths::KILO_DB_FILE_NAME);
        let db = sqlite::open(&db_path).unwrap();
        db.execute("CREATE TABLE message (id TEXT, session_id TEXT, data TEXT)")
            .unwrap();
        db.execute(
            "INSERT INTO message (id, session_id, data) VALUES ('bad-row', 'session-x', 'not valid json')",
        )
        .unwrap();
        db.execute(
            "INSERT INTO message (id, session_id, data) VALUES ('good-row', 'session-y', '{\"id\":\"msg-good\",\"role\":\"assistant\",\"providerID\":\"anthropic\",\"modelID\":\"claude-sonnet-4-20250514\",\"time\":{\"created\":1767312000000},\"tokens\":{\"input\":10,\"output\":5,\"reasoning\":0,\"cache\":{\"read\":0,\"write\":0}},\"cost\":0.01,\"agent\":\"build\"}')",
        )
        .unwrap();
        drop(db);
        let _cleanup = EnvVarGuard::set(super::super::paths::KILO_DATA_DIR_ENV, fixture.root());
        let shared = SharedArgs {
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();
        assert_eq!(entries.len(), 1, "must return only the good row");
        assert_eq!(entries[0].data.message.id.as_deref(), Some("msg-good"));
    }

    #[test]
    fn fingerprint_varies_with_row_count() {
        let fixture = fs_fixture!({});
        let db_path = fixture.root().join("kilo.db");

        // Empty DB (no message table) → fingerprint is None.
        {
            let db = sqlite::open(&db_path).unwrap();
            db.execute("CREATE TABLE other (id TEXT)").unwrap();
        }
        assert!(
            super::fingerprint_kilo_db(&db_path).is_none(),
            "DB without message table must return None"
        );

        {
            let db = sqlite::open(&db_path).unwrap();
            db.execute("DROP TABLE other").unwrap();
            db.execute(
                "CREATE TABLE message (id TEXT, session_id TEXT, data TEXT, time_updated INTEGER)",
            )
            .unwrap();
            let mut st = db
                .prepare("INSERT INTO message (id, session_id, data, time_updated) VALUES (?1, ?2, ?3, ?4)")
                .unwrap();
            st.bind((1, "row-1")).unwrap();
            st.bind((2, "s1")).unwrap();
            st.bind((3, "{}")).unwrap();
            st.bind((4, 1000i64)).unwrap();
            st.next().unwrap();
        }
        let fp1 = super::fingerprint_kilo_db(&db_path).expect("must return Some after insert");

        // Insert a second row → fingerprint must change.
        {
            let db = sqlite::open(&db_path).unwrap();
            let mut st = db
                .prepare("INSERT INTO message (id, session_id, data, time_updated) VALUES (?1, ?2, ?3, ?4)")
                .unwrap();
            st.bind((1, "row-2")).unwrap();
            st.bind((2, "s1")).unwrap();
            st.bind((3, "{}")).unwrap();
            st.bind((4, 2000i64)).unwrap();
            st.next().unwrap();
        }
        let fp2 =
            super::fingerprint_kilo_db(&db_path).expect("must return Some after second insert");
        assert_ne!(fp1, fp2, "fingerprint must differ after inserting a row");
    }

    #[test]
    fn fingerprint_detects_inplace_time_updated_bump() {
        let fixture = fs_fixture!({});
        let db_path = fixture.root().join("kilo.db");

        {
            let db = sqlite::open(&db_path).unwrap();
            db.execute(
                "CREATE TABLE message (id TEXT, session_id TEXT, data TEXT, time_updated INTEGER)",
            )
            .unwrap();
            let mut st = db
                .prepare(
                    "INSERT INTO message (id, session_id, data, time_updated) VALUES (?1, ?2, ?3, ?4)",
                )
                .unwrap();
            st.bind((1, "row-1")).unwrap();
            st.bind((2, "s1")).unwrap();
            st.bind((3, "{}")).unwrap();
            st.bind((4, 1000i64)).unwrap();
            st.next().unwrap();
        }
        let fp1 = super::fingerprint_kilo_db(&db_path)
            .expect("must return Some with time_updated schema");

        // Bump time_updated in-place (same rowid, same count).
        {
            let db = sqlite::open(&db_path).unwrap();
            db.execute("UPDATE message SET time_updated = 9999 WHERE id = 'row-1'")
                .unwrap();
        }
        let fp2 = super::fingerprint_kilo_db(&db_path).expect("must return Some after update");
        assert_ne!(
            fp1, fp2,
            "fingerprint must change when time_updated is bumped in-place"
        );
    }

    #[test]
    fn fingerprint_fallback_without_time_updated() {
        let fixture = fs_fixture!({});
        let db_path = fixture.root().join("kilo.db");

        {
            let db = sqlite::open(&db_path).unwrap();
            db.execute("CREATE TABLE message (id TEXT, session_id TEXT, data TEXT)")
                .unwrap();
            let mut st = db
                .prepare("INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)")
                .unwrap();
            st.bind((1, "row-1")).unwrap();
            st.bind((2, "s1")).unwrap();
            st.bind((3, "{}")).unwrap();
            st.next().unwrap();
        }
        let fp1 = super::fingerprint_kilo_db(&db_path)
            .expect("must return Some even without time_updated");

        // Add a row — fingerprint must change.
        {
            let db = sqlite::open(&db_path).unwrap();
            let mut st = db
                .prepare("INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)")
                .unwrap();
            st.bind((1, "row-2")).unwrap();
            st.bind((2, "s1")).unwrap();
            st.bind((3, "{}")).unwrap();
            st.next().unwrap();
        }
        let fp2 =
            super::fingerprint_kilo_db(&db_path).expect("must return Some after second insert");
        assert_ne!(
            fp1, fp2,
            "fingerprint must differ after adding a row (fallback path)"
        );
    }

    #[test]
    fn fingerprint_detects_inplace_data_edit_fallback() {
        let fixture = fs_fixture!({});
        let db_path = fixture.root().join("kilo.db");

        {
            let db = sqlite::open(&db_path).unwrap();
            db.execute("CREATE TABLE message (id TEXT, session_id TEXT, data TEXT)")
                .unwrap();
            let mut st = db
                .prepare("INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)")
                .unwrap();
            st.bind((1, "row-1")).unwrap();
            st.bind((2, "s1")).unwrap();
            st.bind((3, "{}")).unwrap();
            st.next().unwrap();
        }
        let fp1 =
            super::fingerprint_kilo_db(&db_path).expect("must return Some without time_updated");

        {
            let db = sqlite::open(&db_path).unwrap();
            db.execute("UPDATE message SET data = '{\"cost\":1}' WHERE id = 'row-1'")
                .unwrap();
        }
        let fp2 =
            super::fingerprint_kilo_db(&db_path).expect("must return Some after in-place edit");
        assert_ne!(
            fp1, fp2,
            "fallback fingerprint must change when row data is edited in place"
        );
    }

    #[test]
    fn fingerprint_full_and_fallback_never_collide() {
        let fixture_full = fs_fixture!({});
        let fixture_fb = fs_fixture!({});

        let fp_full = {
            let db_path = fixture_full.root().join("full.db");
            let db = sqlite::open(&db_path).unwrap();
            db.execute(
                "CREATE TABLE message (id TEXT, session_id TEXT, data TEXT, time_updated INTEGER)",
            )
            .unwrap();
            db.execute(
                "INSERT INTO message (id, session_id, data, time_updated) VALUES ('r1', 's1', '{}', 0)",
            )
            .unwrap();
            super::fingerprint_kilo_db(&db_path).expect("Some")
        };

        let fp_fallback = {
            let db_path = fixture_fb.root().join("fallback.db");
            let db = sqlite::open(&db_path).unwrap();
            db.execute("CREATE TABLE message (id TEXT, session_id TEXT, data TEXT)")
                .unwrap();
            db.execute("INSERT INTO message (id, session_id, data) VALUES ('r1', 's1', '{}')")
                .unwrap();
            super::fingerprint_kilo_db(&db_path).expect("Some")
        };

        assert_ne!(
            fp_full, fp_fallback,
            "full-schema and fallback fingerprints must never collide for identical row state"
        );
    }

    /// Fingerprint must use a stable hasher so the persisted value is
    /// consistent across toolchain versions.
    #[test]
    fn fingerprint_uses_stable_hasher() {
        let fixture = fs_fixture!({});
        let db_path = fixture.root().join("kilo.db");

        {
            let db = sqlite::open(&db_path).unwrap();
            db.execute(
                "CREATE TABLE message (id TEXT, session_id TEXT, data TEXT, time_updated INTEGER)",
            )
            .unwrap();
            db.execute(
                "INSERT INTO message (id, session_id, data, time_updated) VALUES ('r1', 's1', '{}', 42)",
            )
            .unwrap();
        }

        // Compute fingerprint twice with fresh hashers — must be deterministic.
        let fp_a = super::fingerprint_kilo_db(&db_path).expect("Some");
        let fp_b = super::fingerprint_kilo_db(&db_path).expect("Some");
        assert_eq!(fp_a, fp_b, "fingerprint must be deterministic across calls");

        // The fingerprint is persisted in cache.db, so it must stay stable across
        // toolchains and architectures. Pin a hardcoded literal — NOT a value
        // recomputed with the same hasher, which would move in lockstep and never
        // catch a regression.
        const GOLDEN: u64 = 14619525373561997556;
        assert_eq!(
            fp_a, GOLDEN,
            "kilo fingerprint changed — persisted cache.db values are now stale; \
             migrate the cache or restore the hasher (see fingerprint_kilo_db)"
        );
    }
}
