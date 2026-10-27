//! GH #349: a legacy 0.2.0 archive with a populated *internal-content* FTS5
//! `fts_messages` must complete `MIGRATION_V14` without the unbounded teardown
//! that OOMed, while preserving canonical messages.
//!
//! This lives at the crate boundary (public `FrankenStorage::open`) so the
//! regression runs even when test-only modules elsewhere are mid-refactor.

use std::path::{Path, PathBuf};

use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::sources::provenance::LOCAL_SOURCE_ID;
use coding_agent_search::storage::sqlite::{CURRENT_SCHEMA_VERSION, FrankenStorage};
use tempfile::TempDir;

fn fixture_conn(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path).expect("open rusqlite fixture connection")
}

#[test]
fn legacy_internal_content_fts_migrates_and_preserves_canonical_messages() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("legacy-internal-content.db");

    {
        let storage = FrankenStorage::open(&db_path).unwrap();
        let agent_id = storage
            .ensure_agent(&Agent {
                id: None,
                slug: "codex".into(),
                name: "Codex".into(),
                version: None,
                kind: AgentKind::Cli,
            })
            .unwrap();
        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(PathBuf::from("/legacy/workspace")),
            external_id: Some("legacy-internal-content".into()),
            title: Some("Legacy internal-content FTS".into()),
            source_path: PathBuf::from("/legacy/session.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_100),
            approx_tokens: Some(64),
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: Some("user".into()),
                created_at: Some(1_700_000_000_050),
                content: "canonical message must survive the derived-fts teardown".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.into(),
            origin_host: None,
        };
        storage
            .insert_conversation_tree(agent_id, None, &conversation)
            .unwrap();
    }

    // Rewind to the 0.2.0 bookkeeping shape: no migration ledger, meta
    // version 13, and a populated STOCK internal-content FTS5 table (the
    // `_content`/`_docsize`/`_data`/`_idx` shadows frankensqlite must tear
    // down). `_schema_migrations` is dropped so the real transition path
    // backfills 1..=13 and leaves v14 pending.
    {
        let fixture = fixture_conn(&db_path);
        // ubs:ignore[rust.domain.sql-concat] -- This immutable disposable legacy fixture uses SQL recursion and arithmetic only; no runtime text constructs the statement.
        const LEGACY_FTS_FIXTURE_SQL: &str = "DROP TABLE IF EXISTS fts_messages;
                 DROP TABLE IF EXISTS _schema_migrations;
                 -- A 0.2.0 archive predates V23. Its re-seed is not
                 -- idempotent while the authority row exists, because the
                 -- BEFORE INSERT guard aborts even under OR IGNORE.
                 DROP TABLE IF EXISTS semantic_source_identity_v1;
                 DROP TABLE IF EXISTS semantic_source_certificates_v1;
                 INSERT OR REPLACE INTO meta(key, value)
                 VALUES ('schema_version', '13');
                 CREATE VIRTUAL TABLE fts_messages USING fts5(
                     content, title, agent, workspace, source_path,
                     created_at UNINDEXED, message_id UNINDEXED,
                     tokenize='porter');
                 INSERT INTO fts_messages(
                     rowid, content, title, agent, workspace, source_path,
                     created_at, message_id)
                 SELECT id, content, 'legacy', 'codex', '/legacy/workspace',
                        '/legacy/session.jsonl', created_at, id
                   FROM messages;
                 BEGIN;
                 WITH RECURSIVE seq(n) AS (
                     SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 8192
                 )
                 INSERT INTO fts_messages(
                     content, title, agent, workspace, source_path,
                     created_at, message_id)
                 SELECT printf('legacy-fts-bulk-%06d ', n) || hex(randomblob(1024)),
                        'legacy', 'codex', '/legacy/workspace',
                        '/legacy/session.jsonl', 1700000000000, 100000 + n
                   FROM seq;
                 COMMIT;";
        fixture.execute_batch(LEGACY_FTS_FIXTURE_SQL).unwrap();
    }

    // The fixture must be genuinely structural: real, populated
    // internal-content shadows spanning multiple teardown batches.
    {
        let check = fixture_conn(&db_path);
        let content_rows: i64 = check
            .query_row("SELECT COUNT(*) FROM fts_messages_content", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(content_rows, 8193);
        let content_bytes: i64 = check
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(c0)), 0) FROM fts_messages_content",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            content_bytes > 8 * 1024 * 1024,
            "fixture corpus must span multiple bounded teardown batches, got {content_bytes} bytes"
        );
        let ledger_present: i64 = check
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = '_schema_migrations'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ledger_present, 0, "fixture must take the transition path");
    }

    let storage = FrankenStorage::open(&db_path).expect("populated legacy archive must migrate");
    assert_eq!(storage.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);

    let canonical: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM messages \
             WHERE content = 'canonical message must survive the derived-fts teardown'",
            &[],
            |row| row.get_typed(0),
        )
        .unwrap();
    assert_eq!(
        canonical, 1,
        "the derived-FTS teardown must preserve messages"
    );

    let versions: Vec<i64> = storage
        .raw()
        .query("SELECT version FROM _schema_migrations ORDER BY version;")
        .unwrap()
        .iter()
        .map(|row| row.get_typed(0).unwrap())
        .collect();
    assert_eq!(
        versions,
        (1..=CURRENT_SCHEMA_VERSION).collect::<Vec<i64>>(),
        "transition plus attributed steps must record the full ledger"
    );

    let residual: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE name = 'fts_messages' OR name LIKE 'fts_messages!_%' ESCAPE '!';",
            &[],
            |row| row.get_typed(0),
        )
        .unwrap();
    assert_eq!(residual, 0, "bounded teardown must remove the whole shadow");

    drop(storage);
    let reopened = FrankenStorage::open(&db_path).expect("migrated archive must reopen");
    let canonical_after_reopen: i64 = reopened
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM messages \
             WHERE content = 'canonical message must survive the derived-fts teardown'",
            &[],
            |row| row.get_typed(0),
        )
        .unwrap();
    assert_eq!(canonical_after_reopen, 1);
}

/// The bounded teardown is only bounded if it commits its page-free batches on
/// its own. A caller that wraps `run_migrations` in a transaction must be
/// refused rather than silently turning every batch into one transaction
/// again (GH #349).
#[test]
fn legacy_fts_teardown_requires_the_autocommit_lane() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("pending-v14.db");
    let storage = FrankenStorage::open(&db_path).unwrap();

    // Rewind the same open connection to a pending-V14 shape with a legacy
    // `fts_messages` present, without reopening (open runs migrations).
    storage
        .raw()
        .execute_batch(
            "DROP TABLE IF EXISTS _schema_migrations;
             DROP TABLE IF EXISTS semantic_source_identity_v1;
             DROP TABLE IF EXISTS semantic_source_certificates_v1;
             INSERT OR REPLACE INTO meta(key, value) VALUES ('schema_version', '13');
             DROP TABLE IF EXISTS fts_messages;
             CREATE VIRTUAL TABLE fts_messages USING fts5(content);",
        )
        .unwrap();

    storage.raw().execute("BEGIN IMMEDIATE").unwrap();
    let err = storage
        .run_migrations()
        .expect_err("an explicit transaction must refuse the bounded teardown");
    assert!(
        format!("{err:#}").contains("outside an explicit transaction"),
        "refusal must name the autocommit lane: {err:#}"
    );
    storage.raw().execute("ROLLBACK").unwrap();

    // The same connection in autocommit completes the migration.
    storage.run_migrations().unwrap();
    assert_eq!(storage.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
    let residual: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE name = 'fts_messages' OR name LIKE 'fts_messages!_%' ESCAPE '!';",
            &[],
            |row| row.get_typed(0),
        )
        .unwrap();
    assert_eq!(
        residual, 0,
        "autocommit teardown must drop the legacy table"
    );
}
