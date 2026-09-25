use anyhow::{Context, Result, ensure};
use coding_agent_search::connectors::kilo::KiloConnector;
use coding_agent_search::connectors::{Connector, ScanContext, ScanRoot};
use coding_agent_search::franken_sync::Connection;
use coding_agent_search::franken_sync::compat::ConnectionExt;
use coding_agent_search::franken_sync::params;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn context(path: &Path) -> ScanContext {
    ScanContext::with_roots(
        path.to_path_buf(),
        vec![ScanRoot::local(path.to_path_buf())],
        None,
    )
}

fn database(path: &Path) -> Result<()> {
    let db = Connection::open(path.to_string_lossy().to_string())?;
    db.execute_batch("CREATE TABLE session (id TEXT PRIMARY KEY,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER);
        CREATE TABLE message (id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,data TEXT);
        CREATE TABLE part (id TEXT PRIMARY KEY,message_id TEXT,session_id TEXT,time_created INTEGER,data TEXT);
        INSERT INTO session VALUES ('native-1','/work/kilo','History qualification',1000,3000);")?;
    for (id, time, role_json, part_json) in [
        (
            "m1",
            1000i64,
            r#"{"role":"user"}"#,
            r#"{"type":"text","text":"Remember exact repository ownership"}"#,
        ),
        (
            "m2",
            2000,
            r#"{"role":"assistant"}"#,
            r#"{"type":"text","text":"BR owns work; EE contains reviewed evidence"}"#,
        ),
    ] {
        db.execute_compat(
            "INSERT INTO message VALUES (?1,'native-1',?2,?3)",
            params![id, time, role_json],
        )?;
        db.execute_compat(
            "INSERT INTO part VALUES (?1,?1,'native-1',?2,?3)",
            params![id, time, part_json],
        )?;
    }
    Ok(())
}

#[test]
fn cli_identity_replay_and_source_database_are_preserved() -> Result<()> {
    let tmp = TempDir::new()?;
    let path = tmp.path().join("kilo.db");
    database(&path)?;
    let before = fs::read(&path)?;
    let connector = KiloConnector::new();
    let ctx = context(&path);
    let first = connector.scan(&ctx)?;
    let second = connector.scan(&ctx)?;
    ensure!(first.len() == 1, "expected one Kilo conversation");
    let c = first.first().context("Kilo conversation missing")?;
    ensure!(c.agent_slug == "kilo");
    ensure!(c.external_id.as_deref() == Some("cli:native-1"));
    ensure!(c.workspace.as_deref() == Some(Path::new("/work/kilo")));
    ensure!(c.messages.len() == 2);
    let user_message = c.messages.first().context("Kilo user message missing")?;
    let assistant_message = c
        .messages
        .get(1)
        .context("Kilo assistant message missing")?;
    ensure!(user_message.role == "user");
    ensure!(assistant_message.content.contains("reviewed evidence"));
    ensure!(
        c.metadata.get("content_hash")
            == second
                .first()
                .context("second Kilo conversation missing")?
                .metadata
                .get("content_hash")
    );
    ensure!(before == fs::read(&path)?, "read must not mutate source DB");
    let mut overlapping = ctx.clone();
    overlapping
        .scan_roots
        .push(ScanRoot::local(tmp.path().to_path_buf()));
    ensure!(connector.scan(&overlapping)?.len() == 1);
    let sources = connector.discover_source_files(&ctx)?;
    ensure!(
        sources
            .first()
            .context("Kilo source discovery returned no sources")?
            .provider_slug
            == "kilo"
    );
    Ok(())
}

#[test]
fn bulk_database_scan_keeps_session_boundaries_and_part_order() -> Result<()> {
    let tmp = TempDir::new()?;
    let path = tmp.path().join("kilo.db");
    database(&path)?;
    let db = Connection::open(path.to_string_lossy().to_string())?;
    db.execute_batch("INSERT INTO session VALUES ('native-2','/work/other','Other session',4000,5000);
        INSERT INTO message VALUES ('m3','native-2',4000,'{\"role\":\"user\"}');
        INSERT INTO part VALUES ('p-late','m3','native-2',4200,'{\"type\":\"text\",\"text\":\"second\"}');
        INSERT INTO part VALUES ('p-early','m3','native-2',4100,'{\"type\":\"text\",\"text\":\"first\"}');
        INSERT INTO part VALUES ('p-orphan','m1','native-2',4100,'{\"type\":\"text\",\"text\":\"must not cross sessions\"}');")?;
    drop(db);
    let connector = KiloConnector::new();
    let mut ctx = context(&path);
    let result = connector.scan(&ctx)?;
    ensure!(result.len() == 2);
    let first_session = result.first().context("first Kilo session missing")?;
    let second_session = result.get(1).context("second Kilo session missing")?;
    ensure!(first_session.messages.len() == 2);
    let first_message = first_session
        .messages
        .first()
        .context("first Kilo message missing")?;
    ensure!(!first_message.content.contains("cross sessions"));
    ensure!(second_session.workspace.as_deref() == Some(Path::new("/work/other")));
    ensure!(second_session.messages.len() == 1);
    ensure!(
        second_session
            .messages
            .first()
            .context("second session message missing")?
            .content
            == "first\nsecond"
    );
    ctx.since_ts = Some(3500);
    let recent = connector.scan(&ctx)?;
    ensure!(recent.len() == 1);
    ensure!(
        recent
            .first()
            .context("recent Kilo session missing")?
            .external_id
            .as_deref()
            == Some("cli:native-2")
    );
    Ok(())
}

#[test]
fn native_cli_index_replay_is_searchable_without_duplicate_sessions() -> Result<()> {
    let tmp = TempDir::new()?;
    let source = tmp.path().join("kilo.db");
    database(&source)?;
    let data = tmp.path().join("archive");
    let command = || -> Result<assert_cmd::Command> {
        let mut cmd = assert_cmd::Command::cargo_bin("cass")?;
        cmd.current_dir(tmp.path())
            .env("HOME", tmp.path())
            .env("CODEX_HOME", tmp.path().join("codex"))
            .env("XDG_CONFIG_HOME", tmp.path().join("config"))
            .env("XDG_DATA_HOME", tmp.path().join("xdg"))
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_CLOUDMCP_DATA_ROOT", tmp.path().join("cloudmcp"));
        Ok(cmd)
    };
    for _ in 0..2 {
        command()?
            .args(["index", "--watch-once"])
            .arg(&source)
            .args(["--data-dir"])
            .arg(&data)
            .arg("--json")
            .assert()
            .success();
    }
    let output = command()?
        .args(["sessions", "--workspace", "/work/kilo", "--data-dir"])
        .arg(&data)
        .arg("--json")
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sessions: Value = serde_json::from_slice(&output.stdout)?;
    let session_rows = sessions
        .get("sessions")
        .and_then(Value::as_array)
        .context("sessions response has no sessions array")?;
    ensure!(session_rows.len() == 1, "{sessions}");
    let output = command()?
        .args([
            "search",
            "reviewed evidence",
            "--agent",
            "kilo",
            "--mode",
            "lexical",
            "--data-dir",
        ])
        .arg(&data)
        .arg("--json")
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout)?;
    let hits = result
        .get("hits")
        .and_then(Value::as_array)
        .context("search response has no hits array")?;
    ensure!(!hits.is_empty(), "{result}");
    Ok(())
}

#[test]
fn explicit_roots_never_import_opencode_or_host_defaults() -> Result<()> {
    let tmp = TempDir::new()?;
    database(&tmp.path().join("opencode.db"))?;
    ensure!(KiloConnector::new().scan(&context(tmp.path()))?.is_empty());
    ensure!(
        KiloConnector::new()
            .scan(&context(&tmp.path().join("opencode.db")))?
            .is_empty()
    );
    Ok(())
}

#[test]
fn ide_prefers_full_native_history_and_keeps_distinct_identity() -> Result<()> {
    let tmp = TempDir::new()?;
    let tasks = tmp.path().join("kilocode.kilo-code/tasks");
    let task = tasks.join("native-1");
    fs::create_dir_all(&task)?;
    fs::write(
        task.join("api_conversation_history.json"),
        json!([
            {"role":"user","content":[{"type":"text","text":"full Kilo input"}],"ts":1000},
            {"role":"assistant","content":[{"type":"text","text":"full Kilo reply"}],"ts":2000}
        ])
        .to_string(),
    )?;
    fs::write(task.join("ui_messages.json"), "[]")?;
    fs::write(
        task.join("task_metadata.json"),
        json!({"workspace":"/work/ide"}).to_string(),
    )?;
    let out = KiloConnector::new().scan(&context(&tasks))?;
    ensure!(out.len() == 1);
    let conversation = out.first().context("Kilo IDE conversation missing")?;
    ensure!(conversation.external_id.as_deref() == Some("ide:native-1"));
    ensure!(conversation.messages.len() == 2);
    ensure!(
        conversation
            .messages
            .first()
            .context("Kilo IDE user message missing")?
            .content
            == "full Kilo input"
    );
    ensure!(conversation.workspace.as_deref() == Some(Path::new("/work/ide")));
    ensure!(
        conversation
            .metadata
            .get("coverage")
            .and_then(Value::as_str)
            == Some("native_api_history")
    );
    Ok(())
}

#[test]
fn missing_ide_workspace_is_not_invented() -> Result<()> {
    let tmp = TempDir::new()?;
    let task = tmp.path().join("kilocode.kilo-code/tasks/1");
    fs::create_dir_all(&task)?;
    fs::write(
        task.join("api_conversation_history.json"),
        r#"[{"role":"user","content":"history without location"}]"#,
    )?;
    let out = KiloConnector::new().scan(&context(&task))?;
    let conversation = out.first().context("Kilo IDE conversation missing")?;
    ensure!(conversation.workspace.is_none());
    ensure!(
        conversation
            .metadata
            .get("workspace_observed")
            .and_then(Value::as_bool)
            == Some(false)
    );
    Ok(())
}

#[test]
fn callback_failure_propagates_and_new_connectors_are_registered() -> Result<()> {
    let tmp = TempDir::new()?;
    let path = tmp.path().join("kilo.db");
    database(&path)?;
    let err = KiloConnector::new()
        .scan_with_callback(&context(&path), &mut |_| anyhow::bail!("stop fixture"))
        .err()
        .context("callback failure did not propagate")?;
    ensure!(err.to_string().contains("stop fixture"));
    let factories = coding_agent_search::connectors::get_connector_factories();
    for name in ["kilo", "cloudmcp"] {
        ensure!(
            factories.iter().filter(|(n, _)| *n == name).count() == 1,
            "connector factory {name} should be registered exactly once"
        );
    }
    Ok(())
}
