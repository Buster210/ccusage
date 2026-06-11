use std::{
    collections::HashSet,
    hash::{Hash, Hasher},
    path::Path,
};

use crate::{LoadedEntry, PricingMap, Result, cli::SharedArgs};

use super::{
    parser::{HermesEntry, read_session_row, reprice, to_loaded_entry},
    paths::hermes_state_db_paths,
};

pub(crate) fn load_entries(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    crate::progress::track_usage_load(crate::progress::UsageLoadAgent::Hermes, shared.json, || {
        load_entries_inner(shared, pricing)
    })
}

fn load_entries_inner(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    let tz = crate::parse_tz(shared.timezone.as_deref());
    let db_paths = hermes_state_db_paths()?;
    let all = crate::cache::load_with_cache(
        "hermes",
        &db_paths,
        crate::cache::CacheOpts {
            single_thread: shared.single_thread,
            live_only: shared.live_only,
        },
        crate::cache::Freshness::Fingerprint(fingerprint_hermes_db),
        |db_path| {
            let entries = load_state_db_entries(db_path, shared);
            Ok(entries
                .into_iter()
                .map(|entry| to_loaded_entry(entry, tz.as_ref(), pricing))
                .collect())
        },
        |e| reprice(e, shared.mode, pricing),
    )?;
    let mut seen_sessions = HashSet::new();
    let mut entries: Vec<LoadedEntry> = all
        .into_iter()
        .filter(|e| seen_sessions.insert(e.session_id.to_string()))
        .collect();
    entries.sort_by_key(|entry| entry.timestamp);
    Ok(entries)
}

/// Content fingerprint for a Hermes state database.
///
/// Hashes each billable row's content columns in stable `ORDER BY id` so
/// in-place edits are detected even when row count is unchanged.
/// A discriminant byte (2) avoids cross-schema collisions with kilo.
fn fingerprint_hermes_db(path: &Path) -> Option<u64> {
    let conn = crate::adapter::sqlite_util::open_readonly(path).ok()?;
    {
        let mut st = conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='sessions'")
            .ok()?;
        if !matches!(st.next().ok()?, sqlite::State::Row) {
            return None;
        }
    }
    let mut st = conn
        .prepare(
            "SELECT id, model, started_at, message_count, \
             input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, \
             reasoning_tokens, estimated_cost_usd, actual_cost_usd \
             FROM sessions WHERE model IS NOT NULL AND TRIM(model) != '' \
             ORDER BY id",
        )
        .ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    2u8.hash(&mut hasher); // discriminant
    while let Ok(sqlite::State::Row) = st.next() {
        let id: String = st.read(0).ok()?;
        let model: String = st.read(1).ok()?;
        let started_at: f64 = st.read(2).ok()?;
        let message_count: i64 = st.read(3).ok()?;
        let input_tokens: i64 = st.read(4).ok()?;
        let output_tokens: i64 = st.read(5).ok()?;
        let cache_read_tokens: i64 = st.read(6).ok()?;
        let cache_write_tokens: i64 = st.read(7).ok()?;
        let reasoning_tokens: i64 = st.read(8).ok()?;
        let estimated_cost: f64 = st.read(9).ok()?;
        let actual_cost: f64 = st.read(10).ok()?;
        id.hash(&mut hasher);
        model.hash(&mut hasher);
        started_at.to_bits().hash(&mut hasher);
        message_count.hash(&mut hasher);
        input_tokens.hash(&mut hasher);
        output_tokens.hash(&mut hasher);
        cache_read_tokens.hash(&mut hasher);
        cache_write_tokens.hash(&mut hasher);
        reasoning_tokens.hash(&mut hasher);
        estimated_cost.to_bits().hash(&mut hasher);
        actual_cost.to_bits().hash(&mut hasher);
    }
    Some(hasher.finish())
}

fn load_state_db_entries(db_path: &Path, shared: &SharedArgs) -> Vec<HermesEntry> {
    let Ok(connection) =
        sqlite::Connection::open_with_flags(db_path, sqlite::OpenFlags::new().with_read_only())
    else {
        crate::debug_log(
            shared,
            format!(
                "Failed to open Hermes state database: {}",
                db_path.display()
            ),
        );
        return Vec::new();
    };
    let Ok(mut statement) = connection.prepare(
        "
            SELECT
                id,
                model,
                billing_provider,
                started_at,
                message_count,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                reasoning_tokens,
                estimated_cost_usd,
                actual_cost_usd
            FROM sessions
            WHERE model IS NOT NULL
                AND TRIM(model) != ''
        ",
    ) else {
        crate::debug_log(
            shared,
            format!(
                "Failed to read Hermes state database: {}",
                db_path.display()
            ),
        );
        return Vec::new();
    };
    let mut entries = Vec::new();
    loop {
        match statement.next() {
            Ok(sqlite::State::Row) => {
                if let Some(entry) = read_session_row(&statement) {
                    entries.push(entry);
                }
            }
            Ok(sqlite::State::Done) => break,
            Err(_) => {
                crate::debug_log(
                    shared,
                    format!(
                        "Failed to query Hermes state database: {}",
                        db_path.display()
                    ),
                );
                break;
            }
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{PricingMap, cache::tests::CacheEnv};
    use ccusage_test_support::{EnvVarGuard, fs_fixture};

    fn create_state_db(path: &Path) {
        let db = sqlite::open(path).unwrap();
        db.execute(
            "
                CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    source TEXT NOT NULL,
                    model TEXT,
                    started_at REAL NOT NULL,
                    message_count INTEGER DEFAULT 0,
                    input_tokens INTEGER DEFAULT 0,
                    output_tokens INTEGER DEFAULT 0,
                    cache_read_tokens INTEGER DEFAULT 0,
                    cache_write_tokens INTEGER DEFAULT 0,
                    reasoning_tokens INTEGER DEFAULT 0,
                    billing_provider TEXT,
                    estimated_cost_usd REAL,
                    actual_cost_usd REAL
                );
            ",
        )
        .unwrap();
    }

    fn insert_session(db: &sqlite::Connection, id: &str, model: &str, input: i64, output: i64) {
        let mut st = db
            .prepare(
                "
                    INSERT INTO sessions (
                        id, source, model, started_at, message_count,
                        input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                        billing_provider, estimated_cost_usd, actual_cost_usd
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                ",
            )
            .unwrap();
        st.bind((1, id)).unwrap();
        st.bind((2, "cli")).unwrap();
        st.bind((3, model)).unwrap();
        st.bind((4, 1_750_000_000.25_f64)).unwrap();
        st.bind((5, 1_i64)).unwrap();
        st.bind((6, input)).unwrap();
        st.bind((7, output)).unwrap();
        st.bind((8, 0_i64)).unwrap();
        st.bind((9, 0_i64)).unwrap();
        st.bind((10, 0_i64)).unwrap();
        st.bind((11, "anthropic")).unwrap();
        st.bind((12, 0.01_f64)).unwrap();
        st.bind((13, 0.01_f64)).unwrap();
        st.next().unwrap();
    }

    #[test]
    fn loads_billable_hermes_sessions_from_state_db() {
        let _cache_env = CacheEnv::new("hermes-loads-sqlite");
        let fixture = fs_fixture!({});
        let db_path = fixture.path("state.db");
        create_state_db(&db_path);
        let db = sqlite::open(&db_path).unwrap();
        let mut statement = db
            .prepare(
                "
                    INSERT INTO sessions (
                        id, source, model, started_at, message_count,
                        input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                        billing_provider, estimated_cost_usd, actual_cost_usd
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                ",
            )
            .unwrap();
        statement.bind((1, "session-1")).unwrap();
        statement.bind((2, "cli")).unwrap();
        statement.bind((3, "claude-sonnet-4-20250514")).unwrap();
        statement.bind((4, 1_750_000_000.25)).unwrap();
        statement.bind((5, 42_i64)).unwrap();
        statement.bind((6, 1200_i64)).unwrap();
        statement.bind((7, 300_i64)).unwrap();
        statement.bind((8, 50_i64)).unwrap();
        statement.bind((9, 20_i64)).unwrap();
        statement.bind((10, 10_i64)).unwrap();
        statement.bind((11, "anthropic")).unwrap();
        statement.bind((12, 0.12)).unwrap();
        statement.bind((13, 0.34)).unwrap();
        statement.next().unwrap();

        let pricing = PricingMap::load_embedded();
        let shared = SharedArgs {
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let _cleanup = EnvVarGuard::set("HERMES_HOME", fixture.root());
        let entries = load_entries(&shared, &pricing).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].date, "2025-06-15");
        assert_eq!(entries[0].session_id.as_ref(), "session-1");
        assert_eq!(
            entries[0].model.as_deref(),
            Some("claude-sonnet-4-20250514")
        );
        assert_eq!(entries[0].data.message.usage.input_tokens, 1200);
        assert_eq!(entries[0].data.message.usage.output_tokens, 300);
        assert_eq!(
            entries[0].data.message.usage.cache_creation_input_tokens,
            20
        );
        assert_eq!(entries[0].data.message.usage.cache_read_input_tokens, 50);
        assert_eq!(entries[0].extra_total_tokens, 10);
        assert_eq!(entries[0].message_count, Some(42));
        assert_eq!(entries[0].cost, 0.34);
    }

    #[test]
    fn fingerprint_detects_inplace_token_edit() {
        let fixture = fs_fixture!({});
        let db_path = fixture.root().join("hermes.db");
        create_state_db(&db_path);
        {
            let db = sqlite::open(&db_path).unwrap();
            insert_session(&db, "s1", "claude-sonnet-4-20250514", 100, 50);
        }
        let fp1 = fingerprint_hermes_db(&db_path).expect("must return Some after insert");

        // Update input_tokens in place (same id, same row count).
        {
            let db = sqlite::open(&db_path).unwrap();
            db.execute("UPDATE sessions SET input_tokens = 999 WHERE id = 's1'")
                .unwrap();
        }
        let fp2 = fingerprint_hermes_db(&db_path).expect("must return Some after update");
        assert_ne!(
            fp1, fp2,
            "fingerprint must change when tokens are edited in place"
        );
    }

    #[test]
    fn fingerprint_uses_stable_hasher() {
        let fixture = fs_fixture!({});
        let db_path = fixture.root().join("hermes.db");
        create_state_db(&db_path);
        {
            let db = sqlite::open(&db_path).unwrap();
            insert_session(&db, "s1", "claude-sonnet-4-20250514", 100, 50);
        }
        let fp_a = fingerprint_hermes_db(&db_path).expect("Some");
        let fp_b = fingerprint_hermes_db(&db_path).expect("Some");
        assert_eq!(fp_a, fp_b, "fingerprint must be deterministic across calls");
    }

    #[test]
    fn dedup_first_path_order_survives() {
        let _cache_env = CacheEnv::new("hermes-dedup-order");
        let first = fs_fixture!({});
        let second = fs_fixture!({});
        {
            let db_path = first.root().join("state.db");
            create_state_db(&db_path);
            let db = sqlite::open(&db_path).unwrap();
            insert_session(&db, "shared-session", "claude-sonnet-4-20250514", 100, 50);
        }
        {
            let db_path = second.root().join("state.db");
            create_state_db(&db_path);
            let db = sqlite::open(&db_path).unwrap();
            insert_session(&db, "shared-session", "claude-sonnet-4-20250514", 999, 999);
        }
        let env_value = format!("{},{}", first.root().display(), second.root().display());
        let _cleanup = EnvVarGuard::set("HERMES_HOME", &env_value);
        let shared = SharedArgs {
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();

        assert_eq!(entries.len(), 1, "only one session should survive dedup");
        assert_eq!(entries[0].data.message.usage.input_tokens, 100);
        assert_eq!(entries[0].data.message.usage.output_tokens, 50);
    }
}
