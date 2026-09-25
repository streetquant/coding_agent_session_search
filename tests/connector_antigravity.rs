//! cass-level integration tests for the Antigravity (agy) connector.
//!
//! These exercise the connector through cass's re-export
//! (`coding_agent_search::connectors::antigravity`) against a checked-in fixture
//! that mirrors agy's on-disk layout
//! (`<base>/brain/<uuid>/.system_generated/logs/transcript.jsonl` plus the
//! sibling `<base>/conversations/<uuid>.db`). The normalized conversations these
//! produce are exactly what cass's indexer + FTS search consume, so verifying the
//! scan here proves the index/search path covers agy. A companion check confirms
//! the legacy Gemini CLI connector is unaffected (bd-47kjh.3.3).
//!
//! NOTE: the agy connector defaults to the REAL `~/.gemini/antigravity-cli` when
//! `scan_roots` is empty, so every test passes an explicit `ScanRoot` at the
//! fixture base — never relying on default detection.

use anyhow::{Context, Result, ensure};
use coding_agent_search::connectors::antigravity::AntigravityConnector;
use coding_agent_search::connectors::gemini::GeminiConnector;
use coding_agent_search::connectors::{Connector, DiscoveredSourceRole, ScanContext, ScanRoot};
use coding_agent_search::franken_sync::Connection;
use coding_agent_search::franken_sync::compat::ConnectionExt;
use coding_agent_search::franken_sync::params;
use std::fs;
use std::path::{Path, PathBuf};

const FIXTURE_UUID: &str = "aaaa1111-bbbb-2222-cccc-333344445555";

fn fixture_base() -> PathBuf {
    PathBuf::from("tests/fixtures/antigravity")
}

/// A scan context rooted explicitly at the fixture (never the real ~/.gemini).
fn fixture_ctx() -> ScanContext {
    let base = fixture_base();
    ScanContext {
        data_dir: base.clone(),
        scan_roots: vec![ScanRoot::local(base)],
        since_ts: None,
        progress_tick: None,
    }
}

fn only_conversation() -> coding_agent_search::connectors::NormalizedConversation {
    let convs = AntigravityConnector::new()
        .scan(&fixture_ctx())
        .expect("scan");
    assert_eq!(convs.len(), 1, "fixture is a single agy conversation");
    convs.into_iter().next().unwrap()
}

#[test]
fn antigravity_scans_fixture_into_one_conversation() {
    let c = only_conversation();
    assert_eq!(c.agent_slug, "antigravity");
    assert_eq!(c.external_id.as_deref(), Some(FIXTURE_UUID));
    assert!(
        c.source_path.to_string_lossy().contains("transcript.jsonl"),
        "source_path should be the transcript we parse"
    );
}

#[test]
fn antigravity_pins_model_from_settings_change() {
    let c = only_conversation();
    assert_eq!(
        c.metadata.get("model").and_then(|v| v.as_str()),
        Some("Gemini 3.1 Pro (High)"),
        "model must be extracted from the <USER_SETTINGS_CHANGE> wrapper"
    );
    assert_eq!(
        c.metadata.get("source").and_then(|v| v.as_str()),
        Some("antigravity")
    );
}

#[test]
fn antigravity_maps_all_roles() {
    let c = only_conversation();
    assert!(c.messages.iter().any(|m| m.role == "user"));
    assert!(c.messages.iter().any(|m| m.role == "assistant"));
    assert!(c.messages.iter().any(|m| m.role == "tool"));
    assert!(c.messages.iter().any(|m| m.role == "system"));
    // CONVERSATION_HISTORY (null content) is dropped; the other 7 records map.
    assert_eq!(c.messages.len(), 7);
    for (i, m) in c.messages.iter().enumerate() {
        assert_eq!(m.idx, i64::try_from(i).unwrap());
    }
}

#[test]
fn antigravity_unwraps_user_request_and_keeps_settings() {
    let c = only_conversation();
    let user = c.messages.iter().find(|m| m.role == "user").unwrap();
    assert!(user.content.contains("FLYWHEEL"));
    assert!(!user.content.contains("USER_REQUEST"));
    assert!(
        user.extra
            .get("settings_change")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.contains("Gemini 3.1 Pro (High)"))
    );
}

#[test]
fn antigravity_preserves_thinking_on_planner_response() {
    let c = only_conversation();
    let planner = c
        .messages
        .iter()
        .find(|m| m.role == "assistant" && m.extra.get("thinking").is_some())
        .expect("a planner response carrying thinking");
    assert!(
        planner.extra["thinking"]
            .as_str()
            .unwrap()
            .contains("grep_search")
    );
}

#[test]
fn antigravity_tool_steps_carry_invocations() {
    let c = only_conversation();
    // VIEW_FILE -> synthesized "view_file" invocation, result content present.
    let view = c
        .messages
        .iter()
        .find(|m| m.extra.get("agy_type").and_then(|v| v.as_str()) == Some("VIEW_FILE"))
        .expect("VIEW_FILE message");
    assert_eq!(view.role, "tool");
    assert_eq!(view.invocations[0].name, "view_file");
    assert!(view.content.contains("FLYWHEEL"));

    // RUN_COMMAND -> structured grep_search invocation with its query argument.
    let run = c
        .messages
        .iter()
        .find(|m| m.invocations.iter().any(|i| i.name == "grep_search"))
        .expect("grep_search invocation");
    assert_eq!(run.role, "tool");
    assert_eq!(
        run.invocations[0]
            .arguments
            .as_ref()
            .and_then(|a| a.get("query"))
            .and_then(|v| v.as_str()),
        Some("FLYWHEEL")
    );
}

/// The content cass's FTS would index must contain the searchable marker — this
/// is the scan-level proxy for "search a real agy conversation". The generic
/// index+search machinery covers any registered connector, so a present,
/// correctly-roled marker here means it is findable end-to-end.
#[test]
fn antigravity_indexable_content_contains_searchable_marker() {
    let c = only_conversation();
    let haystack: String = c
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        haystack.contains("FLYWHEEL") && haystack.contains("42"),
        "indexable conversation text must contain the searchable FLYWHEEL/42 marker"
    );
}

#[test]
fn antigravity_discovers_transcript_and_db_sources() {
    let discovered = AntigravityConnector::new()
        .discover_source_files(&fixture_ctx())
        .expect("discover");
    assert!(discovered.iter().all(|d| d.provider_slug == "antigravity"));
    assert!(
        discovered
            .iter()
            .any(|d| d.source_path.ends_with("transcript.jsonl")
                && d.role == DiscoveredSourceRole::PrimarySessionLog
                && d.required_for_reconstruction)
    );
    assert!(
        discovered
            .iter()
            .any(|d| d.role == DiscoveredSourceRole::SqliteDatabase
                && d.source_path.extension().is_some_and(|e| e == "db")),
        "the conversations/<uuid>.db must be discovered for archival"
    );
}

#[test]
fn antigravity_ignores_legacy_gemini_layout_under_shared_dot_gemini() {
    // Both agy and the legacy Gemini CLI live under ~/.gemini. Rooted at the
    // shared parent containing BOTH layouts, the agy connector must resolve only
    // the agy conversation.
    let tmp = tempfile::TempDir::new().unwrap();
    let dot_gemini = tmp.path().join(".gemini");

    let chats = dot_gemini.join("tmp").join("deadbeef").join("chats");
    fs::create_dir_all(&chats).unwrap();
    fs::write(chats.join("session-1.json"), "{\"messages\":[]}").unwrap();

    let logs = dot_gemini
        .join("antigravity-cli")
        .join("brain")
        .join("99990000-1111-2222-3333-444455556666")
        .join(".system_generated")
        .join("logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(
        logs.join("transcript.jsonl"),
        "{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:42Z\",\"content\":\"<USER_REQUEST>\\nhi\\n</USER_REQUEST>\"}\n",
    )
    .unwrap();

    let base = dot_gemini.clone();
    let ctx = ScanContext {
        data_dir: base.clone(),
        scan_roots: vec![ScanRoot::local(base.clone())],
        since_ts: None,
        progress_tick: None,
    };
    let convs = AntigravityConnector::new().scan(&ctx).expect("scan");
    assert_eq!(convs.len(), 1, "only the agy conversation should be found");
    assert_eq!(convs[0].agent_slug, "antigravity");
}

#[test]
fn antigravity_prunes_non_session_trees_in_production_factory() {
    let tmp = tempfile::TempDir::new().unwrap();
    let base = tmp.path().join("antigravity-cli");
    let conversation = base.join("brain").join(FIXTURE_UUID);
    let real_logs = conversation.join(".system_generated/logs");
    fs::create_dir_all(&real_logs).unwrap();
    let fixture_transcript = fixture_base()
        .join("brain")
        .join(FIXTURE_UUID)
        .join(".system_generated/logs/transcript.jsonl");
    fs::copy(&fixture_transcript, real_logs.join("transcript.jsonl")).unwrap();

    for excluded in [".git/objects/decoy", "skills/decoy"] {
        let decoy_logs = conversation.join(excluded).join(".system_generated/logs");
        fs::create_dir_all(&decoy_logs).unwrap();
        fs::copy(&fixture_transcript, decoy_logs.join("transcript.jsonl")).unwrap();
    }

    let ctx = ScanContext {
        data_dir: base.clone(),
        scan_roots: vec![ScanRoot::local(base.clone())],
        since_ts: None,
        progress_tick: None,
    };
    let connector = coding_agent_search::connectors::get_connector_factories()
        .into_iter()
        .find(|(name, _)| *name == "antigravity")
        .map(|(_, factory)| factory())
        .expect("production antigravity factory");
    let conversations = connector.scan(&ctx).unwrap();
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].external_id.as_deref(), Some(FIXTURE_UUID));

    let discovered = connector.discover_source_files(&ctx).unwrap();
    assert!(
        discovered.iter().all(|source| source.scan_root == base),
        "source provenance must retain the original scan root"
    );
    assert_eq!(
        discovered
            .iter()
            .filter(|source| source.role == DiscoveredSourceRole::PrimarySessionLog)
            .count(),
        1,
        "decoy transcripts below .git and skills must not be discovered"
    );
}

/// The migration must not regress the legacy Gemini CLI connector: its own
/// fixture still scans into conversations labeled "gemini".
#[test]
fn legacy_gemini_connector_still_indexes() {
    let ctx = ScanContext {
        data_dir: PathBuf::from("tests/fixtures/gemini"),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = GeminiConnector::new().scan(&ctx).expect("gemini scan");
    assert!(!convs.is_empty(), "legacy gemini fixture must still index");
    assert!(convs.iter().all(|c| c.agent_slug == "gemini"));
}

fn workspace_fixture(base: &Path, native_id: &str, workspace_uris: &str) -> Result<ScanContext> {
    let logs = base
        .join("brain")
        .join(FIXTURE_UUID)
        .join(".system_generated/logs");
    fs::create_dir_all(&logs)?;
    fs::copy(
        fixture_base()
            .join("brain")
            .join(FIXTURE_UUID)
            .join(".system_generated/logs/transcript.jsonl"),
        logs.join("transcript.jsonl"),
    )?;
    let db = Connection::open(
        base.join("conversation_summaries.db")
            .to_string_lossy()
            .to_string(),
    )?;
    db.execute("CREATE TABLE conversation_summaries (conversation_id TEXT PRIMARY KEY, workspace_uris TEXT NOT NULL)")?;
    db.execute_compat(
        "INSERT INTO conversation_summaries VALUES (?1, ?2)",
        params![native_id, workspace_uris],
    )?;
    Ok(ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf())],
        None,
    ))
}

#[test]
fn native_workspace_binding_is_read_only_and_streaming_matches_scan() -> Result<()> {
    let tmp = tempfile::TempDir::new()?;
    let base = tmp.path().join("antigravity-cli");
    let ctx = workspace_fixture(&base, FIXTURE_UUID, r#"["file:///work/exact%20workspace"]"#)?;
    let database = base.join("conversation_summaries.db");
    let original = fs::read(&database)?;
    let connector = AntigravityConnector::new();
    let conversations = connector.scan(&ctx)?;
    let conversation = conversations
        .first()
        .context("native conversation missing")?;
    ensure!(conversation.workspace.as_deref() == Some(Path::new("/work/exact workspace")));
    ensure!(conversation.metadata["workspace_binding"]["state"] == "bound");
    let mut streamed = Vec::new();
    connector.scan_with_callback(&ctx, &mut |c| {
        streamed.push(c);
        Ok(())
    })?;
    ensure!(serde_json::to_value(&streamed)? == serde_json::to_value(&conversations)?);
    ensure!(
        fs::read(&database)? == original,
        "agent-owned summary database changed"
    );
    let sources = connector.discover_source_files(&ctx)?;
    ensure!(sources.iter().any(|source| source.source_path == database
        && source.role == DiscoveredSourceRole::MetadataSidecar
        && source.scan_root == base));
    Ok(())
}

#[test]
fn explicit_native_transcript_root_preserves_workspace_and_discovery() -> Result<()> {
    let tmp = tempfile::TempDir::new()?;
    let base = tmp.path().join(".gemini/antigravity-cli");
    let mut ctx = workspace_fixture(&base, FIXTURE_UUID, r#"["file:///work/exact"]"#)?;
    let transcript = base
        .join("brain")
        .join(FIXTURE_UUID)
        .join(".system_generated/logs/transcript.jsonl");
    ctx.scan_roots = vec![ScanRoot::local(transcript.clone())];
    let connector = AntigravityConnector::new();
    let conversations = connector.scan(&ctx)?;
    ensure!(conversations.len() == 1);
    ensure!(conversations[0].workspace.as_deref() == Some(Path::new("/work/exact")));
    let sources = connector.discover_source_files(&ctx)?;
    ensure!(
        sources
            .iter()
            .any(|source| source.source_path == transcript)
    );
    ensure!(
        sources
            .iter()
            .any(|source| source.source_path == base.join("conversation_summaries.db"))
    );
    ensure!(sources.iter().all(|source| source.scan_root == transcript));
    let mut streamed = Vec::new();
    connector.scan_with_callback(&ctx, &mut |c| {
        streamed.push(c);
        Ok(())
    })?;
    ensure!(serde_json::to_value(&streamed)? == serde_json::to_value(&conversations)?);
    Ok(())
}

#[test]
fn workspace_binding_does_not_guess_missing_ambiguous_or_invalid_roots() -> Result<()> {
    for (native_id, uris, state) in [
        (
            "different-session",
            r#"["file:///work/elsewhere"]"#,
            "missing",
        ),
        (
            FIXTURE_UUID,
            r#"["file:///work/a","file:///work/b"]"#,
            "ambiguous",
        ),
        (FIXTURE_UUID, "[]", "missing"),
        (FIXTURE_UUID, "not-json", "invalid"),
        (
            FIXTURE_UUID,
            r#"["https://example.invalid/workspace"]"#,
            "invalid",
        ),
        (
            FIXTURE_UUID,
            r#"["file:///work/a","file://remote-host/work/b"]"#,
            "invalid",
        ),
    ] {
        let tmp = tempfile::TempDir::new()?;
        let ctx = workspace_fixture(&tmp.path().join("antigravity-cli"), native_id, uris)?;
        let conversations = AntigravityConnector::new().scan(&ctx)?;
        let c = conversations
            .first()
            .context("native conversation missing")?;
        ensure!(c.workspace.is_none(), "unexpected binding for {uris}");
        ensure!(
            c.metadata["workspace_binding"]["state"] == state,
            "wrong state for {uris}"
        );
    }
    Ok(())
}

#[test]
fn native_workspace_binding_preserves_ide_and_cli_identity() -> Result<()> {
    let tmp = tempfile::TempDir::new()?;
    let cli = workspace_fixture(
        &tmp.path().join(".gemini/antigravity-cli"),
        FIXTURE_UUID,
        r#"["file:///work/cli"]"#,
    )?;
    let ide = workspace_fixture(
        &tmp.path().join(".gemini/antigravity"),
        FIXTURE_UUID,
        r#"["file:///work/ide"]"#,
    )?;
    let mut ctx = cli;
    ctx.scan_roots.extend(ide.scan_roots);
    let conversations = AntigravityConnector::new().scan(&ctx)?;
    ensure!(conversations.len() == 2);
    let ide_id = format!("ide/{FIXTURE_UUID}");
    ensure!(
        conversations
            .iter()
            .any(|c| c.external_id.as_deref() == Some(FIXTURE_UUID)
                && c.workspace.as_deref() == Some(Path::new("/work/cli")))
    );
    ensure!(
        conversations
            .iter()
            .any(|c| c.external_id.as_deref() == Some(ide_id.as_str())
                && c.workspace.as_deref() == Some(Path::new("/work/ide")))
    );
    Ok(())
}

#[test]
fn changed_native_metadata_reindexes_an_unchanged_transcript() -> Result<()> {
    let tmp = tempfile::TempDir::new()?;
    let base = tmp.path().join("antigravity-cli");
    let mut ctx = workspace_fixture(&base, FIXTURE_UUID, r#"["file:///work/bound"]"#)?;
    let transcript = base
        .join("brain")
        .join(FIXTURE_UUID)
        .join(".system_generated/logs/transcript.jsonl");
    fs::File::open(&transcript)?
        .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1))?;
    ctx.since_ts = Some(2000);
    let conversations = AntigravityConnector::new().scan(&ctx)?;
    let c = conversations
        .first()
        .context("metadata change failed to refresh old transcript")?;
    ensure!(c.workspace.as_deref() == Some(Path::new("/work/bound")));
    Ok(())
}
