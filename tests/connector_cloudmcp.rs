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

fn assert_raw_pair_retained_before_parse(ctx: &ScanContext) -> Result<()> {
    use coding_agent_search::raw_mirror::{RawMirrorCaptureInput, capture_source_file};

    let sources = CloudMcpConnector::new().discover_source_files(ctx)?;
    ensure!(
        sources.len() == 2,
        "canonical publication must retain both raw files"
    );
    let origin = &ctx
        .scan_roots
        .first()
        .context("explicit fixture root missing")?
        .origin;
    let archive = ctx.data_dir.join("raw-capture-fixture");
    for source in sources {
        ensure!(source.source_path.is_absolute() && source.scan_root.is_absolute());
        ensure!(source.source_path.starts_with(&source.scan_root));
        ensure!(source.required_for_reconstruction);
        ensure!(source.origin.source_id == origin.source_id);
        ensure!(source.origin.kind == origin.kind && source.origin.host == origin.host);
        let original = fs::read(&source.source_path)?;
        let capture = || {
            capture_source_file(RawMirrorCaptureInput {
                data_dir: &archive,
                provider: "cloudmcp",
                source_id: &source.origin.source_id,
                origin_kind: source.origin.kind.as_str(),
                origin_host: source.origin.host.as_deref(),
                source_path: &source.source_path,
                db_links: &[],
            })
        };
        let first = capture()?;
        let replay = capture()?;
        ensure!(replay.already_present);
        ensure!(first.manifest_id == replay.manifest_id);
        ensure!(first.blob_blake3 == replay.blob_blake3);
        ensure!(first.source_content_blake3 == blake3::hash(&original).to_hex().as_str());
        ensure!(first.source_size_bytes == original.len() as u64);
        ensure!(
            fs::read(
                archive
                    .join("raw-mirror/v1")
                    .join(&first.blob_relative_path)
            )? == original,
            "retained blob must contain the exact unsupported raw bytes"
        );
        ensure!(
            fs::read(&source.source_path)? == original,
            "source must remain unchanged"
        );
    }
    Ok(())
}

#[test]
fn unsupported_manifest_schema_is_discovered_and_retained_before_rejection() -> Result<()> {
    let tmp = TempDir::new()?;
    let ctx = fixture(tmp.path(), &[event(1, "e1", "tool", "output")])?;
    let path = tmp.path().join("manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&path)?)?;
    manifest
        .as_object_mut()
        .context("manifest fixture must be an object")?
        .insert(
            "schema_version".to_owned(),
            json!("cloudmcp.conversation-manifest.v2"),
        );
    fs::write(&path, manifest.to_string())?;
    assert_raw_pair_retained_before_parse(&ctx)?;
    let err = CloudMcpConnector::new()
        .scan(&ctx)
        .err()
        .context("unsupported manifest was accepted")?;
    ensure!(
        err.to_string()
            .contains("unsupported CloudMCP manifest schema")
    );
    Ok(())
}

#[test]
fn malformed_manifest_remains_discoverable_before_parse_error() -> Result<()> {
    let tmp = TempDir::new()?;
    let ctx = fixture(tmp.path(), &[event(1, "e1", "tool", "output")])?;
    fs::write(tmp.path().join("manifest.json"), b"{malformed manifest\n")?;
    assert_raw_pair_retained_before_parse(&ctx)?;
    ensure!(CloudMcpConnector::new().scan(&ctx).is_err());
    Ok(())
}

#[test]
fn explicit_publication_file_roots_retain_the_adjacent_pair_and_origin() -> Result<()> {
    let tmp = TempDir::new()?;
    fixture(tmp.path(), &[event(1, "e1", "tool", "output")])?;
    for filename in ["manifest.json", "transcript.jsonl"] {
        let ctx = ScanContext::with_roots(
            tmp.path().to_path_buf(),
            vec![ScanRoot::local(tmp.path().join(filename))],
            None,
        );
        assert_raw_pair_retained_before_parse(&ctx)?;
        let conversations = CloudMcpConnector::new().scan(&ctx)?;
        ensure!(conversations.len() == 1);
        ensure!(
            conversations
                .first()
                .context("explicit-file conversation missing")?
                .external_id
                .as_deref()
                == Some("chat-session-1")
        );
    }
    Ok(())
}

#[test]
fn duplicate_source_events_reject_conflicting_native_or_unknown_fields() -> Result<()> {
    for (field, value) in [
        ("role", json!("user")),
        ("session_id", json!("other-native-session")),
        ("agent_id", json!("other-native-agent")),
        ("event_coverage", json!("complete")),
        ("unknown_future_field", json!({"retained":true})),
    ] {
        let tmp = TempDir::new()?;
        let first = event(1, "e1", "tool", "output");
        let mut duplicate = event(2, "e1", "tool", "output");
        duplicate
            .as_object_mut()
            .context("event fixture must be an object")?
            .insert(field.to_owned(), value);
        let ctx = fixture(tmp.path(), &[first, duplicate])?;
        assert_raw_pair_retained_before_parse(&ctx)?;
        let err = CloudMcpConnector::new()
            .scan(&ctx)
            .err()
            .context("conflicting duplicate event was silently discarded")?;
        ensure!(
            err.to_string().contains("conflicting duplicate"),
            "{field}: {err}"
        );
    }
    Ok(())
}

#[test]
fn duplicate_roles_schemas_and_empty_ids_are_validated_before_deduplication() -> Result<()> {
    for (field, value, error_fragment) in [
        ("role", json!("unknown"), "role"),
        ("role", Value::Null, "role"),
        (
            "schema_version",
            json!("cloudmcp.conversation-event.v2"),
            "unsupported CloudMCP event schema",
        ),
    ] {
        let tmp = TempDir::new()?;
        let mut duplicate = event(2, "e1", "tool", "output");
        duplicate
            .as_object_mut()
            .context("event fixture must be an object")?
            .insert(field.to_owned(), value);
        let ctx = fixture(tmp.path(), &[event(1, "e1", "tool", "output"), duplicate])?;
        assert_raw_pair_retained_before_parse(&ctx)?;
        let err = CloudMcpConnector::new()
            .scan(&ctx)
            .err()
            .context("invalid duplicate event was silently discarded")?;
        ensure!(err.to_string().contains(error_fragment));
    }
    let tmp = TempDir::new()?;
    let ctx = fixture(tmp.path(), &[event(1, "", "tool", "output")])?;
    assert_raw_pair_retained_before_parse(&ctx)?;
    let err = CloudMcpConnector::new()
        .scan(&ctx)
        .err()
        .context("empty source event ID was accepted")?;
    ensure!(err.to_string().contains("source event ID missing"));
    Ok(())
}

#[test]
fn unknown_event_fields_remain_in_normalized_historical_evidence() -> Result<()> {
    let tmp = TempDir::new()?;
    let mut row = event(1, "e1", "tool", "output");
    let unknown = json!({"native_payload":{"nested":[1,2,3]}});
    row.as_object_mut()
        .context("event fixture must be an object")?
        .insert("unknown_future_field".to_owned(), unknown.clone());
    let ctx = fixture(tmp.path(), &[row])?;
    assert_raw_pair_retained_before_parse(&ctx)?;
    let conversations = CloudMcpConnector::new().scan(&ctx)?;
    let conversation = conversations.first().context("conversation missing")?;
    let message = conversation.messages.first().context("message missing")?;
    ensure!(message.extra.get("unknown_future_field") == Some(&unknown));
    ensure!(
        message.extra.get("session_id").and_then(Value::as_str) == Some("native-client-session")
    );
    ensure!(
        conversation
            .metadata
            .get("authority")
            .and_then(Value::as_str)
            == Some("historical_evidence_only")
    );
    ensure!(
        conversation
            .metadata
            .get("all_transcript_complete")
            .and_then(Value::as_bool)
            == Some(false)
    );
    Ok(())
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
    let original_events = vec![
        event(1, "u1", "user", "cloudmcphandoffneedle exact workspace"),
        event(2, "a1", "assistant", "Reviewed history remains scoped"),
    ];
    fixture(&source, &original_events)?;
    let manifest_path = source.join("manifest.json");
    let transcript_path = source.join("transcript.jsonl");
    let original_bytes = (fs::read(&manifest_path)?, fs::read(&transcript_path)?);
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
    let indexed_session = || -> Result<Value> {
        let output = command()?
            .args([
                "sessions",
                "--exact-workspace",
                "/work/cloudmcp",
                "--exact-source-id",
                "local",
                "--exact-agent",
                "cloudmcp",
                "--exact-external-id",
                "chat-session-1",
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
        let body: Value = serde_json::from_slice(&output.stdout)?;
        let sessions = body
            .get("sessions")
            .and_then(Value::as_array)
            .context("exact sessions response has no sessions array")?;
        ensure!(sessions.len() == 1, "exact native identity was duplicated");
        let session = sessions.first().context("exact session missing")?;
        ensure!(session["external_id"] == "chat-session-1");
        ensure!(session["source_id"] == "local");
        ensure!(session["agent"] == "cloudmcp");
        ensure!(session["workspace"] == "/work/cloudmcp");
        ensure!(session["path"] == transcript_path.display().to_string());
        ensure!(session["source_path_session_count"] == 1);
        ensure!(session["human_turns"] == 1);
        ensure!(session["coverage_provenance"]["all_transcript_complete"] == false);
        ensure!(session["coverage_provenance"]["coverage_source"].is_null());
        Ok(session.clone())
    };
    for _ in 0..2 {
        command()?
            .args(["index", "--watch-once"])
            .arg(&manifest_path)
            .args(["--data-dir"])
            .arg(&data)
            .arg("--json")
            .assert()
            .success();
    }
    let original_session = indexed_session()?;
    ensure!(original_session["message_count"] == 2);
    ensure!(
        original_session["coverage_provenance"]["transcript_sha256"] == digest(&original_bytes.1)
    );
    ensure!(
        (fs::read(&manifest_path)?, fs::read(&transcript_path)?) == original_bytes,
        "native replay changed original publication bytes"
    );

    let mut grown_events = original_events.clone();
    grown_events.push(event(
        3,
        "t1",
        "tool",
        "原始 native append retains provenance",
    ));
    fixture(&source, &grown_events)?;
    let grown_bytes = (fs::read(&manifest_path)?, fs::read(&transcript_path)?);
    for _ in 0..2 {
        command()?
            .args(["index", "--watch-once"])
            .arg(&manifest_path)
            .arg("--data-dir")
            .arg(&data)
            .arg("--json")
            .assert()
            .success();
    }
    let grown_session = indexed_session()?;
    ensure!(grown_session["conversation_id"] == original_session["conversation_id"]);
    ensure!(grown_session["message_count"] == 3);
    ensure!(grown_session["content_hash"] != original_session["content_hash"]);
    ensure!(grown_session["coverage_provenance"]["transcript_sha256"] == digest(&grown_bytes.1));
    ensure!(
        (fs::read(&manifest_path)?, fs::read(&transcript_path)?) == grown_bytes,
        "append replay changed original publication bytes"
    );

    fixture(&source, &original_events)?;
    command()?
        .args(["index", "--watch-once"])
        .arg(&manifest_path)
        .arg("--data-dir")
        .arg(&data)
        .arg("--json")
        .assert()
        .success();
    let after_stale = indexed_session()?;
    ensure!(after_stale["conversation_id"] == grown_session["conversation_id"]);
    ensure!(after_stale["message_count"] == grown_session["message_count"]);
    ensure!(after_stale["content_hash"] == grown_session["content_hash"]);
    ensure!(
        after_stale["coverage_provenance"]["transcript_sha256"]
            == grown_session["coverage_provenance"]["transcript_sha256"],
        "older publication replaced current provenance"
    );
    ensure!(
        (fs::read(&manifest_path)?, fs::read(&transcript_path)?) == original_bytes,
        "stale replay changed source publication bytes"
    );
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

#[test]
fn unsupported_manifest_is_quarantined_at_its_publication_path() -> Result<()> {
    use coding_agent_search::connector_ingest_diagnostics::{
        ConnectorIngestRun, IngestFailureKind, SourceIngestDisposition, UnsupportedSourceSchema,
    };

    let tmp = TempDir::new()?;
    let content = "private manifest fixture content must stay in raw evidence";
    let original_event = event(1, "native-manifest-event", "tool", content);
    let ctx = fixture(tmp.path(), std::slice::from_ref(&original_event))?;
    let manifest_path = tmp.path().join("manifest.json");
    let transcript_path = tmp.path().join("transcript.jsonl");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    manifest["schema_version"] = json!("cloudmcp.conversation-manifest.v2");
    fs::write(&manifest_path, manifest.to_string())?;
    let manifest_bytes = fs::read(&manifest_path)?;
    let transcript_bytes = fs::read(&transcript_path)?;
    let connector = CloudMcpConnector::new();
    let sources = connector.discover_source_files(&ctx)?;
    assert_raw_pair_retained_before_parse(&ctx)?;

    let error = connector
        .scan(&ctx)
        .err()
        .context("unsupported manifest was accepted")?
        .context("fixture connector scan failed");
    let unsupported = error
        .downcast_ref::<UnsupportedSourceSchema>()
        .context("schema marker was lost through context wrapping")?;
    let actual_path = fs::canonicalize(&manifest_path)?;
    ensure!(unsupported.source_path() == actual_path.as_path());
    ensure!(unsupported.line_or_row().is_none());
    let mut run = ConnectorIngestRun::begin("cloudmcp", &ctx.data_dir, &ctx, &sources);
    let fallback = tmp.path().join("unrelated-cass-data");
    run.observe_connector_scan_error(&fallback, &error);
    let report = run.finish();
    ensure!(report.provider == "cloudmcp");
    ensure!(report.summary.total() == 2);
    ensure!(report.summary.quarantined == 1 && report.summary.discovered == 1);
    ensure!(report.summary.skipped == 0 && report.summary.with_content() == 0);
    ensure!(report.diagnostics.len() == 1);
    let diagnostic = report.diagnostics.first().context("diagnostic missing")?;
    ensure!(diagnostic.source_path == actual_path.display().to_string());
    ensure!(diagnostic.source_path != fallback.display().to_string());
    ensure!(diagnostic.failure_kind == IngestFailureKind::UnsupportedSchema);
    ensure!(diagnostic.disposition == SourceIngestDisposition::Quarantined);
    ensure!(diagnostic.disposition.is_explicit_limitation());
    ensure!(!diagnostic.retryable && diagnostic.line_or_row.is_none());
    ensure!(diagnostic.external_id.is_none() && diagnostic.canonical_id.is_none());
    ensure!(!serde_json::to_string(&report)?.contains(content));
    ensure!(fs::read(&manifest_path)? == manifest_bytes);
    ensure!(fs::read(&transcript_path)? == transcript_bytes);
    let retained_event: Value = serde_json::from_slice(&transcript_bytes)?;
    ensure!(retained_event == original_event);
    ensure!(retained_event["session_id"] == "native-client-session");
    ensure!(retained_event["agent_id"] == "cloudmcp-client");
    Ok(())
}

#[test]
fn unsupported_event_is_quarantined_at_its_physical_transcript_line() -> Result<()> {
    use coding_agent_search::connector_ingest_diagnostics::{
        ConnectorIngestRun, IngestFailureKind, SourceIngestDisposition, UnsupportedSourceSchema,
    };

    let tmp = TempDir::new()?;
    let content = "private future event bytes remain available for a supporting adapter";
    let first = event(1, "native-first-event", "user", "accepted schema");
    let mut future = event(2, "native-future-event", "assistant", content);
    future["schema_version"] = json!("cloudmcp.conversation-event.v2");
    future["future_native_metadata"] = json!({"opaque_identity":"preserve-exactly"});
    let ctx = fixture(tmp.path(), &[first.clone(), future.clone()])?;
    let manifest_path = tmp.path().join("manifest.json");
    let transcript_path = tmp.path().join("transcript.jsonl");
    let transcript_bytes = format!("{first}\n\n{future}\n").into_bytes();
    fs::write(&transcript_path, &transcript_bytes)?;
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    manifest["transcript_sha256"] = json!(digest(&transcript_bytes));
    fs::write(&manifest_path, manifest.to_string())?;
    let manifest_bytes = fs::read(&manifest_path)?;
    let connector = CloudMcpConnector::new();
    let sources = connector.discover_source_files(&ctx)?;
    assert_raw_pair_retained_before_parse(&ctx)?;

    let error = connector
        .scan(&ctx)
        .err()
        .context("unsupported event was accepted")?
        .context("fixture connector scan failed");
    let unsupported = error
        .downcast_ref::<UnsupportedSourceSchema>()
        .context("event schema marker missing")?;
    let actual_path = fs::canonicalize(&transcript_path)?;
    ensure!(unsupported.source_path() == actual_path.as_path());
    ensure!(unsupported.line_or_row() == Some(3));
    let mut run = ConnectorIngestRun::begin("cloudmcp", &ctx.data_dir, &ctx, &sources);
    let fallback = tmp.path().join("unrelated-cass-data");
    run.observe_connector_scan_error(&fallback, &error);
    let report = run.finish();
    ensure!(report.summary.total() == 2);
    ensure!(report.summary.quarantined == 1 && report.summary.discovered == 1);
    ensure!(report.summary.skipped == 0 && report.summary.with_content() == 0);
    ensure!(report.diagnostics.len() == 1);
    let diagnostic = report.diagnostics.first().context("diagnostic missing")?;
    ensure!(diagnostic.source_path == actual_path.display().to_string());
    ensure!(diagnostic.failure_kind == IngestFailureKind::UnsupportedSchema);
    ensure!(diagnostic.disposition == SourceIngestDisposition::Quarantined);
    ensure!(!diagnostic.retryable && diagnostic.line_or_row == Some(3));
    ensure!(diagnostic.external_id.is_none() && diagnostic.canonical_id.is_none());
    ensure!(!serde_json::to_string(&report)?.contains(content));
    ensure!(fs::read(&manifest_path)? == manifest_bytes);
    ensure!(fs::read(&transcript_path)? == transcript_bytes);
    let retained_events: Vec<Value> = std::str::from_utf8(&transcript_bytes)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    ensure!(retained_events == vec![first, future]);
    ensure!(retained_events[1]["session_id"] == "native-client-session");
    ensure!(retained_events[1]["agent_id"] == "cloudmcp-client");
    ensure!(retained_events[1]["source_event_id"] == "native-future-event");
    ensure!(retained_events[1]["future_native_metadata"]["opaque_identity"] == "preserve-exactly");
    Ok(())
}
