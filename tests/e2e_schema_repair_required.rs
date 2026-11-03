//! Bead `coding_agent_session_search-2l1b0.65`: the `schema-repair-required`
//! contract for an archive whose `sqlite_master` carries duplicate
//! `fts_messages` virtual-table rows.
//!
//! frankensqlite refuses to open such a catalog, and the only engine that
//! can repair it is the stock `sqlite3` CLI. Ordinary cass surfaces must
//! therefore never repair it behind the operator's back:
//!
//! * `cass health --json` and `cass status --json` leave the archive
//!   byte-identical and report that a repair is required;
//! * `cass doctor --fix` performs the repair;
//! * a host without the `sqlite3` tool still gets a truthful answer instead
//!   of a spawn failure.

use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;
use tempfile::TempDir;

mod util;
use util::e2e_log::{E2eLogContext, E2eLogger, PhaseTracker};

const LEGACY_V13_FTS_SQL: &str = "CREATE VIRTUAL TABLE fts_messages USING fts5(content, title, agent, workspace, source_path, created_at UNINDEXED, content='', tokenize='porter')";
const DUPLICATE_FTS_SQL: &str = "CREATE VIRTUAL TABLE fts_messages USING fts5(content, title, agent, workspace, source_path, created_at UNINDEXED, message_id UNINDEXED, tokenize='porter')";

fn cass_cmd(tracker: &PhaseTracker, temp_home: &Path) -> Command {
    let mut cmd = tracker.cass_assert_command();
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("CASS_IGNORE_SOURCES_CONFIG", "1");
    cmd.env("HOME", temp_home);
    cmd.env("XDG_DATA_HOME", temp_home.join(".local/share"));
    cmd.env("XDG_CONFIG_HOME", temp_home.join(".config"));
    cmd.env("XDG_CACHE_HOME", temp_home.join(".cache"));
    cmd.env("CODEX_HOME", temp_home.join(".codex"));
    cmd.env("CASS_CLOUDMCP_DATA_ROOT", temp_home.join("cloudmcp-empty"));
    cmd.env(
        "CASS_OPENWEBUI_DATA_ROOT",
        temp_home.join("openwebui-empty"),
    );
    cmd.env("CLAUDE_CONFIG_DIR", temp_home.join(".claude"));
    cmd.env("CASS_AUTO_REFRESH", "0");
    cmd.current_dir(temp_home);
    cmd
}

/// Build a canonical archive whose catalog carries the legacy duplicate
/// `fts_messages` rows.
fn build_archive_with_duplicate_fts_rows(data_dir: &Path) -> PathBuf {
    let db_path = data_dir.join("agent_search.db");
    {
        let storage = coding_agent_search::storage::sqlite::SqliteStorage::open(&db_path)
            .expect("create the canonical archive");
        drop(storage);
    }
    let conn = rusqlite::Connection::open(&db_path).expect("open the fixture connection");
    conn.execute_batch("PRAGMA writable_schema = ON;")
        .expect("enable writable_schema for the fixture");
    for sql in [LEGACY_V13_FTS_SQL, DUPLICATE_FTS_SQL] {
        conn.execute(
            "INSERT INTO sqlite_master(type, name, tbl_name, rootpage, sql)
             VALUES('table', 'fts_messages', 'fts_messages', 0, ?1)",
            [sql],
        )
        .expect("inject the duplicate fts_messages catalog row");
    }
    conn.execute_batch("PRAGMA writable_schema = OFF;")
        .expect("restore writable_schema for the fixture");
    drop(conn);
    db_path
}

fn fts_messages_catalog_row_count(db_path: &Path) -> i64 {
    let conn = rusqlite::Connection::open(db_path).expect("open the fixture connection");
    // The catalog carries two `fts_messages` rows, so a plain open fails the
    // schema load ("table fts_messages already exists"). `writable_schema`
    // relaxes that check for the read; it does not write, and the test proves
    // the archive is byte-identical afterwards.
    conn.execute_batch("PRAGMA writable_schema = ON;")
        .expect("enable writable_schema for the fixture read");
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name = 'fts_messages'",
        [],
        |row| row.get(0),
    )
    .expect("count the fts_messages catalog rows")
}

/// Run one cass command and emit its structured per-step log line: the
/// command, its exit code, the key JSON fields the step inspected, and the
/// elapsed time (`docs/reference/E2E_LOGGING_SCHEMA.md`).
fn run_step(step: &str, label: &str, command: &mut Command) -> std::process::Output {
    static LOGGER: OnceLock<E2eLogger> = OnceLock::new();
    let logger = LOGGER.get_or_init(|| E2eLogger::new("rust").expect("create E2E step log"));
    let started = Instant::now();
    let output = command.output().expect("run the cass command");
    let duration_ms = started.elapsed().as_millis() as u64;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut fields: Vec<(&str, Value)> = vec![
        (
            "json_parsed",
            Value::Bool(serde_json::from_str::<Value>(stdout.trim()).is_ok()),
        ),
        (
            "json_keys",
            match serde_json::from_str::<Value>(stdout.trim()) {
                Ok(Value::Object(map)) => {
                    Value::Array(map.keys().map(|key| Value::String(key.clone())).collect())
                }
                _ => Value::Array(Vec::new()),
            },
        ),
        (
            "reports_repair_required",
            Value::Bool(stdout.contains("doctor --fix")),
        ),
        ("stdout_bytes", Value::from(stdout.len())),
    ];
    let stderr = String::from_utf8_lossy(&output.stderr);
    let report = serde_json::from_str::<Value>(stdout.trim()).ok();
    let error = stderr
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Value>(line).ok());
    if let Some(Value::Object(map)) = report.as_ref().or(error.as_ref()) {
        for key in ["kind", "code", "status"] {
            if let Some(value) = map.get(key) {
                fields.push((key, value.clone()));
            }
        }
        if let Some(error) = map.get("error") {
            fields.push(("error", error.clone()));
        }
        if let Some(checks) = map.get("checks") {
            fields.push(("checks", checks.clone()));
        }
    }
    let event = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "event": "log",
        "run_id": logger.run_id(),
        "runner": "rust",
        "level": "INFO",
        "msg": "schema repair CLI step",
        "step": step,
        "command": label,
        "exit_code": output.status.code(),
        "duration_ms": duration_ms,
        "fields": fields
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect::<serde_json::Map<String, Value>>(),
    });
    logger
        .log(
            "INFO",
            &event.to_string(),
            Some(E2eLogContext {
                phase: Some(step.to_string()),
                command: Some(label.to_string()),
                test_name: None,
            }),
        )
        .expect("write structured E2E step log");
    logger.flush().expect("flush structured E2E step log");
    println!("{event}");
    output
}

/// 2l1b0.65: the read-only truth surfaces report the repair requirement and
/// leave the archive byte-identical.
#[test]
fn health_and_status_report_the_repair_and_leave_the_archive_untouched() {
    let tracker = PhaseTracker::new(
        "e2e_schema_repair_required",
        "health_and_status_report_the_repair_and_leave_the_archive_untouched",
    );
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let data_dir = home.join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db_path = build_archive_with_duplicate_fts_rows(&data_dir);
    let before = fs::read(&db_path).unwrap();

    for surface in ["health", "status"] {
        let mut command = cass_cmd(&tracker, &home);
        command
            .arg(surface)
            .arg("--json")
            .arg("--data-dir")
            .arg(&data_dir);
        let output = run_step(
            surface,
            &format!("cass {surface} --json --data-dir <data-dir>"),
            &mut command,
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("doctor --fix"),
            "{surface} must report the required repair: {stdout}"
        );
        // The truth probes must preserve the archive even when its derived
        // catalog needs repair. Ordinary canonical reads and strict content
        // hydration have separate refusal controls in e2e_search_index.
        assert_eq!(
            fs::read(&db_path).unwrap(),
            before,
            "{surface} must leave the archive byte-identical"
        );
        assert_eq!(
            fts_messages_catalog_row_count(&db_path),
            2,
            "{surface} must not remove any sqlite_master row"
        );
    }
    tracker.complete();
}

/// 2l1b0.65: `cass doctor --fix` is the one surface that repairs the
/// conflicting catalog, and it hands the archive back to frankensqlite.
#[test]
fn doctor_fix_repairs_the_conflicting_catalog() {
    if Command::new("sqlite3").arg("-version").output().is_err() {
        eprintln!("SKIPPED doctor_fix_repairs_the_conflicting_catalog: sqlite3 is not installed");
        return;
    }
    let tracker = PhaseTracker::new(
        "e2e_schema_repair_required",
        "doctor_fix_repairs_the_conflicting_catalog",
    );
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let data_dir = home.join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db_path = build_archive_with_duplicate_fts_rows(&data_dir);

    let mut command = cass_cmd(&tracker, &home);
    command
        .arg("doctor")
        .arg("--fix")
        .arg("--json")
        .arg("--data-dir")
        .arg(&data_dir);
    let output = run_step(
        "doctor_fix",
        "cass doctor --fix --json --data-dir <data-dir>",
        &mut command,
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: Value = serde_json::from_slice(&output.stdout).expect("parse doctor report");
    let repair = report["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .find(|check| check["name"] == "conflicting_fts_schema_rows")
        .expect("the doctor report must name the repair it applied");
    assert_eq!(repair["status"], "pass", "{stdout}");
    assert_eq!(repair["fix_applied"], true, "{stdout}");
    assert_eq!(
        fts_messages_catalog_row_count(&db_path),
        1,
        "the repair must keep exactly the canonical row"
    );

    let mut health_command = cass_cmd(&tracker, &home);
    health_command
        .arg("health")
        .arg("--json")
        .arg("--data-dir")
        .arg(&data_dir);
    let health = run_step(
        "health_after_repair",
        "cass health --json --data-dir <data-dir>",
        &mut health_command,
    );
    let stdout = String::from_utf8_lossy(&health.stdout);
    assert!(
        !stdout.contains("doctor --fix"),
        "the repaired archive must no longer report a required repair: {stdout}"
    );
    tracker.complete();
}

/// 2l1b0.65: a host without the `sqlite3` tool never sees a spawn failure
/// on a read path, and the doctor repair reports the missing tool as a
/// typed, actionable message.
#[test]
fn a_host_without_the_sqlite3_tool_still_gets_a_truthful_answer() {
    let tracker = PhaseTracker::new(
        "e2e_schema_repair_required",
        "a_host_without_the_sqlite3_tool_still_gets_a_truthful_answer",
    );
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let data_dir = home.join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db_path = build_archive_with_duplicate_fts_rows(&data_dir);
    let before = fs::read(&db_path).unwrap();

    let mut command = cass_cmd(&tracker, &home);
    command
        .env("PATH", temp.path().join("empty-bin"))
        .arg("status")
        .arg("--json")
        .arg("--data-dir")
        .arg(&data_dir);
    let output = run_step(
        "status_without_sqlite3",
        "cass status --json --data-dir <data-dir> (PATH without sqlite3)",
        &mut command,
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("doctor --fix"),
        "the read surface must still report the required repair: {stdout}"
    );
    assert!(
        !stdout.contains("No such file or directory"),
        "the read surface must not surface a spawn failure: {stdout}"
    );
    assert_eq!(
        fs::read(&db_path).unwrap(),
        before,
        "the read surface must leave the archive byte-identical"
    );

    let mut doctor = cass_cmd(&tracker, &home);
    doctor
        .env("PATH", temp.path().join("empty-bin"))
        .args(["doctor", "--fix", "--json", "--data-dir"])
        .arg(&data_dir);
    let output = run_step(
        "doctor_without_sqlite3",
        "cass doctor --fix --json --data-dir <data-dir> (PATH without sqlite3)",
        &mut doctor,
    );
    assert!(
        !output.status.success(),
        "doctor must not claim success when the required repair tool is absent"
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("parse doctor report");
    let repair = report["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .find(|check| check["name"] == "conflicting_fts_schema_rows")
        .expect("doctor must report the blocked catalog repair");
    assert_eq!(repair["status"], "fail", "{report}");
    assert_eq!(repair["fix_applied"], false, "{report}");
    let message = repair["message"].as_str().expect("repair failure message");
    assert!(
        message.contains("needs the sqlite3 command-line tool"),
        "{message}"
    );
    assert!(message.contains("install sqlite3 and retry"), "{message}");
    assert!(!message.contains("No such file or directory"), "{message}");
    assert_eq!(
        fs::read(&db_path).unwrap(),
        before,
        "a failed doctor repair must leave the archive byte-identical"
    );
    assert_eq!(fts_messages_catalog_row_count(&db_path), 2);
    tracker.complete();
}
