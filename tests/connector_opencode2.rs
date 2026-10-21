//! OpenCode2's native session_v2 format, independent of legacy OpenCode fixtures.

use anyhow::{Context, Result, ensure};
use coding_agent_search::connectors::opencode::OpenCodeConnector;
use coding_agent_search::connectors::{
    Connector, Origin, ScanContext, ScanRoot, get_connector_factories,
};
use coding_agent_search::franken_sync::Connection;
use coding_agent_search::franken_sync::compat::ConnectionExt;
use coding_agent_search::franken_sync::params;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn context(path: &Path) -> ScanContext {
    ScanContext::with_roots(
        path.with_file_name("cass-archive"),
        vec![ScanRoot::remote(
            path.to_path_buf(),
            Origin::remote("v2-fixture"),
            None,
        )],
        None,
    )
}

fn database(path: &Path) -> Result<Connection> {
    let db = Connection::open(path.to_string_lossy().as_ref())?;
    db.execute_batch(
        "CREATE TABLE session_v2 (id TEXT PRIMARY KEY,directory TEXT NOT NULL,title TEXT,
            time_created INTEGER NOT NULL,time_updated INTEGER NOT NULL);
         CREATE TABLE session_message (id TEXT PRIMARY KEY,session_id TEXT NOT NULL,type TEXT NOT NULL,
            seq INTEGER NOT NULL,time_created INTEGER NOT NULL,time_updated INTEGER NOT NULL,data TEXT NOT NULL);
         INSERT INTO session_v2 VALUES ('ses_native','/work/native','Native source',1000,3000);",
    )?;
    db.execute_compat(
        "INSERT INTO session_message VALUES ('msg_user','ses_native','user',0,1000,1000,?1)",
        params![json!({"text":"Keep the original workspace","time":{"created":1000}}).to_string()],
    )?;
    db.execute_compat(
        "INSERT INTO session_message VALUES ('msg_reply','ses_native','assistant',1,2000,3000,?1)",
        params![json!({"content":[{"type":"text","text":"I retained the source"},
            {"type":"tool","name":"inspect","state":{"input":{"path":"src"},"content":"native tool result"}}],
            "time":{"created":2000},"model":{"providerID":"original-provider","id":"original-model"}}).to_string()],
    )?;
    Ok(db)
}

#[test]
fn native_identity_full_message_payload_replay_and_read_only_source() -> Result<()> {
    let temporary = TempDir::new()?;
    let path = temporary.path().join("opencode2.db");
    drop(database(&path)?);
    let before = fs::read(&path)?;
    let connector = OpenCodeConnector::new();
    let first = connector.scan(&context(&path))?;
    let second = connector.scan(&context(&path))?;
    ensure!(
        first.len() == 1 && second.len() == 1,
        "exactly one native session per scan"
    );
    let session = first.first().context("native session missing")?;
    ensure!(session.agent_slug == "opencode");
    ensure!(session.external_id.as_deref() == Some("ses_native"));
    ensure!(session.workspace.as_deref() == Some(Path::new("/work/native")));
    ensure!(session.metadata["harness"] == "opencode2");
    ensure!(session.metadata["origin"]["source_id"] == "v2-fixture");
    ensure!(session.messages.len() == 2);
    let user = session.messages.first().context("user message missing")?;
    let assistant = session
        .messages
        .get(1)
        .context("assistant message missing")?;
    ensure!(user.role == "user" && user.extra["native_message_id"] == "msg_user");
    ensure!(assistant.role == "assistant");
    ensure!(assistant.extra["model"]["providerID"] == "original-provider");
    ensure!(assistant.content.contains("native tool result"));
    ensure!(assistant.content.contains("inspect"));
    ensure!(
        session.metadata["content_hash"]
            == second.first().context("replay missing")?.metadata["content_hash"]
    );
    ensure!(
        before == fs::read(&path)?,
        "scan must leave native DB bytes unchanged"
    );
    let sources = connector.discover_source_files(&context(&path))?;
    ensure!(
        sources
            .iter()
            .filter(|source| source.role
                == coding_agent_search::connectors::DiscoveredSourceRole::SqliteDatabase)
            .count()
            == 1,
        "native database must be discovered once"
    );
    Ok(())
}

#[test]
fn appended_turns_and_message_only_updates_remain_incrementally_visible() -> Result<()> {
    let temporary = TempDir::new()?;
    let path = temporary.path().join("opencode2.db");
    let db = database(&path)?;
    let connector = OpenCodeConnector::new();
    let first = connector.scan(&context(&path))?;
    let large_output = "retained-output".repeat(20_000);
    db.execute_compat(
        "INSERT INTO session_message VALUES ('msg_future','ses_native','synthetic',2,9000,9000,?1)",
        params![json!({"text":large_output,"description":"native synthetic turn"}).to_string()],
    )?;
    drop(db);
    let mut ctx = context(&path);
    ctx.since_ts = Some(8500);
    let changed = connector.scan(&ctx)?;
    let session = changed.first().context("message-only update was skipped")?;
    ensure!(session.messages.len() == 3);
    let last = session
        .messages
        .last()
        .context("appended message missing")?;
    ensure!(
        last.role == "synthetic",
        "native synthetic turn must not become a user turn"
    );
    ensure!(
        last.content == large_output,
        "large native content must not be truncated"
    );
    ensure!(session.ended_at == Some(9000));
    ensure!(
        session.metadata["content_hash"]
            != first.first().context("initial session missing")?.metadata["content_hash"]
    );
    ctx.since_ts = Some(11_000);
    ensure!(
        connector.scan(&ctx)?.is_empty(),
        "unchanged source should respect cutoff"
    );
    Ok(())
}

#[test]
fn empty_native_session_keeps_metadata_and_later_message_only_growth() -> Result<()> {
    let temporary = TempDir::new()?;
    let path = temporary.path().join("opencode2.db");
    let db = database(&path)?;
    db.execute_batch(
        "INSERT INTO session_v2 VALUES ('ses_empty','/work/empty','Awaiting first turn',4000,4000);",
    )?;
    drop(db);
    let before = fs::read(&path)?;
    let connector = OpenCodeConnector::new();
    let mut ctx = context(&path);
    ctx.since_ts = Some(4000);
    let first = connector.scan(&ctx)?;
    let empty = first
        .iter()
        .find(|session| session.external_id.as_deref() == Some("ses_empty"))
        .context("empty native session was skipped")?;
    ensure!(empty.agent_slug == "opencode");
    ensure!(empty.workspace.as_deref() == Some(Path::new("/work/empty")));
    ensure!(empty.source_path == path.canonicalize()?);
    ensure!(empty.title.as_deref() == Some("Awaiting first turn"));
    ensure!(empty.started_at == Some(4000) && empty.ended_at == Some(4000));
    ensure!(empty.metadata["native_session_id"] == "ses_empty");
    ensure!(empty.metadata["origin"]["source_id"] == "v2-fixture");
    ensure!(empty.messages.is_empty(), "capture invented a native turn");
    ensure!(
        empty.metadata["content_hash"]
            == "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945"
    );
    let replay = connector.scan(&ctx)?;
    let same = replay
        .iter()
        .find(|session| session.external_id == empty.external_id)
        .context("empty native session missing on replay")?;
    ensure!(same.metadata == empty.metadata && same.messages.is_empty());
    ensure!(
        fs::read(&path)? == before,
        "empty capture changed native DB"
    );

    let db = Connection::open(path.to_string_lossy().as_ref())?;
    db.execute_compat(
        "INSERT INTO session_message VALUES ('first_turn','ses_empty','user',0,5000,5000,?1)",
        params![json!({"text":"The first original turn"}).to_string()],
    )?;
    drop(db);
    ctx.since_ts = Some(5000);
    let grown = connector.scan(&ctx)?;
    let session = grown
        .iter()
        .find(|session| session.external_id == empty.external_id)
        .context("message-only growth lost the native session")?;
    ensure!(session.messages.len() == 1 && session.started_at == empty.started_at);
    let message = session
        .messages
        .first()
        .context("first native turn missing")?;
    ensure!(message.extra["native_message_id"] == "first_turn");
    ensure!(message.role == "user" && message.idx == 0);
    ensure!(session.metadata["content_hash"] != empty.metadata["content_hash"]);
    ensure!(session.ended_at == Some(5000));
    Ok(())
}

#[test]
fn explicit_roots_do_not_leak_defaults_and_aliases_do_not_duplicate() -> Result<()> {
    let temporary = TempDir::new()?;
    let path = temporary.path().join("opencode2.db");
    drop(database(&path)?);
    let connector = OpenCodeConnector::new();
    let mut ctx = context(&path);
    ctx.scan_roots.push(ScanRoot::remote(
        temporary.path().to_path_buf(),
        Origin::remote("v2-fixture"),
        None,
    ));
    ensure!(
        connector.scan(&ctx)?.len() == 1,
        "overlapping roots duplicate the native session"
    );
    let empty = temporary.path().join("unrelated");
    fs::create_dir(&empty)?;
    ensure!(
        connector.scan(&context(&empty))?.is_empty(),
        "explicit empty scope included a default store"
    );
    Ok(())
}

#[test]
fn malformed_message_and_duplicate_sequence_are_explicit_failures() -> Result<()> {
    let temporary = TempDir::new()?;
    let path = temporary.path().join("opencode2.db");
    let db = database(&path)?;
    db.execute_batch("UPDATE session_message SET data='[]' WHERE id='msg_user'")?;
    ensure!(
        OpenCodeConnector::new().scan(&context(&path)).is_err(),
        "non-object native data must be rejected"
    );
    db.execute_batch("UPDATE session_message SET data='{}',seq=1 WHERE id='msg_user'")?;
    ensure!(
        OpenCodeConnector::new().scan(&context(&path)).is_err(),
        "ambiguous native ordering must be rejected"
    );
    Ok(())
}

#[test]
fn registered_factory_uses_the_native_v2_parser() -> Result<()> {
    let temporary = TempDir::new()?;
    let path = temporary.path().join("opencode2.db");
    drop(database(&path)?);
    let factory = get_connector_factories()
        .into_iter()
        .find(|(name, _)| *name == "opencode")
        .context("OpenCode factory missing")?
        .1;
    ensure!(factory().scan(&context(&path))?.len() == 1);
    Ok(())
}

#[test]
#[ignore = "requires explicit native DB and original export paths; never opens a live store by default"]
fn original_native_export_matches_database_messages() -> Result<()> {
    let database = PathBuf::from(dotenvy::var("CASS_OPENCODE2_VERIFY_DB")?);
    let export = PathBuf::from(dotenvy::var("CASS_OPENCODE2_VERIFY_EXPORT")?);
    let original: Value = serde_json::from_slice(&fs::read(export)?)?;
    let id = original
        .get("info")
        .and_then(|info| info.get("id"))
        .and_then(Value::as_str)
        .context("native export session ID missing")?;
    let all = OpenCodeConnector::new().scan(&context(&database))?;
    let session = all
        .iter()
        .find(|session| session.external_id.as_deref() == Some(id)) // ubs:ignore -- This assertion compares a public session ID, not secret material.
        .context("exported session absent from native DB scan")?;
    let rows = original
        .get("messages")
        .and_then(Value::as_array)
        .context("native export messages missing")?;
    ensure!(
        session.messages.len() == rows.len(),
        "native export and database message counts differ"
    );
    for (stored, original) in session.messages.iter().zip(rows) {
        ensure!(
            stored
                .extra
                .get("native_message_id")
                .context("stored native message ID missing")?
                == original.get("id").context("export message ID missing")?,
            "native message identity differs"
        );
        ensure!(
            stored.role
                == original
                    .get("type")
                    .context("export message type field missing")?
                    .as_str()
                    .context("export message type missing")?
        );
        let expected = original
            .as_object()
            .context("export message object missing")?;
        let actual = stored
            .extra
            .as_object()
            .context("stored native message object missing")?;
        let added_keys = [
            "native_message_id",
            "native_message_type",
            "native_sequence",
            "native_updated_at",
        ];
        let mut expected_fields = expected
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "id" | "type"));
        let actual_fields = actual
            .keys()
            .filter(|key| !added_keys.contains(&key.as_str()));
        ensure!(
            actual_fields.count()
                == expected
                    .len()
                    .checked_sub(2)
                    .context("export identity fields missing")?
                && expected_fields.all(|(key, value)| actual.get(key) == Some(value)),
            "original native message fields changed during ingestion"
        );
    }
    Ok(())
}
