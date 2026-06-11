use std::{
    collections::HashSet,
    hash::{DefaultHasher, Hash, Hasher},
    path::Path,
};

use jiff::tz::TimeZone as JiffTimeZone;

use crate::{
    LoadedEntry, PricingMap, Result, adapter::sqlite_util::open_readonly, cli::SharedArgs,
    debug_log, parse_tz,
};

use super::{parser::row_to_entry, paths::goose_db_paths};

const GOOSE_SESSION_QUERY: &str = r#"
SELECT
    id,
    model_config_json,
    provider_name,
    created_at,
    total_tokens,
    input_tokens,
    output_tokens,
    accumulated_total_tokens,
    accumulated_input_tokens,
    accumulated_output_tokens
FROM sessions
WHERE model_config_json IS NOT NULL
    AND TRIM(model_config_json) != ''
"#;

pub(crate) fn load_entries(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    crate::progress::track_usage_load(crate::progress::UsageLoadAgent::Goose, shared.json, || {
        load_entries_inner(shared, pricing)
    })
}
fn load_entries_inner(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    let tz = parse_tz(shared.timezone.as_deref());
    let db_paths = goose_db_paths()?;
    let all = crate::cache::load_with_cache(
        "goose",
        &db_paths,
        crate::cache::CacheOpts {
            single_thread: shared.single_thread,
            live_only: shared.live_only,
        },
        crate::cache::Freshness::Fingerprint(fingerprint_goose_db),
        |db_path| {
            let entries = load_entries_from_db(db_path, tz.as_ref(), pricing, shared)?;
            let mut seen = HashSet::new();
            Ok(entries
                .into_iter()
                .filter(|e| seen.insert(e.session_id.to_string()))
                .collect())
        },
        |e| super::parser::reprice(e, pricing),
    )?;
    let mut entries = all;
    entries.sort_by_key(|entry| entry.timestamp);
    Ok(entries)
}

/// Content fingerprint for a Goose SQLite database.
///
/// Hashes all billable columns per row in stable order (`ORDER BY id`)
/// so any in-place edit to token counts, model config, provider, or
/// timestamp changes the fingerprint.
fn fingerprint_goose_db(path: &Path) -> Option<u64> {
    let conn = open_readonly(path).ok()?;

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
            "SELECT id, model_config_json, provider_name, created_at, \
             total_tokens, input_tokens, output_tokens, \
             accumulated_total_tokens, accumulated_input_tokens, \
             accumulated_output_tokens \
             FROM sessions ORDER BY id",
        )
        .ok()?;

    let mut hasher = DefaultHasher::new();
    2u8.hash(&mut hasher); // discriminant: goose schema

    let mut count: u64 = 0;
    while let Ok(sqlite::State::Row) = st.next() {
        count += 1;
        let id: String = st.read(0).ok()?;
        let model_config: String = st.read(1).ok()?;
        let provider: Option<String> = st.read(2).ok();
        let created_at: String = st.read(3).ok()?;
        let total: Option<i64> = st.read(4).ok();
        let input: Option<i64> = st.read(5).ok();
        let output: Option<i64> = st.read(6).ok();
        let acc_total: Option<i64> = st.read(7).ok();
        let acc_input: Option<i64> = st.read(8).ok();
        let acc_output: Option<i64> = st.read(9).ok();

        id.hash(&mut hasher);
        model_config.hash(&mut hasher);
        provider.hash(&mut hasher);
        created_at.hash(&mut hasher);
        total.hash(&mut hasher);
        input.hash(&mut hasher);
        output.hash(&mut hasher);
        acc_total.hash(&mut hasher);
        acc_input.hash(&mut hasher);
        acc_output.hash(&mut hasher);
    }
    count.hash(&mut hasher);

    Some(hasher.finish())
}

fn load_entries_from_db(
    db_path: &Path,
    tz: Option<&JiffTimeZone>,
    pricing: &PricingMap,
    shared: &SharedArgs,
) -> Result<Vec<LoadedEntry>> {
    let Ok(connection) =
        sqlite::Connection::open_with_flags(db_path, sqlite::OpenFlags::new().with_read_only())
    else {
        debug_log(
            shared,
            format!("Failed to open Goose database: {}", db_path.display()),
        );
        return Ok(Vec::new());
    };
    let Ok(mut statement) = connection.prepare(GOOSE_SESSION_QUERY) else {
        debug_log(
            shared,
            format!("Failed to read Goose database: {}", db_path.display()),
        );
        return Ok(Vec::new());
    };

    let mut entries = Vec::new();
    loop {
        match statement.next() {
            Ok(sqlite::State::Row) => {
                if let Some(entry) = row_to_entry(&statement, tz, pricing) {
                    entries.push(entry);
                }
            }
            Ok(sqlite::State::Done) => break,
            Err(_) => {
                debug_log(
                    shared,
                    format!("Failed to query Goose database: {}", db_path.display()),
                );
                break;
            }
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use ccusage_test_support::fs_fixture;

    fn create_goose_db(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let db = sqlite::open(path).unwrap();
        db.execute(
            r#"
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    model_config_json TEXT,
    provider_name TEXT,
    created_at TEXT,
    total_tokens INTEGER,
    input_tokens INTEGER,
    output_tokens INTEGER,
    accumulated_total_tokens INTEGER,
    accumulated_input_tokens INTEGER,
    accumulated_output_tokens INTEGER
)
"#,
        )
        .unwrap();
    }

    struct SessionFixture<'a> {
        id: &'a str,
        model_config: &'a str,
        provider: Option<&'a str>,
        created_at: &'a str,
        total: i64,
        input: i64,
        output: i64,
    }

    fn insert_session(path: &Path, fixture: SessionFixture<'_>) {
        let db = sqlite::open(path).unwrap();
        let mut statement = db
            .prepare(
                r#"
INSERT INTO sessions (
    id,
    model_config_json,
    provider_name,
    created_at,
    accumulated_total_tokens,
    accumulated_input_tokens,
    accumulated_output_tokens
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
"#,
            )
            .unwrap();
        statement.bind((1, fixture.id)).unwrap();
        statement.bind((2, fixture.model_config)).unwrap();
        statement.bind((3, fixture.provider)).unwrap();
        statement.bind((4, fixture.created_at)).unwrap();
        statement.bind((5, fixture.total)).unwrap();
        statement.bind((6, fixture.input)).unwrap();
        statement.bind((7, fixture.output)).unwrap();
        statement.next().unwrap();
    }

    #[test]
    fn loads_accumulated_tokens_from_goose_sqlite() {
        let fixture = fs_fixture!({});
        let db_path = fixture.path(super::super::paths::GOOSE_DB_FILE_NAME);
        create_goose_db(&db_path);
        insert_session(
            &db_path,
            SessionFixture {
                id: "session-a",
                model_config: r#"{"model_name":"claude-sonnet-4-20250514"}"#,
                provider: Some("anthropic"),
                created_at: "2026-05-01 01:02:03",
                total: 180,
                input: 100,
                output: 50,
            },
        );

        let pricing = PricingMap::load_embedded();
        let entries = load_entries_from_db(
            &db_path,
            Some(&jiff::tz::TimeZone::UTC),
            &pricing,
            &SharedArgs::default(),
        )
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].date, "2026-05-01");
        assert_eq!(entries[0].session_id.as_ref(), "session-a");
        assert_eq!(entries[0].data.message.usage.input_tokens, 100);
        assert_eq!(entries[0].data.message.usage.output_tokens, 50);
        assert_eq!(entries[0].extra_total_tokens, 30);
    }
    #[test]
    fn loads_entries_through_cache() {
        use crate::cache::tests::CacheEnv;
        let _cache_env = CacheEnv::new("goose-cache-correctness");
        let fixture = fs_fixture!({});
        let db_path = fixture.path("data/sessions/sessions.db");
        create_goose_db(&db_path);
        insert_session(
            &db_path,
            SessionFixture {
                id: "session-a",
                model_config: r#"{"model_name":"claude-sonnet-4-20250514"}"#,
                provider: Some("anthropic"),
                created_at: "2026-05-01 01:02:03",
                total: 180,
                input: 100,
                output: 50,
            },
        );
        let _cleanup = ccusage_test_support::EnvVarGuard::set(
            super::super::paths::GOOSE_PATH_ROOT_ENV,
            fixture.root(),
        );
        let shared = SharedArgs {
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].date, "2026-05-01");
        assert_eq!(entries[0].session_id.as_ref(), "session-a");
        assert_eq!(entries[0].data.message.usage.input_tokens, 100);
        assert_eq!(entries[0].data.message.usage.output_tokens, 50);
        assert_eq!(entries[0].extra_total_tokens, 30);
    }

    #[test]
    fn fingerprint_detects_inplace_token_edit() {
        let fixture = fs_fixture!({});
        let db_path = fixture.path(super::super::paths::GOOSE_DB_FILE_NAME);
        create_goose_db(&db_path);
        insert_session(
            &db_path,
            SessionFixture {
                id: "session-a",
                model_config: r#"{"model_name":"claude-sonnet-4-20250514"}"#,
                provider: Some("anthropic"),
                created_at: "2026-05-01 01:02:03",
                total: 180,
                input: 100,
                output: 50,
            },
        );
        let fp1 =
            super::fingerprint_goose_db(&db_path).expect("must return Some for valid sessions DB");

        // Bump input_tokens in-place (same row, same row count).
        {
            let db = sqlite::open(&db_path).unwrap();
            db.execute("UPDATE sessions SET input_tokens = 999 WHERE id = 'session-a'")
                .unwrap();
        }
        let fp2 = super::fingerprint_goose_db(&db_path).expect("must return Some after update");
        assert_ne!(
            fp1, fp2,
            "fingerprint must change when tokens are bumped in-place"
        );
    }

    #[test]
    fn deduplicates_within_db_but_preserves_across_dbs() {
        use crate::cache::tests::CacheEnv;
        let _cache_env = CacheEnv::new("goose-dedup-cross-db");
        let first = fs_fixture!({});
        let second = fs_fixture!({});
        // Same session_id in both DBs — both should survive.
        for fixture in [&first, &second] {
            let db_path = fixture.path("data/sessions/sessions.db");
            create_goose_db(&db_path);
            insert_session(
                &db_path,
                SessionFixture {
                    id: "session-a",
                    model_config: r#"{"model_name":"claude-sonnet-4-20250514"}"#,
                    provider: Some("anthropic"),
                    created_at: "2026-05-01 01:02:03",
                    total: 180,
                    input: 100,
                    output: 50,
                },
            );
        }
        let _cleanup = ccusage_test_support::EnvVarGuard::set(
            super::super::paths::GOOSE_PATH_ROOT_ENV,
            format!("{},{}", first.root().display(), second.root().display()),
        );
        let shared = SharedArgs {
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();
        assert_eq!(
            entries.len(),
            2,
            "same session_id in different DBs must both survive"
        );
    }
}
