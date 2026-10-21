use anyhow::{Context, ensure};
use coding_agent_search::connectors::cline::ClineConnector;
use coding_agent_search::connectors::{Connector, DiscoveredSourceRole, ScanContext, ScanRoot};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tempfile::TempDir;

fn write_session(
    root: &Path,
    session_id: &str,
    envelope: &Value,
    sidecar: &Value,
) -> anyhow::Result<PathBuf> {
    let session_dir = root.join(session_id);
    fs::create_dir_all(&session_dir)?;
    let messages_path = session_dir.join(format!("{session_id}.messages.json"));
    fs::write(&messages_path, serde_json::to_vec(envelope)?)?;
    fs::write(
        session_dir.join(format!("{session_id}.json")),
        serde_json::to_vec(sidecar)?,
    )?;
    Ok(messages_path)
}

fn sample_envelope(session_id: &str) -> Value {
    json!({
        "version": 1,
        "updated_at": "2026-09-27T10:00:00Z",
        "agent": "cline-cli",
        "sessionId": session_id,
        "system_prompt": "preserved native header context",
        "future_header": {"retained": true},
        "messages": [
            {
                "id": "user-tool-result-1",
                "role": "user",
                "content": [
                    {"type": "text", "text": "before tool output"},
                    {
                        "type": "tool_result",
                        "tool_use_id": "tool-call-1",
                        "content": [{"type": "text", "text": "nested tool result payload"}]
                    },
                    {"type": "text", "text": "after tool output"}
                ],
                "ts": "2026-09-27T09:59:00Z",
                "future_message_field": {"retained": "user"}
            },
            {
                "id": "assistant-2",
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "assistant reply"},
                    {"type": "tool_use", "id": "tool-call-1", "name": "read_file", "input": {"path": "src/lib.rs"}}
                ],
                "ts": "2026-09-27T09:59:30Z",
                "modelInfo": {"provider": "test-provider", "model": "test-model"},
                "metrics": {"tokens": 12},
                "future_message_field": {"retained": "assistant"}
            }
        ]
    })
}

fn sample_sidecar(session_id: &str, workspace: &str) -> Value {
    json!({
        "session_id": session_id,
        "workspace_root": workspace,
        "title": "Native CLI session",
        "created_at": "2026-09-27T09:58:00Z",
        "future_sidecar_field": {"retained": true}
    })
}

fn explicit_context(root: &Path, selected: &Path) -> ScanContext {
    ScanContext::with_roots(
        root.join("archive"),
        vec![ScanRoot::local(selected.to_path_buf())],
        None,
    )
}

#[test]
fn cli_scan_preserves_native_envelope_messages_tool_results_and_replay_identity()
-> anyhow::Result<()> {
    let fixture = TempDir::new()?;
    let session_id = "cline-session-001";
    let workspace = "/work/native-cline";
    let envelope = sample_envelope(session_id);
    let sidecar = sample_sidecar(session_id, workspace);
    let source = write_session(fixture.path(), session_id, &envelope, &sidecar)?;
    let connector = ClineConnector::new();
    let context = explicit_context(fixture.path(), fixture.path());

    let first = connector.scan(&context)?;
    let replay = connector.scan(&context)?;
    ensure!(
        first.len() == 1 && replay.len() == 1,
        "one native session must yield one conversation on replay"
    );
    let conversation = first.first().context("missing Cline CLI conversation")?;
    let replayed = replay
        .first()
        .context("missing replayed Cline CLI conversation")?;
    ensure!(conversation.external_id.as_deref() == Some(session_id)); // ubs:ignore -- Public fixture native identity, not authentication material.
    ensure!(conversation.source_path == source); // ubs:ignore -- Read-only provenance path assertion, not a secret comparison.
    ensure!(conversation.workspace.as_deref() == Some(Path::new(workspace))); // ubs:ignore -- Public fixture workspace routing assertion.
    ensure!(conversation.agent_slug == "cline"); // ubs:ignore -- Public connector label assertion.
    ensure!(conversation.messages.len() == 2);
    ensure!(conversation.messages[0].idx == 0 && conversation.messages[1].idx == 1);
    ensure!(conversation.messages[0].role == "user"); // ubs:ignore -- Public fixture role, vector length checked above.
    ensure!(conversation.messages[0].extra == envelope["messages"][0]); // ubs:ignore -- Raw synthetic message round-trip, not a credential verifier.
    ensure!(conversation.messages[1].role == "assistant"); // ubs:ignore -- Public fixture role, vector length checked above.
    ensure!(conversation.messages[1].extra == envelope["messages"][1]); // ubs:ignore -- Raw synthetic message round-trip, not a credential verifier.
    ensure!(
        conversation.messages[0]
            .content
            .contains("nested tool result payload")
    );
    let first_message = conversation
        .messages
        .first()
        .context("missing first native message")?;
    let content = &first_message.content;
    ensure!(content.find("before tool output") < content.find("nested tool result payload"));
    ensure!(content.find("nested tool result payload") < content.find("after tool output"));
    ensure!(conversation.messages[1].content.contains("assistant reply"));
    ensure!(conversation.messages[1].invocations.len() == 1);
    ensure!(conversation.metadata["native_header"]["version"] == envelope["version"]); // ubs:ignore -- Synthetic header version round-trip.
    ensure!(conversation.metadata["native_header"]["agent"] == envelope["agent"]); // ubs:ignore -- Public harness identity round-trip.
    ensure!(conversation.metadata["native_header"]["updated_at"] == envelope["updated_at"]); // ubs:ignore -- Public fixture timestamp round-trip.
    ensure!(conversation.metadata["native_header"]["system_prompt"] == envelope["system_prompt"]); // ubs:ignore -- Synthetic prompt round-trip, not authentication.
    ensure!(conversation.metadata["native_header"]["future_header"] == envelope["future_header"]); // ubs:ignore -- Unknown synthetic header field retention assertion.
    ensure!(conversation.metadata["native_session_metadata"] == sidecar); // ubs:ignore -- Synthetic sidecar retention assertion.
    ensure!(conversation.metadata["source_read_complete"] == true);
    ensure!(
        conversation
            .metadata
            .get("all_transcript_complete")
            .is_none()
    );
    ensure!(conversation.metadata["transcript_finality"] == "unknown"); // ubs:ignore -- Public coverage label assertion.
    ensure!(replayed.external_id == conversation.external_id); // ubs:ignore -- Public native identity must stay stable on replay.
    ensure!(replayed.source_path == conversation.source_path); // ubs:ignore -- Provenance path must stay stable on replay.
    ensure!(replayed.messages.len() == conversation.messages.len());
    for (actual, replayed) in conversation.messages.iter().zip(&replayed.messages) {
        ensure!(actual.idx == replayed.idx);
        ensure!(actual.role == replayed.role);
        ensure!(actual.extra == replayed.extra);
    }

    let discovered = connector.discover_source_files(&context)?;
    ensure!(
        discovered
            .iter()
            .filter(|source| source.role == DiscoveredSourceRole::PrimarySessionLog) // ubs:ignore -- Public source-role enum, not credentials.
            .filter(|source| source.source_path == conversation.source_path) // ubs:ignore -- Public source path must be discovered exactly once.
            .count()
            == 1,
        "native message source must be discovered exactly once"
    );
    ensure!(discovered.iter().any(|source| {
        source.role == DiscoveredSourceRole::MetadataSidecar // ubs:ignore -- Public source-role enum, not credentials.
            && source
                .source_path
                .file_name()
                .is_some_and(|name| name == "cline-session-001.json")
    }));
    Ok(())
}

#[test]
fn cline_api_history_retains_tool_results_in_native_block_order() -> anyhow::Result<()> {
    let fixture = TempDir::new()?;
    let task_id = "api-history-tool-results";
    let task = fixture
        .path()
        .join("saoudrizwan.claude-dev/tasks")
        .join(task_id);
    fs::create_dir_all(&task)?;
    let api_history = json!([
        {
            "id": "mixed-content",
            "role": "user",
            "content": [
                {"type": "text", "text": "before native result"},
                {
                    "type": "tool_result",
                    "tool_use_id": "native-tool-call",
                    "content": [{"type": "text", "text": "native tool result"}]
                },
                {"type": "text", "text": "after native result"}
            ],
            "ts": 1000,
            "native_extra": {"preserve": ["all", "fields"]}
        },
        {
            "id": "tool-result-only",
            "role": "tool",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "native-tool-call-2",
                "content": [{"type": "text", "text": "tool-only native result"}]
            }],
            "ts": 2000,
            "native_extra": {"preserve": true}
        }
    ]);
    fs::write(
        task.join("api_conversation_history.json"),
        serde_json::to_vec(&api_history)?,
    )?;

    let context = ScanContext::with_roots(
        fixture.path().join("archive"),
        vec![ScanRoot::local(
            fixture.path().join("saoudrizwan.claude-dev"),
        )],
        None,
    );
    let conversations = ClineConnector::new().scan(&context)?;
    ensure!(conversations.len() == 1);
    let conversation = conversations.first().context("missing API history")?;
    ensure!(conversation.messages.len() == 2);
    ensure!(conversation.messages[0].extra == api_history[0]); // ubs:ignore -- Synthetic native message retention; count checked above.
    ensure!(conversation.messages[1].extra == api_history[1]); // ubs:ignore -- Synthetic native message retention; count checked above.
    let first_message = conversation
        .messages
        .first()
        .context("missing first API message")?;
    let content = &first_message.content;
    ensure!(content.find("before native result") < content.find("native tool result"));
    ensure!(content.find("native tool result") < content.find("after native result"));
    ensure!(
        conversation.messages[1]
            .content
            .contains("tool-only native result")
    );
    ensure!(conversation.messages[0].idx == 0 && conversation.messages[1].idx == 1);
    ensure!(conversation.messages[0].role == "user"); // ubs:ignore -- Synthetic native role; count checked above.
    ensure!(conversation.messages[1].role == "tool"); // ubs:ignore -- Synthetic native role; count checked above.
    Ok(())
}

#[test]
fn cli_explicit_root_isolation_and_overlapping_roots_do_not_duplicate_sessions()
-> anyhow::Result<()> {
    let fixture = TempDir::new()?;
    let sessions = fixture.path().join("sessions");
    let selected_id = "selected-session";
    let sibling_id = "sibling-session";
    let selected = write_session(
        &sessions,
        selected_id,
        &sample_envelope(selected_id),
        &sample_sidecar(selected_id, "/work/selected"),
    )?;
    let sibling = write_session(
        &sessions,
        sibling_id,
        &sample_envelope(sibling_id),
        &sample_sidecar(sibling_id, "/work/sibling"),
    )?;
    let mut context = explicit_context(fixture.path(), &selected);
    context.scan_roots.push(ScanRoot::local(selected.clone()));
    let conversations = ClineConnector::new().scan(&context)?;
    ensure!(conversations.len() == 1);
    ensure!(conversations[0].external_id.as_deref() == Some(selected_id)); // ubs:ignore -- Public fixture session identity; count checked above.
    ensure!(conversations[0].source_path == selected); // ubs:ignore -- Explicit-source routing assertion.
    ensure!(conversations.iter().all(|item| item.source_path != sibling)); // ubs:ignore -- Adjacent fixture source must remain outside the selected scope.
    Ok(())
}

#[test]
fn cli_default_root_override_is_discovered_without_importing_host_defaults() -> anyhow::Result<()> {
    static ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());
    struct RestoreClineRoot(Option<std::ffi::OsString>);
    impl Drop for RestoreClineRoot {
        fn drop(&mut self) {
            // The test serializes its own environment mutation and restores the inherited value.
            unsafe {
                match &self.0 {
                    Some(value) => std::env::set_var("CASS_CLINE_DATA_ROOT", value),
                    None => std::env::remove_var("CASS_CLINE_DATA_ROOT"),
                }
            }
        }
    }

    let _lock = ENVIRONMENT_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("Cline root environment lock was poisoned by another test"))?;
    let previous = std::env::var_os("CASS_CLINE_DATA_ROOT");
    let _restore = RestoreClineRoot(previous);
    let fixture = TempDir::new()?;
    let session_id = "default-root-session";
    write_session(
        fixture.path(),
        session_id,
        &sample_envelope(session_id),
        &sample_sidecar(session_id, "/work/default-root"),
    )?;
    // SAFETY: this integration-test binary serializes access to this variable and restores it on drop.
    unsafe { std::env::set_var("CASS_CLINE_DATA_ROOT", fixture.path()) };

    let context = ScanContext::local_default(fixture.path().to_path_buf(), None);
    let connector = ClineConnector::new();
    let conversations = connector.scan(&context)?;
    ensure!(
        conversations.len() == 1,
        "expected one configured native source; found {:?}",
        conversations
            .iter()
            .map(|item| (
                item.external_id.as_deref(),
                item.source_path.file_name().and_then(|name| name.to_str())
            ))
            .collect::<Vec<_>>()
    );
    ensure!(conversations[0].external_id.as_deref() == Some(session_id)); // ubs:ignore -- Public fixture session identity; count checked above.
    let detection = connector.detect();
    ensure!(detection.detected);
    ensure!(detection.root_paths.contains(&fixture.path().to_path_buf()));
    Ok(())
}

#[test]
fn cli_malformed_truncated_and_identity_mismatches_fail_closed() -> anyhow::Result<()> {
    let fixture = TempDir::new()?;
    let session_id = "invalid-session";
    let source = write_session(
        fixture.path(),
        session_id,
        &sample_envelope(session_id),
        &sample_sidecar("different-session", "/work/invalid"),
    )?;
    let connector = ClineConnector::new();
    let context = explicit_context(fixture.path(), &source);
    ensure!(
        connector.scan(&context).is_err(),
        "mismatched sidecar identity must be rejected"
    );

    fs::write(&source, b"{\"messages\":[")?;
    ensure!(
        connector.scan(&context).is_err(),
        "truncated native JSON must be rejected"
    );

    let mismatch = json!({
        "sessionId": "other-session",
        "messages": []
    });
    let mismatch_source = write_session(
        fixture.path(),
        "path-session",
        &mismatch,
        &json!({"session_id": "other-session"}),
    )?;
    ensure!(
        connector
            .scan(&explicit_context(fixture.path(), &mismatch_source))
            .is_err(),
        "native session identity must match the directory and filename"
    );
    Ok(())
}

#[test]
fn cli_oversize_native_source_is_rejected_before_json_parsing() -> anyhow::Result<()> {
    let fixture = TempDir::new()?;
    let session_id = "oversize-session";
    let source = write_session(
        fixture.path(),
        session_id,
        &sample_envelope(session_id),
        &sample_sidecar(session_id, "/work/oversize"),
    )?;
    let file = fs::OpenOptions::new().write(true).open(&source)?;
    file.set_len(100 * 1024 * 1024 + 1)?;
    ensure!(
        ClineConnector::new()
            .scan(&explicit_context(fixture.path(), &source))
            .is_err(),
        "native source above the 100 MiB bound must fail closed"
    );
    Ok(())
}

#[test]
#[ignore = "set CASS_CLINE_VERIFY_SOURCE to a native Cline CLI messages file for a read-only live comparison"]
fn cli_live_source_verifier_matches_every_native_record_without_outputting_bodies()
-> anyhow::Result<()> {
    let source = PathBuf::from(
        dotenvy::var("CASS_CLINE_VERIFY_SOURCE")
            .context("CASS_CLINE_VERIFY_SOURCE must name one native Cline CLI message file")?,
    );
    let source_before = fs::read(&source)?;
    let envelope: Value = serde_json::from_slice(&source_before)?;
    let object = envelope
        .as_object()
        .context("live Cline CLI source must be a JSON object")?;
    let native_messages = object
        .get("messages")
        .and_then(Value::as_array)
        .context("live Cline CLI source is missing its messages array")?;
    let session_id = object
        .get("sessionId")
        .or_else(|| object.get("session_id"))
        .and_then(Value::as_str)
        .context("live Cline CLI source is missing its native session identifier")?;
    let sidecar_path = source.with_file_name(format!("{session_id}.json"));
    let sidecar_before = sidecar_path
        .is_file()
        .then(|| fs::read(&sidecar_path))
        .transpose()?;
    let sidecar = sidecar_before
        .as_deref()
        .map(serde_json::from_slice::<Value>)
        .transpose()?;

    let conversations = ClineConnector::new().scan(&ScanContext::with_roots(
        source
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf(),
        vec![ScanRoot::local(source.clone())],
        None,
    ))?;
    ensure!(conversations.len() == 1);
    let conversation = conversations
        .first()
        .context("live native Cline source produced no conversation")?;
    ensure!(conversation.source_path == source); // ubs:ignore -- Exact provenance path, not a secret comparison.
    ensure!(conversation.external_id.as_deref() == Some(session_id)); // ubs:ignore -- Native session identity, not a bearer secret.
    ensure!(conversation.messages.len() == native_messages.len());
    for (actual, native) in conversation.messages.iter().zip(native_messages) {
        ensure!(
            actual.extra == *native,
            "a native Cline message field changed during normalization"
        );
    }
    let mut expected_header = envelope;
    expected_header
        .as_object_mut()
        .context("live Cline CLI source must remain an object")?
        .remove("messages");
    ensure!(conversation.metadata["native_header"] == expected_header); // ubs:ignore -- Verifies raw header preservation without printing it; not authentication.
    if let (Some(expected), Some(actual)) = (
        sidecar.as_ref(),
        conversation.metadata.get("native_session_metadata"),
    ) {
        ensure!(actual == expected);
    }
    if let Some(metadata) = sidecar.as_ref()
        && let Some(workspace) = [
            "workspace_root",
            "workspaceHome",
            "rootPath",
            "cwd",
            "workspace",
            "cwdOnTaskInitialization",
        ]
        .iter()
        .find_map(|key| metadata.get(*key).and_then(Value::as_str))
        .filter(|path| !path.trim().is_empty())
    {
        ensure!(conversation.workspace.as_deref() == Some(Path::new(workspace))); // ubs:ignore -- Native workspace provenance verification.
    }
    ensure!(
        fs::read(&source)? == source_before, // ubs:ignore -- Detects any source mutation during a read-only verifier; not authentication.
        "native Cline source changed during verification"
    );
    if let Some(before) = sidecar_before {
        ensure!(
            fs::read(&sidecar_path)? == before, // ubs:ignore -- Detects source-sidecar mutation without exposing file bytes.
            "Cline sidecar changed during verification"
        );
    }
    Ok(())
}
