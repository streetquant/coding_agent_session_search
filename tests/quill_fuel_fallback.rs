//! GH518: ordinary CLI queries on a fragmented index, plus explicitly opted-in
//! supervised SQLite recovery after a real Quill refusal.
//! No environment mutation in the test process: fuel belongs to each child.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::search::quill_bridge::live_segment_count;
use coding_agent_search::search::tantivy::{TantivyIndex, expected_index_dir};
use coding_agent_search::storage::sqlite::FrankenStorage;
use frankensearch::quill::cass::CassDocument;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wait_timeout::ChildExt;

struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn serve(
    root: &Path,
    index: &Path,
    archive: Option<&Path>,
    enabled: bool,
    fuel: u64,
    requests: &[Value],
) -> Result<Vec<Value>> {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .args(["serve", "--stdio", "--index"])
        .arg(index)
        .current_dir(root)
        .env("CASS_QUILL_QUERY_FUEL_BUDGET", fuel.to_string())
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(archive) = archive {
        command.arg("--db").arg(archive);
    }
    if enabled {
        command.arg("--lexical-fallback");
    }
    let mut child = Child(command.spawn()?);
    let mut input = child.0.stdin.take().context("child stdin")?;
    for request in requests {
        writeln!(input, "{request}")?;
    }
    drop(input);
    let status = child
        .0
        .wait_timeout(Duration::from_secs(40))?
        .context("service did not terminate after EOF")?;
    // This fixture sends only a few small responses, well below pipe capacity.
    // Read after exit; the Drop guard reaps on every early failure as well.
    let mut stdout = String::new();
    let mut stderr = String::new();
    use std::io::Read;
    child
        .0
        .stdout
        .take()
        .context("child stdout")?
        .read_to_string(&mut stdout)?;
    child
        .0
        .stderr
        .take()
        .context("child stderr")?
        .read_to_string(&mut stderr)?;
    ensure!(status.success(), "worker exited {status}: {stderr}");
    stdout
        .lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn document(conversation_id: i64, ordinal: usize, content: &str) -> CassDocument {
    CassDocument {
        agent: "codex".into(),
        workspace: Some("/work".into()),
        workspace_original: None,
        source_path: format!("/history/gh518/{ordinal}.jsonl"),
        msg_idx: 0,
        created_at: Some(1_700_000_000_000 + ordinal as i64),
        title: Some(format!("gh518 case {ordinal}")),
        content: content.into(),
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: Some(conversation_id),
    }
}

fn fixture(root: &Path) -> Result<(PathBuf, PathBuf, Vec<i64>)> {
    let archive = root.join("archive.db");
    let index = expected_index_dir(root);
    let storage = FrankenStorage::open(&archive)?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    let mut writer = TantivyIndex::open_or_create(&index)?;
    let mut identities = Vec::new();
    for (ordinal, content) in [
        "git fixture establishes the repository test environment",
        "test isolation prevents state from leaking between cases",
    ]
    .iter()
    .enumerate()
    {
        let inserted = storage.insert_conversation_tree(
            agent,
            None,
            &Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: None,
                external_id: Some(format!("gh518-{ordinal}")),
                title: Some(format!("gh518 case {ordinal}")),
                source_path: format!("/history/gh518/{ordinal}.jsonl").into(),
                started_at: None,
                ended_at: None,
                approx_tokens: None,
                metadata_json: json!({}),
                source_id: "local".into(),
                origin_host: None,
                messages: vec![Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::Agent,
                    author: None,
                    created_at: Some(1_700_000_000_000 + ordinal as i64),
                    content: (*content).into(),
                    extra_json: json!({}),
                    snippets: Vec::new(),
                }],
            },
        )?;
        writer.add_prebuilt_documents_slice(&[document(
            inserted.conversation_id,
            ordinal,
            content,
        )])?;
        identities.push(inserted.conversation_id);
    }
    writer.commit()?;
    drop(writer);
    drop(storage);
    Ok((index, archive, identities))
}

const FRAGMENTED_SEGMENTS: usize = 206;
const DOCS_PER_SEGMENT: usize = 16;

fn binary_sha256(path: &Path) -> Result<String> {
    use std::fmt::Write as _;
    use std::io::Read;
    let mut binary = std::fs::File::open(path).context("opening selected CLI binary")?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = binary.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let mut sha256 = String::with_capacity(64);
    for byte in digest.finalize() {
        write!(&mut sha256, "{byte:02x}")?;
    }
    Ok(sha256)
}

fn selected_cli_binary() -> Result<(PathBuf, String)> {
    let optional_env = |name: &str| match dotenvy::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(dotenvy::Error::EnvVar(std::env::VarError::NotPresent)) => Ok(None),
        Err(error) => Err(anyhow::anyhow!("invalid {name}: {error}")),
    };
    let override_path = optional_env("CASS_TEST_CANDIDATE_BIN")?;
    let expected = optional_env("CASS_TEST_CANDIDATE_SHA256")?;
    ensure!(
        override_path.is_some() || expected.is_none(),
        "CASS_TEST_CANDIDATE_SHA256 requires CASS_TEST_CANDIDATE_BIN"
    );
    let path = PathBuf::from(
        override_path
            .as_deref()
            .unwrap_or(env!("CARGO_BIN_EXE_cass")),
    );
    ensure!(
        path.is_file(),
        "selected CLI binary is not a file: {}",
        path.display()
    );
    let path = path
        .canonicalize()
        .context("resolving selected CLI binary")?;
    let sha256 = binary_sha256(&path)?;
    if override_path.is_some() {
        let expected =
            expected.context("candidate override requires CASS_TEST_CANDIDATE_SHA256")?;
        ensure!(
            expected.len() == 64 && expected.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "CASS_TEST_CANDIDATE_SHA256 must contain 64 hexadecimal digits"
        );
        ensure!(
            sha256 == expected.to_ascii_lowercase(),
            "selected candidate SHA256 mismatch"
        );
    }
    eprintln!(
        "GH518_BINARY SHA256={sha256} OVERRIDE={} PATH={}",
        override_path.is_some(),
        path.display()
    );
    Ok((path, sha256))
}

fn fragmented_index(root: &Path) -> Result<PathBuf> {
    let index = expected_index_dir(root);
    let append = |writer: &mut TantivyIndex, segment: usize| -> Result<()> {
        let documents: Vec<_> = (0..DOCS_PER_SEGMENT)
            .map(|within_segment| {
                let ordinal = segment * DOCS_PER_SEGMENT + within_segment;
                document(
                    ordinal as i64 + 1,
                    ordinal,
                    &format!("git fixture test isolation segment{segment} item{within_segment}"),
                )
            })
            .collect();
        writer.add_prebuilt_documents_slice(&documents)?;
        writer.commit()?;
        Ok(())
    };
    let mut writer = TantivyIndex::open_or_create(&index)?;
    for segment in 0..FRAGMENTED_SEGMENTS / 2 {
        append(&mut writer, segment)?;
    }
    drop(writer);
    // Reopening writers reserves sparse document-ID ranges. Exercise those
    // separately from continuous commits without assuming the reporter's layout.
    for segment in FRAGMENTED_SEGMENTS / 2..FRAGMENTED_SEGMENTS {
        let mut writer = TantivyIndex::open_or_create(&index)?;
        append(&mut writer, segment)?;
    }
    assert_eq!(
        live_segment_count(&index),
        Some(FRAGMENTED_SEGMENTS),
        "fixture must retain its actual live fragmentation"
    );
    Ok(index)
}

fn ordinary_search(
    binary: &Path,
    root: &Path,
    query: &str,
    fuel: Option<u64>,
    timeout_ms: u64,
    robot: bool,
) -> assert_cmd::Command {
    // ubs:ignore[rust.security.command-executable] -- Test-only Cargo executable or explicit SHA256-checked release candidate; archive and query inputs never select this path.
    let mut command = assert_cmd::Command::new(binary);
    command
        .current_dir(root)
        .env("HOME", root)
        .env("XDG_DATA_HOME", root.join(".local/share"))
        .env("XDG_CONFIG_HOME", root.join(".config"))
        .env("XDG_CACHE_HOME", root.join(".cache"))
        .env("CODEX_HOME", root.join(".codex"))
        .env("CASS_DATA_DIR", root)
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env_remove("CASS_QUILL_QUERY_FUEL_BUDGET")
        .env_remove("CASS_AUTOMATIC_WILDCARD_FALLBACK_MAX_DOCS")
        .env_remove("CASS_DB_PATH")
        .env_remove("CASS_OUTPUT_FORMAT")
        .env_remove("TOON_DEFAULT_FORMAT")
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "error")
        .args([
            "search",
            query,
            "--mode",
            "lexical",
            "--limit",
            "1",
            "--no-daemon",
        ])
        .arg("--timeout")
        .arg(timeout_ms.to_string())
        // A deadline here fails the test; it cannot become a successful empty
        // search response through the CLI's optional request timeout.
        .timeout(Duration::from_secs(40));
    if let Some(fuel) = fuel {
        command.env("CASS_QUILL_QUERY_FUEL_BUDGET", fuel.to_string());
    }
    if robot {
        command.args(["--robot", "--robot-meta"]);
    }
    command
}

#[test]
fn gh518_ordinary_cli_answers_both_queries_and_no_match_on_206_sparse_segments() -> Result<()> {
    // A supplied release candidate must match its attested identity. Invalid
    // overrides fail before fixture creation and never fall back to Cargo's ELF.
    let (binary, initial_sha256) = selected_cli_binary()?;
    let temp = tempfile::tempdir()?;
    // Keep dotenvy from loading configuration outside this disposable fixture.
    std::fs::write(temp.path().join(".env"), b"")?;
    let index = fragmented_index(temp.path())?;
    let mut first_identity = None;
    for query in ["git fixture", "test isolation", "zzz qqq", "git fixture"] {
        let started = std::time::Instant::now();
        let output = ordinary_search(&binary, temp.path(), query, None, 0, true)
            .assert()
            .success()
            .get_output()
            .clone();
        let payload: Value = serde_json::from_slice(&output.stdout)?;
        let hits = payload["hits"].as_array().context("ordinary CLI hits")?;
        assert_eq!(hits.len(), usize::from(query != "zzz qqq"), "{payload}");
        assert_eq!(payload["_meta"]["search_mode"], "lexical", "{payload}");
        assert_ne!(payload["budget"]["timed_out"], true, "{payload}");
        assert_eq!(payload["_meta"]["wildcard_fallback"], false, "{payload}");
        assert!(
            payload["_meta"]["lexical_degrade_reason"].is_null(),
            "{payload}"
        );
        if query == "git fixture" {
            let identity = hits[0]["source_path"]
                .as_str()
                .context("ordinary CLI result source identity")?;
            assert!(identity.starts_with("/history/gh518/"), "{payload}");
            if let Some(first) = &first_identity {
                assert_eq!(identity, first, "a later request must preserve identity");
            } else {
                first_identity = Some(identity.to_string());
            }
        }
        eprintln!(
            "GH518 ordinary CLI: segments={FRAGMENTED_SEGMENTS} documents={} query={query:?} wall_ms={} hits={}",
            FRAGMENTED_SEGMENTS * DOCS_PER_SEGMENT,
            started.elapsed().as_millis(),
            hits.len()
        );
    }
    assert_eq!(live_segment_count(&index), Some(FRAGMENTED_SEGMENTS));

    // The same ordinary CLI must retain the actionable error when even its
    // required primary lookup is refused. This is distinct from an optional
    // wildcard retry, which may return the already successful exact page.
    let explicit_archive = temp.path().join("explicit archive.db");
    for (timeout_ms, archive) in [
        (0, None),
        (120_000, None),
        (0, Some(explicit_archive.as_path())),
    ] {
        let mut command = ordinary_search(
            &binary,
            temp.path(),
            "git fixture",
            Some(1),
            timeout_ms,
            true,
        );
        if let Some(archive) = archive {
            command.arg("--db").arg(archive);
        }
        let output = command.assert().failure().get_output().clone();
        assert_eq!(output.status.code(), Some(9));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        let envelope: Value = serde_json::from_str(
            stderr
                .lines()
                .rfind(|line| !line.trim().is_empty())
                .context("robot fuel-refusal envelope")?,
        )?;
        assert_eq!(envelope["error"]["kind"], "search", "{envelope}");
        let message = envelope["error"]["message"]
            .as_str()
            .context("fuel refusal message")?;
        assert!(message.contains("query fuel exhausted"), "{envelope}");
        let hint = envelope["error"]["hint"]
            .as_str()
            .context("fuel refusal recovery hint")?;
        assert!(hint.contains("index --full --json"), "{envelope}");
        assert!(
            hint.contains(&temp.path().display().to_string()),
            "{envelope}"
        );
        if let Some(archive) = archive {
            assert!(hint.contains("cass --db"), "{envelope}");
            assert!(hint.contains(&archive.display().to_string()), "{envelope}");
        }
    }
    let output = ordinary_search(&binary, temp.path(), "git fixture", Some(1), 0, false)
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_eq!(output.status.code(), Some(9));
    let failure = String::from_utf8_lossy(&output.stderr);
    assert!(failure.contains("query fuel exhausted"), "{failure}");
    assert!(failure.contains("cass index --full"), "{failure}");
    assert!(
        failure.lines().any(|line| line.starts_with("Hint: ")),
        "{failure}"
    );
    assert!(
        failure.contains(&temp.path().display().to_string()),
        "{failure}"
    );
    let final_sha256 = binary_sha256(&binary)?;
    assert_eq!(
        final_sha256, initial_sha256,
        "selected CLI binary changed during acceptance"
    );
    eprintln!("GH518_BINARY_AFTER SHA256={final_sha256}");
    Ok(())
}

#[test]
fn gh518_fuel_refusal_uses_explicit_archive_and_reports_actual_backend() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (index, archive, identities) = fixture(temp.path())?;
    let before = std::fs::read(&archive)?;
    let replies = serve(
        temp.path(),
        &index,
        Some(&archive),
        true,
        1,
        &[
            json!({"id":1,"op":"search","query":"git fixture","limit":1}),
            json!({"id":2,"op":"search","query":"test isolation","limit":1}),
            json!({"id":3,"op":"status"}),
        ],
    )?;
    assert_eq!(replies.len(), 3);
    for (ordinal, reply) in replies[..2].iter().enumerate() {
        assert_eq!(reply["ok"], true, "{reply}");
        let result = &reply["result"];
        assert_eq!(result["backend"], "frankensqlite");
        assert_eq!(result["degraded"], true);
        assert_eq!(result["reader_reused"], false);
        assert!(result["result_reader_epoch"].is_null());
        assert_eq!(result["fallback"]["reason"], "quill_query_fuel_exhausted");
        assert!(result["fallback"]["sqlite_lane"].is_null());
        assert_eq!(result["count"], 1);
        assert_eq!(result["hits"][0]["conversation_id"], identities[ordinal]);
        assert_eq!(result["hits"][0]["message_index"], 1);
    }
    let state = &replies[2]["result"];
    assert_eq!(state["successful_opens"], 1);
    assert_eq!(state["lexical_fallback"]["attempts"], 2);
    assert_eq!(state["lexical_fallback"]["completed"], 2);
    assert_eq!(state["canonical_database_accessed"], true);
    assert_eq!(state["maintenance_performed"], false);
    assert_eq!(std::fs::read(&archive)?, before);
    Ok(())
}

#[test]
fn gh518_db_option_alone_does_not_grant_fallback_consent() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (index, _, _) = fixture(temp.path())?;
    let corrupt = temp.path().join("must-not-open.db");
    std::fs::write(&corrupt, b"not SQLite")?;
    let replies = serve(
        temp.path(),
        &index,
        Some(&corrupt),
        false,
        1,
        &[
            json!({"id":1,"op":"search","query":"git fixture","limit":1}),
            json!({"id":2,"op":"status"}),
        ],
    )?;
    assert_eq!(replies[0]["ok"], false);
    let message = replies[0]["error"]["message"].as_str().unwrap();
    assert!(message.contains("fallback is disabled"), "{message}");
    assert!(message.contains("query fuel exhausted"), "{message}");
    assert!(message.contains("cass index --full --json"), "{message}");
    assert_eq!(replies[1]["result"]["canonical_database_accessed"], false);
    assert_eq!(replies[1]["result"]["lexical_fallback"]["attempts"], 0);
    assert_eq!(std::fs::read(&corrupt)?, b"not SQLite");
    Ok(())
}

#[test]
fn gh518_oversized_and_corrupt_fallbacks_preserve_the_original_refusal() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (index, _, _) = fixture(temp.path())?;
    let oversized = temp.path().join("oversized.db");
    std::fs::File::create(&oversized)?.set_len(64 * 1024 * 1024 + 1)?;
    let corrupt = temp.path().join("corrupt.db");
    std::fs::write(&corrupt, b"not SQLite")?;
    for path in [&oversized, &corrupt] {
        let replies = serve(
            temp.path(),
            &index,
            Some(path),
            true,
            1,
            &[
                json!({"id":1,"op":"search","query":"git fixture","limit":1}),
                json!({"id":2,"op":"status"}),
            ],
        )?;
        assert_eq!(replies[0]["ok"], false, "{}", replies[0]);
        let message = replies[0]["error"]["message"].as_str().unwrap();
        assert!(message.contains("query fuel exhausted"), "{message}");
        assert!(message.contains("fallback failed"), "{message}");
        assert!(message.contains("cass index --full --json"), "{message}");
        if path == &oversized {
            assert!(
                message.contains("exceeds the automatic search budget"),
                "{message}"
            );
        }
        assert_eq!(replies[1]["result"]["lexical_fallback"]["completed"], 0);
    }
    Ok(())
}

#[test]
fn gh518_authoritative_empty_result_never_falls_back_to_archive_matches() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (_, archive, _) = fixture(temp.path())?;
    let index = temp.path().join("different-published-snapshot");
    let mut writer = TantivyIndex::open_or_create(&index)?;
    writer.add_prebuilt_documents_slice(&[document(99, 99, "unrelated corpus")])?;
    writer.commit()?;
    drop(writer);
    let replies = serve(
        temp.path(),
        &index,
        Some(&archive),
        true,
        10_000_000,
        &[
            json!({"id":1,"op":"search","query":"git fixture","limit":1}),
            json!({"id":2,"op":"status"}),
        ],
    )?;
    assert_eq!(replies[0]["ok"], true, "{}", replies[0]);
    assert_eq!(replies[0]["result"]["count"], 0);
    assert_eq!(replies[0]["result"]["backend"], "quill");
    assert_eq!(replies[0]["result"]["degraded"], false);
    assert_eq!(replies[1]["result"]["lexical_fallback"]["attempts"], 0);
    assert_eq!(replies[1]["result"]["canonical_database_accessed"], false);
    Ok(())
}

#[test]
fn gh518_206_real_segments_serve_both_two_word_queries_without_fallback() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let index = expected_index_dir(temp.path());
    let mut writer = TantivyIndex::open_or_create(&index)?;
    for segment in 0..206 {
        writer.add_prebuilt_documents_slice(&[document(
            segment as i64 + 1,
            segment,
            &format!("git fixture test isolation segment{segment}"),
        )])?;
        writer.commit()?;
    }
    drop(writer);
    assert_eq!(
        live_segment_count(&index),
        Some(206),
        "fixture must remain fragmented"
    );
    let replies = serve(
        temp.path(),
        &index,
        None,
        false,
        80_000,
        &[
            json!({"id":1,"op":"search","query":"git fixture","limit":1}),
            json!({"id":2,"op":"search","query":"test isolation","limit":1}),
        ],
    )?;
    assert_eq!(replies.len(), 2);
    for reply in &replies {
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(reply["result"]["count"], 1);
        assert_eq!(reply["result"]["backend"], "quill");
        assert_eq!(reply["result"]["degraded"], false);
        assert_eq!(
            reply["result"]["snapshot"]["lexical_fallback"]["attempts"],
            0
        );
    }
    Ok(())
}
