use anyhow::{Context, ensure};
use coding_agent_search::connectors::cline::ClineConnector;
use coding_agent_search::connectors::{Connector, ScanContext};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

fn modern_fixture(root: &std::path::Path, id: &str) -> anyhow::Result<PathBuf> {
    let task = root.join("saoudrizwan.claude-dev/tasks").join(id);
    fs::create_dir_all(&task)?;
    fs::write(
        task.join("ui_messages.json"),
        serde_json::to_vec(&serde_json::json!([
            {"type":"say", "say":"task", "text":"UI presentation", "ts":1000},
            {"type":"ask", "ask":"tool", "text":"UI approval", "ts":2000}
        ]))?,
    )?;
    fs::write(
        task.join("api_conversation_history.json"),
        serde_json::to_vec(&serde_json::json!([
            {"role":"user", "content":[{"type":"text", "text":"Native user request"}], "ts":1000},
            {"role":"assistant", "content":[{"type":"text", "text":"Native response"}], "ts":2000}
        ]))?,
    )?;
    let state = root.join("saoudrizwan.claude-dev/state");
    fs::create_dir_all(&state)?;
    fs::write(
        state.join("taskHistory.json"),
        serde_json::to_vec(&serde_json::json!([
            {"id":id,"cwdOnTaskInitialization":"/work/native-cline"}
        ]))?,
    )?;
    Ok(task)
}

#[test]
fn cline_modern_tasks_preserve_native_roles_workspace_and_single_identity() -> anyhow::Result<()> {
    use coding_agent_search::connectors::{DiscoveredSourceRole, ScanRoot};
    let fixture = TempDir::new()?;
    let task = modern_fixture(fixture.path(), "native-task")?;
    let ctx = ScanContext::with_roots(
        fixture.path().join("archive"),
        vec![ScanRoot::local(
            fixture.path().join("saoudrizwan.claude-dev"),
        )],
        None,
    );
    let connector_factory = coding_agent_search::connectors::get_connector_factories()
        .into_iter()
        .find(|(name, _)| *name == "cline")
        .map(|(_, factory)| factory)
        .context("Cline connector factory is not registered")?;
    let connector = connector_factory();
    let conversations = connector.scan(&ctx)?;
    ensure!(conversations.len() == 1);
    let conversation = conversations.first().context("missing conversation")?;
    ensure!(conversation.external_id.as_deref() == Some("native-task"));
    ensure!(conversation.agent_slug == "cline");
    ensure!(conversation.workspace.as_deref() == Some(std::path::Path::new("/work/native-cline")));
    ensure!(conversation.source_path == task.join("api_conversation_history.json"));
    ensure!(
        conversation
            .messages
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>()
            == vec!["user", "assistant"]
    );
    let sources = connector.discover_source_files(&ctx)?;
    ensure!(
        sources
            .iter()
            .filter(|source| source.role == DiscoveredSourceRole::PrimarySessionLog)
            .count()
            == 1
    );
    ensure!(
        sources
            .iter()
            .any(|source| source.source_path == task.join("ui_messages.json"))
    );
    ensure!(
        sources
            .iter()
            .any(|source| source.source_path.ends_with("state/taskHistory.json"))
    );
    Ok(())
}

#[test]
fn cline_modern_exact_file_and_overlapping_roots_do_not_import_siblings() -> anyhow::Result<()> {
    use coding_agent_search::connectors::ScanRoot;
    let fixture = TempDir::new()?;
    let task = modern_fixture(fixture.path(), "selected")?;
    modern_fixture(fixture.path(), "sibling")?;
    let connector = ClineConnector::new();
    let mut ctx = ScanContext::with_roots(
        fixture.path().join("archive"),
        vec![ScanRoot::local(task.join("api_conversation_history.json"))],
        None,
    );
    let conversations = connector.scan(&ctx)?;
    ensure!(conversations.len() == 1);
    ensure!(
        conversations
            .first()
            .and_then(|conversation| conversation.external_id.as_deref())
            == Some("selected")
    );
    ctx.scan_roots.push(ScanRoot::local(task.clone()));
    ctx.scan_roots
        .push(ScanRoot::local(task.join("ui_messages.json")));
    ensure!(connector.scan(&ctx)?.len() == 1);
    Ok(())
}

#[test]
fn cline_modern_invalid_api_falls_back_without_inventing_roles_or_workspace() -> anyhow::Result<()>
{
    use coding_agent_search::connectors::ScanRoot;
    let fixture = TempDir::new()?;
    let task = modern_fixture(fixture.path(), "fallback")?;
    fs::write(task.join("api_conversation_history.json"), b"{ malformed")?;
    fs::write(
        fixture
            .path()
            .join("saoudrizwan.claude-dev/state/taskHistory.json"),
        b"[]",
    )?;
    let ctx = ScanContext::with_roots(
        fixture.path().join("archive"),
        vec![ScanRoot::local(task.clone())],
        None,
    );
    let conversations = ClineConnector::new().scan(&ctx)?;
    let conversation = conversations.first().context("missing fallback")?;
    ensure!(conversation.source_path == task.join("ui_messages.json"));
    ensure!(conversation.workspace.is_none());
    ensure!(
        conversation
            .metadata
            .get("coverage")
            .and_then(serde_json::Value::as_str)
            == Some("ui_events_only")
    );
    ensure!(
        conversation
            .messages
            .first()
            .map(|message| message.role.as_str())
            == Some("say")
    );
    Ok(())
}

#[test]
fn cline_modern_metadata_change_reindexes_complete_history() -> anyhow::Result<()> {
    use coding_agent_search::connectors::ScanRoot;
    let fixture = TempDir::new()?;
    let task = modern_fixture(fixture.path(), "metadata-update")?;
    for name in ["api_conversation_history.json", "ui_messages.json"] {
        fs::File::open(task.join(name))?
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))?;
    }
    let ctx = ScanContext::with_roots(
        fixture.path().join("archive"),
        vec![ScanRoot::local(task)],
        Some(2000),
    );
    let connector = ClineConnector::new();
    let conversations = connector.scan(&ctx)?;
    ensure!(conversations.len() == 1);
    ensure!(
        conversations
            .first()
            .map(|conversation| conversation.messages.len())
            == Some(2)
    );
    let sources = connector.discover_source_files(&ctx)?;
    ensure!(sources.iter().any(
        |source| source.source_path.ends_with("state/taskHistory.json")
            && source.required_for_reconstruction
    ));
    Ok(())
}

#[test]
fn cline_modern_ambiguous_history_does_not_bind_workspace() -> anyhow::Result<()> {
    use coding_agent_search::connectors::ScanRoot;
    let fixture = TempDir::new()?;
    let task = modern_fixture(fixture.path(), "duplicate")?;
    fs::write(
        fixture
            .path()
            .join("saoudrizwan.claude-dev/state/taskHistory.json"),
        serde_json::to_vec(&serde_json::json!([
            {"id":"duplicate","cwdOnTaskInitialization":"/work/one"},
            {"id":"duplicate","cwdOnTaskInitialization":"/work/two"}
        ]))?,
    )?;
    let ctx = ScanContext::with_roots(
        fixture.path().join("archive"),
        vec![ScanRoot::local(task)],
        None,
    );
    let conversations = ClineConnector::new().scan(&ctx)?;
    ensure!(conversations.len() == 1);
    ensure!(
        conversations
            .first()
            .and_then(|conversation| conversation.workspace.as_ref())
            == None
    );
    Ok(())
}

// ============================================================================
// Fixture-based tests
// ============================================================================

#[test]
fn cline_parses_fixture_task() -> anyhow::Result<()> {
    let fixture_root = PathBuf::from("tests/fixtures/cline");
    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: fixture_root.clone(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(
        convs.len() == 1,
        "expected exactly 1 conversation from fixture"
    );
    let c = convs
        .first()
        .context("Cline fixture conversation missing")?;
    ensure!(
        c.title.as_deref() == Some("Cline fixture task"),
        "title should match fixture's task metadata"
    );
    // We now prefer ui_messages.json (2 msgs) over api_conversation_history.json (1 msg)
    // to avoid duplicates and prefer user-facing content.
    ensure!(
        c.messages.len() == 2,
        "expected 2 messages from ui_messages.json"
    );
    ensure!(
        c.messages.iter().any(|m| m.content.contains("Hello Cline")),
        "should contain 'Hello Cline' message from fixture"
    );
    Ok(())
}

#[test]
fn cline_respects_since_ts_and_resequences_indices() -> anyhow::Result<()> {
    let dir = tempfile::TempDir::new()?;
    let storage_root = dir.path().join("saoudrizwan.claude-dev");
    let root = storage_root.join("task-123");
    std::fs::create_dir_all(&root)?;

    let ui_messages_path = root.join("ui_messages.json");

    // Two messages: older (timestamp=1_000) and newer (timestamp=2_000).
    let msgs = serde_json::json!([
        {
            "timestamp": 1_000,
            "role": "user",
            "content": "old msg"
        },
        {
            "timestamp": 2_000,
            "role": "assistant",
            "content": "new msg"
        }
    ]);
    std::fs::write(&ui_messages_path, serde_json::to_string(&msgs)?)?;

    let connector = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: storage_root,
        scan_roots: Vec::new(),
        since_ts: Some(1_500),
        progress_tick: None,
    };

    let convs = connector.scan(&ctx)?;
    ensure!(
        convs.len() == 1,
        "expected exactly 1 conversation after since_ts filtering"
    );
    let c = convs
        .first()
        .context("filtered Cline conversation missing")?;

    // Incremental filtering for Cline is file-level, not per-message.
    // Since the file is newer than since_ts, we ingest all messages and resequence.
    ensure!(
        c.messages.len() == 2,
        "expected file-level since_ts filtering to keep full conversation payload"
    );
    let first = c.messages.first().context("first Cline message missing")?;
    let second = c.messages.get(1).context("second Cline message missing")?;
    ensure!(
        first.idx == 0,
        "first message idx should be 0 after re-sequencing"
    );
    ensure!(
        first.content.contains("old msg"),
        "first message should contain 'old msg'"
    );
    ensure!(
        second.idx == 1,
        "second message idx should be 1 after re-sequencing"
    );
    ensure!(
        second.role == "assistant",
        "second message should be assistant role"
    );
    ensure!(
        second.content.contains("new msg"),
        "second message should contain 'new msg'"
    );
    Ok(())
}

#[test]
fn cline_skips_unmodified_files_for_since_ts() -> anyhow::Result<()> {
    let dir = tempfile::TempDir::new()?;
    let storage_root = dir.path().join("saoudrizwan.claude-dev");
    let root = storage_root.join("task-older");
    std::fs::create_dir_all(&root)?;

    let ui_messages_path = root.join("ui_messages.json");
    let msgs = serde_json::json!([
        {
            "timestamp": 1_000,
            "role": "user",
            "content": "persisted msg"
        }
    ]);
    std::fs::write(&ui_messages_path, serde_json::to_string(&msgs)?)?;

    let modified_duration = std::fs::metadata(&ui_messages_path)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .context("Cline fixture modification time precedes Unix epoch")?;
    let modified_ms = i64::try_from(modified_duration.as_millis()).unwrap_or(i64::MAX);

    let connector = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: storage_root,
        scan_roots: Vec::new(),
        since_ts: Some(modified_ms.saturating_add(2_000)),
        progress_tick: None,
    };

    let convs = connector.scan(&ctx)?;
    ensure!(
        convs.is_empty(),
        "expected conversation to be skipped when file mtime is older than since_ts threshold"
    );
    Ok(())
}

// ============================================================================
// Unit tests with temp directories
// ============================================================================

/// Helper to create a Cline-style task directory
fn create_task_dir(root: &std::path::Path, task_id: &str) -> anyhow::Result<PathBuf> {
    let task_dir = root.join(task_id);
    fs::create_dir_all(&task_dir)?;
    Ok(task_dir)
}

/// Test ui_messages.json is preferred over api_conversation_history.json
#[test]
fn cline_prefers_ui_messages() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "task-prefer")?;

    // Create both files with different content
    let ui_msgs = serde_json::json!([
        {"role": "user", "content": "UI message", "timestamp": 1000}
    ]);
    let api_msgs = serde_json::json!([
        {"role": "user", "content": "API message", "timestamp": 1000}
    ]);
    fs::write(task.join("ui_messages.json"), ui_msgs.to_string())?;
    fs::write(
        task.join("api_conversation_history.json"),
        api_msgs.to_string(),
    )?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(
        convs.len() == 1,
        "expected exactly 1 conversation when ui_messages.json exists"
    );
    let conversation = convs.first().context("Cline conversation missing")?;
    let message = conversation
        .messages
        .first()
        .context("Cline user message missing")?;
    ensure!(
        message.content.contains("UI message"),
        "should prefer ui_messages.json content over api_conversation_history.json"
    );
    Ok(())
}

/// Test fallback to api_conversation_history.json when ui_messages.json is missing
#[test]
fn cline_fallback_to_api_history() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "task-fallback")?;

    // Only create api_conversation_history.json
    let api_msgs = serde_json::json!([
        {"role": "user", "content": "API only message", "timestamp": 1000}
    ]);
    fs::write(
        task.join("api_conversation_history.json"),
        api_msgs.to_string(),
    )?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(
        convs.len() == 1,
        "expected exactly 1 conversation from api_conversation_history fallback"
    );
    let conversation = convs.first().context("fallback conversation missing")?;
    let message = conversation
        .messages
        .first()
        .context("fallback user message missing")?;
    ensure!(
        message.content.contains("API only message"),
        "should fallback to api_conversation_history.json when ui_messages.json is missing"
    );
    Ok(())
}

/// Test multiple task directories
#[test]
fn cline_handles_multiple_tasks() -> anyhow::Result<()> {
    let dir = TempDir::new()?;

    for (task_id, message_json) in [
        (
            "task-1",
            r#"[{"role":"user","content":"Message 1","timestamp":1000}]"#,
        ),
        (
            "task-2",
            r#"[{"role":"user","content":"Message 2","timestamp":2000}]"#,
        ),
        (
            "task-3",
            r#"[{"role":"user","content":"Message 3","timestamp":3000}]"#,
        ),
    ] {
        let task = create_task_dir(dir.path(), task_id)?;
        fs::write(task.join("ui_messages.json"), message_json)?;
    }

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(
        convs.len() == 3,
        "expected 3 conversations from 3 task directories"
    );
    Ok(())
}

/// Test taskHistory.json is skipped
#[test]
fn cline_skips_task_history_json() -> anyhow::Result<()> {
    let dir = TempDir::new()?;

    // Create a real task
    let task = create_task_dir(dir.path(), "task-real")?;
    let msgs = serde_json::json!([{"role": "user", "content": "Real task", "timestamp": 1000}]);
    fs::write(task.join("ui_messages.json"), msgs.to_string())?;

    // Create taskHistory.json directory (should be skipped)
    let task_history = create_task_dir(dir.path(), "taskHistory.json")?;
    let msgs = serde_json::json!([{"role": "user", "content": "Should skip", "timestamp": 1000}]);
    fs::write(task_history.join("ui_messages.json"), msgs.to_string())?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(
        convs.len() == 1,
        "expected 1 conversation - taskHistory.json dir should be skipped"
    );
    let conversation = convs.first().context("real Cline conversation missing")?;
    let message = conversation
        .messages
        .first()
        .context("real Cline message missing")?;
    ensure!(
        message.content.contains("Real task"),
        "should only contain real task, not taskHistory.json"
    );
    Ok(())
}

/// Test title extraction from metadata
#[test]
fn cline_extracts_title_from_metadata() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "task-title")?;

    let meta = serde_json::json!({"title": "Custom Task Title"});
    fs::write(task.join("task_metadata.json"), meta.to_string())?;

    let msgs = serde_json::json!([{"role": "user", "content": "Hello", "timestamp": 1000}]);
    fs::write(task.join("ui_messages.json"), msgs.to_string())?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(
        convs.len() == 1,
        "expected 1 conversation for title metadata test"
    );
    let conversation = convs.first().context("Cline conversation missing")?;
    ensure!(
        conversation.title.as_deref() == Some("Custom Task Title"),
        "title should be extracted from task_metadata.json"
    );
    Ok(())
}

/// Test title fallback to first message
#[test]
fn cline_title_fallback_to_first_message() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "task-no-title")?;

    // No metadata file
    let msgs = serde_json::json!([
        {"role": "user", "content": "First line for title\nSecond line", "timestamp": 1000}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string())?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(
        convs.len() == 1,
        "expected 1 conversation for title fallback test"
    );
    let conversation = convs.first().context("Cline conversation missing")?;
    ensure!(
        conversation.title.as_deref() == Some("First line for title"),
        "title should fallback to first line of first user message"
    );
    Ok(())
}

/// Test workspace extraction from metadata (rootPath)
#[test]
fn cline_extracts_workspace_from_rootpath() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "task-workspace")?;

    let meta = serde_json::json!({"rootPath": "/home/user/project"});
    fs::write(task.join("task_metadata.json"), meta.to_string())?;

    let msgs = serde_json::json!([{"role": "user", "content": "Hello", "timestamp": 1000}]);
    fs::write(task.join("ui_messages.json"), msgs.to_string())?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(
        convs.len() == 1,
        "expected 1 conversation for rootPath workspace test"
    );
    let conversation = convs.first().context("Cline conversation missing")?;
    ensure!(
        conversation.workspace.as_deref() == Some(std::path::Path::new("/home/user/project")),
        "workspace should be extracted from rootPath in task_metadata.json"
    );
    Ok(())
}

/// Test workspace extraction from cwd field
#[test]
fn cline_extracts_workspace_from_cwd() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "task-cwd")?;

    let meta = serde_json::json!({"cwd": "/workspace/myproject"});
    fs::write(task.join("task_metadata.json"), meta.to_string())?;

    let msgs = serde_json::json!([{"role": "user", "content": "Hello", "timestamp": 1000}]);
    fs::write(task.join("ui_messages.json"), msgs.to_string())?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(convs.len() == 1);
    ensure!(
        convs
            .first()
            .context("Cline conversation missing")?
            .workspace
            .as_deref()
            == Some(std::path::Path::new("/workspace/myproject"))
    );
    Ok(())
}

/// Test empty content is filtered
#[test]
fn cline_filters_empty_content() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "task-empty")?;

    let msgs = serde_json::json!([
        {"role": "user", "content": "   ", "timestamp": 1000},
        {"role": "user", "content": "Valid content", "timestamp": 2000},
        {"role": "assistant", "content": "", "timestamp": 3000}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string())?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(convs.len() == 1);
    let conversation = convs.first().context("Cline conversation missing")?;
    ensure!(conversation.messages.len() == 1);
    ensure!(
        conversation
            .messages
            .first()
            .context("filtered Cline message missing")?
            .content
            .contains("Valid content")
    );
    Ok(())
}

/// Test messages are sorted by timestamp
#[test]
fn cline_sorts_messages_by_timestamp() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "task-sort")?;

    // Messages in wrong order
    let msgs = serde_json::json!([
        {"role": "assistant", "content": "Third", "timestamp": 3000},
        {"role": "user", "content": "First", "timestamp": 1000},
        {"role": "assistant", "content": "Second", "timestamp": 2000}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string())?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(convs.len() == 1);

    let c = convs.first().context("Cline conversation missing")?;
    ensure!(c.messages.len() == 3);
    let first = c.messages.first().context("first sorted message missing")?;
    let second = c.messages.get(1).context("second sorted message missing")?;
    let third = c.messages.get(2).context("third sorted message missing")?;
    ensure!(first.content.contains("First"));
    ensure!(second.content.contains("Second"));
    ensure!(third.content.contains("Third"));

    // Indices should be sequential after sorting
    ensure!(first.idx == 0);
    ensure!(second.idx == 1);
    ensure!(third.idx == 2);
    Ok(())
}

/// Test external_id comes from task directory name
#[test]
fn cline_sets_external_id_from_directory() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "unique-task-123")?;

    let msgs = serde_json::json!([{"role": "user", "content": "Test", "timestamp": 1000}]);
    fs::write(task.join("ui_messages.json"), msgs.to_string())?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(convs.len() == 1);
    ensure!(
        convs
            .first()
            .context("Cline conversation missing")?
            .external_id
            .as_deref()
            == Some("unique-task-123")
    );
    Ok(())
}

/// Test source_path is the selected source file
#[test]
fn cline_sets_source_path_to_selected_file() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "task-path")?;

    let msgs = serde_json::json!([{"role": "user", "content": "Test", "timestamp": 1000}]);
    let ui_messages = task.join("ui_messages.json");
    fs::write(&ui_messages, msgs.to_string())?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(convs.len() == 1);
    ensure!(
        convs
            .first()
            .context("Cline conversation missing")?
            .source_path
            == ui_messages
    );
    Ok(())
}

/// Test empty directory returns no conversations
#[test]
fn cline_handles_empty_directory() -> anyhow::Result<()> {
    let dir = TempDir::new()?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        // This test selects an empty fixture, not default discovery of the host.
        scan_roots: vec![coding_agent_search::connectors::ScanRoot::local(
            dir.path().to_path_buf(),
        )],
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(convs.is_empty());
    Ok(())
}

/// Test task directory without message files is skipped
#[test]
fn cline_skips_task_without_messages() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let _task = create_task_dir(dir.path(), "task-no-msgs")?;
    // Don't create any message files

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: vec![coding_agent_search::connectors::ScanRoot::local(
            dir.path().to_path_buf(),
        )],
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(convs.is_empty());
    Ok(())
}

/// Test started_at and ended_at timestamps
#[test]
fn cline_sets_started_and_ended_at() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "task-times")?;

    let msgs = serde_json::json!([
        {"role": "user", "content": "First", "timestamp": 1000},
        {"role": "assistant", "content": "Last", "timestamp": 5000}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string())?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(convs.len() == 1);
    let conversation = convs.first().context("Cline conversation missing")?;
    ensure!(conversation.started_at == Some(1000000)); // 1000 seconds -> 1000000 ms
    ensure!(conversation.ended_at == Some(5000000)); // 5000 seconds -> 5000000 ms
    Ok(())
}

/// Test agent_slug is "cline"
#[test]
fn cline_sets_agent_slug() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "task-slug")?;

    let msgs = serde_json::json!([{"role": "user", "content": "Test", "timestamp": 1000}]);
    fs::write(task.join("ui_messages.json"), msgs.to_string())?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(convs.len() == 1);
    ensure!(
        convs
            .first()
            .context("Cline conversation missing")?
            .agent_slug
            == "cline"
    );
    Ok(())
}

/// Test alternate content fields (text, message)
#[test]
fn cline_parses_alternate_content_fields() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "task-alt-fields")?;

    let msgs = serde_json::json!([
        {"role": "user", "text": "Text field content", "timestamp": 1000},
        {"role": "assistant", "message": "Message field content", "timestamp": 2000}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string())?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(convs.len() == 1);
    let conversation = convs.first().context("Cline conversation missing")?;
    ensure!(conversation.messages.len() == 2);
    let first = conversation
        .messages
        .first()
        .context("first message missing")?;
    let second = conversation
        .messages
        .get(1)
        .context("second message missing")?;
    ensure!(first.content.contains("Text field content"));
    ensure!(second.content.contains("Message field content"));
    Ok(())
}

/// Test alternate timestamp fields (created_at, ts)
#[test]
fn cline_parses_alternate_timestamp_fields() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "task-alt-ts")?;

    let msgs = serde_json::json!([
        {"role": "user", "content": "First", "created_at": 1000},
        {"role": "assistant", "content": "Second", "ts": 2000}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string())?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(convs.len() == 1);
    let conversation = convs.first().context("Cline conversation missing")?;
    ensure!(conversation.messages.len() == 2);
    let first = conversation
        .messages
        .first()
        .context("first message missing")?;
    let second = conversation
        .messages
        .get(1)
        .context("second message missing")?;
    ensure!(first.created_at == Some(1000000)); // 1000 seconds -> 1000000 ms
    ensure!(second.created_at == Some(2000000)); // 2000 seconds -> 2000000 ms
    Ok(())
}

/// Test type field used as role when role is missing
#[test]
fn cline_uses_type_as_role_fallback() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "task-type-role")?;

    let msgs = serde_json::json!([
        {"type": "user", "content": "User message", "timestamp": 1000}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string())?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(convs.len() == 1);
    let conversation = convs.first().context("Cline conversation missing")?;
    let message = conversation
        .messages
        .first()
        .context("Cline message missing")?;
    ensure!(message.role == "user");
    Ok(())
}

/// Test long title is truncated
#[test]
fn cline_truncates_long_title() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "task-long")?;

    let long_text = "A".repeat(200);
    let msgs = serde_json::json!([
        {"role": "user", "content": long_text, "timestamp": 1000}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string())?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(convs.len() == 1);
    let conversation = convs.first().context("Cline conversation missing")?;
    // Bead 7k7pl: check title presence and length together in
    // one condition that captures both preconditions — a
    // regression producing None or the wrong truncation length both
    // fail with a single actionable message.
    ensure!(
        conversation.title.as_ref().map(|title| title.len()) == Some(100),
        "title must be truncated to exactly 100 chars; got {:?}",
        conversation.title
    );
    Ok(())
}

/// Test metadata source is "cline"
#[test]
fn cline_sets_metadata_source() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "task-meta")?;

    let msgs = serde_json::json!([{"role": "user", "content": "Test", "timestamp": 1000}]);
    fs::write(task.join("ui_messages.json"), msgs.to_string())?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(convs.len() == 1);
    let conversation = convs.first().context("Cline conversation missing")?;
    ensure!(
        conversation
            .metadata
            .get("source")
            .and_then(|value| value.as_str())
            == Some("cline")
    );
    Ok(())
}

/// Test files in root (not directories) are ignored
#[test]
fn cline_ignores_files_in_root() -> anyhow::Result<()> {
    let dir = TempDir::new()?;

    // Create a valid task
    let task = create_task_dir(dir.path(), "task-valid")?;
    let msgs = serde_json::json!([{"role": "user", "content": "Valid", "timestamp": 1000}]);
    fs::write(task.join("ui_messages.json"), msgs.to_string())?;

    // Create files in root (should be ignored)
    fs::write(dir.path().join("some_file.json"), "{}")?;
    fs::write(dir.path().join("another.txt"), "text")?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(convs.len() == 1);
    Ok(())
}

/// Test ISO-8601 timestamp parsing
#[test]
fn cline_parses_iso_timestamps() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let task = create_task_dir(dir.path(), "task-iso")?;

    let msgs = serde_json::json!([
        {"role": "user", "content": "ISO timestamp", "timestamp": "2025-11-12T18:31:18.000Z"}
    ]);
    fs::write(task.join("ui_messages.json"), msgs.to_string())?;

    let conn = ClineConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx)?;
    ensure!(convs.len() == 1);
    let conversation = convs.first().context("Cline conversation missing")?;
    let message = conversation
        .messages
        .first()
        .context("Cline message missing")?;
    ensure!(message.created_at.is_some());
    Ok(())
}
