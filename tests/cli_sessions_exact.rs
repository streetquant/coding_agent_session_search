//! Exact native identity selection and bounded coverage metadata on the real CLI.

use anyhow::{Context, Result, ensure};
use assert_cmd::Command;
use coding_agent_search::franken_sync::compat::ConnectionExt;
use coding_agent_search::franken_sync::{Connection, SqliteValue, params};
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::PathBuf;
use std::process::Output;
use std::time::Duration;
use tempfile::TempDir;

const HISTORICAL_ROOT: &str = "/retired/exact-session-fixture/project";
const MAX_METADATA_BYTES: usize = 1_048_576;

struct Archive {
    root: TempDir,
    db: PathBuf,
}

impl Archive {
    fn new() -> Result<Self> {
        let archive = Self::empty()?;
        drop(FrankenStorage::open(&archive.db).context("initialize native archive")?);
        archive.seed_catalog()?;
        Ok(archive)
    }

    fn empty() -> Result<Self> {
        let root = TempDir::new().context("isolated archive directory")?;
        let db = root.path().join("agent_search.db");
        Ok(Self { root, db })
    }

    fn legacy() -> Result<Self> {
        let archive = Self::empty()?;
        let conn = archive.connect()?;
        // Legacy/corrupt archives may lack the current provenance uniqueness
        // constraint. They still must reject ambiguous identities before output.
        conn.execute_batch(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, slug TEXT, name TEXT,
                 kind TEXT, created_at INTEGER, updated_at INTEGER);
             CREATE INDEX agents_slug ON agents(slug);
             CREATE TABLE workspaces (id INTEGER PRIMARY KEY, path TEXT);
             CREATE TABLE sources (id TEXT PRIMARY KEY, kind TEXT,
                 created_at INTEGER, updated_at INTEGER);
             CREATE TABLE conversations (id INTEGER PRIMARY KEY, source_id TEXT,
                 agent_id INTEGER, external_id TEXT, workspace_id INTEGER,
                 title TEXT, source_path TEXT, origin_host TEXT,
                 started_at INTEGER, ended_at INTEGER);
             CREATE INDEX idx_conversations_provenance
                 ON conversations(source_id, agent_id, external_id);
             CREATE INDEX idx_conversations_source_path ON conversations(source_path);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, conversation_id INTEGER,
                 idx INTEGER, role TEXT, content TEXT, UNIQUE(conversation_id, idx));",
        )
        .context("legacy fixture schema")?;
        conn.close().context("close legacy schema writer")?;
        archive.seed_catalog()?;
        Ok(archive)
    }

    fn connect(&self) -> Result<Connection> {
        Connection::open(self.db.to_string_lossy().into_owned()).context("open fixture writer")
    }

    fn seed_catalog(&self) -> Result<()> {
        let conn = self.connect()?;
        conn.execute_batch(
            "INSERT INTO agents (id, slug, name, kind, created_at, updated_at)
                 VALUES (1, 'cloudmcp', 'CloudMCP', 'cli', 0, 0),
                        (2, 'codex', 'Codex', 'cli', 0, 0);
             INSERT OR IGNORE INTO sources (id, kind, created_at, updated_at)
                 VALUES ('local', 'local', 0, 0), ('remote', 'ssh', 0, 0);
             INSERT INTO workspaces (id, path)
                 VALUES (1, '/retired/exact-session-fixture/project'),
                        (2, '/retired/exact-session-fixture/project/child'),
                        (3, '/retired/exact-session-fixture');",
        )
        .context("seed catalog")?;
        conn.close().context("close catalog writer")?;
        Ok(())
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass")); // ubs:ignore -- Cargo-selected test binary; no shell or untrusted executable path.
        cmd.current_dir(self.root.path())
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("CASS_DATA_DIR", self.root.path())
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_AUTO_REFRESH", "0")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("NO_COLOR", "1")
            .env_remove("CASS_ROBOT_FORMAT")
            .arg("--db")
            .arg(&self.db)
            .args(["sessions", "--json"])
            .timeout(Duration::from_secs(60));
        cmd
    }

    fn exact(&self, source: &str, agent: &str, ids: &[&str]) -> Command {
        let mut cmd = self.command();
        cmd.args(["--exact-source-id", source, "--exact-agent", agent]);
        for id in ids {
            cmd.args(["--exact-external-id", id]);
        }
        cmd
    }
}

struct Session<'a> {
    id: i64,
    native_id: &'a str,
    source: &'a str,
    agent: i64,
    workspace: i64,
    path: String,
    started_at: i64,
}

impl<'a> Session<'a> {
    fn new(id: i64, native_id: &'a str) -> Self {
        Self {
            id,
            native_id,
            source: "local",
            agent: 1,
            workspace: 1,
            path: format!("/retired/exact-session-fixture/{id}/transcript.jsonl"),
            started_at: 1_700_000_000_000 + id,
        }
    }

    fn insert(&self, conn: &Connection) -> Result<()> {
        conn.execute_compat(
            "INSERT INTO conversations
             (id, source_id, agent_id, external_id, workspace_id, source_path, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                self.id,
                self.source,
                self.agent,
                self.native_id,
                self.workspace,
                self.path.as_str(),
                self.started_at
            ],
        )
        .context("insert fixture session")?;
        for (idx, role, content) in [(0_i64, "user", "question"), (1, "tool", "observed result")] {
            conn.execute_compat(
                "INSERT INTO messages (conversation_id, idx, role, content)
                 VALUES (?1, ?2, ?3, ?4)",
                params![self.id, idx, role, content],
            )
            .context("insert archived message")?;
        }
        Ok(())
    }
}

fn set_metadata(
    conn: &Connection,
    id: i64,
    text: Option<&str>,
    binary: Option<&[u8]>,
) -> Result<()> {
    let text = text.map_or(SqliteValue::Null, |value| SqliteValue::Text(value.into()));
    let binary = binary.map_or(SqliteValue::Null, |value| SqliteValue::Blob(value.into()));
    conn.execute_with_params(
        "UPDATE conversations SET metadata_json = ?1, metadata_bin = ?2 WHERE id = ?3",
        &[text, binary, SqliteValue::Integer(id)],
    )
    .context("set fixture metadata")?;
    Ok(())
}

fn sessions(cmd: &mut Command) -> Result<Vec<Value>> {
    let output = cmd.output().context("run session request")?;
    ensure!(
        output.status.success(),
        "session request failed ({}); stderr: {}; stdout: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let body: Value = serde_json::from_slice(&output.stdout).context("session response JSON")?;
    body.get("sessions")
        .and_then(Value::as_array)
        .cloned()
        .context("sessions array")
}

fn usage_error(cmd: &mut Command, contains: &str) -> Result<()> {
    let Output {
        status,
        stdout,
        stderr,
    } = cmd.output().context("run rejected exact request")?;
    ensure!(
        status.code() == Some(2),
        "expected usage exit 2, got {status}; stderr: {}",
        String::from_utf8_lossy(&stderr)
    );
    ensure!(stdout.is_empty(), "no partial session response may escape");
    let body: Value = serde_json::from_slice(&stderr).with_context(|| {
        format!(
            "structured usage error; stderr: {}",
            String::from_utf8_lossy(&stderr)
        )
    })?;
    ensure!(
        body.pointer("/error/kind").and_then(Value::as_str) == Some("usage"),
        "{body}"
    );
    ensure!(
        body.pointer("/error/retryable").and_then(Value::as_bool) == Some(false),
        "{body}"
    );
    ensure!(
        body.pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.contains(contains)),
        "expected {contains:?}: {body}"
    );
    Ok(())
}

#[test]
fn exact_selects_eighteen_native_ids_without_default_truncation_or_rewriting() -> Result<()> {
    let archive = Archive::new()?;
    let conn = archive.connect()?;
    conn.execute("BEGIN").context("begin fixture transaction")?;
    let ids: Vec<String> = (0..18)
        .map(|n| match n {
            0 => "goal:33c2d39b-5e0f-4488-b650-e4e4ca3?".into(),
            1 => "workspace: repeated  component\t/repeated?".into(),
            2 => "  native identity with surrounding spaces  ".into(),
            _ => format!("goal:native-{n}"),
        })
        .collect();
    for (idx, id) in ids.iter().enumerate() {
        Session::new(i64::try_from(idx).context("small index")? + 1, id).insert(&conn)?;
    }
    Session::new(100, "unrequested-newer-session").insert(&conn)?;
    conn.execute("COMMIT")
        .context("commit fixture transaction")?;
    conn.close().context("close fixture writer")?;

    let selected = sessions(&mut archive.exact(
        "local",
        "cloudmcp",
        &ids.iter().map(String::as_str).collect::<Vec<_>>(),
    ))?;
    ensure!(
        selected.len() == 18,
        "expected 18 selected sessions: {selected:?}"
    );
    let actual: BTreeSet<&str> = selected
        .iter()
        .map(|row| {
            row.get("external_id")
                .and_then(Value::as_str)
                .context("original native ID")
        })
        .collect::<Result<_>>()?;
    ensure!(
        actual == ids.iter().map(String::as_str).collect(),
        "selected identities changed: {actual:?}; expected {ids:?}"
    );
    for row in &selected {
        ensure!(
            row.get("source_id").and_then(Value::as_str) == Some("local"),
            "{row}"
        );
        ensure!(
            row.get("agent").and_then(Value::as_str) == Some("cloudmcp"),
            "{row}"
        );
        ensure!(
            row.get("workspace").and_then(Value::as_str) == Some(HISTORICAL_ROOT),
            "{row}"
        );
        ensure!(
            row.get("message_count").and_then(Value::as_u64) == Some(2),
            "{row}"
        );
        ensure!(
            row.get("human_turns").and_then(Value::as_u64) == Some(1),
            "{row}"
        );
        ensure!(
            row.get("source_path_session_count").and_then(Value::as_u64) == Some(1),
            "{row}"
        );
        ensure!(row.get("coverage_provenance").is_none(), "{row}");
    }
    let broad = sessions(archive.command().args(["--limit", "0"]))?;
    for row in selected {
        let original = broad
            .iter()
            .find(|entry| entry.get("conversation_id") == row.get("conversation_id"))
            .context("same session in broad listing")?;
        ensure!(
            row.get("content_hash") == original.get("content_hash"),
            "content hash changed: {row}; original: {original}"
        );
        ensure!(
            row.get("content_hash")
                .and_then(Value::as_str)
                .map(str::len)
                == Some(64),
            "expected a 64-character content hash: {row}"
        );
    }
    Ok(())
}

#[test]
fn exact_rejects_missing_and_mixed_missing_ids_without_partial_output() -> Result<()> {
    let archive = Archive::new()?;
    let conn = archive.connect()?;
    Session::new(1, "present").insert(&conn)?;
    conn.close().context("close fixture writer")?;
    let cases: [&[&str]; 2] = [&["absent"], &["present", "absent"]];
    for ids in cases {
        usage_error(
            &mut archive.exact("local", "cloudmcp", ids),
            "No exact native session",
        )?;
    }
    usage_error(
        &mut archive.exact("local", "missing-agent", &["present"]),
        "no stored agent",
    )?;
    Ok(())
}

#[test]
fn exact_applies_identity_predicates_before_hydrating_unrelated_archive_rows() -> Result<()> {
    let archive = Archive::new()?;
    let conn = archive.connect()?;
    Session::new(1, "selected").insert(&conn)?;
    Session::new(2, "unrelated").insert(&conn)?;
    // The unrelated row cannot be decoded as a session. Filtering a full
    // archive enumeration in Rust would fail before finding the requested ID.
    conn.execute("UPDATE conversations SET started_at = 'invalid-timestamp' WHERE id = 2")
        .context("unrelated legacy decoding defect")?;
    conn.close().context("close fixture writer")?;
    let rows = sessions(&mut archive.exact("local", "cloudmcp", &["selected"]))?;
    ensure!(rows.len() == 1, "expected one selected session: {rows:?}");
    let row = rows.first().context("selected session")?;
    ensure!(
        row.get("external_id").and_then(Value::as_str) == Some("selected"),
        "{row}"
    );
    let broad = archive
        .command()
        .output()
        .context("broad decoding control")?;
    ensure!(
        broad.status.code() == Some(9),
        "broad control must expose the decoding defect: {broad:?}"
    );
    Ok(())
}

#[test]
fn exact_requires_all_selectors_and_rejects_broad_filters() -> Result<()> {
    let archive = Archive::empty()?;
    for args in [
        vec!["--exact-source-id", "local"],
        vec!["--exact-agent", "cloudmcp"],
        vec!["--exact-external-id", "native"],
        vec!["--exact-workspace", HISTORICAL_ROOT],
        vec!["--exact-source-id", "local", "--exact-agent", "cloudmcp"],
    ] {
        let output = archive
            .command()
            .args(args)
            .output()
            .context("partial selector CLI")?;
        ensure!(
            output.status.code() == Some(2),
            "partial selectors must be rejected: {output:?}"
        );
        ensure!(
            output.stdout.is_empty(),
            "partial selector request emitted stdout: {output:?}"
        );
    }
    for args in [
        vec!["--workspace", HISTORICAL_ROOT],
        vec!["--current"],
        vec!["--since", "2024-01-01"],
        vec!["--limit", "1"],
        vec!["--limit", "0"],
    ] {
        let output = archive
            .exact("local", "cloudmcp", &["native"])
            .args(args)
            .output()
            .context("conflicting selector CLI")?;
        ensure!(
            output.status.code() == Some(2),
            "conflicting selectors must be rejected: {output:?}"
        );
        ensure!(
            output.stdout.is_empty(),
            "conflicting selector request emitted stdout: {output:?}"
        );
    }
    ensure!(
        !archive.db.exists(),
        "invalid selectors cannot create an archive"
    );
    Ok(())
}

#[test]
fn exact_rejects_duplicate_blank_and_oversized_selectors_before_opening_storage() -> Result<()> {
    let archive = Archive::empty()?;
    usage_error(
        &mut archive.exact("local", "cloudmcp", &["same", "same"]),
        "duplicate",
    )?;
    for (source, agent, id) in [
        (" ", "cloudmcp", "id"),
        ("local", "\t", "id"),
        ("local", "cloudmcp", " "),
    ] {
        usage_error(&mut archive.exact(source, agent, &[id]), "nonblank")?;
    }
    usage_error(
        archive
            .exact("local", "cloudmcp", &["id"])
            .args(["--exact-workspace", " "]),
        "nonblank",
    )?;
    let oversized = "x".repeat(16_385);
    usage_error(
        &mut archive.exact("local", "cloudmcp", &[&oversized]),
        "16 KiB",
    )?;
    let ids: Vec<String> = (0..257).map(|n| format!("native-{n}")).collect();
    usage_error(
        &mut archive.exact(
            "local",
            "cloudmcp",
            &ids.iter().map(String::as_str).collect::<Vec<_>>(),
        ),
        "between 1 and 256",
    )?;
    ensure!(
        !archive.db.exists(),
        "invalid selectors cannot create an archive"
    );
    Ok(())
}

#[test]
fn exact_accepts_the_256_identity_boundary() -> Result<()> {
    let archive = Archive::new()?;
    let conn = archive.connect()?;
    conn.execute("BEGIN").context("begin bounded fixture")?;
    let ids: Vec<String> = (0..256).map(|n| format!("native-{n}")).collect();
    for (idx, id) in ids.iter().enumerate() {
        Session::new(i64::try_from(idx).context("small index")? + 1, id).insert(&conn)?;
    }
    conn.execute("COMMIT").context("commit bounded fixture")?;
    conn.close().context("close fixture writer")?;
    let rows = sessions(&mut archive.exact(
        "local",
        "cloudmcp",
        &ids.iter().map(String::as_str).collect::<Vec<_>>(),
    ))?;
    ensure!(
        rows.len() == 256,
        "expected all 256 identities, got {}",
        rows.len()
    );
    Ok(())
}

#[test]
fn exact_isolates_source_agent_and_literal_historical_workspace() -> Result<()> {
    let archive = Archive::new()?;
    let conn = archive.connect()?;
    Session::new(1, "shared-native-id").insert(&conn)?;
    let mut remote = Session::new(2, "shared-native-id");
    remote.source = "remote";
    remote.insert(&conn)?;
    let mut other_agent = Session::new(3, "shared-native-id");
    other_agent.agent = 2;
    other_agent.insert(&conn)?;
    let mut child = Session::new(4, "child-session");
    child.workspace = 2;
    child.insert(&conn)?;
    conn.close().context("close fixture writer")?;

    for (source, agent, row_id) in [
        ("local", "cloudmcp", 1),
        ("remote", "cloudmcp", 2),
        ("local", "codex", 3),
    ] {
        let rows = sessions(
            archive
                .exact(source, agent, &["shared-native-id"])
                .args(["--exact-workspace", HISTORICAL_ROOT]),
        )?;
        ensure!(
            rows.len() == 1,
            "expected one session for {source}/{agent}: {rows:?}"
        );
        let row = rows.first().context("isolated session")?;
        ensure!(
            row.get("conversation_id").and_then(Value::as_i64) == Some(row_id),
            "{row}"
        );
        ensure!(
            row.get("source_id").and_then(Value::as_str) == Some(source),
            "{row}"
        );
        ensure!(
            row.get("agent").and_then(Value::as_str) == Some(agent),
            "{row}"
        );
    }
    usage_error(
        &mut archive.exact("missing-source", "cloudmcp", &["shared-native-id"]),
        "No exact native session",
    )?;
    for workspace in [
        "/retired/exact-session-fixture",
        "/retired/exact-session-fixture/project/child",
        "/retired/exact-session-fixture/./project",
    ] {
        usage_error(
            archive
                .exact("local", "cloudmcp", &["shared-native-id"])
                .args(["--exact-workspace", workspace]),
            "literal workspace",
        )?;
    }
    usage_error(
        archive
            .exact("local", "cloudmcp", &["shared-native-id", "child-session"])
            .args(["--exact-workspace", HISTORICAL_ROOT]),
        "literal workspace",
    )?;
    Ok(())
}

#[test]
fn exact_preserves_literal_source_agent_and_workspace_spelling() -> Result<()> {
    let archive = Archive::new()?;
    let conn = archive.connect()?;
    conn.execute_batch(
        "UPDATE sources SET id = ' source: repeated  component? ' WHERE id = 'remote';
         UPDATE agents SET slug = ' original  agent? ' WHERE id = 2;
         UPDATE workspaces SET path = '/retired//spelling/./root/' WHERE id = 1;",
    )
    .context("literal catalog spelling")?;
    let mut row = Session::new(1, "native: ID?");
    row.source = " source: repeated  component? ";
    row.agent = 2;
    row.insert(&conn)?;
    conn.close().context("close fixture writer")?;
    let rows = sessions(
        archive
            .exact(row.source, " original  agent? ", &[row.native_id])
            .args(["--exact-workspace", "/retired//spelling/./root/"]),
    )?;
    let selected = rows.first().context("literal-identity session")?;
    ensure!(
        selected.get("source_id").and_then(Value::as_str) == Some(row.source),
        "{selected}"
    );
    ensure!(
        selected.get("agent").and_then(Value::as_str) == Some(" original  agent? "),
        "{selected}"
    );
    ensure!(
        selected.get("workspace").and_then(Value::as_str) == Some("/retired//spelling/./root/"),
        "{selected}"
    );
    Ok(())
}

#[test]
fn exact_rejects_ambiguous_native_identity_even_with_a_matching_workspace() -> Result<()> {
    let archive = Archive::legacy()?;
    let conn = archive.connect()?;
    Session::new(1, "duplicate").insert(&conn)?;
    let mut duplicate = Session::new(2, "duplicate");
    duplicate.workspace = 2;
    duplicate.insert(&conn)?;
    Session::new(3, "unique").insert(&conn)?;
    conn.close().context("close fixture writer")?;
    usage_error(
        &mut archive.exact("local", "cloudmcp", &["duplicate"]),
        "Ambiguous exact native session",
    )?;
    usage_error(
        archive
            .exact("local", "cloudmcp", &["unique", "duplicate"])
            .args(["--exact-workspace", HISTORICAL_ROOT]),
        "Ambiguous exact native session",
    )?;
    Ok(())
}

#[test]
fn exact_rejects_ambiguous_agent_slugs() -> Result<()> {
    let archive = Archive::legacy()?;
    let conn = archive.connect()?;
    Session::new(1, "native").insert(&conn)?;
    conn.execute("UPDATE agents SET slug = 'cloudmcp' WHERE id = 2")
        .context("ambiguous legacy agent")?;
    conn.close().context("close fixture writer")?;
    usage_error(
        &mut archive.exact("local", "cloudmcp", &["native"]),
        "ambiguous stored agent",
    )?;
    Ok(())
}

#[test]
fn exact_reports_global_shared_path_counts_without_normalizing_path_keys() -> Result<()> {
    let archive = Archive::new()?;
    let conn = archive.connect()?;
    let mut native_id = String::with_capacity(8);
    for id in 1..=4 {
        native_id.clear();
        write!(&mut native_id, "native-{id}").context("format shared-path fixture identity")?;
        let mut row = Session::new(id, &native_id);
        row.path = "/retired/shared.db".into();
        row.workspace = if id == 2 { 2 } else { 1 };
        row.source = if id == 3 { "remote" } else { "local" };
        row.agent = if id == 4 { 2 } else { 1 };
        row.insert(&conn)?;
    }
    let mut spelled_differently = Session::new(5, "different-path-spelling");
    spelled_differently.path = "/retired//shared.db".into();
    spelled_differently.insert(&conn)?;
    conn.close().context("close fixture writer")?;
    let rows = sessions(
        archive
            .exact(
                "local",
                "cloudmcp",
                &["native-1", "different-path-spelling"],
            )
            .args(["--exact-workspace", HISTORICAL_ROOT]),
    )?;
    for row in rows {
        let expected = if row.get("external_id").and_then(Value::as_str) == Some("native-1") {
            4
        } else {
            1
        };
        ensure!(
            row.get("source_path_session_count").and_then(Value::as_u64) == Some(expected),
            "{row}"
        );
    }
    Ok(())
}

#[test]
fn broad_sessions_keep_limit_since_ancestor_matching_and_legacy_metadata_behavior() -> Result<()> {
    let archive = Archive::legacy()?;
    let conn = archive.connect()?;
    let mut native_id = String::with_capacity(9);
    for id in 1..=12 {
        native_id.clear();
        write!(&mut native_id, "native-{id}").context("format broad-session fixture identity")?;
        let mut row = Session::new(id, &native_id);
        row.workspace = if id % 2 == 0 { 2 } else { 1 };
        if id == 12 {
            row.started_at = 1_735_000_000_000;
        }
        row.insert(&conn)?;
    }
    conn.close().context("close fixture writer")?;
    ensure!(
        sessions(&mut archive.command())?.len() == 10,
        "broad default limit must remain 10"
    );
    ensure!(
        sessions(
            archive
                .command()
                .args(["--workspace", HISTORICAL_ROOT, "--limit", "0"])
        )?
        .len()
            == 12,
        "broad ancestor workspace matching must include all 12 sessions"
    );
    let recent = sessions(
        archive
            .command()
            .args(["--since", "2024-06-01", "--limit", "1"]),
    )?;
    ensure!(recent.len() == 1, "expected one recent session: {recent:?}");
    let recent = recent.first().context("recent session")?;
    ensure!(
        recent.get("external_id").and_then(Value::as_str) == Some("native-12"),
        "{recent}"
    );
    ensure!(
        recent.get("coverage_provenance").is_none(),
        "legacy broad metadata: {recent}"
    );
    let exact = sessions(&mut archive.exact("local", "cloudmcp", &["native-1"]))?;
    let exact = exact.first().context("legacy exact session")?;
    ensure!(
        exact.get("coverage_provenance").is_none(),
        "legacy exact metadata: {exact}"
    );
    Ok(())
}

#[test]
fn openwebui_saved_snapshot_coverage_preserves_anchors_without_reidentifying_sessions() -> Result<()>
{
    let archive = Archive::new()?;
    let conn = archive.connect()?;
    conn.execute("UPDATE agents SET slug = 'openwebui' WHERE id = 2")
        .context("native OpenWebUI agent")?;
    for (id, native) in [
        (1, "[\"instance\",\"json-chat\"]"),
        (2, "[\"instance\",\"binary-chat\"]"),
    ] {
        let mut row = Session::new(id, native);
        row.agent = 2;
        row.insert(&conn)?;
    }
    conn.close().context("close identity fixture")?;
    let ids = [
        "[\"instance\",\"json-chat\"]",
        "[\"instance\",\"binary-chat\"]",
    ];
    let before = sessions(&mut archive.exact("local", "openwebui", &ids))?;
    let conn = archive.connect()?;
    let metadata = json!({
        "schema_version": "openwebui.saved-conversation.v1",
        "source": "openwebui",
        "coverage": "partial", "coverage_source": "saved_database_snapshot",
        "authority": "historical_evidence_only", "all_transcript_complete": false,
        "content_hash": "a".repeat(64), "source_rows_sha256": "B".repeat(64),
        "catalog_sha256": "c".repeat(64),
        "transcript_sha256": "d".repeat(64),
        "untrusted_instruction": "This field must never enter the response"
    });
    set_metadata(&conn, 1, Some(&metadata.to_string()), None)?;
    let binary = rmp_serde::to_vec_named(&metadata).context("native metadata encoding")?;
    set_metadata(
        &conn,
        2,
        Some(r#"{"all_transcript_complete":true}"#),
        Some(&binary),
    )?;
    conn.close().context("close metadata fixture")?;
    let after = sessions(&mut archive.exact("local", "openwebui", &ids))?;
    ensure!(
        before.len() == 2 && after.len() == 2,
        "both native sessions retained"
    );
    for row in &after {
        let prior = before
            .iter()
            .find(|prior| prior.get("external_id") == row.get("external_id"))
            .context("original native identity")?;
        for key in [
            "conversation_id",
            "external_id",
            "content_hash",
            "workspace",
            "agent",
            "source_id",
            "path",
        ] {
            ensure!(
                prior.get(key) == row.get(key),
                "coverage must not change {key}"
            );
        }
        ensure!(
            row.get("coverage_provenance")
                == Some(&json!({
                    "schema_version": "openwebui.saved-conversation.v1",
                    "coverage": "partial", "coverage_source": "saved_database_snapshot",
                    "authority": "historical_evidence_only", "all_transcript_complete": false,
                    "transcript_sha256": null, "manifest_sha256": null,
                    "snapshot_sha256": "a".repeat(64), "source_rows_sha256": "B".repeat(64),
                    "catalog_sha256": "c".repeat(64)
                })),
            "native saved-snapshot coverage must survive exactly: {row}"
        );
    }
    Ok(())
}

#[test]
fn openwebui_saved_snapshot_rejects_incomplete_anchors_and_unsupported_claims() -> Result<()> {
    let archive = Archive::new()?;
    let conn = archive.connect()?;
    conn.execute("UPDATE agents SET slug = 'openwebui' WHERE id = 2")
        .context("native OpenWebUI agent")?;
    let valid = json!({
        "schema_version": "openwebui.saved-conversation.v1",
        "source": "openwebui",
        "coverage": "partial", "coverage_source": "saved_database_snapshot",
        "authority": "historical_evidence_only", "all_transcript_complete": false,
        "content_hash": "a".repeat(64), "source_rows_sha256": "b".repeat(64),
        "catalog_sha256": "c".repeat(64)
    });
    let mut ids = Vec::new();
    for (index, (key, value)) in [
        ("schema_version", json!("openwebui.saved-conversation.v2")),
        ("source", json!("cloudmcp")),
        ("source", Value::Null),
        ("coverage", json!("client_context_sync_complete")),
        ("coverage_source", json!("client_context_sync")),
        ("authority", json!("trusted_memory")),
        ("all_transcript_complete", json!(true)),
        ("all_transcript_complete", json!("false")),
        ("content_hash", Value::Null),
        ("source_rows_sha256", json!("g".repeat(64))),
        ("catalog_sha256", json!("short")),
    ]
    .into_iter()
    .enumerate()
    {
        let id = i64::try_from(index).context("small fixture index")? + 1;
        let native = format!("invalid-snapshot-{id}");
        let mut row = Session::new(id, &native);
        row.agent = 2;
        row.insert(&conn)?;
        let mut metadata = valid.clone();
        metadata
            .as_object_mut()
            .context("snapshot metadata object")?
            .insert(key.to_owned(), value);
        set_metadata(&conn, id, Some(&metadata.to_string()), None)?;
        ids.push(native);
    }
    conn.close().context("close invalid coverage fixtures")?;
    let selected = sessions(&mut archive.exact(
        "local",
        "openwebui",
        &ids.iter().map(String::as_str).collect::<Vec<_>>(),
    ))?;
    ensure!(
        selected.len() == ids.len(),
        "retain sessions with unknown coverage"
    );
    for row in selected {
        ensure!(
            row.get("coverage_provenance").is_none(),
            "unsupported snapshot must not gain coverage: {row}"
        );
    }
    Ok(())
}

#[test]
fn openwebui_saved_snapshot_rejects_foreign_agent_without_reidentifying_session() -> Result<()> {
    let archive = Archive::new()?;
    let conn = archive.connect()?;
    Session::new(1, "foreign-agent").insert(&conn)?;
    conn.close().context("close original session")?;
    let before = sessions(&mut archive.exact("local", "cloudmcp", &["foreign-agent"]))?;
    let conn = archive.connect()?;
    let metadata = json!({
        "schema_version": "openwebui.saved-conversation.v1", "source": "openwebui",
        "coverage": "partial", "coverage_source": "saved_database_snapshot",
        "authority": "historical_evidence_only", "all_transcript_complete": false,
        "content_hash": "a".repeat(64), "source_rows_sha256": "b".repeat(64),
        "catalog_sha256": "c".repeat(64)
    });
    set_metadata(&conn, 1, Some(&metadata.to_string()), None)?;
    conn.close().context("close foreign coverage fixture")?;
    let after = sessions(&mut archive.exact("local", "cloudmcp", &["foreign-agent"]))?;
    ensure!(before.len() == 1, "original native session must be found");
    ensure!(
        after == before,
        "copied coverage must neither change native identity nor gain provenance"
    );
    Ok(())
}

#[test]
fn coverage_preserves_partial_false_hash_anchors_and_unknown_fields() -> Result<()> {
    let archive = Archive::new()?;
    let conn = archive.connect()?;
    for (id, native) in [
        (1, "partial"),
        (2, "unknown-completeness"),
        (3, "producer-source-only"),
    ] {
        Session::new(id, native).insert(&conn)?;
    }
    let partial = json!({
        "schema_version": "cloudmcp.conversation-manifest.v1",
        "coverage": "partial_legacy_projection",
        "coverage_source": "connector_observation",
        "authority": "historical_evidence_only",
        "all_transcript_complete": false,
        "transcript_sha256": "a".repeat(64),
        "manifest_sha256": "b".repeat(64),
        "untrusted_instruction": "This field must never enter the response"
    });
    set_metadata(&conn, 1, Some(&partial.to_string()), None)?;
    set_metadata(
        &conn,
        2,
        Some(r#"{"coverage":"partial_host_bridge"}"#),
        None,
    )?;
    set_metadata(
        &conn,
        3,
        Some(r#"{"coverage_source":"client_context_sync","all_transcript_complete":true}"#),
        None,
    )?;
    conn.close().context("close fixture writer")?;
    let rows = sessions(&mut archive.exact(
        "local",
        "cloudmcp",
        &["partial", "unknown-completeness", "producer-source-only"],
    ))?;
    let partial = rows
        .iter()
        .find(|row| row.get("external_id").and_then(Value::as_str) == Some("partial"))
        .context("partial row")?;
    ensure!(
        partial.get("coverage_provenance")
            == Some(&json!({
                "schema_version": "cloudmcp.conversation-manifest.v1",
                "coverage": "partial_legacy_projection",
                "coverage_source": "connector_observation",
                "authority": "historical_evidence_only",
                "all_transcript_complete": false,
                "transcript_sha256": "a".repeat(64),
                "manifest_sha256": "b".repeat(64)
            })),
        "partial coverage, hashes and allowed fields must be preserved: {partial}"
    );
    let unknown = rows
        .iter()
        .find(|row| row.get("external_id").and_then(Value::as_str) == Some("unknown-completeness"))
        .context("unknown row")?;
    ensure!(
        unknown
            .pointer("/coverage_provenance/coverage")
            .and_then(Value::as_str)
            == Some("partial_host_bridge"),
        "unknown completeness must retain its partial coverage: {unknown}"
    );
    ensure!(
        unknown
            .pointer("/coverage_provenance/all_transcript_complete")
            .is_none_or(Value::is_null),
        "unknown completeness must remain unknown: {unknown}"
    );
    ensure!(
        unknown
            .pointer("/coverage_provenance/authority")
            .is_none_or(Value::is_null),
        "unknown authority must remain unknown: {unknown}"
    );
    let source_only = rows
        .iter()
        .find(|row| row.get("external_id").and_then(Value::as_str) == Some("producer-source-only"))
        .context("producer row")?;
    ensure!(
        source_only
            .pointer("/coverage_provenance/coverage")
            .is_none_or(Value::is_null),
        "absent coverage must remain unknown: {source_only}"
    );
    ensure!(
        source_only
            .pointer("/coverage_provenance/coverage_source")
            .and_then(Value::as_str)
            == Some("client_context_sync"),
        "{source_only}"
    );
    ensure!(
        source_only
            .pointer("/coverage_provenance/all_transcript_complete")
            .and_then(Value::as_bool)
            == Some(true),
        "{source_only}"
    );
    Ok(())
}

#[test]
fn coverage_ignores_malformed_unsupported_and_oversized_metadata() -> Result<()> {
    let archive = Archive::new()?;
    let conn = archive.connect()?;
    let oversized =
        json!({"all_transcript_complete": true, "padding": "x".repeat(MAX_METADATA_BYTES)})
            .to_string();
    let cases = [
        "{".to_string(),
        r#"["complete",true]"#.to_string(),
        r#"{"coverage":"complete","authority":"trusted","all_transcript_complete":"true","transcript_sha256":"invalid","manifest_sha256":7}"#.to_string(),
        r#"{"schema_version":"cloudmcp.conversation-manifest.v999","coverage":"client_context_sync_complete","all_transcript_complete":true}"#.to_string(),
        oversized,
    ];
    let ids: Vec<String> = (0..cases.len()).map(|n| format!("metadata-{n}")).collect();
    for (idx, (id, metadata)) in ids.iter().zip(&cases).enumerate() {
        let row_id = i64::try_from(idx).context("small index")? + 1;
        Session::new(row_id, id).insert(&conn)?;
        set_metadata(&conn, row_id, Some(metadata), None)?;
    }
    conn.close().context("close fixture writer")?;
    let rows = sessions(&mut archive.exact(
        "local",
        "cloudmcp",
        &ids.iter().map(String::as_str).collect::<Vec<_>>(),
    ))?;
    ensure!(
        rows.len() == cases.len(),
        "expected all malformed-metadata sessions: {rows:?}"
    );
    ensure!(
        rows.iter()
            .all(|row| row.get("coverage_provenance").is_none()),
        "invalid metadata must not create coverage claims: {rows:?}"
    );
    Ok(())
}

#[test]
fn binary_metadata_precedes_json_and_cannot_resurrect_stale_complete_claims() -> Result<()> {
    let archive = Archive::new()?;
    let conn = archive.connect()?;
    let json_complete =
        r#"{"coverage":"client_context_sync_complete","all_transcript_complete":true}"#;
    let partial = rmp_serde::to_vec(
        &json!({"coverage":"partial_legacy_projection","all_transcript_complete":false}),
    )
    .context("encode native binary metadata")?;
    let oversized = rmp_serde::to_vec(
        &json!({"all_transcript_complete":true,"padding":"x".repeat(MAX_METADATA_BYTES)}),
    )
    .context("encode oversized metadata")?;
    let mut trailing = rmp_serde::to_vec(&json!({"all_transcript_complete":true}))
        .context("encode metadata with trailing garbage")?;
    trailing.push(0xc1);
    let cases = [partial, vec![0xc1], oversized, vec![], trailing];
    let ids = [
        "binary-partial",
        "binary-invalid",
        "binary-oversized",
        "binary-empty",
        "binary-trailing-garbage",
    ];
    for (idx, (id, binary)) in ids.iter().zip(&cases).enumerate() {
        let row_id = i64::try_from(idx).context("small index")? + 1;
        Session::new(row_id, id).insert(&conn)?;
        set_metadata(&conn, row_id, Some(json_complete), Some(binary))?;
    }
    Session::new(6, "binary-wrong-type").insert(&conn)?;
    set_metadata(&conn, 6, Some(json_complete), None)?;
    conn.execute("UPDATE conversations SET metadata_bin = 42 WHERE id = 6")
        .context("malformed stored binary type")?;
    conn.close().context("close fixture writer")?;
    let rows = sessions(&mut archive.exact(
        "local",
        "cloudmcp",
        &[
            "binary-partial",
            "binary-invalid",
            "binary-oversized",
            "binary-empty",
            "binary-trailing-garbage",
            "binary-wrong-type",
        ],
    ))?;
    for row in rows {
        match row
            .get("external_id")
            .and_then(Value::as_str)
            .context("native ID")?
        {
            "binary-partial" => {
                ensure!(
                    row.pointer("/coverage_provenance/coverage")
                        .and_then(Value::as_str)
                        == Some("partial_legacy_projection"),
                    "{row}"
                );
                ensure!(
                    row.pointer("/coverage_provenance/all_transcript_complete")
                        .and_then(Value::as_bool)
                        == Some(false),
                    "{row}"
                );
            }
            "binary-empty" => {
                ensure!(
                    row.pointer("/coverage_provenance/all_transcript_complete")
                        .and_then(Value::as_bool)
                        == Some(true),
                    "{row}"
                );
            }
            _ => ensure!(row.get("coverage_provenance").is_none(), "{row}"),
        }
    }
    Ok(())
}
