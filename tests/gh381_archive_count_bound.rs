//! GH #381 (coding_agent_session_search-2l1b0.5): the canonical-archive
//! count is bounded, measurable, and honest when it cannot answer.
//!
//! The reported shape is 468,216 messages across 10,252 conversations; the
//! `reported_shape_*` test below reproduces that shape with a synthetic
//! fixture and is `#[ignore]`d so the default test run stays cheap. Both
//! tests drive the same production code path
//! (`indexer::archive_count::measure_archive_count`).
//!
//! What is proven here:
//!
//! * the bounded count equals an independent oracle (stock SQLite via the
//!   `sqlite3` CLI) on the same file;
//! * an exhausted budget yields *unknown*, never a fabricated zero, and
//!   never mutates the archive;
//! * the count memo has the three states the acceptance names: cached,
//!   uncached and missing metadata;
//! * the count still answers with an active reader and with an interrupted
//!   (dirty-WAL) writer;
//! * the measurement separates the count stage from the total wall time and
//!   captures the RSS and block-IO deltas.

use std::path::Path;
use std::time::Duration;

use coding_agent_search::franken_sync::compat::TransactionExt;
use coding_agent_search::franken_sync::{Connection, SqliteValue};
use coding_agent_search::indexer::archive_count::{
    ArchiveCountOutcome, ArchiveCountUnknownReason, archive_count_cache_path, measure_archive_count,
};
use coding_agent_search::storage::sqlite::FrankenStorage;

/// Conversations of the shape reported in GH #381.
const REPORTED_CONVERSATIONS: usize = 10_252;
/// Messages of the shape reported in GH #381.
const REPORTED_MESSAGES: usize = 468_216;

/// Conversations of the cheap default shape.
const SMALL_CONVERSATIONS: usize = 64;
/// Messages of the cheap default shape.
const SMALL_MESSAGES: usize = 4_096;

/// A representative view of the archive files, used to prove that a count
/// does not mutate the archive.
#[derive(Debug, PartialEq, Eq)]
struct ArchiveFootprint {
    db_len: u64,
    db_mtime_nanos: (u64, u32),
    wal_len: u64,
    wal_bytes: Vec<u8>,
}

fn footprint(db_path: &Path) -> ArchiveFootprint {
    let db_meta = std::fs::metadata(db_path).expect("stat the canonical archive");
    let db_mtime = db_meta
        .modified()
        .expect("canonical archive mtime")
        .duration_since(std::time::UNIX_EPOCH)
        .expect("canonical archive mtime is after the epoch");
    let wal_path = format!("{}-wal", db_path.to_string_lossy());
    let wal_bytes = std::fs::read(&wal_path).unwrap_or_default();
    ArchiveFootprint {
        db_len: db_meta.len(),
        db_mtime_nanos: (db_mtime.as_secs(), db_mtime.subsec_nanos()),
        wal_len: wal_bytes.len() as u64,
        wal_bytes,
    }
}

/// The independent oracle: stock SQLite counting the same file.
fn oracle_message_count(db_path: &Path) -> i64 {
    let output = std::process::Command::new("sqlite3")
        .arg("-readonly")
        .arg(db_path)
        .arg("SELECT COUNT(*) FROM messages;")
        .output()
        .expect("the GH381 oracle requires the sqlite3 CLI");
    assert!(
        output.status.success(),
        "stock SQLite count failed: {output:?}"
    );
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("numeric count from stock SQLite")
}

/// Build a canonical archive with the production schema and `messages`
/// rows, spread evenly over `conversations` conversations.
fn build_archive(db_path: &Path, conversations: usize, messages: usize) -> (FrankenStorage, u64) {
    let open_started = std::time::Instant::now();
    let storage = FrankenStorage::open(db_path).expect("open the canonical archive");
    let open_ms = open_started.elapsed().as_millis() as u64;
    {
        let raw = storage.raw();
        let mut transaction = raw.transaction().expect("begin the fixture transaction");
        transaction
            .execute(
                "INSERT OR IGNORE INTO agents (id, slug, name, kind, created_at, updated_at) \
             VALUES (1, 'gh381-fixture', 'GH381 fixture', 'test', 0, 0)",
            )
            .expect("seed the fixture agent");

        let conversation_sql = "INSERT INTO conversations \
         (id, agent_id, source_id, external_id, source_path, started_at) \
         VALUES (?, 1, 'local', ?, ?, 0)";
        for id in 1..=conversations {
            transaction
                .execute_with_params(
                    conversation_sql,
                    &[
                        SqliteValue::Integer(id as i64),
                        SqliteValue::Text(format!("gh381-conversation-{id}").into()),
                        SqliteValue::Text(format!("/tmp/gh381/{id}").into()),
                    ],
                )
                .expect("insert a fixture conversation");
        }

        const ROWS_PER_STATEMENT: usize = 256;
        let mut params: Vec<SqliteValue> = Vec::with_capacity(ROWS_PER_STATEMENT * 5);
        let mut inserted = 0usize;
        while inserted < messages {
            let batch = ROWS_PER_STATEMENT.min(messages - inserted);
            // The statement must declare exactly the placeholders this batch
            // binds: fsqlite rejects binding an index the statement does not
            // have, so a full-size statement with a short final batch would
            // fail instead of leaving the tail NULL.
            let mut message_sql = String::from(
                "INSERT INTO messages (conversation_id, idx, role, content, created_at) VALUES ",
            );
            for row in 0..batch {
                if row > 0 {
                    message_sql.push(',');
                }
                message_sql.push_str("(?,?,?,?,?)");
            }
            params.clear();
            for offset in 0..batch {
                let index = inserted + offset;
                let conversation = (index % conversations) + 1;
                params.push(SqliteValue::Integer(conversation as i64));
                params.push(SqliteValue::Integer((index / conversations) as i64));
                params.push(SqliteValue::Text(
                    if offset % 2 == 0 { "user" } else { "assistant" }
                        .to_string()
                        .into(),
                ));
                params.push(SqliteValue::Text(
                    format!(
                "gh381 fixture message {index} with representative content so the b-tree leaf pages carry payload"
            )
                    .into(),
                ));
                params.push(SqliteValue::Integer(index as i64));
            }
            transaction
                .execute_with_params(&message_sql, &params)
                .expect("insert fixture messages");
            inserted += batch;
        }
        transaction
            .commit()
            .expect("commit the fixture transaction");
    }
    (storage, open_ms)
}

#[test]
fn bounded_count_matches_the_independent_oracle_and_leaves_the_archive_untouched() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let db_path = dir.path().join("agent_search.db");
    let index_dir = dir.path().join("index");
    let (storage, open_ms) = build_archive(&db_path, SMALL_CONVERSATIONS, SMALL_MESSAGES);

    let before = footprint(&db_path);
    let measured = measure_archive_count(&storage, &db_path, &index_dir, Duration::from_secs(30))
        .expect("bounded archive count");
    eprintln!(
        "gh381 measurement (small shape): open_ms={open_ms} count_stage_ms={} wall_ms={} rss_before={:?} rss_after={:?} io_before={:?} io_after={:?} outcome={:?}",
        measured.outcome.elapsed_ms(),
        measured.wall_ms,
        measured.rss_bytes_before,
        measured.rss_bytes_after,
        measured.io_bytes_before,
        measured.io_bytes_after,
        measured.outcome,
    );

    assert_eq!(
        measured.outcome.exact_count(),
        Some(SMALL_MESSAGES),
        "the bounded count must be exact"
    );
    assert_eq!(
        oracle_message_count(&db_path),
        SMALL_MESSAGES as i64,
        "the exact count must match the independent oracle"
    );
    assert_eq!(
        footprint(&db_path),
        before,
        "the bounded count must not mutate the archive"
    );
    assert!(
        measured.rss_bytes_after.is_some(),
        "the measurement must capture the resident set size"
    );
    assert!(
        measured.wall_ms >= measured.outcome.elapsed_ms(),
        "the wall time must cover the count stage"
    );
}

#[test]
fn an_exhausted_budget_yields_unknown_and_never_a_fabricated_zero() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let db_path = dir.path().join("agent_search.db");
    let index_dir = dir.path().join("index");
    let (storage, _open_ms) = build_archive(&db_path, SMALL_CONVERSATIONS, SMALL_MESSAGES);

    let before = footprint(&db_path);
    let measured = measure_archive_count(&storage, &db_path, &index_dir, Duration::ZERO)
        .expect("bounded archive count with an exhausted budget");
    eprintln!("gh381 measurement (exhausted budget): {measured:?}");

    assert!(
        measured.outcome.is_unknown(),
        "an exhausted budget must not report an exact count"
    );
    assert_eq!(
        measured.outcome.exact_count(),
        None,
        "an unknown count must never be reported as a number"
    );
    match &measured.outcome {
        ArchiveCountOutcome::Unknown { reason, .. } => assert!(matches!(
            reason,
            ArchiveCountUnknownReason::BudgetExhausted { .. }
        )),
        other => panic!("expected an unknown count, got {other:?}"),
    }
    assert_eq!(
        footprint(&db_path),
        before,
        "an unknown count must not mutate the archive"
    );
    assert!(
        !archive_count_cache_path(&index_dir).exists(),
        "an unknown count must never be memoized as an exact one"
    );
}

#[test]
fn the_count_memo_has_cached_uncached_and_missing_metadata_states() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let db_path = dir.path().join("agent_search.db");
    let index_dir = dir.path().join("index");
    let (storage, _open_ms) = build_archive(&db_path, SMALL_CONVERSATIONS, SMALL_MESSAGES);

    // Uncached: no memo exists yet, so the count is computed.
    assert!(!archive_count_cache_path(&index_dir).exists());
    let uncached = measure_archive_count(&storage, &db_path, &index_dir, Duration::from_secs(30))
        .expect("uncached bounded count");
    match uncached.outcome {
        ArchiveCountOutcome::Exact {
            count,
            from_cache,
            windows,
            ..
        } => {
            assert_eq!(count, SMALL_MESSAGES);
            assert!(!from_cache, "the first count must be computed");
            assert!(windows > 0, "the count must have read at least one window");
        }
        other => panic!("expected an exact count, got {other:?}"),
    }
    assert!(
        archive_count_cache_path(&index_dir).exists(),
        "the computed count must be memoized"
    );

    // Cached: the memo matches the archive identity, so the count is served
    // without reading a single message row.
    let cached = measure_archive_count(&storage, &db_path, &index_dir, Duration::from_secs(30))
        .expect("cached bounded count");
    match cached.outcome {
        ArchiveCountOutcome::Exact {
            count,
            from_cache,
            windows,
            ..
        } => {
            assert_eq!(count, SMALL_MESSAGES);
            assert!(from_cache, "the second count must be served from the memo");
            assert_eq!(windows, 0, "a memo hit must not read a window");
        }
        other => panic!("expected an exact count, got {other:?}"),
    }

    // Missing metadata: the archive is gone, so no count can be attributed
    // to a known archive state. That is unknown, never a zero.
    let missing = dir.path().join("missing.db");
    let missing_measured =
        measure_archive_count(&storage, &missing, &index_dir, Duration::from_secs(30))
            .expect("bounded count without archive metadata");
    assert!(
        missing_measured.outcome.is_unknown(),
        "missing archive metadata must not report an exact count"
    );
    match &missing_measured.outcome {
        ArchiveCountOutcome::Unknown { reason, .. } => assert!(matches!(
            reason,
            ArchiveCountUnknownReason::ArchiveMetadataUnavailable
        )),
        other => panic!("expected an unknown count, got {other:?}"),
    }
}

#[test]
fn the_count_answers_with_an_active_reader_and_an_interrupted_writer() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let db_path = dir.path().join("agent_search.db");
    let index_dir = dir.path().join("index");
    let (storage, _open_ms) = build_archive(&db_path, SMALL_CONVERSATIONS, SMALL_MESSAGES);
    storage.close().expect("close the fixture archive cleanly");

    // An interrupted writer: a second connection commits rows and then dies
    // before it can checkpoint, leaving a dirty WAL behind.
    {
        let writer = Connection::open(db_path.to_string_lossy().to_string())
            .expect("open the interrupted writer");
        writer
            .execute("BEGIN IMMEDIATE")
            .expect("begin the interrupted writer transaction");
        for index in 0..5 {
            writer
                .execute(&format!(
                    "INSERT INTO messages (conversation_id, idx, role, content, created_at) \
                     VALUES (1, 1000{}, 'user', 'interrupted writer row {}', 0)",
                    index, index
                ))
                .expect("insert an interrupted writer row");
        }
        writer
            .execute("COMMIT")
            .expect("commit the interrupted writer");
        // Leaking the connection skips the close-time checkpoint, which is
        // exactly the state a crashed writer leaves behind.
        std::mem::forget(writer);
    }
    let wal_path = format!("{}-wal", db_path.to_string_lossy());
    let wal_len = std::fs::metadata(&wal_path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    assert!(
        wal_len > 0,
        "precondition: the writer left a dirty WAL behind"
    );

    // The archive is reopened on top of the dirty WAL, and then an active
    // reader holds a read transaction open across the count.
    let storage = FrankenStorage::open(&db_path).expect("reopen the canonical archive");
    let reader =
        Connection::open(db_path.to_string_lossy().to_string()).expect("open the active reader");
    reader
        .execute("BEGIN")
        .expect("begin the active reader transaction");
    let reader_rows = reader
        .query("SELECT COUNT(*) FROM messages")
        .expect("the active reader counts the messages");
    assert_eq!(reader_rows.len(), 1, "the active reader sees one count row");

    let before = footprint(&db_path);
    let measured = measure_archive_count(&storage, &db_path, &index_dir, Duration::from_secs(30))
        .expect("bounded count with an active reader and a dirty WAL");
    eprintln!("gh381 measurement (active reader, dirty WAL): {measured:?}");

    assert_eq!(
        measured.outcome.exact_count(),
        Some(SMALL_MESSAGES + 5),
        "the count must see the committed rows of the interrupted writer"
    );
    assert_eq!(
        footprint(&db_path),
        before,
        "the count must not mutate the archive"
    );

    reader
        .execute("ROLLBACK")
        .expect("release the active reader");
}

/// The shape reported in GH #381: 468,216 messages across 10,252
/// conversations. Ignored by default because it writes a large archive.
#[test]
#[ignore = "writes the 468k-message reported shape; run explicitly"]
fn reported_shape_the_bounded_count_stays_exact_and_measured() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let db_path = dir.path().join("agent_search.db");
    let index_dir = dir.path().join("index");
    let (storage, build_open_ms) =
        build_archive(&db_path, REPORTED_CONVERSATIONS, REPORTED_MESSAGES);

    // Distinguish the engine's writable first contact from the count stage:
    // re-open the populated archive and time the open by itself.
    storage.close().expect("close the fixture archive");
    let open_started = std::time::Instant::now();
    let storage = FrankenStorage::open(&db_path).expect("reopen the populated archive");
    let populated_open_ms = open_started.elapsed().as_millis() as u64;

    let before = footprint(&db_path);
    let measured = measure_archive_count(&storage, &db_path, &index_dir, Duration::from_secs(300))
        .expect("bounded archive count on the reported shape");
    eprintln!(
        "gh381 measurement (reported shape): build_open_ms={build_open_ms} populated_open_ms={populated_open_ms} count_stage_ms={} wall_ms={} rss_before={:?} rss_after={:?} io_before={:?} io_after={:?} outcome={:?}",
        measured.outcome.elapsed_ms(),
        measured.wall_ms,
        measured.rss_bytes_before,
        measured.rss_bytes_after,
        measured.io_bytes_before,
        measured.io_bytes_after,
        measured.outcome,
    );

    assert_eq!(
        measured.outcome.exact_count(),
        Some(REPORTED_MESSAGES),
        "the bounded count must be exact on the reported shape"
    );
    assert!(
        measured.wall_ms >= measured.outcome.elapsed_ms(),
        "the wall time must cover the count stage"
    );
    assert!(
        measured.io_bytes_after.is_some(),
        "the measurement must capture block-IO on this platform"
    );
    assert_eq!(
        oracle_message_count(&db_path),
        REPORTED_MESSAGES as i64,
        "the exact count must match the independent oracle"
    );
    assert_eq!(
        footprint(&db_path),
        before,
        "the bounded count must not mutate the archive"
    );

    // Cached first access on the reported shape: the memo is keyed by the
    // archive's physical identity, so a second call must not re-read a row.
    let cached = measure_archive_count(&storage, &db_path, &index_dir, Duration::from_secs(300))
        .expect("cached archive count on the reported shape");
    eprintln!("gh381 measurement (reported shape, cached): {cached:?}");
    match cached.outcome {
        ArchiveCountOutcome::Exact {
            count,
            from_cache,
            windows,
            ..
        } => {
            assert_eq!(count, REPORTED_MESSAGES);
            assert!(from_cache, "the second count must be served from the memo");
            assert_eq!(windows, 0, "a memo hit must not read a window");
        }
        other => panic!("expected an exact cached count, got {other:?}"),
    }
}
