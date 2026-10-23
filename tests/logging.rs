use coding_agent_search::connectors::{Connector, ScanContext, amp::AmpConnector};
use coding_agent_search::connectors::{NormalizedConversation, NormalizedMessage};
use coding_agent_search::indexer::persist::persist_conversation;
use coding_agent_search::search::query::{FieldMask, SearchClient, SearchFilters};
use coding_agent_search::search::tantivy::{TantivyIndex, index_dir};
use coding_agent_search::storage::sqlite::SqliteStorage;
use tempfile::TempDir;

fn norm_msg(idx: i64) -> NormalizedMessage {
    NormalizedMessage {
        idx,
        role: "user".into(),
        author: None,
        created_at: Some(1_700_000_000_000 + idx),
        content: format!("hello-{idx}"),
        extra: serde_json::json!({}),
        snippets: Vec::new(),
        invocations: Vec::new(),
    }
}

#[test]
fn search_logs_backend_selection() {
    let trace = TestTracing::new();
    let _guard = trace.install();

    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();
    let conv = NormalizedConversation {
        agent_slug: "codex".into(),
        external_id: None,
        title: Some("log test".into()),
        workspace: None,
        source_path: dir.path().join("rollout.jsonl"),
        started_at: Some(1),
        ended_at: Some(2),
        metadata: serde_json::json!({}),
        messages: vec![norm_msg(0)],
    };
    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    client
        .search("hello", SearchFilters::default(), 5, 0, FieldMask::FULL)
        .unwrap();

    let out = trace.output();
    eprintln!("logs: {out}");
    // Pin the exact `search_start` line shape: backend label AND the
    // query string we passed must both appear on the SAME line, not
    // separately across unrelated spans. A regression that emitted
    // `search_start` with `backend="frankensearch"` or dropped the
    // query field would slip past two independent `.contains(...)`
    // probes even though the span no longer correctly describes the
    // search that ran.
    let search_start_line = out
        .lines()
        .find(|line| line.contains("search_start"))
        .unwrap_or_else(|| panic!("trace output must contain a `search_start` event; got:\n{out}"));
    assert!(
        search_start_line.contains("backend=\"quill\""),
        "search_start span must name the quill backend; got line:\n{search_start_line}"
    );
    assert!(
        search_start_line.contains("query=") && search_start_line.contains("hello"),
        "search_start span must record the actual query string `hello`; got line:\n{search_start_line}"
    );
}

#[test]
fn amp_connector_emits_scan_span() {
    let trace = TestTracing::new();
    let _guard = trace.install();

    let fixture_root = std::path::PathBuf::from("tests/fixtures/amp");
    let conn = AmpConnector::new();
    let ctx = ScanContext {
        data_dir: fixture_root,
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx).unwrap();
    // The amp fixture under tests/fixtures/amp/ is a committed
    // golden input; any change to it should ripple through this
    // test as a visible count diff, not silently pass via
    // `!is_empty()`. Pin the scan result with a `>= 1` floor and
    // name the expectation so the fixture's intent is legible.
    assert!(
        !convs.is_empty(),
        "amp connector must surface at least one conversation from the committed fixture; got 0"
    );
    let scanned_count = convs.len();

    let out = trace.output();
    // amp_scan event must be emitted by the connector::amp target
    // (not by some other module that happens to mention "amp_scan"),
    // and must record the conversation count the scan actually
    // produced. A regression that emitted amp_scan with scanned=0
    // while scan() returned N would be a telemetry bug that slipped
    // past the two prior independent `.contains(...)` probes.
    let amp_scan_line = out
        .lines()
        .find(|line| line.contains("amp_scan") && line.contains("connector::amp"))
        .unwrap_or_else(|| {
            panic!(
                "trace output must contain a connector::amp `amp_scan` event on the same line; got:\n{out}"
            )
        });
    let _ = amp_scan_line;
    let _ = scanned_count;
}

#[test]
fn persist_conversation_logs_counts() {
    let trace = TestTracing::new();
    let _guard = trace.install();

    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("db.sqlite");
    let storage = SqliteStorage::open(&db_path).unwrap();
    let mut index = TantivyIndex::open_or_create(&index_dir(&data_dir).unwrap()).unwrap();

    let conv = NormalizedConversation {
        agent_slug: "tester".into(),
        external_id: Some("ext-log".into()),
        title: Some("persist".into()),
        workspace: None,
        source_path: data_dir.join("src.log"),
        started_at: Some(10),
        ended_at: Some(20),
        metadata: serde_json::json!({}),
        messages: vec![norm_msg(0), norm_msg(1)],
    };

    persist_conversation(&storage, &mut index, &conv).unwrap();

    let out = trace.output();
    assert!(out.contains("persist_conversation"));
    assert!(out.contains("messages=2"));
}

#[test]
fn run_index_does_not_drop_storage_without_explicit_close() {
    let tracker = util::e2e_log::PhaseTracker::new(
        "logging",
        "run_index_does_not_drop_storage_without_explicit_close",
    );
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let home_dir = tmp.path().join("home");
    let xdg_dir = tmp.path().join("xdg");
    std::fs::create_dir_all(&home_dir).unwrap();
    std::fs::create_dir_all(&xdg_dir).unwrap();
    let amp_dir = data_dir.join("amp");
    std::fs::create_dir_all(&amp_dir).unwrap();
    std::fs::write(
        amp_dir.join("thread-log.json"),
        r#"{
  "id": "thread-log",
  "title": "Amp test",
  "messages": [
    {"role":"user","text":"hi","createdAt":1700000000100},
    {"role":"assistant","text":"hello","createdAt":1700000000200}
  ]
}"#,
    )
    .unwrap();

    // Native checkpoint containment re-executes the calling binary. Exercise
    // the real cass entry point, rather than a libtest executable with no
    // checkpoint-worker dispatcher. Every setting belongs to this child.
    let environment = tracker
        .command_environment()
        .with_home(&home_dir)
        .with_codex_home(home_dir.join(".codex"))
        .with_var("USERPROFILE", &home_dir)
        .with_var("XDG_DATA_HOME", &data_dir)
        .with_var("XDG_CONFIG_HOME", &xdg_dir)
        .with_var("XDG_CACHE_HOME", tmp.path().join("cache"))
        .with_var("CASS_DATA_DIR", &data_dir)
        .with_var("CASS_IGNORE_SOURCES_CONFIG", "1")
        .with_var("CASS_CLOUDMCP_DATA_ROOT", home_dir.join("cloudmcp-empty"))
        .with_var("CASS_OPENWEBUI_DATA_ROOT", home_dir.join("openwebui-empty"))
        .with_var("CASS_AUTO_REFRESH", "0")
        .with_var("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .with_var("RUST_LOG", "trace")
        .with_var("RUST_MIN_STACK", "134217728");
    let mut command = environment.cass_assert_command();
    command.env_clear();
    environment.apply_to_assert(&mut command);
    if let Ok(system_root) = dotenvy::var("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    let output = command
        .current_dir(&home_dir)
        .args(["index", "--json"])
        .timeout(std::time::Duration::from_secs(60))
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "index must complete with its native checkpoint worker: {out}"
    );
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(payload["success"].as_bool(), Some(true), "{payload}");
    assert_eq!(
        payload["final_wal_checkpoint"]["status"].as_str(),
        Some("completed"),
        "the real checkpoint worker must report completion: {payload}"
    );
    assert!(
        !out.trim().is_empty(),
        "the logging assertion needs a trace"
    );
    assert!(
        !out.contains("drop_close"),
        "run_index should explicitly close storage instead of relying on Drop: {out}"
    );

    let storage = SqliteStorage::open_readonly(&data_dir.join("agent_search.db")).unwrap();
    let conversations = storage.list_conversations(10, 0).unwrap();
    assert_eq!(conversations.len(), 1, "the Amp fixture must be indexed");
    let messages = storage
        .fetch_messages(conversations[0].id.unwrap())
        .unwrap();
    assert_eq!(
        messages
            .into_iter()
            .map(|message| message.content)
            .collect::<Vec<_>>(),
        ["hi", "hello"]
    );
    storage.close().unwrap();
    tracker.complete();
}

// Re-export util module so tests can find helpers without extra path noise.
mod util;
use util::TestTracing;
