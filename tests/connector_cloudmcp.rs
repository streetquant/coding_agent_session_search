use anyhow::{Context, Result, ensure};
use coding_agent_search::connectors::cloudmcp::CloudMcpConnector;
use coding_agent_search::connectors::{Connector, ScanContext, ScanRoot};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn digest(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}
fn event(sequence: u64, id: &str, role: &str, content: &str) -> Value {
    json!({
    "schema_version":"cloudmcp.conversation-event.v1","conversation_id":"chat-session-1",
    "workspace_root":"/work/cloudmcp","sequence":sequence,"created_at":"2026-09-24T01:00:00Z",
    "source_event_id":id,"source_event_sha256":digest(id.as_bytes()),"content":content,
    "content_sha256":digest(content.as_bytes()),"role":role,"source":"host_bridge",
    "session_id":"native-client-session","agent_id":"cloudmcp-client","event_coverage":"partial"})
}
fn fixture(path: &Path, events: &[Value]) -> Result<ScanContext> {
    fs::create_dir_all(path)?;
    let text = events.iter().map(|v| format!("{v}\n")).collect::<String>();
    fs::write(path.join("transcript.jsonl"), &text)?;
    fs::write(
        path.join("manifest.json"),
        json!({
            "schema_version":"cloudmcp.conversation-manifest.v1","conversation_id":"chat-session-1",
            "workspace_root":"/work/cloudmcp","transcript_file":"transcript.jsonl",
            "transcript_sha256":digest(text.as_bytes()),"event_count":events.len(),
            "coverage_source":"partial_host_bridge","all_transcript_complete":false
        })
        .to_string(),
    )?;
    Ok(ScanContext::with_roots(
        path.to_path_buf(),
        vec![ScanRoot::local(path.to_path_buf())],
        None,
    ))
}

#[test]
fn scoped_export_replay_retains_roles_and_partial_coverage() -> Result<()> {
    let tmp = TempDir::new()?;
    let ctx = fixture(
        tmp.path(),
        &[
            event(1, "u1", "user", "Resume this exact repository"),
            event(2, "a1", "assistant", "Handoff retains pending BR work"),
            event(3, "t1", "tool", "operation succeeded"),
        ],
    )?;
    let connector = CloudMcpConnector::new();
    let first = connector.scan(&ctx)?;
    let second = connector.scan(&ctx)?;
    ensure!(first.len() == 1, "expected one CloudMCP conversation");
    let c = first.first().context("CloudMCP conversation missing")?;
    ensure!(c.agent_slug == "cloudmcp");
    ensure!(c.external_id.as_deref() == Some("chat-session-1"));
    ensure!(c.workspace.as_deref() == Some(Path::new("/work/cloudmcp")));
    ensure!(c.messages.len() == 3);
    let user_message = c.messages.first().context("user message missing")?;
    let tool_message = c.messages.get(2).context("tool message missing")?;
    ensure!(user_message.role == "user");
    ensure!(tool_message.role == "tool");
    ensure!(
        user_message.extra.get("session_id").and_then(Value::as_str)
            == Some("native-client-session")
    );
    ensure!(
        c.metadata
            .get("all_transcript_complete")
            .and_then(Value::as_bool)
            == Some(false)
    );
    ensure!(
        c.metadata.get("coverage_source").and_then(Value::as_str) == Some("partial_host_bridge")
    );
    ensure!(
        c.metadata.get("content_hash")
            == second
                .first()
                .context("second CloudMCP conversation missing")?
                .metadata
                .get("content_hash")
    );
    let sources = connector.discover_source_files(&ctx)?;
    ensure!(sources.len() == 2);
    ensure!(
        sources
            .iter()
            .all(|s| s.provider_slug == "cloudmcp" && s.required_for_reconstruction)
    );
    Ok(())
}

#[test]
fn native_cli_index_replay_retains_one_cloudmcp_session() -> Result<()> {
    let tmp = TempDir::new()?;
    let source = tmp
        .path()
        .join("contextos/transcripts/v1/workspace/session");
    fixture(
        &source,
        &[
            event(1, "u1", "user", "cloudmcphandoffneedle exact workspace"),
            event(2, "a1", "assistant", "Reviewed history remains scoped"),
        ],
    )?;
    let data = tmp.path().join("archive");
    let command = || -> Result<assert_cmd::Command> {
        let mut cmd = assert_cmd::Command::cargo_bin("cass")?;
        cmd.current_dir(tmp.path())
            .env("HOME", tmp.path())
            .env("CODEX_HOME", tmp.path().join("codex"))
            .env("XDG_CONFIG_HOME", tmp.path().join("config"))
            .env("XDG_DATA_HOME", tmp.path().join("xdg"))
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env(
                "CASS_CLOUDMCP_DATA_ROOT",
                tmp.path().join("contextos/transcripts/v1"),
            );
        Ok(cmd)
    };
    for _ in 0..2 {
        command()?
            .args(["index", "--watch-once"])
            .arg(source.join("manifest.json"))
            .args(["--data-dir"])
            .arg(&data)
            .arg("--json")
            .assert()
            .success();
    }
    let output = command()?
        .args(["sessions", "--workspace", "/work/cloudmcp", "--data-dir"])
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
            "cloudmcphandoffneedle",
            "--agent",
            "cloudmcp",
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
fn duplicate_source_events_are_deduplicated_without_losing_source_identity() -> Result<()> {
    let tmp = TempDir::new()?;
    let ctx = fixture(
        tmp.path(),
        &[
            event(1, "e1", "tool", "output"),
            event(2, "e1", "tool", "output"),
        ],
    )?;
    let out = CloudMcpConnector::new().scan(&ctx)?;
    let conversation = out.first().context("deduplicated conversation missing")?;
    ensure!(conversation.messages.len() == 1);
    ensure!(
        conversation
            .metadata
            .get("event_count")
            .and_then(Value::as_u64)
            == Some(2)
    );
    Ok(())
}

#[test]
fn rejects_tampered_or_incomplete_publication() -> Result<()> {
    let tmp = TempDir::new()?;
    let ctx = fixture(tmp.path(), &[event(1, "e1", "tool", "output")])?;
    fs::write(tmp.path().join("transcript.jsonl"), "tampered")?;
    let err = CloudMcpConnector::new()
        .scan(&ctx)
        .err()
        .context("tampered transcript was accepted")?;
    ensure!(err.to_string().contains("digest mismatch"));
    Ok(())
}

#[test]
fn rejects_cross_workspace_event_even_with_valid_file_hash() -> Result<()> {
    let tmp = TempDir::new()?;
    let mut e = event(1, "e1", "tool", "output");
    e.as_object_mut()
        .context("event fixture must be an object")?
        .insert("workspace_root".to_owned(), json!("/another/repo"));
    let ctx = fixture(tmp.path(), &[e])?;
    let err = CloudMcpConnector::new()
        .scan(&ctx)
        .err()
        .context("cross-workspace event was accepted")?;
    ensure!(err.to_string().contains("identity mismatch"));
    Ok(())
}

#[test]
fn rejects_conflicting_duplicate_and_content_hash_forgery() -> Result<()> {
    let tmp = TempDir::new()?;
    let ctx = fixture(
        tmp.path(),
        &[
            event(1, "e1", "tool", "output"),
            event(2, "e1", "tool", "different"),
        ],
    )?;
    let err = CloudMcpConnector::new()
        .scan(&ctx)
        .err()
        .context("conflicting duplicate source event was accepted")?;
    ensure!(err.to_string().contains("conflicting duplicate"));
    let mut e = event(1, "e1", "user", "content");
    e.as_object_mut()
        .context("event fixture must be an object")?
        .insert("content_sha256".to_owned(), json!("wrong"));
    let ctx = fixture(tmp.path(), &[e])?;
    let err = CloudMcpConnector::new()
        .scan(&ctx)
        .err()
        .context("forged content digest was accepted")?;
    ensure!(err.to_string().contains("content digest mismatch"));
    Ok(())
}

#[test]
fn explicit_empty_root_does_not_fall_back_to_host_history() -> Result<()> {
    let tmp = TempDir::new()?;
    let ctx = ScanContext::with_roots(
        tmp.path().to_path_buf(),
        vec![ScanRoot::local(tmp.path().to_path_buf())],
        None,
    );
    ensure!(CloudMcpConnector::new().scan(&ctx)?.is_empty());
    Ok(())
}

#[test]
fn rejects_transcript_path_escape() -> Result<()> {
    let tmp = TempDir::new()?;
    let ctx = fixture(tmp.path(), &[event(1, "e1", "tool", "output")])?;
    let p = tmp.path().join("manifest.json");
    let mut v: Value = serde_json::from_slice(&fs::read(&p)?)?;
    v.as_object_mut()
        .context("manifest fixture must be an object")?
        .insert("transcript_file".to_owned(), json!("../transcript.jsonl"));
    fs::write(p, v.to_string())?;
    let err = CloudMcpConnector::new()
        .scan(&ctx)
        .err()
        .context("transcript path escape was accepted")?;
    ensure!(err.to_string().contains("adjacent canonical"));
    Ok(())
}

#[test]
fn cloudmcp_rollout_is_not_also_indexed_as_codex() -> Result<()> {
    use coding_agent_search::connectors::codex::CodexConnector;
    let tmp = TempDir::new()?;
    let sessions = tmp.path().join(".codex/sessions");
    fs::create_dir_all(&sessions)?;
    let path = sessions.join("rollout-cloudmcp-fixture.jsonl");
    let rows = [
        json!({"type":"session_meta","payload":{"id":"cloudmcp-fixture","cwd":"/work/cloudmcp","originator":"cloudmcp","source":"cloudmcp-contextos"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"CloudMCP evidence only"}]}}),
    ];
    fs::write(
        &path,
        rows.iter().map(|v| format!("{v}\n")).collect::<String>(),
    )?;
    let ctx = ScanContext::with_roots(tmp.path().to_path_buf(), vec![ScanRoot::local(path)], None);
    let connector = CodexConnector::new();
    ensure!(connector.scan(&ctx)?.is_empty());
    ensure!(connector.discover_source_files(&ctx)?.is_empty());
    let mut count = 0;
    connector.scan_with_callback(&ctx, &mut |_| {
        count += 1;
        Ok(())
    })?;
    ensure!(count == 0);
    Ok(())
}
