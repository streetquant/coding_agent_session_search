use anyhow::{Context, Result, ensure};
use coding_agent_search::connectors::openwebui::OpenWebUiConnector;
use coding_agent_search::connectors::{Connector, ScanContext, ScanRoot};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn json_bytes(value: &Value) -> Vec<u8> {
    let mut canonical = value.clone();
    canonical.sort_all_objects();
    format!("{canonical}\n").into_bytes()
}

struct Fixture {
    temporary: TempDir,
    directory: PathBuf,
    snapshot: Value,
    catalog: Value,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temporary = TempDir::new()?;
        let directory = temporary.path().join("openwebui/v1/instance");
        fs::create_dir_all(&directory)?;
        let instance = json!({
            "schema_version": "openwebui.capture-instance.v1", "instance_id": "original-instance",
            "workspace_root": "/work/webui", "source_database": "/work/webui/webui.db"
        });
        fs::write(directory.join("instance.json"), json_bytes(&instance))?;
        let source = json!({
            "chat": {"id": "original-chat", "user_id": "original-user", "title": "Source title", "created_at": 1700000000},
            "chat_message": [{"id": "original-chat-user-turn", "chat_id": "original-chat", "user_id": "original-user"}],
            "chat_file": []
        });
        let snapshot = json!({
            "schema_version": "openwebui.saved-conversation.v1", "agent": "openwebui",
            "instance_id": "original-instance", "conversation_id": "original-chat", "user_id": "original-user",
            "workspace_root": "/work/webui", "source_database": "/work/webui/webui.db",
            "source_rows_sha256": digest(&json_bytes(&source)), "source": source,
            "coverage": {"capture": "saved_database_snapshot", "conversation_complete": null, "projection": "ready"},
            "projection": {
                "current_message_id": "assistant-turn", "issues": [], "retained_legacy_only_ids": ["old-branch"],
                "messages": [
                    {"id": "user-turn", "origin": {"table": "chat_message", "row_id": "original-chat-user-turn"}, "completion": "reported_done",
                     "message": {"id": "user-turn", "role": "user", "content": "webuihistoryneedle original request", "timestamp": 1700000001}},
                    {"id": "assistant-turn", "origin": {"table": "chat_message", "row_id": "original-chat-assistant-turn"}, "completion": "error",
                     "message": {"id": "assistant-turn", "role": "assistant", "parentId": "user-turn", "model": "original-model", "timestamp": 1700000002000_i64,
                       "content": [{"type": "text", "text": "partial response"}], "done": false,
                       "error": {"message": "output limit"}, "output": [
                         {"type": "function_call", "name": "run_command", "arguments": {"argv": ["echo", "toolargumentneedle"]}},
                         {"type": "function_call_output", "output": "toolresultneedle"}
                       ]}}
                ]
            }
        });
        let session_key = digest(&json_bytes(&json!("original-chat")));
        let catalog = json!({
            "schema_version": "openwebui.capture-catalog.v1", "instance_id": "original-instance", "workspace_root": "/work/webui",
            "sessions": [{"session_key": session_key, "conversation_id": "original-chat", "user_id": "original-user", "projection": "ready", "content_sha256": ""}],
            "retained_missing": []
        });
        let mut fixture = Self {
            temporary,
            directory,
            snapshot,
            catalog,
        };
        fixture.publish()?;
        Ok(fixture)
    }

    fn session_directory(&self) -> Result<PathBuf> {
        let key = self
            .catalog
            .pointer("/sessions/0/session_key")
            .and_then(Value::as_str)
            .context("fixture session key")?;
        Ok(self.directory.join("sessions").join(key))
    }

    fn publish(&mut self) -> Result<()> {
        let session = self.session_directory()?;
        fs::create_dir_all(session.join("revisions"))?;
        let bytes = json_bytes(&self.snapshot);
        let hash = digest(&bytes);
        self.catalog["sessions"][0]["content_sha256"] = json!(hash);
        fs::write(
            session.join("revisions").join(format!("{hash}.json")),
            &bytes,
        )?;
        fs::write(session.join("snapshot.json"), &bytes)?;
        self.publish_catalog()
    }

    fn publish_catalog(&self) -> Result<()> {
        fs::write(
            self.directory.join("catalog.json"),
            json_bytes(&self.catalog),
        )?;
        Ok(())
    }

    fn context(&self) -> ScanContext {
        ScanContext::with_roots(
            self.temporary.path().to_path_buf(),
            vec![ScanRoot::local(self.directory.clone())],
            None,
        )
    }

    fn command(&self) -> Result<assert_cmd::Command> {
        let mut command = assert_cmd::Command::cargo_bin("cass")?;
        command
            .current_dir(self.temporary.path())
            .env("HOME", self.temporary.path())
            .env("CODEX_HOME", self.temporary.path().join("codex"))
            .env("XDG_CONFIG_HOME", self.temporary.path().join("config"))
            .env("XDG_DATA_HOME", self.temporary.path().join("xdg"))
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_AUTO_REFRESH", "0")
            .env(
                "CASS_OPENWEBUI_DATA_ROOT",
                self.directory.parent().context("fixture root")?,
            );
        Ok(command)
    }

    fn index(&self, archive: &Path) -> Result<Value> {
        let output = self
            .command()?
            .args(["index", "--watch-once"])
            .arg(self.directory.join("catalog.json"))
            .arg("--data-dir")
            .arg(archive)
            .arg("--json")
            .output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    fn lexical_hits(&self, archive: &Path, needle: &str) -> Result<Vec<Value>> {
        let output = self
            .command()?
            .args([
                "search",
                needle,
                "--agent",
                "openwebui",
                "--mode",
                "lexical",
                "--data-dir",
            ])
            .arg(archive)
            .arg("--json")
            .output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout)?;
        Ok(result["hits"].as_array().context("lexical hits")?.clone())
    }
}

#[test]
fn native_identity_tools_branches_and_incomplete_status_survive() -> Result<()> {
    let fixture = Fixture::new()?;
    let conversations = OpenWebUiConnector::new().scan(&fixture.context())?;
    ensure!(conversations.len() == 1);
    let conversation = conversations.first().context("missing conversation")?;
    ensure!(conversation.agent_slug == "openwebui");
    ensure!(
        conversation.external_id.as_deref() == Some("[\"original-instance\",\"original-chat\"]")
    );
    ensure!(conversation.workspace.as_deref() == Some(Path::new("/work/webui")));
    ensure!(conversation.messages.len() == 2);
    ensure!(conversation.ended_at.is_none());
    ensure!(conversation.metadata["all_transcript_complete"] == false);
    ensure!(conversation.metadata["retained_legacy_only_ids"] == json!(["old-branch"]));
    let assistant = conversation
        .messages
        .get(1)
        .context("missing assistant message")?;
    ensure!(assistant.created_at == Some(1700000002000));
    ensure!(assistant.author.as_deref() == Some("original-model"));
    ensure!(
        assistant.content.contains("toolargumentneedle")
            && assistant.content.contains("toolresultneedle")
    );
    ensure!(assistant.extra["openwebui"]["completion"] == "error");
    ensure!(assistant.extra["openwebui"]["message"]["parentId"] == "user-turn");
    let replay = OpenWebUiConnector::new().scan(&fixture.context())?;
    ensure!(replay.first().context("missing replay")?.external_id == conversation.external_id);
    ensure!(
        replay.first().context("missing replay")?.metadata["content_hash"]
            == conversation.metadata["content_hash"]
    );
    Ok(())
}

#[test]
fn discovery_tracks_only_current_committed_revision_and_metadata() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.snapshot["projection"]["messages"][0]["message"]["content"] = json!("changed");
    fixture.publish()?;
    let files = OpenWebUiConnector::new().discover_source_files(&fixture.context())?;
    ensure!(files.len() == 4);
    ensure!(
        files
            .iter()
            .all(|file| file.provider_slug == "openwebui" && file.required_for_reconstruction)
    );
    let snapshot = fixture.session_directory()?.join("snapshot.json");
    let context = ScanContext::with_roots(
        fixture.temporary.path().to_path_buf(),
        vec![ScanRoot::local(snapshot)],
        None,
    );
    ensure!(OpenWebUiConnector::new().scan(&context)?.len() == 1);
    Ok(())
}

#[test]
fn changed_message_keeps_original_session_identity() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let previous = OpenWebUiConnector::new().scan(&fixture.context())?;
    fixture.snapshot["projection"]["messages"][0]["message"]["content"] =
        json!("updated-only-needle");
    fixture.publish()?;
    let current = OpenWebUiConnector::new().scan(&fixture.context())?;
    let previous = previous.first().context("previous")?;
    let current = current.first().context("current")?;
    ensure!(
        previous.external_id == current.external_id && previous.source_path == current.source_path
    );
    ensure!(previous.metadata["content_hash"] != current.metadata["content_hash"]);
    ensure!(current.messages.first().context("message")?.content == "updated-only-needle");
    Ok(())
}

#[test]
fn python_capture_unicode_hash_contract_matches_native_reader() -> Result<()> {
    let mut fixture = Fixture::new()?;
    ensure!(
        fixture.snapshot["source_rows_sha256"]
            == "d929ffea37bf329c9bf5fc4a8e1288eb9ef1f7b8c68aca3a6130ed944129be4e"
    );
    fixture.snapshot["source"]["chat"]["title"] = json!("unicode é 🚀 \u{7f}");
    fixture.snapshot["source_rows_sha256"] =
        json!("f502b335d03107c78ddbd9b92af18eb8a62917516d30adec6e677dcbb682d9fe");
    fixture.publish()?;
    let conversations = OpenWebUiConnector::new().scan(&fixture.context())?;
    ensure!(
        conversations
            .first()
            .context("unicode conversation")?
            .title
            .as_deref()
            == Some("unicode é 🚀 \u{7f}")
    );
    Ok(())
}

#[test]
fn rejects_cross_workspace_owner_schema_and_completion_claims() -> Result<()> {
    let cases = [
        ("/workspace_root", json!("/another/workspace")),
        ("/user_id", json!("another-user")),
        ("/agent", json!("codex")),
        ("/source_database", json!("/another/database")),
        ("/schema_version", json!("openwebui.saved-conversation.v99")),
        ("/coverage/conversation_complete", json!(true)),
        ("/source_rows_sha256", json!("incorrect")),
        ("/projection/messages/1/id", json!("user-turn")),
    ];
    for (pointer, value) in cases {
        let mut fixture = Fixture::new()?;
        *fixture
            .snapshot
            .pointer_mut(pointer)
            .context("fixture pointer")? = value;
        fixture.publish()?;
        ensure!(
            OpenWebUiConnector::new().scan(&fixture.context()).is_err(),
            "accepted {pointer}"
        );
    }
    Ok(())
}

#[test]
fn rejects_corrupt_and_uncommitted_snapshot_bytes() -> Result<()> {
    let fixture = Fixture::new()?;
    fs::write(
        fixture.session_directory()?.join("snapshot.json"),
        "unfinished publication",
    )?;
    let error = OpenWebUiConnector::new()
        .scan(&fixture.context())
        .err()
        .context("accepted uncommitted snapshot")?;
    ensure!(error.to_string().contains("publication in progress"));
    Ok(())
}

#[test]
fn rejects_quarantined_and_path_traversal_catalog_entries() -> Result<()> {
    for (field, value) in [
        ("projection", "quarantined"),
        ("session_key", "../../escape"),
        ("content_sha256", "../../escape"),
    ] {
        let mut fixture = Fixture::new()?;
        fixture.catalog["sessions"][0][field] = json!(value);
        fixture.publish_catalog()?;
        ensure!(
            OpenWebUiConnector::new().scan(&fixture.context()).is_err(),
            "accepted {field}"
        );
    }
    Ok(())
}

#[test]
fn retained_missing_sources_stay_marked_absent() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.catalog["retained_missing"] = fixture.catalog["sessions"].clone();
    fixture.catalog["sessions"] = json!([]);
    fixture.publish_catalog()?;
    let conversations = OpenWebUiConnector::new().scan(&fixture.context())?;
    ensure!(conversations.first().context("retained session")?.metadata["source_present"] == false);
    Ok(())
}

#[cfg(unix)]
#[test]
fn rejects_symlink_snapshot_and_instance_without_following() -> Result<()> {
    for instance in [true, false] {
        let fixture = Fixture::new()?;
        let path = if instance {
            fixture.directory.join("instance.json")
        } else {
            fixture.session_directory()?.join("snapshot.json")
        };
        let parked = path.with_extension("parked");
        fs::rename(&path, &parked)?;
        std::os::unix::fs::symlink(&parked, &path)?;
        ensure!(OpenWebUiConnector::new().scan(&fixture.context()).is_err());
    }
    Ok(())
}

fn native_cli_index_round_trip(passes: usize) -> Result<()> {
    let mut fixture = Fixture::new()?;
    let archive = fixture.temporary.path().join("archive");
    let mut index_reports = Vec::new();
    for pass in 0..passes {
        if pass == 2 {
            fixture.snapshot["projection"]["messages"][0]["message"]["content"] =
                json!("changedwebuihistoryneedle");
            fixture.publish()?;
        }
        let output = fixture
            .command()?
            .args(["index", "--watch-once"])
            .arg(fixture.directory.join("catalog.json"))
            .arg("--data-dir")
            .arg(&archive)
            .arg("--json")
            .output()?;
        ensure!(
            output.status.success(),
            "index pass {pass}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        index_reports.push(serde_json::from_slice::<Value>(&output.stdout)?);
    }
    let output = fixture
        .command()?
        .args(["sessions", "--workspace", "/work/webui", "--data-dir"])
        .arg(&archive)
        .arg("--json")
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sessions: Value = serde_json::from_slice(&output.stdout)?;
    let sessions = sessions["sessions"].as_array().context("sessions array")?;
    ensure!(sessions.len() == 1);
    let session = sessions.first().context("indexed session")?;
    ensure!(session["agent"] == "openwebui" && session["workspace"] == "/work/webui");
    ensure!(session["message_count"] == 2);
    for needle in [
        if passes > 2 {
            "changedwebuihistoryneedle"
        } else {
            "webuihistoryneedle"
        },
        "toolargumentneedle",
        "toolresultneedle",
    ] {
        let output = fixture
            .command()?
            .args([
                "search",
                needle,
                "--agent",
                "openwebui",
                "--mode",
                "lexical",
                "--data-dir",
            ])
            .arg(&archive)
            .arg("--json")
            .output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let results: Value = serde_json::from_slice(&output.stdout)?;
        ensure!(
            results["hits"]
                .as_array()
                .context("hits")?
                .iter()
                .any(|hit| hit["agent"] == "openwebui"),
            "missing {needle}: {results}; session: {session}; index passes: {index_reports:?}"
        );
    }
    Ok(())
}

#[test]
fn native_cli_index_replay_and_tool_search() -> Result<()> {
    native_cli_index_round_trip(2)
}

#[test]
fn native_cli_index_changed_message_and_tool_search() -> Result<()> {
    native_cli_index_round_trip(4)
}

#[test]
fn native_cli_empty_replacement_retires_old_hits_and_reappears_once() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let archive = fixture.temporary.path().join("archive");
    ensure!(fixture.index(&archive)?["messages"] == 2);
    ensure!(fixture.lexical_hits(&archive, "webuihistoryneedle")?.len() == 1);
    fixture.snapshot["projection"]["messages"][0]["message"]["content"] = json!("");
    fixture.publish()?;
    for _pass in 0..2 {
        ensure!(fixture.index(&archive)?["messages"] == 0);
        ensure!(
            fixture
                .lexical_hits(&archive, "webuihistoryneedle")?
                .is_empty()
        );
        ensure!(fixture.lexical_hits(&archive, "toolresultneedle")?.len() == 1);
    }
    fixture.snapshot["projection"]["messages"][0]["message"]["content"] =
        json!("restoreduniquenative request");
    fixture.publish()?;
    for _pass in 0..2 {
        ensure!(fixture.index(&archive)?["messages"] == 0);
        ensure!(
            fixture
                .lexical_hits(&archive, "webuihistoryneedle")?
                .is_empty()
        );
        ensure!(
            fixture
                .lexical_hits(&archive, "restoreduniquenative")?
                .len()
                == 1
        );
        ensure!(fixture.lexical_hits(&archive, "toolresultneedle")?.len() == 1);
    }
    Ok(())
}

#[test]
fn native_cli_preserves_distinct_identical_message_ids() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let archive = fixture.temporary.path().join("archive");
    let original = fixture.snapshot["projection"]["messages"][0].clone();
    for (pass, expected_messages, expected_insertions) in
        [(0, 3, 3), (1, 3, 0), (2, 4, 1), (3, 4, 0)]
    {
        if pass == 0 || pass == 2 {
            let identifier = format!("distinct-user-{pass}");
            let mut duplicate_content = original.clone();
            duplicate_content["id"] = json!(identifier);
            duplicate_content["message"]["id"] = json!(identifier);
            duplicate_content["origin"]["row_id"] = json!(format!("original-chat-{identifier}"));
            fixture.snapshot["projection"]["messages"]
                .as_array_mut()
                .context("messages")?
                .push(duplicate_content);
            fixture.publish()?;
        }
        let output = fixture
            .command()?
            .args(["index", "--watch-once"])
            .arg(fixture.directory.join("catalog.json"))
            .arg("--data-dir")
            .arg(&archive)
            .arg("--json")
            .output()?;
        ensure!(
            output.status.success(),
            "index pass {pass}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout)?;
        let output = fixture
            .command()?
            .args(["sessions", "--workspace", "/work/webui", "--data-dir"])
            .arg(&archive)
            .arg("--json")
            .output()?;
        ensure!(output.status.success());
        let sessions: Value = serde_json::from_slice(&output.stdout)?;
        let sessions = sessions["sessions"].as_array().context("sessions")?;
        ensure!(sessions.len() == 1);
        let session = sessions.first().context("original session")?;
        ensure!(
            session["message_count"] == expected_messages,
            "distinct native IDs collapsed on pass {pass}: {session}; index: {report}"
        );
        ensure!(
            report["messages"] == expected_insertions,
            "unexpected insertion count on pass {pass}: {report}"
        );
    }
    let output = fixture
        .command()?
        .args([
            "search",
            "webuihistoryneedle",
            "--agent",
            "openwebui",
            "--mode",
            "lexical",
            "--data-dir",
        ])
        .arg(&archive)
        .arg("--json")
        .output()?;
    ensure!(output.status.success());
    let results: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(results["hits"].as_array().context("hits")?.len() == 3);
    Ok(())
}
