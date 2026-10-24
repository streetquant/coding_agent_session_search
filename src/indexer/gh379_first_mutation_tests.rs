//! GH #379 — the first mutation of an archive that carries a populated
//! contentless FTS5 shadow.
//!
//! GH #379 reported that the *first* mutation of a contentless FTS shadow
//! loaded the whole inverted index into memory: a 2.9 GB archive reached
//! 66 GB RSS plus 24 GB swap, and one 624 KB delta reportedly reached about
//! 10.6 GB against a 176 MB control. The landed policy (bead iify0) bounds
//! the derived shadow by message count (`CASS_FTS_SHADOW_MAX_MESSAGES`,
//! default 100,000 messages) and the indexer's pre-write admission drops a
//! shadow whose corpus is over that bound before any reader or writer opens
//! the archive.
//!
//! This module is the reproducible witness for the current tree. It builds a
//! sanitized *structural* fixture — the production canonical schema, the
//! production contentless shadow DDL, synthetic content only — records the
//! baseline archive health, the table and segment sizes, the inserted bytes
//! and an independent row/query oracle, and then exercises, in one measured
//! run per scenario:
//!
//! * the **first** mutation (the reported 624 KB delta), which carries the
//!   production inline shadow write — the reported shape is a *contentless
//!   FTS* mutation, so the witness measures the mutation that really is one;
//! * the **later** mutations, with their shadow rows removed afterwards to
//!   model a process that died between the canonical write and the shadow
//!   write;
//! * the **interruption/restart**: the shadow is left behind the canonical
//!   rows and the next run's repair path has to reconcile them;
//! * the **deferred shadow rebuild**: the production repair path's verdict,
//!   and what the explicit rebuild does over the bound;
//! * a **held reader**: a read-only connection open across a write.
//!
//! The two scenarios separate the engine's own allocation behavior from the
//! CASS admission: with the bound disabled the shadow is retained and the
//! first mutation meets the engine; with the bound below the fixture's corpus
//! the production admission retires the shadow first, so the first mutation
//! meets only the delta.
//!
//! The measurement runs in a child process (the test binary re-executed with
//! a name filter) so the resident-set numbers are not polluted by the other
//! unit tests the gate runs in the same process.
//!
//! The module is Linux-only: the resident-set probe reads
//! `/proc/self/statm` and `/proc/self/status`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::franken_sync::SqliteValue;
use crate::franken_sync::compat::{ConnectionExt, RowExt};
use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use crate::storage::sqlite::{
    FTS5_REGISTER_SQL, FrankenStorage, FtsConsistencyRepair,
    error_message_indicates_fts_shadow_not_viable,
};

use super::preflight_fts_shadow_before_lexical_readers;

/// The token the query oracle and the shadow query both use. It is
/// alphanumeric, so the porter tokenizer keeps it as one token and the
/// canonical `LIKE` oracle agrees with the FTS5 match exactly.
const NEEDLE: &str = "gh379needletoken";
/// The filler word: ordinary tokens the inverted index has to hold.
const FILLER_WORD: &str = "cassgh379filler";
/// The agent slug of the sanitized corpus.
const AGENT_SLUG: &str = "gh379";
/// The workspace path of the sanitized corpus.
const WORKSPACE_PATH: &str = "/gh379/workspace";
/// Conversations in the fixture corpus.
const FIXTURE_CONVERSATIONS: usize = 200;
/// Messages per fixture conversation.
const FIXTURE_MESSAGES_PER_CONVERSATION: usize = 10;
/// Filler words per fixture message.
const FIXTURE_MESSAGE_WORDS: usize = 64;
/// Messages in the reported-shape delta: one small session's worth.
const DELTA_MESSAGES: usize = 8;
/// The reported delta size (GH #379: one 624 KB delta).
const DELTA_BYTES: usize = 624 * 1024;
/// Later mutations: small conversations written after the first one.
const LATER_MUTATIONS: usize = 2;
/// Messages per later-mutation conversation.
const LATER_MUTATION_MESSAGES: usize = 2;
/// Words per later-mutation message (small: the point is the count, not the
/// size).
const LATER_MUTATION_WORDS: usize = 8;
/// Rows per fixture shadow insert statement. Each row binds seven values, so
/// this keeps the parameter list well inside the engine's statement limit.
const FIXTURE_SHADOW_INSERT_CHUNK: usize = 100;
/// The delete statement the production shadow maintenance uses.
const SHADOW_DELETE_SQL: &str = "DELETE FROM fts_messages WHERE rowid = ?1";

/// Environment variable the child process reads its archive path from.
const CHILD_DB_ENV: &str = "CASS_GH379_CHILD_DB";
/// Environment variable the child process writes its measurement to.
const CHILD_OUT_ENV: &str = "CASS_GH379_CHILD_OUT";
/// Environment variable carrying the fixture's measured content bytes into
/// the child that measures a scenario.
const CHILD_FIXTURE_BYTES_ENV: &str = "CASS_GH379_CHILD_FIXTURE_BYTES";
/// The name filter that selects the child test in the re-executed binary.
const CHILD_TEST_FILTER: &str = "gh379_child_measurement";

/// What one repair attempt did.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RepairOutcome {
    /// `already_healthy`, `incremental_catch_up`, `rebuilt` or `err`.
    kind: String,
    /// The rows the repair considered complete.
    rows: Option<usize>,
    /// The rows the repair inserted.
    inserted_rows: Option<usize>,
    /// The rows the repair considers complete.
    total_rows: Option<usize>,
    /// The failure detail, when the repair refused.
    error: Option<String>,
}

impl RepairOutcome {
    /// Record what the production repair path did.
    fn from_result(result: Result<FtsConsistencyRepair>) -> Self {
        match result {
            Ok(FtsConsistencyRepair::AlreadyHealthy { rows }) => Self {
                kind: "already_healthy".to_string(),
                rows: Some(rows),
                inserted_rows: None,
                total_rows: None,
                error: None,
            },
            Ok(FtsConsistencyRepair::IncrementalCatchUp {
                inserted_rows,
                total_rows,
            }) => Self {
                kind: "incremental_catch_up".to_string(),
                rows: None,
                inserted_rows: Some(inserted_rows),
                total_rows: Some(total_rows),
                error: None,
            },
            Ok(FtsConsistencyRepair::Rebuilt { inserted_rows }) => Self {
                kind: "rebuilt".to_string(),
                rows: None,
                inserted_rows: Some(inserted_rows),
                total_rows: None,
                error: None,
            },
            Err(err) => Self {
                kind: "err".to_string(),
                rows: None,
                inserted_rows: None,
                total_rows: None,
                error: Some(format!("{err:#}")),
            },
        }
    }
}

/// One derived shadow table's physical shape: how many rows it holds and the
/// logical bytes of the payload column the inverted index lives in.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ShadowTableSize {
    /// The shadow table name.
    table: String,
    /// Rows the table holds.
    rows: i64,
    /// Total logical bytes of its payload column (0 for `_config`).
    payload_bytes: i64,
}

/// What one scenario measured.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Witness {
    /// The archive the scenario ran against.
    db_path: String,
    /// The source tree the measuring binary was built from.
    source_sha: String,
    /// The cass version the measuring binary was built from.
    crate_version: String,
    /// The resolved fsqlite engine-family versions the binary links.
    engine_dependency: String,
    /// The measuring binary itself (the ELF under test).
    test_binary: String,
    /// A stable fingerprint of the corpus the scenario measured.
    corpus_fingerprint: String,
    /// The fixture's synthetic content bytes the scenario inherited.
    fixture_inserted_bytes: u64,
    /// The shadow bound in force (`None` = the bound was disabled).
    bound_messages: Option<u64>,
    /// The production pre-write admission's outcome, if it failed.
    admission_error: Option<String>,
    /// The indexable message count the admission judged against.
    corpus_messages: u64,
    /// Was the shadow still in the catalog after the admission?
    shadow_registered_after_admission: bool,
    /// Rows in the shadow after the admission.
    shadow_rows_after_admission: i64,
    /// Per-table baseline sizes of the derived shadow after the admission.
    shadow_tables_baseline: Vec<ShadowTableSize>,
    /// Canonical messages after the admission.
    canonical_messages_after_admission: i64,
    /// The persisted not-viable marker, if the shadow was retired.
    not_viable_marker: Option<String>,
    /// The persisted repair-pending marker, if the shadow was retired.
    repair_pending: Option<String>,
    /// Resident set before the archive was opened.
    rss_start_bytes: u64,
    /// Resident set after the writable open, before any mutation.
    rss_after_open_bytes: u64,
    /// Resident set after the first mutation.
    rss_after_first_mutation_bytes: u64,
    /// Peak resident set of the whole process.
    peak_rss_bytes: u64,
    /// Bytes the process read from storage during the first mutation.
    first_mutation_read_bytes: u64,
    /// Bytes the process wrote to storage during the first mutation.
    first_mutation_write_bytes: u64,
    /// Wall time of the first mutation, shadow write included.
    first_mutation_elapsed_ms: u64,
    /// The first mutation's own content bytes.
    first_mutation_bytes: u64,
    /// The engine's count of full live-vtab content rebuilds before the first
    /// mutation (fsqlite's GH#408 hook, per connection).
    engine_rebuilds_before_first_mutation: u64,
    /// The engine's count of full live-vtab content rebuilds after the first
    /// mutation.
    engine_rebuilds_after_first_mutation: u64,
    /// Canonical messages after the first mutation.
    canonical_messages_after_first_mutation: i64,
    /// Shadow rows after the first mutation (`None` when there is no shadow).
    shadow_rows_after_first_mutation: Option<i64>,
    /// Do the shadow's rowids equal the canonical message ids after the
    /// first mutation?
    shadow_rowids_equal_canonical_after_first_mutation: bool,
    /// The production repair path's verdict after the first mutation.
    repair_after_first_mutation: RepairOutcome,
    /// Wall time of the later mutations, in total.
    later_mutations_elapsed_ms: u64,
    /// Resident set after the later mutations.
    rss_after_later_mutations_bytes: u64,
    /// Canonical messages after the later mutations.
    canonical_messages_after_later_mutations: i64,
    /// Shadow rows after the later mutations' shadow rows were removed.
    shadow_rows_after_later_mutations: Option<i64>,
    /// The production repair path's verdict after the restart.
    repair_after_restart: RepairOutcome,
    /// Resident set after the restart's repair.
    rss_after_restart_bytes: u64,
    /// Did the mutation complete while a read-only connection was held open?
    held_reader_mutation_ok: bool,
    /// Wall time of the mutation that ran under the held reader.
    held_reader_mutation_elapsed_ms: u64,
    /// Resident set after the held-reader mutation.
    rss_after_held_reader_bytes: u64,
    /// The production repair path's verdict for the deferred rebuild.
    deferred_rebuild_repair: RepairOutcome,
    /// The explicit rebuild's outcome.
    explicit_rebuild_error: Option<String>,
    /// Was the shadow present after the explicit rebuild?
    shadow_present_after_explicit_rebuild: bool,
    /// Canonical messages at the end of the scenario.
    canonical_messages_final: i64,
    /// Shadow rows at the end of the scenario (`None` when there is no shadow).
    shadow_rows_final: Option<i64>,
    /// Per-table sizes of the derived shadow at the end of the scenario.
    shadow_tables_final: Vec<ShadowTableSize>,
    /// Do the shadow's rowids equal the canonical message ids exactly?
    shadow_rowids_equal_canonical: bool,
    /// The independent oracle: canonical ids whose content carries the needle.
    oracle_hit_ids: Vec<i64>,
    /// The shadow's own answer to the same query.
    shadow_hit_ids: Vec<i64>,
    /// The archive's on-disk size before the mutations.
    db_bytes_before: u64,
    /// The archive's on-disk size after the mutations.
    db_bytes_after: u64,
    /// The archive's page count after the mutations.
    page_count: i64,
    /// The archive's freelist page count after the mutations.
    freelist_count: i64,
    /// The archive's `PRAGMA quick_check` verdict.
    quick_check: String,
}

impl Witness {
    /// Resident set the first mutation cost, in bytes.
    fn first_mutation_rss_delta(&self) -> u64 {
        self.rss_after_first_mutation_bytes
            .saturating_sub(self.rss_after_open_bytes)
    }

    /// Resident set the whole scenario cost, in bytes.
    fn total_rss_delta(&self) -> u64 {
        self.rss_after_first_mutation_bytes
            .saturating_sub(self.rss_start_bytes)
    }
}

/// The fixture's canonical content: one needle token plus filler words.
fn message_content(words: usize) -> String {
    let mut content = String::with_capacity(NEEDLE.len() + words * (FILLER_WORD.len() + 1));
    content.push_str(NEEDLE);
    content.push(' ');
    for _ in 0..words {
        content.push_str(FILLER_WORD);
        content.push(' ');
    }
    content
}

/// A synthetic conversation. No private payload: every field is generated.
fn synthetic_conversation(
    prefix: &str,
    index: usize,
    message_count: usize,
    words_per_message: usize,
) -> Conversation {
    let messages = (0..message_count)
        .map(|idx| Message {
            id: None,
            idx: idx as i64,
            role: MessageRole::User,
            author: None,
            created_at: Some(1_760_000_000_000 + idx as i64),
            content: message_content(words_per_message),
            extra_json: serde_json::Value::Null,
            snippets: Vec::new(),
        })
        .collect();
    Conversation {
        id: None,
        agent_slug: AGENT_SLUG.to_string(),
        workspace: Some(PathBuf::from(WORKSPACE_PATH)),
        external_id: Some(format!("{prefix}-{index}")),
        title: Some(format!("gh379 {prefix} conversation {index}")),
        source_path: PathBuf::from(format!("/gh379/{prefix}/rollout-{index}.jsonl")),
        started_at: Some(1_760_000_000_000),
        ended_at: None,
        approx_tokens: None,
        metadata_json: serde_json::Value::Null,
        messages,
        source_id: "local".to_string(),
        origin_host: None,
    }
}

/// The fixture's agent.
fn synthetic_agent() -> Agent {
    Agent {
        id: None,
        slug: AGENT_SLUG.to_string(),
        name: "gh379 sanitized fixture".to_string(),
        version: None,
        kind: AgentKind::Cli,
    }
}

/// Build the sanitized structural fixture: the production canonical schema,
/// a synthetic corpus, and a populated contentless shadow built with the
/// production shadow DDL.
fn build_fixture(dir: &Path) -> Result<(PathBuf, u64)> {
    let db_path = dir.join("agent_search.db");
    let storage = FrankenStorage::open(&db_path).context("creating the fixture archive")?;
    let agent_id = storage
        .ensure_agent(&synthetic_agent())
        .context("inserting the fixture agent")?;
    let workspace_id = storage
        .ensure_workspace(Path::new(WORKSPACE_PATH), None)
        .context("inserting the fixture workspace")?;

    let mut inserted_bytes = 0u64;
    for index in 0..FIXTURE_CONVERSATIONS {
        let conversation = synthetic_conversation(
            "gh379-corpus",
            index,
            FIXTURE_MESSAGES_PER_CONVERSATION,
            FIXTURE_MESSAGE_WORDS,
        );
        for message in &conversation.messages {
            inserted_bytes += message.content.len() as u64;
        }
        storage
            .insert_conversation_tree(agent_id, Some(workspace_id), &conversation)
            .context("inserting a fixture conversation")?;
    }

    // Populate the derived shadow with the production DDL and the production
    // row shape, then prove the count on the connection and again after the
    // archive is closed and reopened: the scenario children copy the closed
    // archive, so a shadow that only exists in this connection's memory would
    // silently shrink the fixture.
    populate_shadow(&storage).context("populating the contentless shadow")?;
    let expected_rows = i64::try_from(FIXTURE_CONVERSATIONS * FIXTURE_MESSAGES_PER_CONVERSATION)
        .unwrap_or(i64::MAX);
    let shadow_rows = shadow_row_count(&storage)?.unwrap_or(0);
    if shadow_rows != expected_rows {
        bail!("the fixture shadow holds {shadow_rows} rows, expected {expected_rows}");
    }
    storage.close().context("closing the fixture archive")?;

    let reopened = FrankenStorage::open(&db_path).context("reopening the fixture archive")?;
    let persisted_rows = shadow_row_count(&reopened)?.unwrap_or(0);
    reopened
        .close()
        .context("closing the reopened fixture archive")?;
    if persisted_rows != expected_rows {
        bail!("the reopened fixture shadow holds {persisted_rows} rows, expected {expected_rows}");
    }
    Ok((db_path, inserted_bytes))
}

/// Create the contentless shadow with the production DDL and mirror the
/// canonical corpus into it.
fn populate_shadow(storage: &FrankenStorage) -> Result<i64> {
    storage
        .raw()
        .execute(FTS5_REGISTER_SQL)
        .context("creating the contentless fts_messages shadow")?;
    let rows = storage
        .raw()
        .query(
            "SELECT m.id, m.content, COALESCE(c.title, ''), a.slug, COALESCE(w.path, ''), \
             c.source_path, COALESCE(m.created_at, 0) \
             FROM messages m \
             JOIN conversations c ON m.conversation_id = c.id \
             JOIN agents a ON c.agent_id = a.id \
             LEFT JOIN workspaces w ON c.workspace_id = w.id \
             ORDER BY m.id",
        )
        .context("reading the canonical corpus for the shadow")?;
    let mut inserted = 0i64;
    for chunk in rows.chunks(FIXTURE_SHADOW_INSERT_CHUNK) {
        // One value tuple per row. The previous single-row template bound
        // seven values while the loop supplied seven per row, so every
        // statement silently inserted only its first row.
        let placeholders = chunk
            .iter()
            .enumerate()
            .map(|(index, _)| {
                let base = index * 7 + 1;
                format!(
                    "(?{base},?{},?{},?{},?{},?{},?{})",
                    base + 1,
                    base + 2,
                    base + 3,
                    base + 4,
                    base + 5,
                    base + 6
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "INSERT INTO fts_messages(rowid, content, title, agent, workspace, source_path, \
             created_at) VALUES {placeholders}"
        );
        let mut params: Vec<SqliteValue> = Vec::with_capacity(chunk.len() * 7);
        for row in chunk {
            params.push(SqliteValue::from(
                row.get_typed::<i64>(0)
                    .context("reading a canonical message id")?,
            ));
            params.push(SqliteValue::from(
                row.get_typed::<String>(1)
                    .context("reading a canonical message content")?,
            ));
            params.push(SqliteValue::from(
                row.get_typed::<String>(2)
                    .context("reading a canonical conversation title")?,
            ));
            params.push(SqliteValue::from(
                row.get_typed::<String>(3)
                    .context("reading the canonical agent slug")?,
            ));
            params.push(SqliteValue::from(
                row.get_typed::<String>(4)
                    .context("reading the canonical workspace path")?,
            ));
            params.push(SqliteValue::from(
                row.get_typed::<String>(5)
                    .context("reading the canonical source path")?,
            ));
            params.push(SqliteValue::from(
                row.get_typed::<i64>(6)
                    .context("reading the canonical created_at")?,
            ));
        }
        storage
            .raw()
            .execute_with_params(&sql, &params)
            .context("inserting fixture rows into the contentless shadow")?;
        inserted += i64::try_from(chunk.len()).unwrap_or(i64::MAX);
    }
    Ok(inserted)
}

/// Is the shadow in the catalog? Virtual tables carry `rootpage = 0`, so the
/// probe must not filter on rootpage.
fn shadow_registered(storage: &FrankenStorage) -> Result<bool> {
    let rows = storage.raw().query_row_map(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'fts_messages'",
        &[],
        |row| row.get_typed::<i64>(0),
    );
    Ok(rows.unwrap_or(0) > 0)
}

/// The shadow's row count, or `None` when there is no shadow.
fn shadow_row_count(storage: &FrankenStorage) -> Result<Option<i64>> {
    if !shadow_registered(storage)? {
        return Ok(None);
    }
    let count = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM fts_messages", &[], |row| {
            row.get_typed::<i64>(0)
        })
        .context("counting the contentless shadow rows")?;
    Ok(Some(count))
}

/// The derived shadow's per-table physical shape. `%_data` carries the
/// inverted-index blocks, `%_idx` the term-to-page map, `%_docsize` the
/// per-document sizes and `%_config` the table configuration. An absent
/// shadow has no tables, and a table the engine did not create is skipped.
fn shadow_table_sizes(storage: &FrankenStorage) -> Result<Vec<ShadowTableSize>> {
    if !shadow_registered(storage)? {
        return Ok(Vec::new());
    }
    let mut sizes = Vec::new();
    for (table, payload_column) in [
        ("fts_messages_data", Some("block")),
        ("fts_messages_idx", Some("term")),
        ("fts_messages_docsize", Some("sz")),
        ("fts_messages_config", None),
    ] {
        let present = storage
            .raw()
            .query_row_map(
                &format!(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '{table}'"
                ),
                &[],
                |row| row.get_typed::<i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !present {
            continue;
        }
        let sql = match payload_column {
            Some(column) => {
                format!("SELECT COUNT(*), COALESCE(SUM(LENGTH({column})), 0) FROM {table}")
            }
            None => format!("SELECT COUNT(*), 0 FROM {table}"),
        };
        let (rows, payload_bytes) = storage
            .raw()
            .query_row_map(&sql, &[], |row| {
                Ok((row.get_typed::<i64>(0)?, row.get_typed::<i64>(1)?))
            })
            .with_context(|| format!("measuring the derived shadow table {table}"))?;
        sizes.push(ShadowTableSize {
            table: table.to_string(),
            rows,
            payload_bytes,
        });
    }
    Ok(sizes)
}

/// The canonical message count.
fn canonical_message_count(storage: &FrankenStorage) -> Result<i64> {
    storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM messages", &[], |row| {
            row.get_typed::<i64>(0)
        })
        .context("counting the canonical messages")
}

/// The canonical message ids, in order.
fn canonical_message_ids(storage: &FrankenStorage) -> Result<Vec<i64>> {
    let rows = storage
        .raw()
        .query("SELECT id FROM messages ORDER BY id")
        .context("reading the canonical message ids")?;
    let mut ids = Vec::with_capacity(rows.len());
    for row in rows {
        ids.push(
            row.get_typed::<i64>(0)
                .context("reading a canonical message id")?,
        );
    }
    Ok(ids)
}

/// The canonical message ids of one conversation, in order.
fn conversation_message_ids(storage: &FrankenStorage, conversation_id: i64) -> Result<Vec<i64>> {
    let rows = storage
        .raw()
        .query_with_params(
            "SELECT id FROM messages WHERE conversation_id = ?1 ORDER BY id",
            &[SqliteValue::from(conversation_id)],
        )
        .context("reading a conversation's message ids")?;
    let mut ids = Vec::with_capacity(rows.len());
    for row in rows {
        ids.push(row.get_typed::<i64>(0).context("reading a message id")?);
    }
    Ok(ids)
}

/// The shadow's rowids, in order.
fn shadow_rowids(storage: &FrankenStorage) -> Result<Option<Vec<i64>>> {
    if !shadow_registered(storage)? {
        return Ok(None);
    }
    let rows = storage
        .raw()
        .query("SELECT rowid FROM fts_messages ORDER BY rowid")
        .context("reading the shadow rowids")?;
    let mut ids = Vec::with_capacity(rows.len());
    for row in rows {
        ids.push(row.get_typed::<i64>(0).context("reading a shadow rowid")?);
    }
    Ok(Some(ids))
}

/// Remove shadow rows the way the production shadow maintenance does, to
/// model a process that died between the canonical write and the shadow
/// write.
fn remove_shadow_rows(storage: &FrankenStorage, message_ids: &[i64]) -> Result<()> {
    for message_id in message_ids {
        storage
            .raw()
            .execute_with_params(SHADOW_DELETE_SQL, &[SqliteValue::from(*message_id)])
            .context("removing a shadow row")?;
    }
    Ok(())
}

/// The independent oracle: the canonical rows whose content carries the
/// needle. The shadow is derived data, so the canonical table is the truth.
fn oracle_hit_ids(storage: &FrankenStorage) -> Result<Vec<i64>> {
    let needle = format!("%{NEEDLE}%");
    let rows = storage
        .raw()
        .query_with_params(
            "SELECT id FROM messages WHERE content LIKE ?1 ORDER BY id",
            &[SqliteValue::from(needle.as_str())],
        )
        .context("running the canonical query oracle")?;
    let mut ids = Vec::with_capacity(rows.len());
    for row in rows {
        ids.push(row.get_typed::<i64>(0).context("reading an oracle hit")?);
    }
    Ok(ids)
}

/// The shadow's own answer to the same query.
fn shadow_hit_ids(storage: &FrankenStorage) -> Result<Option<Vec<i64>>> {
    if !shadow_registered(storage)? {
        return Ok(None);
    }
    let rows = storage
        .raw()
        .query_with_params(
            "SELECT rowid FROM fts_messages WHERE fts_messages MATCH ?1 ORDER BY rowid",
            &[SqliteValue::from(NEEDLE)],
        )
        .context("querying the contentless shadow")?;
    let mut ids = Vec::with_capacity(rows.len());
    for row in rows {
        ids.push(row.get_typed::<i64>(0).context("reading a shadow hit")?);
    }
    Ok(Some(ids))
}

/// The persisted markers that make a retired shadow a truthful state.
fn persisted_markers(storage: &FrankenStorage) -> Result<(Option<String>, Option<String>)> {
    let not_viable = storage
        .raw()
        .query_row_map(
            "SELECT value FROM meta WHERE key = 'fts_shadow_not_viable'",
            &[],
            |row| row.get_typed::<String>(0),
        )
        .ok();
    let repair_pending = storage
        .raw()
        .query_row_map(
            "SELECT value FROM meta WHERE key = 'fts_fallback_repair_pending'",
            &[],
            |row| row.get_typed::<String>(0),
        )
        .ok();
    Ok((not_viable, repair_pending))
}

/// The resolved fsqlite engine-family versions this test binary links.
///
/// Cargo exposes no compile-time variable for a dependency's version, so
/// read the lockfile the build resolved. A receipt has to name the engine as
/// well as the source tree: an engine change and a policy change look the
/// same in resident-set numbers alone.
fn engine_dependency_identity() -> String {
    const LOCKFILE: &str = include_str!("../../Cargo.lock");
    let mut packages: Vec<String> = Vec::new();
    let mut name: Option<&str> = None;
    for line in LOCKFILE.lines() {
        if let Some(rest) = line.strip_prefix("name = \"") {
            name = rest.strip_suffix('"');
        } else if let Some(rest) = line.strip_prefix("version = \"")
            && let Some(package) = name
            && matches!(package, "fsqlite" | "fsqlite-core")
        {
            let version = rest.strip_suffix('"').unwrap_or(rest);
            packages.push(format!("{package} {version}"));
        }
    }
    packages.join(", ")
}

/// A stable fingerprint of the corpus a scenario measured: the canonical
/// rows' identity and content, hashed. FNV-1a, so no dependency is added.
fn corpus_fingerprint(storage: &FrankenStorage) -> Result<String> {
    let rows = storage
        .raw()
        .query("SELECT id, content FROM messages ORDER BY id")
        .context("reading the corpus for its fingerprint")?;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
    };
    for row in rows {
        let id = row
            .get_typed::<i64>(0)
            .context("reading a canonical message id for the fingerprint")?;
        let content = row
            .get_typed::<String>(1)
            .context("reading a canonical message content for the fingerprint")?;
        mix(&id.to_le_bytes());
        mix(content.as_bytes());
    }
    Ok(format!("{hash:016x}"))
}

/// The archive's on-disk size, including a leftover WAL.
fn archive_bytes(db_path: &Path) -> u64 {
    let mut total = fs::metadata(db_path).map(|meta| meta.len()).unwrap_or(0);
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = db_path.as_os_str().to_os_string();
        sidecar.push(suffix);
        total += fs::metadata(PathBuf::from(sidecar))
            .map(|meta| meta.len())
            .unwrap_or(0);
    }
    total
}

/// The process's resident set in bytes (Linux `/proc/self/statm`).
fn rss_bytes() -> u64 {
    crate::daemon::resource::ResourceMonitor::new().memory_usage()
}

/// The process's peak resident set in bytes (Linux `/proc/self/status`).
fn peak_rss_bytes() -> u64 {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: u64 = rest
                .trim()
                .trim_end_matches(" kB")
                .trim()
                .parse()
                .unwrap_or(0);
            return kb.saturating_mul(1024);
        }
    }
    0
}

/// The process's cumulative block I/O in bytes (Linux `/proc/self/io`).
fn io_bytes() -> (u64, u64) {
    let Ok(io) = fs::read_to_string("/proc/self/io") else {
        return (0, 0);
    };
    let mut read = 0u64;
    let mut write = 0u64;
    for line in io.lines() {
        if let Some(value) = line.strip_prefix("read_bytes:") {
            read = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("write_bytes:") {
            write = value.trim().parse().unwrap_or(0);
        }
    }
    (read, write)
}

/// Run one scenario against `db_path` and report what it measured.
///
/// `fixture_inserted_bytes` is the parent's measured fixture content size,
/// carried into the child so the receipt records what it measured without
/// re-reading the fixture constants.
fn run_scenario(db_path: &Path, fixture_inserted_bytes: u64) -> Result<Witness> {
    let rss_start = rss_bytes();

    // The production pre-write admission: the indexer's own preflight, which
    // is the code path that decides whether a populated shadow may exist
    // before any reader or writer opens the archive.
    let admission_error = preflight_fts_shadow_before_lexical_readers(db_path)
        .err()
        .map(|err| format!("{err:#}"));

    let storage =
        FrankenStorage::open(db_path).context("opening the archive for its first mutation")?;
    let rss_after_open = rss_bytes();

    let corpus_messages = storage
        .fts_shadow_corpus_messages()
        .context("counting the indexable corpus")?;
    let shadow_registered_after_admission = shadow_registered(&storage)?;
    let shadow_rows_after_admission = shadow_row_count(&storage)?.unwrap_or(0);
    let shadow_tables_baseline = shadow_table_sizes(&storage)?;
    let canonical_messages_after_admission = canonical_message_count(&storage)?;
    let (not_viable_marker, repair_pending) = persisted_markers(&storage)?;
    let db_bytes_before = archive_bytes(db_path);

    let agent_id = storage
        .ensure_agent(&synthetic_agent())
        .context("ensuring the fixture agent")?;
    let workspace_id = storage
        .ensure_workspace(Path::new(WORKSPACE_PATH), None)
        .context("ensuring the fixture workspace")?;

    // The reported shape: one small delta against an archive that already
    // carries a populated shadow. The production insert path writes the
    // shadow rows inline, so this really is a contentless-FTS mutation.
    let delta_words = DELTA_BYTES / (DELTA_MESSAGES * (FILLER_WORD.len() + 1));
    let delta = synthetic_conversation("gh379-delta", 0, DELTA_MESSAGES, delta_words);
    let first_mutation_bytes: u64 = delta
        .messages
        .iter()
        .map(|message| message.content.len() as u64)
        .sum();

    let engine_rebuilds_before_first_mutation =
        storage.raw().as_async().fts5_reload_rebuild_count();
    let (io_read_before_first_mutation, io_write_before_first_mutation) = io_bytes();
    let started = Instant::now();
    storage
        .insert_conversation_tree(agent_id, Some(workspace_id), &delta)
        .context("performing the first mutation")?;
    let first_mutation_elapsed_ms =
        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let engine_rebuilds_after_first_mutation = storage.raw().as_async().fts5_reload_rebuild_count();
    let (io_read_after_first_mutation, io_write_after_first_mutation) = io_bytes();
    let rss_after_first_mutation = rss_bytes();
    let canonical_messages_after_first_mutation = canonical_message_count(&storage)?;
    let shadow_rows_after_first_mutation = shadow_row_count(&storage)?;
    let shadow_rowids_equal_canonical_after_first_mutation = match shadow_rowids(&storage)? {
        Some(ids) => ids == canonical_message_ids(&storage)?,
        None => false,
    };

    // The production repair path's verdict on the shadow as it stands now.
    let repair_after_first_mutation =
        RepairOutcome::from_result(storage.ensure_search_fallback_fts_consistency());

    // Later mutations. Their shadow rows are removed afterwards, to model a
    // process that died between the canonical write and the shadow write.
    let started = Instant::now();
    let mut later_message_ids: Vec<i64> = Vec::new();
    for index in 0..LATER_MUTATIONS {
        let later = synthetic_conversation(
            "gh379-later",
            index,
            LATER_MUTATION_MESSAGES,
            LATER_MUTATION_WORDS,
        );
        let outcome = storage
            .insert_conversation_tree(agent_id, Some(workspace_id), &later)
            .context("performing a later mutation")?;
        later_message_ids.extend(conversation_message_ids(&storage, outcome.conversation_id)?);
    }
    let later_mutations_elapsed_ms =
        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let rss_after_later_mutations = rss_bytes();
    let canonical_messages_after_later_mutations = canonical_message_count(&storage)?;
    if shadow_registered(&storage)? {
        remove_shadow_rows(&storage, &later_message_ids)
            .context("removing the later mutations' shadow rows")?;
    }
    let shadow_rows_after_later_mutations = shadow_row_count(&storage)?;

    // The interruption: the process stops here, with the shadow behind the
    // canonical rows and nothing published in between.
    storage
        .close_without_checkpoint()
        .context("closing the archive at the interruption")?;

    // The restart: a fresh open and the production repair path.
    let storage =
        FrankenStorage::open(db_path).context("reopening the archive after the interruption")?;
    let repair_after_restart =
        RepairOutcome::from_result(storage.ensure_search_fallback_fts_consistency());
    let rss_after_restart = rss_bytes();

    // A held reader: a read-only connection stays open across a write.
    let reader = FrankenStorage::open_readonly(db_path).context("opening the held reader")?;
    let held = synthetic_conversation(
        "gh379-held",
        0,
        LATER_MUTATION_MESSAGES,
        LATER_MUTATION_WORDS,
    );
    let started = Instant::now();
    let held_reader_mutation_ok = storage
        .insert_conversation_tree(agent_id, Some(workspace_id), &held)
        .is_ok();
    let held_reader_mutation_elapsed_ms =
        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let rss_after_held_reader = rss_bytes();
    reader
        .close_without_checkpoint()
        .context("closing the held reader")?;

    // The deferred rebuild: the repair path's verdict, then the explicit
    // rebuild the doctor and the maintenance paths use.
    let deferred_rebuild_repair =
        RepairOutcome::from_result(storage.ensure_search_fallback_fts_consistency());
    let explicit_rebuild_error = storage.rebuild_fts().err().map(|err| format!("{err:#}"));
    let shadow_present_after_explicit_rebuild = shadow_registered(&storage)?;

    let canonical_messages_final = canonical_message_count(&storage)?;
    let shadow_rows_final = shadow_row_count(&storage)?;
    let shadow_tables_final = shadow_table_sizes(&storage)?;
    let corpus_fingerprint = corpus_fingerprint(&storage)?;
    let canonical_ids = canonical_message_ids(&storage)?;
    let shadow_ids = shadow_rowids(&storage)?;
    let shadow_rowids_equal_canonical = match &shadow_ids {
        Some(ids) => *ids == canonical_ids,
        None => false,
    };
    let oracle_hit_ids = oracle_hit_ids(&storage)?;
    let shadow_hit_ids = shadow_hit_ids(&storage)?.unwrap_or_default();

    let page_count = storage
        .raw()
        .query_row_map("PRAGMA page_count", &[], |row| row.get_typed::<i64>(0))
        .unwrap_or(0);
    let freelist_count = storage
        .raw()
        .query_row_map("PRAGMA freelist_count", &[], |row| row.get_typed::<i64>(0))
        .unwrap_or(0);
    let quick_check = storage
        .raw()
        .query_row_map("PRAGMA quick_check(1)", &[], |row| {
            row.get_typed::<String>(0)
        })
        .unwrap_or_else(|err| format!("quick_check failed: {err}"));
    let db_bytes_after = archive_bytes(db_path);

    storage
        .close_without_checkpoint()
        .context("closing the archive after the scenario")?;

    Ok(Witness {
        db_path: db_path.display().to_string(),
        source_sha: option_env!("VERGEN_GIT_SHA")
            .unwrap_or("unknown")
            .to_string(),
        crate_version: env!("CARGO_PKG_VERSION").to_string(),
        engine_dependency: engine_dependency_identity(),
        test_binary: std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "unknown".to_string()),
        corpus_fingerprint,
        fixture_inserted_bytes,
        bound_messages: crate::storage::sqlite::fts_shadow_max_messages(),
        admission_error,
        corpus_messages,
        shadow_registered_after_admission,
        shadow_rows_after_admission,
        shadow_tables_baseline,
        canonical_messages_after_admission,
        not_viable_marker,
        repair_pending,
        rss_start_bytes: rss_start,
        rss_after_open_bytes: rss_after_open,
        rss_after_first_mutation_bytes: rss_after_first_mutation,
        peak_rss_bytes: peak_rss_bytes(),
        first_mutation_read_bytes: io_read_after_first_mutation
            .saturating_sub(io_read_before_first_mutation),
        first_mutation_write_bytes: io_write_after_first_mutation
            .saturating_sub(io_write_before_first_mutation),
        first_mutation_elapsed_ms,
        first_mutation_bytes,
        engine_rebuilds_before_first_mutation,
        engine_rebuilds_after_first_mutation,
        canonical_messages_after_first_mutation,
        shadow_rows_after_first_mutation,
        shadow_rowids_equal_canonical_after_first_mutation,
        repair_after_first_mutation,
        later_mutations_elapsed_ms,
        rss_after_later_mutations_bytes: rss_after_later_mutations,
        canonical_messages_after_later_mutations,
        shadow_rows_after_later_mutations,
        repair_after_restart,
        rss_after_restart_bytes: rss_after_restart,
        held_reader_mutation_ok,
        held_reader_mutation_elapsed_ms,
        rss_after_held_reader_bytes: rss_after_held_reader,
        deferred_rebuild_repair,
        explicit_rebuild_error,
        shadow_present_after_explicit_rebuild,
        canonical_messages_final,
        shadow_rows_final,
        shadow_tables_final,
        shadow_rowids_equal_canonical,
        oracle_hit_ids,
        shadow_hit_ids,
        db_bytes_before,
        db_bytes_after,
        page_count,
        freelist_count,
        quick_check,
    })
}

/// Copy the fixture archive (and its sidecars) to a scenario path.
fn copy_fixture(source: &Path, target: &Path) -> Result<()> {
    fs::copy(source, target).context("copying the fixture archive")?;
    for suffix in ["-wal", "-shm"] {
        let mut from = source.as_os_str().to_os_string();
        from.push(suffix);
        let mut to = target.as_os_str().to_os_string();
        to.push(suffix);
        let from = PathBuf::from(from);
        if from.exists() {
            fs::copy(&from, PathBuf::from(to)).context("copying an archive sidecar")?;
        }
    }
    Ok(())
}

/// Run one scenario in a child process and read back its measurement.
fn measure_in_child(
    db_path: &Path,
    bound_messages: &str,
    fixture_inserted_bytes: u64,
) -> Result<Witness> {
    let out_path = db_path.with_extension("witness.json");
    let current_exe = std::env::current_exe().context("locating the test binary")?;
    let output = Command::new(&current_exe)
        .arg(CHILD_TEST_FILTER)
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(CHILD_DB_ENV, db_path)
        .env(CHILD_OUT_ENV, &out_path)
        .env(CHILD_FIXTURE_BYTES_ENV, fixture_inserted_bytes.to_string())
        .env("CASS_FTS_SHADOW_MAX_MESSAGES", bound_messages)
        .output()
        .context("running the witness child process")?;
    if !output.status.success() {
        bail!(
            "the witness child process failed: status={:?}\nstdout={}\nstderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let raw = fs::read(&out_path).context("reading the witness measurement")?;
    serde_json::from_slice(&raw).context("parsing the witness measurement")
}

/// The child half of the measurement: it only runs when the parent asks for
/// it through the environment.
#[test]
fn gh379_child_measurement() {
    let Ok(db_path) = std::env::var(CHILD_DB_ENV) else {
        return;
    };
    let out_path =
        PathBuf::from(std::env::var(CHILD_OUT_ENV).expect("the child needs an output path"));
    let fixture_inserted_bytes = std::env::var(CHILD_FIXTURE_BYTES_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let witness = run_scenario(Path::new(&db_path), fixture_inserted_bytes)
        .expect("running the witness scenario");
    fs::write(
        &out_path,
        serde_json::to_vec_pretty(&witness).expect("serializing the witness"),
    )
    .expect("writing the witness measurement");
}

/// The witness: the first mutation of an archive with a populated contentless
/// shadow, measured with and without the landed shadow policy in force.
#[test]
fn gh379_first_mutation_is_bounded_by_the_landed_shadow_policy() {
    let temp = tempfile::TempDir::new().expect("creating the witness scratch directory");
    let (fixture_db, inserted_bytes) =
        build_fixture(temp.path()).expect("building the sanitized structural fixture");
    assert!(
        inserted_bytes > 0,
        "the fixture must carry synthetic content bytes"
    );

    // Scenario 1: the bound is disabled, so the shadow is retained and the
    // first mutation meets the engine's own allocation behavior.
    let engine_db = temp.path().join("engine.db");
    copy_fixture(&fixture_db, &engine_db).expect("copying the fixture for the engine scenario");
    let engine =
        measure_in_child(&engine_db, "0", inserted_bytes).expect("measuring the engine scenario");

    // Scenario 2: the bound is below the fixture's corpus, so the production
    // pre-write admission retires the shadow before the first mutation.
    let admission_db = temp.path().join("admission.db");
    copy_fixture(&fixture_db, &admission_db)
        .expect("copying the fixture for the admission scenario");
    let admission = measure_in_child(&admission_db, "1", inserted_bytes)
        .expect("measuring the admission scenario");

    eprintln!(
        "gh379 engine scenario: {}\nadmission scenario: {}",
        serde_json::to_string_pretty(&engine).expect("serializing the engine scenario"),
        serde_json::to_string_pretty(&admission).expect("serializing the admission scenario"),
    );

    // The fixture itself must be healthy and complete.
    assert_eq!(
        engine.quick_check.trim().to_ascii_lowercase(),
        "ok",
        "the fixture archive must be healthy: {engine:?}"
    );
    // Identity: both scenarios ran in the same binary, from the same source
    // tree, over the same corpus.
    assert_eq!(
        engine.source_sha, admission.source_sha,
        "both scenarios must run in the same build: {engine:?} {admission:?}"
    );
    assert_eq!(
        engine.test_binary, admission.test_binary,
        "both scenarios must run in the same binary: {engine:?} {admission:?}"
    );
    assert!(
        !engine.crate_version.is_empty(),
        "the witness must record the cass version it ran under: {engine:?}"
    );
    assert!(
        engine.engine_dependency.contains("fsqlite-core "),
        "the witness must record the engine dependency it links: {engine:?}"
    );
    assert_eq!(
        engine.engine_dependency, admission.engine_dependency,
        "both scenarios must run the same engine dependency: {engine:?} {admission:?}"
    );
    assert_eq!(
        engine.corpus_fingerprint, admission.corpus_fingerprint,
        "both scenarios must have measured the same corpus: {engine:?} {admission:?}"
    );
    assert_eq!(
        engine.fixture_inserted_bytes, inserted_bytes,
        "the engine receipt must carry the fixture's measured content bytes: {engine:?}"
    );
    assert_eq!(
        admission.fixture_inserted_bytes, inserted_bytes,
        "the admission receipt must carry the fixture's measured content bytes: {admission:?}"
    );
    let fixture_messages =
        i64::try_from(FIXTURE_CONVERSATIONS * FIXTURE_MESSAGES_PER_CONVERSATION).unwrap();
    assert_eq!(
        engine.canonical_messages_after_admission, fixture_messages,
        "the fixture corpus must be complete: {engine:?}"
    );
    assert_eq!(
        engine.corpus_messages,
        u64::try_from(fixture_messages).unwrap(),
        "the indexable corpus the admission judged must be the fixture's corpus: {engine:?}"
    );
    assert_eq!(
        engine.shadow_rows_after_admission, fixture_messages,
        "the fixture shadow must cover the whole corpus: {engine:?}"
    );
    let engine_data_baseline = engine
        .shadow_tables_baseline
        .iter()
        .find(|table| table.table == "fts_messages_data")
        .expect("the retained fixture shadow must have its %_data table");
    assert!(
        engine_data_baseline.rows > 0 && engine_data_baseline.payload_bytes > 0,
        "the retained fixture shadow must hold inverted-index blocks: {engine:?}"
    );
    assert!(
        engine
            .shadow_tables_baseline
            .iter()
            .any(|table| table.table == "fts_messages_idx" && table.rows > 0),
        "the retained fixture shadow must hold a term index: {engine:?}"
    );
    assert!(
        admission.shadow_tables_baseline.is_empty(),
        "a retired shadow must leave no derived table behind: {admission:?}"
    );
    assert_eq!(
        engine.bound_messages, None,
        "the engine scenario must run with the shadow bound disabled: {engine:?}"
    );
    assert!(
        engine.not_viable_marker.is_none() && engine.repair_pending.is_none(),
        "a retained shadow must not leave a retirement marker behind: {engine:?}"
    );
    assert!(
        engine.peak_rss_bytes >= engine.rss_after_first_mutation_bytes,
        "the peak resident set must cover the first mutation: {engine:?}"
    );
    assert!(
        engine.first_mutation_bytes >= DELTA_BYTES as u64 - (DELTA_BYTES as u64 / 16),
        "the first mutation must carry the reported shape's bytes: {engine:?}"
    );
    // The engine's own counter of full live-vtab content rebuilds (fsqlite's
    // GH#408 hook) is monotonic per connection: the first mutation is where
    // the shadow would be re-derived.
    assert!(
        engine.engine_rebuilds_after_first_mutation >= engine.engine_rebuilds_before_first_mutation,
        "the engine's rebuild counter must not go backwards: {engine:?}"
    );
    assert!(
        admission.engine_rebuilds_after_first_mutation
            >= admission.engine_rebuilds_before_first_mutation,
        "the engine's rebuild counter must not go backwards: {admission:?}"
    );
    assert!(
        engine.db_bytes_after >= engine.db_bytes_before,
        "the archive must not shrink across the mutations: {engine:?}"
    );
    assert!(
        engine.page_count > 0 && engine.freelist_count >= 0,
        "the archive's page accounting must be readable: {engine:?}"
    );

    // The engine scenario keeps the shadow and pays for it on the first
    // mutation; the admission scenario retires the shadow first and does not.
    assert!(
        engine.shadow_registered_after_admission,
        "with the bound disabled the shadow must survive the admission: {engine:?}"
    );
    assert!(
        engine.admission_error.is_none(),
        "the engine scenario's admission must not fail: {engine:?}"
    );
    assert!(
        !admission.shadow_registered_after_admission,
        "with the bound below the corpus the shadow must be retired before the first mutation: \
         {admission:?}"
    );
    assert!(
        admission.admission_error.is_none(),
        "the admission scenario's admission must not fail: {admission:?}"
    );
    let not_viable = admission
        .not_viable_marker
        .as_deref()
        .unwrap_or_default()
        .to_string();
    assert!(
        not_viable.contains("message bound"),
        "the retired shadow must record why it was retired: {not_viable:?}"
    );
    assert!(
        admission.repair_pending.is_some(),
        "the retired shadow must leave the repair-pending marker behind: {admission:?}"
    );
    assert_eq!(
        admission.bound_messages,
        Some(1),
        "the admission scenario must run under a one-message shadow bound: {admission:?}"
    );
    assert_eq!(
        admission.shadow_rows_after_admission, 0,
        "the retired shadow must hold no rows: {admission:?}"
    );
    assert_eq!(
        admission.canonical_messages_after_admission, fixture_messages,
        "the admission must not touch a canonical row: {admission:?}"
    );
    assert!(
        admission.corpus_messages == u64::try_from(fixture_messages).unwrap(),
        "the admission must have judged the whole corpus: {admission:?}"
    );
    assert!(
        admission.db_bytes_after >= admission.db_bytes_before,
        "the archive must not shrink across the admitted mutation: {admission:?}"
    );
    assert!(
        admission.page_count > 0 && admission.freelist_count >= 0,
        "the admitted archive's page accounting must be readable: {admission:?}"
    );
    assert!(
        admission.peak_rss_bytes >= admission.rss_after_first_mutation_bytes,
        "the peak resident set must cover the admitted mutation: {admission:?}"
    );

    // Bounded first mutation: the resident set stays inside a fixed envelope
    // (bytes do not depend on how busy the machine is) and the wall time is
    // bounded against the work actually done, never against a magic clock.
    // A fixed wall-clock ceiling is the defect the campaign's gate failures
    // exposed: it fails under load without the behaviour having changed.
    const RSS_ENVELOPE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
    /// The wall-clock budget the first mutation may spend per canonical
    /// message it carries. The mutation's cost is the shadow's, so the budget
    /// scales with the corpus the mutation has to carry, and a loaded machine
    /// slows both the work and the budget's subject in the same proportion.
    const MS_PER_CORPUS_MESSAGE: u64 = 250;
    /// The slack two processes' wall clocks may differ by.
    const ELAPSED_COMPARISON_SLACK_MS: u64 = 30_000;

    let corpus_budget_ms = u64::try_from(engine.canonical_messages_after_first_mutation)
        .unwrap_or(u64::MAX)
        .saturating_mul(MS_PER_CORPUS_MESSAGE);
    assert!(
        engine.total_rss_delta() < RSS_ENVELOPE_BYTES,
        "the first mutation must stay inside the resident-set envelope: {engine:?}"
    );
    assert!(
        admission.total_rss_delta() < RSS_ENVELOPE_BYTES,
        "the admitted first mutation must stay inside the resident-set envelope: {admission:?}"
    );
    // I/O is bounded by the archive the mutation meets, not by the rest of
    // the run: an engine that hydrates or rewrites the whole shadow pays
    // archive-scale reads and writes for a delta two orders of magnitude
    // smaller. The slack covers page-cache machinery and WAL bookkeeping.
    let io_envelope = engine
        .db_bytes_before
        .saturating_mul(8)
        .saturating_add(64 * 1024 * 1024);
    for witness in [&engine, &admission] {
        assert!(
            witness.first_mutation_read_bytes < io_envelope,
            "the first mutation must stay inside the read-I/O envelope \
             ({io_envelope} bytes): {witness:?}"
        );
        assert!(
            witness.first_mutation_write_bytes < io_envelope,
            "the first mutation must stay inside the write-I/O envelope \
             ({io_envelope} bytes): {witness:?}"
        );
    }
    assert!(
        engine.first_mutation_elapsed_ms < corpus_budget_ms,
        "the first mutation must finish inside the per-message time budget \
         ({corpus_budget_ms} ms for {} messages): {engine:?}",
        engine.canonical_messages_after_first_mutation
    );
    assert!(
        admission.first_mutation_elapsed_ms < corpus_budget_ms,
        "the admitted first mutation must finish inside the per-message time budget \
         ({corpus_budget_ms} ms): {admission:?}"
    );
    assert!(
        engine.later_mutations_elapsed_ms < corpus_budget_ms,
        "the later mutations must finish inside the per-message time budget: {engine:?}"
    );
    assert!(
        admission.later_mutations_elapsed_ms < corpus_budget_ms,
        "the admitted later mutations must finish inside the per-message time budget: \
         {admission:?}"
    );
    assert!(
        engine.held_reader_mutation_elapsed_ms < corpus_budget_ms,
        "the mutation under a held reader must finish inside the per-message time budget: \
         {engine:?}"
    );
    assert!(
        admission.held_reader_mutation_elapsed_ms < corpus_budget_ms,
        "the admitted mutation under a held reader must finish inside the per-message time \
         budget: {admission:?}"
    );

    // The policy must not make the first mutation *more* expensive. The two
    // numbers come from two different processes, so the comparison carries a
    // slack rather than demanding an exact ordering.
    const RSS_COMPARISON_SLACK_BYTES: u64 = 64 * 1024 * 1024;
    assert!(
        admission.first_mutation_rss_delta()
            <= engine.first_mutation_rss_delta() + RSS_COMPARISON_SLACK_BYTES,
        "the admission must not raise the first mutation's resident-set cost: \
         engine={} admission={}",
        engine.first_mutation_rss_delta(),
        admission.first_mutation_rss_delta()
    );
    assert!(
        admission.first_mutation_elapsed_ms
            <= engine.first_mutation_elapsed_ms + ELAPSED_COMPARISON_SLACK_MS,
        "the admission must not make the first mutation slower: engine={} ms admission={} ms",
        engine.first_mutation_elapsed_ms,
        admission.first_mutation_elapsed_ms
    );
    assert!(
        admission.later_mutations_elapsed_ms
            <= engine.later_mutations_elapsed_ms + ELAPSED_COMPARISON_SLACK_MS,
        "the admission must not make the later mutations slower: engine={} ms admission={} ms",
        engine.later_mutations_elapsed_ms,
        admission.later_mutations_elapsed_ms
    );

    // Exact parity where the shadow is complete: the production insert path
    // carried the shadow with the canonical rows, and the independent oracle
    // agrees with the shadow's own query.
    assert_eq!(
        engine.repair_after_first_mutation.kind, "already_healthy",
        "the production repair path must find the retained shadow complete: {engine:?}"
    );
    assert_eq!(
        engine.shadow_rows_after_first_mutation,
        Some(engine.canonical_messages_after_first_mutation),
        "the production first mutation must carry the shadow with it: {engine:?}"
    );
    assert!(
        engine.shadow_rowids_equal_canonical_after_first_mutation,
        "the retained shadow must hold exactly the canonical message ids: {engine:?}"
    );
    assert_eq!(
        engine.shadow_hit_ids, engine.oracle_hit_ids,
        "the shadow's answer must equal the independent canonical oracle: {engine:?}"
    );
    assert!(
        !engine.oracle_hit_ids.is_empty(),
        "the oracle must find the fixture's needle: {engine:?}"
    );
    assert_eq!(
        admission.shadow_rows_after_first_mutation, None,
        "a retired shadow leaves nothing to carry: {admission:?}"
    );
    assert_eq!(
        admission.canonical_messages_after_first_mutation,
        engine.canonical_messages_after_first_mutation,
        "the retired shadow must not cost a canonical row: {admission:?}"
    );

    // The interruption/restart: the shadow was left behind the canonical
    // rows, and the restart's repair path caught it up.
    assert_eq!(
        engine.shadow_rows_after_later_mutations,
        Some(
            engine.canonical_messages_after_later_mutations
                - i64::try_from(LATER_MUTATIONS * LATER_MUTATION_MESSAGES).unwrap()
        ),
        "the later mutations' shadow rows must be the ones missing: {engine:?}"
    );
    assert_eq!(
        engine.repair_after_restart.kind, "incremental_catch_up",
        "the restart's repair path must catch the shadow up: {engine:?}"
    );
    assert!(
        engine.shadow_rowids_equal_canonical,
        "after the restart the shadow must cover every canonical row again: {engine:?}"
    );

    // A held reader must not wedge the write.
    assert!(
        engine.held_reader_mutation_ok && admission.held_reader_mutation_ok,
        "a write under a held reader must complete: engine={engine:?} admission={admission:?}"
    );

    // Truthful partial state where the shadow was retired: the deferred
    // rebuild is refused with the recorded reason instead of recreating an
    // over-bound shadow, and the canonical rows stay complete.
    assert_eq!(
        admission.deferred_rebuild_repair.kind, "err",
        "the deferred rebuild must be refused for an over-bound corpus: {admission:?}"
    );
    let deferred_error = admission
        .deferred_rebuild_repair
        .error
        .as_deref()
        .unwrap_or_default()
        .to_string();
    assert!(
        error_message_indicates_fts_shadow_not_viable(&deferred_error),
        "the refusal must carry the not-viable detail: {deferred_error:?}"
    );
    assert!(
        deferred_error.contains("message bound"),
        "the refusal must name the bound: {deferred_error:?}"
    );
    // The explicit rebuild is the one shadow-writing path that does not
    // consult the corpus bound, so over a bound it recreates the shadow the
    // admission retired. The witness records that behavior instead of
    // hiding it: the next index run's admission retires it again.
    assert!(
        admission.shadow_present_after_explicit_rebuild,
        "the explicit rebuild path does not consult the corpus bound: {admission:?}"
    );
    assert_eq!(
        admission.explicit_rebuild_error, None,
        "the explicit rebuild must succeed over the bound: {admission:?}"
    );
    assert!(
        admission.canonical_messages_final >= fixture_messages,
        "the retired shadow must not cost a canonical row: {admission:?}"
    );
    assert_eq!(
        admission.oracle_hit_ids.len(),
        engine.oracle_hit_ids.len(),
        "the canonical oracle must not change when the shadow is retired: {admission:?}"
    );
    assert!(
        engine.rss_after_later_mutations_bytes > 0
            && engine.rss_after_restart_bytes > 0
            && engine.rss_after_held_reader_bytes > 0,
        "the later phases must be measured: {engine:?}"
    );
    assert!(
        !engine.db_path.is_empty() && !admission.db_path.is_empty(),
        "the witness must record the archive it measured: {engine:?} {admission:?}"
    );
    assert_eq!(
        engine.shadow_rows_final,
        Some(engine.canonical_messages_final),
        "the final shadow must cover every canonical message: {engine:?}"
    );
    assert!(
        engine
            .shadow_tables_final
            .iter()
            .any(|table| table.table == "fts_messages_data"
                && table.rows > 0
                && table.payload_bytes > 0),
        "the rebuilt shadow must hold inverted-index blocks: {engine:?}"
    );
    assert!(
        admission
            .shadow_tables_final
            .iter()
            .any(|table| table.table == "fts_messages_data" && table.rows > 0),
        "the explicit rebuild must have recreated the derived shadow tables: {admission:?}"
    );
}
