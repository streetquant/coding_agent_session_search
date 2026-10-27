//! GH #402 / bead `coding_agent_session_search-2l1b0.4`: read-only upgrade
//! admission over historical archive shapes.
//!
//! A 0.9-era `cass status` / `cass view` must admit a 0.6.24-era archive
//! without mutating it. The engine's strict read-only schema load refuses the
//! pre-#434 `fts_messages_config` catalog (a primary key whose implicit
//! autoindex has no `sqlite_master` row), even though the canonical rows are
//! readable. Admission must use a read-only lane, report the pending derived
//! repair on `status`, and leave repair to explicit `doctor`/`index`.
//!
//! This matrix builds each historical shape from the same live archive, then
//! drives two byte-identical copies through the read surfaces and proves:
//!
//! * canonical rows and source bytes are readable;
//! * no DB/WAL/SHM/source byte changes across a read;
//! * `status` names the exact pending repair (and `health` stays usable);
//! * `doctor --rebuild-canonical-fts` plans from the refusal, and only `--yes`
//!   mutates the derived shadow;
//! * a broken derived asset (duplicate rows, missing/stale FTS) is reported,
//!   never silently repaired by a read.

use assert_cmd::Command;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};
use tempfile::TempDir;

mod util;

const SENTINEL: &str = "gh402upgradeadmissionneedle";
const GH503_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/gh503/legacy_fts_config_shadow.db"
);
/// The exact 0.6.24 catalog declaration for the derived FTS config table.
const LEGACY_CONFIG_SQL: &str = "CREATE TABLE fts_messages_config (k PRIMARY KEY, v)";
/// A second pre-#434 virtual-table declaration, used for the duplicate-row
/// catalog shape already covered by `e2e_schema_repair_required`.
const DUPLICATE_FTS_SQL: &str = "CREATE VIRTUAL TABLE fts_messages USING fts5(content, title, agent, workspace, source_path, created_at UNINDEXED, message_id UNINDEXED, tokenize='porter')";

fn cass_cmd(home: &Path, data_dir: &Path, db: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cass"));
    cmd.current_dir(home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        // The 37-connector default scan includes these two roots even under a
        // temporary HOME; without empty roots it discovers unrelated archives
        // and a "tiny" base index never settles.
        .env("CASS_CLOUDMCP_DATA_ROOT", home.join("cloudmcp-empty"))
        .env("CASS_OPENWEBUI_DATA_ROOT", home.join("openwebui-empty"))
        .env("CASS_DATA_DIR", data_dir)
        .env("CASS_DB_PATH", db)
        .env_remove("CASS_OUTPUT_FORMAT")
        .env_remove("TOON_DEFAULT_FORMAT")
        .timeout(Duration::from_secs(120));
    cmd
}

fn run_step(step: &str, label: &str, command: &mut Command) -> Output {
    let started = Instant::now();
    let output = command.output().expect("run cass");
    println!(
        "{{\"event\":\"log\",\"msg\":\"gh402 upgrade admission step\",\"step\":{step:?},\
         \"command\":{label:?},\"exit_code\":{},\"duration_ms\":{}}}",
        output.status.code().unwrap_or(-1),
        started.elapsed().as_millis()
    );
    output
}

fn stdout_json(output: &Output, label: &str) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
        panic!(
            "{label}: stdout is not one JSON document ({err}); stdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn error_json(output: &Output) -> Option<Value> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .and_then(|value| value.get("error").cloned().or(Some(value)))
}

fn error_kind(output: &Output) -> Option<String> {
    error_json(output)
        .and_then(|value| value.get("kind").and_then(Value::as_str).map(str::to_owned))
}

fn error_text(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Every physical file that makes up the archive bundle. `-journal` is
/// included so an unexpected rollback journal is caught too.
fn bundle_snapshot(db: &Path) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let path = sidecar_path(db, suffix);
        match fs::read(&path) {
            Ok(bytes) => {
                files.insert(
                    suffix.to_string(),
                    blake3::hash(&bytes).to_hex().to_string(),
                );
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => panic!("reading {}: {err}", path.display()),
        }
    }
    files
}

fn hash_file(path: &Path) -> String {
    blake3::hash(&fs::read(path).expect("read source file"))
        .to_hex()
        .to_string()
}

/// A read may create or recycle the empty WAL/shared-memory sidecars that
/// SQLite itself owns, but it must not change one canonical byte, write a WAL
/// frame, or leave a rollback journal behind.
fn assert_unchanged(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    db: &Path,
    label: &str,
) {
    assert_eq!(
        before.get(""),
        after.get(""),
        "{label} changed the canonical database; before={before:?} after={after:?}"
    );
    match (before.get("-journal"), after.get("-journal")) {
        (None, None) => {}
        (Some(before), Some(after)) if before == after => {}
        _ => panic!("{label} left a rollback journal; before={before:?} after={after:?}"),
    }
    match (before.get("-wal"), after.get("-wal")) {
        (None, None) => {}
        (Some(before), Some(after)) if before == after => {}
        _ => {
            // The engine may create a header-only WAL or rewrite its salts.
            // Anything larger than the 32-byte header carries a frame.
            let size = fs::metadata(sidecar_path(db, "-wal"))
                .map(|meta| meta.len())
                .unwrap_or(0);
            assert!(
                size <= 32,
                "{label} wrote a {size}-byte WAL; before={before:?} after={after:?}"
            );
        }
    }
    // `-shm` is the engine's shared-memory index and may be created or
    // reinitialized by a read-only open; it is never canonical source.
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create destination");
    for entry in fs::read_dir(from).expect("read source directory") {
        let entry = entry.expect("directory entry");
        let file_type = entry.file_type().expect("file type");
        let destination = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &destination);
        } else if file_type.is_file() {
            fs::copy(entry.path(), &destination).expect("copy file");
        }
    }
}

fn clone_data_dir(base: &Path, destination: &Path) {
    copy_tree(base, destination);
}

/// The lexical checkpoint is identity-keyed by the absolute database path.
/// A byte-identical copy at another path must rewrite that observation, or
/// `search --no-maintenance` treats the index as belonging to another archive
/// and demands a rebuild it is not allowed to perform.
fn rewrite_lexical_db_path(data_dir: &Path, old_db: &Path, new_db: &Path) {
    let old = old_db.to_string_lossy();
    let new = new_db.to_string_lossy();
    let mut pending = vec![data_dir.join("index")];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            if text.contains(old.as_ref()) {
                fs::write(&path, text.replace(old.as_ref(), new.as_ref()))
                    .expect("rewrite lexical checkpoint database path");
            }
        }
    }
}

struct BaseFixture {
    data_dir: PathBuf,
    db: PathBuf,
    source_path: PathBuf,
    conversation_id: i64,
}

fn build_base(root: &Path) -> BaseFixture {
    let home = root.join("base-home");
    let codex_home = home.join(".codex");
    fs::create_dir_all(&codex_home).expect("create codex home");
    // franken_agent_detection only admits files whose name starts with
    // `rollout-`; anything else leaves a valid but empty base archive.
    util::seed_codex_session(&codex_home, "rollout-gh402-base.jsonl", SENTINEL, true);

    let data_dir = home.join("data");
    fs::create_dir_all(&data_dir).expect("create data dir");
    let db = data_dir.join("agent_search.db");
    let mut command = cass_cmd(&home, &data_dir, &db);
    command.args([
        "index",
        "--full",
        "--json",
        "--no-progress-events",
        "--data-dir",
    ]);
    command.arg(&data_dir);
    let output = run_step(
        "build_base",
        "cass index --full --json --no-progress-events --data-dir <base>",
        &mut command,
    );
    assert!(
        output.status.success(),
        "building the base archive failed: {}",
        error_text(&output)
    );
    assert!(db.exists(), "the base archive DB was not created");

    let conn = rusqlite::Connection::open(&db).expect("open base archive");
    // Leave the base fixture at a deterministic, fully-checkpointed boundary
    // before any copy is taken. A frame-carrying WAL copied mid-flight would
    // make the two disposable copies start from different logical states.
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .expect("checkpoint the base archive");
    let (conversation_id, source_path, conversation_count, message_count): (i64, String, i64, i64) =
        conn.query_row(
            "SELECT c.id, c.source_path,
                    (SELECT COUNT(*) FROM conversations),
                    (SELECT COUNT(*) FROM messages)
             FROM conversations c ORDER BY c.id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read the canonical row");
    drop(conn);
    assert_eq!(
        (conversation_count, message_count),
        (1, 2),
        "the isolated base home must index exactly the one seeded session"
    );
    let source_path = PathBuf::from(source_path);
    assert!(
        source_path.exists(),
        "the indexed source file must exist at {}",
        source_path.display()
    );

    BaseFixture {
        data_dir,
        db,
        source_path,
        conversation_id,
    }
}

fn open_stock(db: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(db).expect("open fixture with stock SQLite")
}

fn bump_schema_cookie(conn: &rusqlite::Connection) {
    let version: i64 = conn
        .query_row("PRAGMA schema_version", [], |row| row.get(0))
        .expect("read schema version");
    conn.pragma_update(None, "schema_version", version + 1)
        .expect("bump schema version");
}

/// Undo exactly the v23 additions (`MIGRATION_V23` only creates two tables
/// and the `cass_semantic_source_*` triggers), yielding a structurally v22
/// archive. The archive keeps its intact derived FTS.
fn downgrade_to_v22(db: &Path) {
    let conn = open_stock(db);
    conn.execute_batch("PRAGMA foreign_keys = OFF;")
        .expect("disable foreign keys");
    let triggers: Vec<String> = {
        let mut statement = conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'trigger' AND name LIKE 'cass_semantic_source_%'",
            )
            .expect("prepare trigger query");
        statement
            .query_map([], |row| row.get(0))
            .expect("query v23 triggers")
            .collect::<Result<_, _>>()
            .expect("collect v23 triggers")
    };
    for trigger in triggers {
        conn.execute_batch(&format!(
            "DROP TRIGGER \"{}\";",
            trigger.replace('"', "\"\"")
        ))
        .expect("drop v23 trigger");
    }
    conn.execute_batch(
        "DROP TABLE IF EXISTS semantic_source_identity_v1;
         DROP TABLE IF EXISTS semantic_source_certificates_v1;
         DELETE FROM _schema_migrations WHERE version = 23;
         UPDATE meta SET value = '22' WHERE key = 'schema_version';",
    )
    .expect("remove v23 objects");
}

/// Rewrite the derived FTS config catalog to the exact pre-#434 declaration
/// (a primary key with no `sqlite_autoindex_fts_messages_config_1` row).
fn break_fts_config_autoindex(db: &Path) {
    let conn = open_stock(db);
    conn.execute_batch("PRAGMA writable_schema = ON;")
        .expect("enable writable_schema");
    let updated = conn
        .execute(
            "UPDATE sqlite_master SET sql = ?1
             WHERE type = 'table' AND name = 'fts_messages_config'",
            [LEGACY_CONFIG_SQL],
        )
        .expect("rewrite fts_messages_config declaration");
    assert_eq!(
        updated, 1,
        "the base archive must carry fts_messages_config"
    );
    // The base FTS5 config is `... WITHOUT ROWID`, so it has no implicit
    // autoindex row. Dropping WITHOUT ROWID from the declaration makes the
    // catalog require `sqlite_autoindex_fts_messages_config_1`, which is
    // exactly the pre-#434 refusal.
    bump_schema_cookie(&conn);
    conn.execute_batch("PRAGMA writable_schema = OFF;")
        .expect("disable writable_schema");
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .expect("checkpoint the fixture");
}

fn duplicate_fts_catalog_rows(db: &Path) {
    let conn = open_stock(db);
    conn.execute_batch("PRAGMA writable_schema = ON;")
        .expect("enable writable_schema");
    conn.execute(
        "INSERT INTO sqlite_master(type, name, tbl_name, rootpage, sql)
         VALUES('table', 'fts_messages', 'fts_messages', 0, ?1)",
        [DUPLICATE_FTS_SQL],
    )
    .expect("inject the duplicate fts_messages catalog row");
    bump_schema_cookie(&conn);
    conn.execute_batch("PRAGMA writable_schema = OFF;")
        .expect("disable writable_schema");
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .expect("checkpoint the fixture");
}

/// Remove the entire derived FTS shadow from the catalog. Canonical
/// `conversations`/`messages` stay intact.
fn remove_derived_fts(db: &Path) {
    let conn = open_stock(db);
    conn.execute_batch("PRAGMA writable_schema = ON;")
        .expect("enable writable_schema");
    let removed = conn
        .execute(
            "DELETE FROM sqlite_master
             WHERE name LIKE 'fts_messages%' OR tbl_name LIKE 'fts_messages%'",
            [],
        )
        .expect("remove the derived FTS catalog rows");
    assert!(removed >= 1, "the base archive must carry fts_messages");
    bump_schema_cookie(&conn);
    conn.execute_batch("PRAGMA writable_schema = OFF;")
        .expect("disable writable_schema");
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .expect("checkpoint the fixture");
}

/// Canonical rows move ahead of the derived FTS/lexical assets without an
/// index run. The archive stays openable; the derived generation is stale.
fn add_canonical_message_ahead_of_derived(db: &Path, conversation_id: i64) {
    let conn = open_stock(db);
    // ubs:ignore[rust.domain.sql-concat] -- SQL advances the ordinal inside this immutable statement; the runtime conversation ID remains a bound parameter.
    const NEXT_MESSAGE_ORDINAL_SQL: &str =
        "SELECT COALESCE(MAX(idx), -1) + 1 FROM messages WHERE conversation_id = ?1";
    let next_idx: i64 = conn
        .query_row(NEXT_MESSAGE_ORDINAL_SQL, [conversation_id], |row| {
            row.get(0)
        })
        .expect("choose the next message ordinal");
    conn.execute(
        "INSERT INTO messages(conversation_id, idx, role, content, created_at)
         VALUES(?1, ?2, 'user', ?3, 1714000003000)",
        rusqlite::params![
            conversation_id,
            next_idx,
            format!("{SENTINEL} canonical-ahead-of-derived")
        ],
    )
    .expect("insert the canonical message");
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .expect("checkpoint the fixture");
}

/// Leave a WAL sidecar with no frames (at most the 32-byte header), the
/// shape a clean shutdown can leave behind.
fn park_wal_header(db: &Path) -> u64 {
    let conn = open_stock(db);
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .expect("truncate the WAL");
    drop(conn);
    let mut name = db.as_os_str().to_os_string();
    name.push("-wal");
    let wal = PathBuf::from(name);
    fs::metadata(&wal).map_or(0, |meta| meta.len())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    CurrentV23,
    HistoricalV22,
    Gh402LegacyShadowCatalog,
    Gh434DuplicateRows,
    MissingDerivedFts,
    StaleDerivedFts,
    WalHeaderReadOnlyPermissions,
}

impl Shape {
    fn name(self) -> &'static str {
        match self {
            Self::CurrentV23 => "current_v23",
            Self::HistoricalV22 => "historical_v22",
            Self::Gh402LegacyShadowCatalog => "gh402_legacy_shadow_catalog",
            Self::Gh434DuplicateRows => "gh434_duplicate_fts_rows",
            Self::MissingDerivedFts => "missing_derived_fts",
            Self::StaleDerivedFts => "stale_derived_fts",
            Self::WalHeaderReadOnlyPermissions => "wal_header_readonly_permissions",
        }
    }

    fn mutate(self, db: &Path, conversation_id: i64) {
        match self {
            Self::CurrentV23 => {}
            Self::HistoricalV22 => downgrade_to_v22(db),
            Self::Gh402LegacyShadowCatalog => {
                downgrade_to_v22(db);
                break_fts_config_autoindex(db);
            }
            Self::Gh434DuplicateRows => duplicate_fts_catalog_rows(db),
            Self::MissingDerivedFts => remove_derived_fts(db),
            Self::StaleDerivedFts => add_canonical_message_ahead_of_derived(db, conversation_id),
            Self::WalHeaderReadOnlyPermissions => {
                let wal_len = park_wal_header(db);
                assert!(
                    wal_len <= 32,
                    "precondition: the parked WAL must have no frames (len {wal_len})"
                );
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SearchClass {
    Hits(usize),
    Empty,
    TypedRepair,
}

#[derive(Debug, Clone, PartialEq)]
struct Observed {
    status_exit: Option<i32>,
    status_opened: bool,
    status_repair: bool,
    status_conversations: i64,
    status_messages: i64,
    status_repair_kind: Option<String>,
    status_repair_command: Option<String>,
    status_repair_detail_mentions_autoindex: bool,
    status_open_error_has_fix: bool,
    status_open_error_has_fts_repair: bool,
    health_exit: Option<i32>,
    health_opened: bool,
    health_has_fix: bool,
    health_has_fts_repair: bool,
    search: SearchClass,
    search_typed: bool,
    search_hint: Option<String>,
    view_exit: Option<i32>,
    view_body: bool,
    view_typed: bool,
}

fn observe(
    fixture: &BaseFixture,
    root: &Path,
    data_dir: &Path,
    db: &Path,
    label: &str,
    archive_view: bool,
) -> Observed {
    let home = root.join(format!("{label}-home"));
    fs::create_dir_all(&home).expect("create observation home");

    let mut status = cass_cmd(&home, data_dir, db);
    status
        .args(["status", "--json", "--data-dir"])
        .arg(data_dir);
    let status_output = run_step(
        &format!("{label}_status"),
        "cass status --json",
        &mut status,
    );
    assert!(
        status_output.status.success(),
        "{label}: status must succeed; {}",
        error_text(&status_output)
    );
    let status_json = stdout_json(&status_output, label);
    let status_opened = status_json["database"]["opened"].as_bool().unwrap_or(false);
    let status_repair_json = status_json["database"]
        .get("fts_shadow_repair")
        .cloned()
        .unwrap_or(Value::Null);
    let status_repair = !status_repair_json.is_null();
    let status_repair_kind = status_repair_json["kind"].as_str().map(str::to_owned);
    let status_repair_command = status_repair_json["command"].as_str().map(str::to_owned);
    let status_repair_detail_mentions_autoindex = status_repair_json["detail"]
        .as_str()
        .is_some_and(|detail| detail.contains("missing implicit autoindex"));
    let status_open_error = status_json["database"]["open_error"]
        .as_str()
        .unwrap_or_default();
    let status_open_error_has_fix = status_open_error.contains("doctor --fix");
    let status_open_error_has_fts_repair =
        status_open_error.contains("doctor --rebuild-canonical-fts");

    let mut health = cass_cmd(&home, data_dir, db);
    health
        .args(["health", "--json", "--data-dir"])
        .arg(data_dir);
    let health_output = run_step(
        &format!("{label}_health"),
        "cass health --json",
        &mut health,
    );
    let health_json = stdout_json(&health_output, label);
    let health_opened = health_json["db"]["opened"]
        .as_bool()
        .or_else(|| health_json["state"]["database"]["opened"].as_bool())
        .unwrap_or(false);
    let health_errors = health_json
        .get("errors")
        .map(Value::to_string)
        .unwrap_or_default();
    let health_has_fix = health_errors.contains("doctor --fix");
    let health_has_fts_repair = health_errors.contains("doctor --rebuild-canonical-fts");

    let mut search = cass_cmd(&home, data_dir, db);
    search
        .args([
            "search",
            SENTINEL,
            "--json",
            "--no-maintenance",
            "--data-dir",
        ])
        .arg(data_dir);
    let search_output = run_step(
        &format!("{label}_search"),
        "cass search --json",
        &mut search,
    );
    let search_typed = matches!(
        error_kind(&search_output).as_deref(),
        Some("schema-repair-required") | Some("maintenance-required")
    );
    let search_hint = error_json(&search_output)
        .and_then(|error| error.get("hint").and_then(Value::as_str).map(str::to_owned));
    let search = if search_output.status.success() {
        let value = stdout_json(&search_output, label);
        let hits = value["hits"].as_array().map_or(0, Vec::len);
        if hits > 0 {
            SearchClass::Hits(hits)
        } else {
            SearchClass::Empty
        }
    } else if search_typed {
        SearchClass::TypedRepair
    } else {
        panic!(
            "{label}: search failed with an unclassified error: {}",
            error_text(&search_output)
        );
    };

    let mut view = cass_cmd(&home, data_dir, db);
    view.arg("view").arg(&fixture.source_path);
    if archive_view {
        view.arg("--conversation-id")
            .arg(fixture.conversation_id.to_string());
    }
    view.arg("--json");
    let view_output = run_step(&format!("{label}_view"), "cass view --json", &mut view);
    let view_typed = error_kind(&view_output).as_deref() == Some("schema-repair-required");
    let view_body = if view_output.status.success() {
        let value = stdout_json(&view_output, label);
        value["lines"].as_array().is_some_and(|lines| {
            lines
                .iter()
                .filter_map(|line| line["content"].as_str())
                .any(|content| content.contains(SENTINEL))
        })
    } else {
        false
    };

    let status_conversations = status_json["database"]["conversations"]
        .as_i64()
        .unwrap_or(0);
    let status_messages = status_json["database"]["messages"].as_i64().unwrap_or(0);

    Observed {
        status_exit: status_output.status.code(),
        status_opened,
        status_repair,
        status_conversations,
        status_messages,
        status_repair_kind,
        status_repair_command,
        status_repair_detail_mentions_autoindex,
        status_open_error_has_fix,
        status_open_error_has_fts_repair,
        health_exit: health_output.status.code(),
        health_opened,
        health_has_fix,
        health_has_fts_repair,
        search,
        search_typed,
        search_hint,
        view_exit: view_output.status.code(),
        view_body,
        view_typed,
    }
}

fn assert_shape_truth(shape: Shape, observed: &Observed) {
    match shape {
        Shape::CurrentV23 => {
            assert!(observed.status_opened, "{shape:?}: status must open");
            assert!(!observed.status_repair, "{shape:?}: no repair is pending");
            assert_eq!(observed.status_conversations, 1);
            assert_eq!(observed.status_messages, 2);
            assert!(observed.health_opened, "{shape:?}: health must open");
            assert!(
                matches!(observed.search, SearchClass::Hits(_)),
                "{shape:?}: an unchanged copy must still serve the indexed hit, got {:?}",
                observed.search
            );
            assert!(
                observed.view_body,
                "{shape:?}: view must serve canonical body"
            );
            assert_eq!(observed.view_exit, Some(0));
        }
        Shape::HistoricalV22 => {
            assert!(observed.status_opened, "{shape:?}: status must open");
            assert!(!observed.status_repair, "{shape:?}: no repair is pending");
            assert_eq!(observed.status_conversations, 1);
            assert_eq!(observed.status_messages, 2);
            assert!(observed.health_opened, "{shape:?}: health must open");
            assert!(
                matches!(observed.search, SearchClass::Hits(_))
                    || observed.search == SearchClass::TypedRepair,
                "{shape:?}: the older schema must serve the lexical hit or a typed \
                 maintenance answer, got {:?} / {:?}",
                observed.search,
                observed.search_hint
            );
            assert!(
                observed.view_body,
                "{shape:?}: view must serve canonical body"
            );
            assert_eq!(observed.view_exit, Some(0));
        }
        Shape::Gh402LegacyShadowCatalog => {
            assert!(
                observed.status_opened,
                "the shadow catalog must be admitted"
            );
            assert!(
                observed.status_repair,
                "status must name the pending repair"
            );
            assert_eq!(
                observed.status_repair_kind.as_deref(),
                Some("schema-repair-required")
            );
            assert!(
                observed
                    .status_repair_command
                    .as_deref()
                    .is_some_and(|command| command.contains("doctor --rebuild-canonical-fts")),
                "{:?}",
                observed.status_repair_command
            );
            assert!(observed.status_repair_detail_mentions_autoindex);
            assert_eq!(observed.status_conversations, 1);
            assert_eq!(observed.status_messages, 2);
            assert!(observed.health_opened, "health must admit canonical rows");
            assert!(
                matches!(observed.search, SearchClass::Hits(n) if n >= 1)
                    || (observed.search == SearchClass::TypedRepair
                        && observed
                            .search_hint
                            .as_deref()
                            .is_some_and(|hint| !hint.trim().is_empty())),
                "search must serve the lexical hit or name the derived-shadow repair, got {:?} / {:?}",
                observed.search,
                observed.search_hint
            );
            assert!(observed.view_body, "view must serve canonical body");
            assert_eq!(observed.view_exit, Some(0));
        }
        Shape::Gh434DuplicateRows => {
            assert!(
                !observed.status_opened,
                "the duplicate catalog is refused by the strict open"
            );
            assert!(
                observed.status_open_error_has_fix,
                "status must name the explicit duplicate-row repair"
            );
            assert!(
                observed.health_has_fix,
                "health must name the explicit duplicate-row repair"
            );
            assert_eq!(
                observed.health_exit,
                Some(1),
                "health must report the unusable archive"
            );
            assert!(
                matches!(
                    observed.search,
                    SearchClass::Hits(_) | SearchClass::TypedRepair
                ),
                "duplicate rows: search must serve the lexical index or a typed repair, got {:?}",
                observed.search
            );
            if observed.search == SearchClass::TypedRepair {
                assert!(
                    observed
                        .search_hint
                        .as_deref()
                        .is_some_and(|hint| !hint.trim().is_empty()),
                    "a typed search failure must carry a bounded hint: {:?}",
                    observed.search_hint
                );
            }
            assert!(
                observed.view_body,
                "duplicate rows: the untouched source file must still serve via the direct fast path"
            );
            assert_eq!(observed.view_exit, Some(0));
        }
        Shape::MissingDerivedFts => {
            assert!(observed.status_opened, "canonical DB must still open");
            assert!(
                !observed.status_repair,
                "no shadow catalog repair is pending"
            );
            assert_eq!(observed.status_conversations, 1);
            assert_eq!(observed.status_messages, 2);
            assert!(
                matches!(observed.search, SearchClass::Hits(_))
                    || observed.search == SearchClass::TypedRepair,
                "the lexical index must serve or name a typed rebuild, got {:?} / {:?}",
                observed.search,
                observed.search_hint
            );
            assert!(observed.view_body, "view must serve canonical body");
        }
        Shape::StaleDerivedFts => {
            assert!(observed.status_opened, "canonical DB must still open");
            assert_eq!(observed.status_conversations, 1);
            assert_eq!(observed.status_messages, 3);
            assert!(
                matches!(observed.search, SearchClass::Hits(n) if n >= 1)
                    || observed.search == SearchClass::TypedRepair,
                "search serves the lexical generation or names the typed rebuild, got {:?} / {:?}",
                observed.search,
                observed.search_hint
            );
            assert!(observed.view_body, "view must serve the canonical body");
        }
        Shape::WalHeaderReadOnlyPermissions => {
            assert!(observed.status_opened, "a parked WAL must not block status");
            assert!(!observed.status_repair);
            assert!(observed.health_opened);
            assert!(
                matches!(observed.search, SearchClass::Hits(_)),
                "search must work with a parked WAL, got {:?}",
                observed.search
            );
            assert!(observed.view_body, "view must work with a parked WAL");
        }
    }
}

fn sidecar_path(db: &Path, suffix: &str) -> PathBuf {
    let name = db
        .file_name()
        .and_then(|name| name.to_str())
        .expect("utf-8 DB filename");
    db.with_file_name(format!("{name}{suffix}"))
}

fn make_readonly(fixture: &BaseFixture, db: &Path, data_dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for path in [db, fixture.source_path.as_path()] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o444)).expect("chmod file 0444");
    }
    for sidecar in ["-wal", "-shm"] {
        let path = sidecar_path(db, sidecar);
        if path.exists() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o444))
                .expect("chmod sidecar 0444");
        }
    }
    fs::set_permissions(data_dir, fs::Permissions::from_mode(0o555)).expect("chmod data dir 0555");
}

fn restore_writable(fixture: &BaseFixture, db: &Path, data_dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(data_dir, fs::Permissions::from_mode(0o755)).expect("chmod data dir 0755");
    for path in [db, fixture.source_path.as_path()] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).expect("chmod file 0644");
    }
    for sidecar in ["-wal", "-shm"] {
        let path = sidecar_path(db, sidecar);
        if path.exists() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
                .expect("chmod sidecar 0644");
        }
    }
}

fn run_shape(
    shape: Shape,
    base: &BaseFixture,
    root: &Path,
    copy: &str,
    read_only_permissions: bool,
) -> (Observed, BTreeMap<String, String>, BTreeMap<String, String>) {
    let data_dir = root.join(format!("{}-{copy}", shape.name()));
    clone_data_dir(&base.data_dir, &data_dir);
    let db = data_dir.join("agent_search.db");
    rewrite_lexical_db_path(&data_dir, &base.db, &db);
    shape.mutate(&db, base.conversation_id);
    if read_only_permissions {
        make_readonly(base, &db, &data_dir);
    }
    let before = bundle_snapshot(&db);
    let source_before = hash_file(&base.source_path);
    let observed = observe(
        base,
        root,
        &data_dir,
        &db,
        &format!("{}_{copy}", shape.name()),
        !matches!(
            shape,
            Shape::Gh434DuplicateRows | Shape::WalHeaderReadOnlyPermissions
        ),
    );
    let after = bundle_snapshot(&db);
    assert_unchanged(&before, &after, &db, &format!("{shape:?} {copy}"));
    assert_eq!(
        source_before,
        hash_file(&base.source_path),
        "{shape:?} {copy} mutated the source file"
    );
    if read_only_permissions {
        restore_writable(base, &db, &data_dir);
    }
    (observed, before, after)
}

#[test]
fn historical_archive_shapes_are_admitted_without_source_mutation() {
    let temp = TempDir::new().expect("tempdir");
    let base = build_base(temp.path());

    let shapes = [
        Shape::CurrentV23,
        Shape::HistoricalV22,
        Shape::Gh402LegacyShadowCatalog,
        Shape::Gh434DuplicateRows,
        Shape::MissingDerivedFts,
        Shape::StaleDerivedFts,
        Shape::WalHeaderReadOnlyPermissions,
    ];

    for shape in shapes {
        let (a, before_a, after_a) = run_shape(
            shape,
            &base,
            temp.path(),
            "a",
            shape == Shape::WalHeaderReadOnlyPermissions,
        );
        let (b, before_b, after_b) = run_shape(
            shape,
            &base,
            temp.path(),
            "b",
            shape == Shape::WalHeaderReadOnlyPermissions,
        );
        // The canonical DB and source are byte-identical across the two
        // copies. WAL/SHM headers may carry engine-generated salts, so only
        // their presence is compared.
        assert_eq!(
            before_a.get(""),
            before_b.get(""),
            "{shape:?}: the two disposable copies must start with identical canonical DB bytes"
        );
        assert_eq!(
            before_a.keys().collect::<Vec<_>>(),
            before_b.keys().collect::<Vec<_>>(),
            "{shape:?}: the two copies must start with the same sidecar set"
        );
        assert_eq!(
            after_a.get(""),
            after_b.get(""),
            "{shape:?}: two identical copies must end with identical canonical DB bytes"
        );
        assert_eq!(
            after_a.keys().collect::<Vec<_>>(),
            after_b.keys().collect::<Vec<_>>(),
            "{shape:?}: two identical copies must end with the same sidecar set"
        );
        assert_eq!(
            a, b,
            "{shape:?}: identical copies must observe identical admission outcomes"
        );
        assert_shape_truth(shape, &a);
    }

    assert_foreign_lexical_namespace_is_a_typed_refusal(&base, temp.path());
}

/// A lexical checkpoint records the absolute database path it indexed. Copying
/// the archive without rebinding that identity must not silently serve another
/// namespace's hits: `search --no-maintenance` reports the typed rebuild and
/// leaves both the DB and the checkpoint untouched.
fn assert_foreign_lexical_namespace_is_a_typed_refusal(base: &BaseFixture, root: &Path) {
    let data_dir = root.join("foreign-lexical-namespace");
    clone_data_dir(&base.data_dir, &data_dir);
    let db = data_dir.join("agent_search.db");
    let before = bundle_snapshot(&db);
    let source_before = hash_file(&base.source_path);
    let home = root.join("foreign-lexical-namespace-home");
    fs::create_dir_all(&home).expect("create foreign namespace home");

    let mut search = cass_cmd(&home, &data_dir, &db);
    search
        .args([
            "search",
            SENTINEL,
            "--json",
            "--no-maintenance",
            "--data-dir",
        ])
        .arg(&data_dir);
    let output = run_step(
        "foreign_lexical_namespace_search",
        "cass search --json (foreign checkpoint identity)",
        &mut search,
    );
    assert!(
        !output.status.success(),
        "a foreign lexical namespace must be refused, got {}",
        error_text(&output)
    );
    assert!(
        matches!(
            error_kind(&output).as_deref(),
            Some("maintenance-required") | Some("schema-repair-required")
        ),
        "the foreign namespace must be a typed refusal: {}",
        error_text(&output)
    );
    let error = error_json(&output);
    let hint = error
        .as_ref()
        .and_then(|error| error.get("hint").and_then(Value::as_str));
    assert!(
        hint.is_some_and(|hint| !hint.trim().is_empty()),
        "the foreign namespace refusal must carry a bounded hint: {}",
        error_text(&output)
    );
    assert_unchanged(&before, &bundle_snapshot(&db), &db, "foreign namespace");
    assert_eq!(
        source_before,
        hash_file(&base.source_path),
        "the foreign-namespace probe mutated the source file"
    );
}

fn copy_fixture_into_data_dir(fixture: &Path, root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let home = root.join("gh503-home");
    let data_dir = home.join("data");
    fs::create_dir_all(&data_dir).expect("create GH503 data dir");
    let db = data_dir.join("agent_search.db");
    fs::copy(fixture, &db).expect("copy the checked-in GH503 fixture");
    (home, data_dir, db)
}

#[test]
fn checked_in_gh503_catalog_is_read_only_admitted_and_repairs_only_on_request() {
    let temp = TempDir::new().expect("tempdir");
    let (home, data_dir, db) = copy_fixture_into_data_dir(Path::new(GH503_FIXTURE), temp.path());

    let source_path = PathBuf::from(
        "/data/tmp/claude-1000/-data-projects-coding-agent-session-search/34433b5e-a87b-40be-a33f-ea08502061b3/scratchpad/gh503-tiny-home/.claude/projects/-work-tiny/tiny-session.jsonl",
    );
    let source_existed = source_path.exists();
    let source_hash_before = source_existed.then(|| hash_file(&source_path));

    let before_bundle = bundle_snapshot(&db);

    let mut status = cass_cmd(&home, &data_dir, &db);
    status
        .args(["status", "--json", "--data-dir"])
        .arg(&data_dir);
    let status_output = run_step("gh503_status", "cass status --json", &mut status);
    assert!(
        status_output.status.success(),
        "{}",
        error_text(&status_output)
    );
    let status_json = stdout_json(&status_output, "gh503_status");
    assert_eq!(status_json["database"]["opened"], true);
    assert_eq!(status_json["database"]["conversations"], 1);
    assert_eq!(status_json["database"]["messages"], 2);
    assert_eq!(
        status_json["database"]["fts_shadow_repair"]["kind"],
        "schema-repair-required"
    );
    assert!(
        status_json["database"]["fts_shadow_repair"]["command"]
            .as_str()
            .unwrap_or_default()
            .contains("doctor --rebuild-canonical-fts")
    );
    assert_unchanged(&before_bundle, &bundle_snapshot(&db), &db, "gh503 status");

    let mut health = cass_cmd(&home, &data_dir, &db);
    health
        .args(["health", "--json", "--data-dir"])
        .arg(&data_dir);
    let health_output = run_step("gh503_health", "cass health --json", &mut health);
    let health_json = stdout_json(&health_output, "gh503_health");
    assert_eq!(health_json["db"]["opened"], true);
    assert_unchanged(&before_bundle, &bundle_snapshot(&db), &db, "gh503 health");

    let mut view = cass_cmd(&home, &data_dir, &db);
    view.arg("view")
        .arg(&source_path)
        .arg("--conversation-id")
        .arg("1")
        .arg("--json");
    let view_output = run_step("gh503_view", "cass view archive-only", &mut view);
    assert!(
        view_output.status.success(),
        "archive-only view must be admitted: {}",
        error_text(&view_output)
    );
    let view_json = stdout_json(&view_output, "gh503_view");
    if source_existed {
        assert_eq!(view_json["source_exists"], true);
    } else {
        assert_eq!(view_json["archive_only"], true);
    }
    let body = view_json["lines"]
        .as_array()
        .expect("view lines")
        .iter()
        .filter_map(|line| line["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(body.contains("legacyshadowneedle"), "{body}");
    assert!(
        body.contains("canonicalreplyneedle"),
        "the canonical reply must also be readable: {body}"
    );
    assert_unchanged(&before_bundle, &bundle_snapshot(&db), &db, "gh503 view");
    if let Some(before) = source_hash_before {
        assert_eq!(
            before,
            hash_file(&source_path),
            "gh503 view mutated the source file"
        );
    }

    let mut search = cass_cmd(&home, &data_dir, &db);
    search
        .args([
            "search",
            "legacyshadowneedle",
            "--json",
            "--no-maintenance",
            "--data-dir",
        ])
        .arg(&data_dir);
    let search_output = run_step("gh503_search", "cass search --json", &mut search);
    if search_output.status.success() {
        let search_json = stdout_json(&search_output, "gh503_search");
        let hits = search_json["hits"].as_array().map_or(0, Vec::len);
        assert!(
            hits <= 2,
            "a derived-search read cannot invent canonical hits: {search_json}"
        );
    } else {
        let kind = error_kind(&search_output);
        assert!(
            matches!(
                kind.as_deref(),
                Some("schema-repair-required") | Some("maintenance-required")
            ),
            "a missing lexical index or the legacy catalog must be a typed, non-mutating \
             answer; got {kind:?}: {}",
            error_text(&search_output)
        );
    }
    assert_unchanged(&before_bundle, &bundle_snapshot(&db), &db, "gh503 search");

    let mut dry_run = cass_cmd(&home, &data_dir, &db);
    dry_run
        .args([
            "doctor",
            "--rebuild-canonical-fts",
            "--dry-run",
            "--json",
            "--data-dir",
        ])
        .arg(&data_dir);
    let dry_run_output = run_step("gh503_doctor_dry_run", "cass doctor dry-run", &mut dry_run);
    assert!(
        dry_run_output.status.success(),
        "{}",
        error_text(&dry_run_output)
    );
    let dry_run_json = stdout_json(&dry_run_output, "gh503_doctor_dry_run");
    assert_eq!(dry_run_json["status"], "shadow_structure_corrupt");
    assert_eq!(
        dry_run_json["planned_action"],
        "drop_recreate_rebuild_from_canonical"
    );
    assert_unchanged(
        &before_bundle,
        &bundle_snapshot(&db),
        &db,
        "gh503 doctor dry-run",
    );

    // Explicit repair is the only mutation. Canonical rows must survive.
    let mut repair = cass_cmd(&home, &data_dir, &db);
    repair
        .args([
            "doctor",
            "--rebuild-canonical-fts",
            "--yes",
            "--json",
            "--data-dir",
        ])
        .arg(&data_dir);
    let repair_output = run_step(
        "gh503_doctor_repair",
        "cass doctor --rebuild-canonical-fts --yes",
        &mut repair,
    );
    assert!(
        repair_output.status.success(),
        "explicit repair failed: {}",
        error_text(&repair_output)
    );
    assert_ne!(
        before_bundle,
        bundle_snapshot(&db),
        "the explicit repair must change the derived archive bytes"
    );
    let conn_after = rusqlite::Connection::open(&db).expect("open repaired archive");
    let (conversations, messages): (i64, i64) = conn_after
        .query_row(
            "SELECT (SELECT COUNT(*) FROM conversations), (SELECT COUNT(*) FROM messages)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("canonical rows after repair");
    assert_eq!((conversations, messages), (1, 2));
    drop(conn_after);

    let mut status_after = cass_cmd(&home, &data_dir, &db);
    status_after
        .args(["status", "--json", "--data-dir"])
        .arg(&data_dir);
    let status_after_output = run_step(
        "gh503_status_after",
        "cass status after repair",
        &mut status_after,
    );
    let status_after_json = stdout_json(&status_after_output, "gh503_status_after");
    assert_eq!(status_after_json["database"]["opened"], true);
    assert!(
        status_after_json["database"]
            .get("fts_shadow_repair")
            .is_none_or(Value::is_null),
        "the pending repair must clear after --yes: {status_after_json}"
    );

    let mut search_after = cass_cmd(&home, &data_dir, &db);
    search_after
        .args([
            "search",
            "legacyshadowneedle",
            "--json",
            "--no-maintenance",
            "--data-dir",
        ])
        .arg(&data_dir);
    let search_after_output = run_step(
        "gh503_search_after",
        "cass search after repair",
        &mut search_after,
    );
    if !search_after_output.status.success() {
        // The fixture had no lexical metadata before repair; with
        // --no-maintenance search may still refuse to self-heal. The FTS
        // assertion below proves the explicit repair itself succeeded.
        assert_eq!(
            error_kind(&search_after_output).as_deref(),
            Some("maintenance-required"),
            "{}",
            error_text(&search_after_output)
        );
    } else {
        let search_after_json = stdout_json(&search_after_output, "gh503_search_after");
        let hits = search_after_json["hits"]
            .as_array()
            .expect("search hits after repair");
        assert!(
            hits.iter().any(|hit| {
                hit["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("legacyshadowneedle"))
            }),
            "a successful search after repair must serve the canonical hit: {search_after_json}"
        );
    }

    let conn_after = rusqlite::Connection::open(&db).expect("open repaired archive for FTS");
    let fts_hits: i64 = conn_after
        .query_row(
            "SELECT COUNT(*) FROM fts_messages WHERE fts_messages MATCH 'legacyshadowneedle'",
            [],
            |row| row.get(0),
        )
        .expect("query the repaired canonical FTS shadow");
    assert!(
        fts_hits >= 1,
        "the explicit repair must rebuild the derived FTS from canonical rows"
    );
}
