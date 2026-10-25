//! Exercise the real binary module and its Quill-backed session regressions.
// The run entrypoint is exercised through the real binary below.
#[allow(dead_code)]
#[path = "../src/search_service.rs"]
mod search_service;

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

#[test]
fn cass_serve_dispatches_stdio_without_entering_ordinary_cli_setup() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let absent = temp.path().join("explicit-never-opened-index");
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["serve", "--stdio", "--index"])
        .arg(&absent)
        .env("CASS_DATA_DIR", temp.path().join("unrelated"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().unwrap();
    input.write_all(b"{\"op\":\"status\",\"id\":1}\n{\"op\":\"shutdown\",\"id\":2}\n")?;
    drop(input);
    if child.wait_timeout(Duration::from_secs(20))?.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("stdio service did not terminate after shutdown");
    }
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replies: Vec<serde_json::Value> = std::str::from_utf8(&output.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[0]["result"]["loaded"], false);
    assert_eq!(replies[0]["result"]["open_attempts"], 0);
    assert_eq!(replies[1]["result"]["shutdown"], true);
    assert!(!absent.exists());
    assert!(!temp.path().join("unrelated").exists());
    Ok(())
}

#[test]
fn cass_serve_help_is_available_without_archive_access() {
    let output = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["serve", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for flag in ["--index", "--data-dir", "--stdio"] {
        assert!(help.contains(flag), "{help}");
    }
}

#[test]
fn cass_serve_searches_repeatedly_through_the_real_cli() -> anyhow::Result<()> {
    use coding_agent_search::search::tantivy::{TantivyIndex, expected_index_dir};
    use frankensearch::quill::cass::CassDocument;

    let temp = tempfile::tempdir()?;
    let path = expected_index_dir(temp.path());
    let mut writer = TantivyIndex::open_or_create(&path)?;
    writer.add_prebuilt_documents_slice(&[CassDocument {
        agent: "codex".into(),
        workspace: Some("/work".into()),
        workspace_original: None,
        source_path: "/history/session.jsonl".into(),
        msg_idx: 12,
        created_at: Some(1_700_000_000_000),
        title: Some("fixture".into()),
        content: "retainedreaderneedle".into(),
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: Some(42),
    }])?;
    writer.commit()?;
    drop(writer);
    std::fs::write(temp.path().join("agent_search.db"), b"must not be opened")?;
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["serve", "--stdio", "--data-dir"])
        .arg(temp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().unwrap();
    for id in [1, 2] {
        writeln!(
            input,
            "{}",
            serde_json::json!({
                "op": "search", "id": id, "query": "retainedreaderneedle", "limit": 1
            })
        )?;
    }
    // EOF is also a clean lifecycle boundary; no explicit shutdown required.
    drop(input);
    if child.wait_timeout(Duration::from_secs(30))?.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("search worker did not terminate after EOF");
    }
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replies: Vec<serde_json::Value> = std::str::from_utf8(&output.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(replies.len(), 2);
    for (position, reply) in replies.iter().enumerate() {
        assert_eq!(reply["ok"], true, "{reply}");
        let result = &reply["result"];
        assert_eq!(result["count"], 1);
        assert_eq!(result["reader_reused"], position > 0);
        assert_eq!(result["snapshot"]["successful_opens"], 1);
        assert_eq!(result["hits"][0]["conversation_id"], 42);
        assert_eq!(result["hits"][0]["message_index"], 13);
        assert_eq!(result["hits"][0]["source_id"], "local");
    }
    assert_eq!(
        std::fs::read(temp.path().join("agent_search.db"))?,
        b"must not be opened"
    );
    Ok(())
}

#[test]
fn cass_serve_mcp_dispatches_real_negotiation_without_archive_access() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let absent = temp.path().join("never-opened");
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["serve", "--stdio", "--mcp", "--index"])
        .arg(&absent)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().unwrap();
    for request in [
        serde_json::json!({"jsonrpc": "2.0", "id": "handshake", "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "fixture", "version": "1"}}}),
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        serde_json::json!({"jsonrpc": "2.0", "id": "catalog", "method": "tools/list"}),
        serde_json::json!({"jsonrpc": "2.0", "id": "status", "method": "tools/call",
            "params": {"name": "cass_status", "arguments": {}}}),
    ] {
        writeln!(input, "{request}")?;
    }
    drop(input);
    // tools/list exceeds an 8 KiB pipe on Linux. Drain while the service is
    // alive: waiting first blocks its write, so it can never observe EOF.
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut bytes).map(|_| bytes)
    });
    if child.wait_timeout(Duration::from_secs(20))?.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        anyhow::bail!("MCP service did not terminate after EOF");
    }
    let mut output = child.wait_with_output()?;
    output.stdout = reader
        .join()
        .map_err(|_| anyhow::anyhow!("MCP output reader panicked"))??;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replies: Vec<serde_json::Value> = std::str::from_utf8(&output.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(replies.len(), 3, "notifications get no response");
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 4);
    assert_eq!(replies[2]["id"], "status");
    assert_eq!(
        replies[2]["result"]["structuredContent"]["open_attempts"],
        0
    );
    assert!(!absent.exists());
    Ok(())
}

#[test]
fn cass_serve_reads_explicit_canonical_archive_without_opening_an_index() -> anyhow::Result<()> {
    use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
    use coding_agent_search::storage::sqlite::FrankenStorage;
    use serde_json::{Value, json};

    let temp = tempfile::tempdir()?;
    let db = temp.path().join("archive.db");
    let storage = FrankenStorage::open(&db)?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    let outcome = storage.insert_conversation_tree(
        agent,
        None,
        &Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: None,
            external_id: Some("binary-canonical-service".into()),
            title: None,
            source_path: "/absent/source.jsonl".into(),
            started_at: None,
            ended_at: None,
            approx_tokens: None,
            metadata_json: json!({}),
            source_id: "local".into(),
            origin_host: None,
            messages: vec![Message {
                id: None,
                idx: 12,
                role: MessageRole::Agent,
                author: None,
                created_at: None,
                content: "complete canonical evidence δ".into(),
                extra_json: json!({}),
                snippets: Vec::new(),
            }],
        },
    )?;
    drop(storage);
    let before = std::fs::read(&db)?;
    for enabled in [false, true] {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        command
            .args(["serve", "--stdio", "--index"])
            .arg(temp.path().join("absent-index"));
        if enabled {
            command.arg("--db").arg(&db);
        }
        let mut child = command
            .current_dir(temp.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut input = child.stdin.take().unwrap();
        writeln!(
            input,
            "{}",
            json!({"op":"view", "id":1,
            "source_path":"/absent/source.jsonl", "source_id":"local",
            "conversation_id":outcome.conversation_id, "message_index":13})
        )?;
        writeln!(input, "{}", json!({"op":"status", "id":2}))?;
        drop(input);
        if child.wait_timeout(Duration::from_secs(20))?.is_none() {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("canonical service failed to terminate after EOF");
        }
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let replies: Vec<Value> = std::str::from_utf8(&output.stdout)?
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0]["ok"], enabled, "{}", replies[0]);
        if enabled {
            assert_eq!(
                replies[0]["result"]["messages"][0]["content"],
                "complete canonical evidence δ"
            );
            assert_eq!(replies[0]["result"]["messages"][0]["message_index"], 13);
        } else {
            assert_eq!(replies[0]["error"]["kind"], "canonical_access_disabled");
        }
        assert_eq!(replies[1]["result"]["open_attempts"], 0);
        assert_eq!(
            replies[1]["result"]["canonical_read_attempts"],
            u64::from(enabled)
        );
        assert!(!temp.path().join("absent-index").exists());
        assert_eq!(std::fs::read(&db)?, before);
    }
    Ok(())
}

// Keep every owned child reaped even when a regression assertion fails.
struct DeadlineChild(std::process::Child);

impl Drop for DeadlineChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn deadline_child(index: &std::path::Path, mcp: bool) -> std::io::Result<DeadlineChild> {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command.args(["serve", "--stdio", "--request-timeout-ms", "500", "--index"]);
    command.arg(index);
    if mcp {
        command.arg("--mcp");
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map(DeadlineChild)
}

#[test]
fn partial_jsonl_and_mcp_frames_cannot_hold_the_worker_forever() -> anyhow::Result<()> {
    for mcp in [false, true] {
        let temp = tempfile::tempdir()?;
        let index = temp.path().join("never-opened");
        let mut child = deadline_child(&index, mcp)?;
        let mut input = child.0.stdin.take().unwrap();
        input.write_all(b"{\"unfinished\":")?;
        input.flush()?;
        // Keep stdin open: EOF would end the frame without exercising the limit.
        let status = child.0.wait_timeout(Duration::from_secs(10))?;
        assert_eq!(status.and_then(|status| status.code()), Some(124));
        assert!(!index.exists());
        drop(input);
    }
    Ok(())
}

#[test]
fn idle_connections_and_completed_requests_do_not_inherit_old_deadlines() -> anyhow::Result<()> {
    use std::io::{BufRead, BufReader};
    for mcp in [false, true] {
        let temp = tempfile::tempdir()?;
        let index = temp.path().join("never-opened");
        let mut child = deadline_child(&index, mcp)?;
        let mut input = child.0.stdin.take().unwrap();
        let mut output = BufReader::new(child.0.stdout.take().unwrap());
        let (sender, receiver) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            for _ in 0..2 {
                let mut line = String::new();
                let result = output.read_line(&mut line).map(|_| line);
                if sender.send(result).is_err() {
                    break;
                }
            }
        });
        for id in [1, 2] {
            std::thread::sleep(Duration::from_millis(750));
            assert!(
                child.0.try_wait()?.is_none(),
                "idle time is not request work"
            );
            let request = if mcp {
                serde_json::json!({"jsonrpc":"2.0", "id":id, "method":"ping"})
            } else {
                serde_json::json!({"op":"status", "id":id})
            };
            writeln!(input, "{request}")?;
            input.flush()?;
            // Independent outer bound: the test must fail even if the service's
            // own watchdog regresses. DeadlineChild reaps it on an early return.
            let line = receiver.recv_timeout(Duration::from_secs(10))??;
            assert!(!line.is_empty());
            let response: serde_json::Value = serde_json::from_str(&line)?;
            assert_eq!(response["id"], id);
            assert!(response.get("error").is_none(), "{response}");
        }
        drop(input);
        assert!(
            child
                .0
                .wait_timeout(Duration::from_secs(10))?
                .is_some_and(|s| s.success())
        );
        reader.join().expect("bounded response reader panicked");
        assert!(!index.exists());
    }
    Ok(())
}

#[test]
fn blocked_response_pipes_terminate_both_transports() -> anyhow::Result<()> {
    for mcp in [false, true] {
        let temp = tempfile::tempdir()?;
        let index = temp.path().join("never-opened");
        let mut child = deadline_child(&index, mcp)?;
        let mut input = child.0.stdin.take().unwrap();
        // Do not drain stdout. Bounded request count, with an 8-KiB correlation
        // ID for MCP, fills the pipe without loading any index or database.
        let request = if mcp {
            serde_json::json!({"jsonrpc":"2.0", "id":"x".repeat(8192), "method":"ping"})
        } else {
            serde_json::json!({"op":"status", "id":1})
        };
        let writer = std::thread::spawn(move || {
            for _ in 0..2048 {
                if writeln!(input, "{request}").is_err() {
                    break;
                }
            }
        });
        let status = child.0.wait_timeout(Duration::from_secs(15))?;
        // Kill/reap on failure before joining a writer that may be in write().
        if status.is_none() {
            let _ = child.0.kill();
            let _ = child.0.wait();
        }
        writer.join().expect("bounded input writer panicked");
        assert_eq!(status.and_then(|status| status.code()), Some(124));
        assert!(!index.exists());
    }
    Ok(())
}

#[test]
fn service_rejects_disabled_or_unbounded_deadlines_before_access() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let index = temp.path().join("never-opened");
    for value in ["0", "300001", "18446744073709551616"] {
        let mut child = DeadlineChild(
            Command::new(assert_cmd::cargo::cargo_bin!("cass"))
                .args(["serve", "--stdio", "--request-timeout-ms", value, "--index"])
                .arg(&index)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let status = child.0.wait_timeout(Duration::from_secs(10))?;
        assert!(status.is_some_and(|status| !status.success()));
        assert!(!index.exists());
    }
    Ok(())
}

#[test]
fn trickling_input_cannot_restart_the_whole_request_deadline() -> anyhow::Result<()> {
    for mcp in [false, true] {
        let temp = tempfile::tempdir()?;
        let index = temp.path().join("never-opened");
        let mut child = deadline_child(&index, mcp)?;
        let mut input = child.0.stdin.take().unwrap();
        input.write_all(b"{")?;
        input.flush()?;
        let writer = std::thread::spawn(move || {
            // Keep presenting frame bytes much faster than the 500-ms deadline.
            // A per-read/sliding timer would let this incomplete request survive
            // for five seconds; the whole-request guard must not be extended.
            for _ in 0..100 {
                std::thread::sleep(Duration::from_millis(50));
                if input.write_all(b" ").and_then(|()| input.flush()).is_err() {
                    break;
                }
            }
        });
        let status = child.0.wait_timeout(Duration::from_secs(3))?;
        if status.is_none() {
            let _ = child.0.kill();
            let _ = child.0.wait();
        }
        writer.join().expect("bounded trickle writer panicked");
        assert_eq!(status.and_then(|status| status.code()), Some(124));
        assert!(!index.exists());
    }
    Ok(())
}

#[test]
fn admission_refusal_reaches_both_transports_without_loading_storage() -> anyhow::Result<()> {
    use std::fs::OpenOptions;
    for mcp in [false, true] {
        let temp = tempfile::tempdir()?;
        let pool = temp.path().join("pool");
        std::fs::create_dir(&pool)?;
        std::fs::write(pool.join("policy-v1"), b"CASS-READER-POOL-1\nslots=1\n")?;
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(pool.join("reader-00.lock"))?;
        lease.try_lock()?;
        let index = temp.path().join("absent-index");
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        command
            .args(["serve", "--stdio", "--index"])
            .arg(&index)
            .arg("--admission-dir")
            .arg(&pool);
        if mcp {
            command.arg("--mcp");
        }
        let mut child = DeadlineChild(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let mut input = child.0.stdin.take().unwrap();
        let requests = if mcp {
            vec![
                serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
                    "protocolVersion":"2025-11-25", "capabilities":{}, "clientInfo":{"name":"fixture","version":"1"}}}),
                serde_json::json!({"jsonrpc":"2.0", "method":"notifications/initialized"}),
                serde_json::json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{
                    "name":"cass_search", "arguments":{"query":"needle"}}}),
                serde_json::json!({"jsonrpc":"2.0", "id":3, "method":"tools/call", "params":{
                    "name":"cass_status", "arguments":{}}}),
            ]
        } else {
            vec![
                serde_json::json!({"op":"search", "id":2, "query":"needle"}),
                serde_json::json!({"op":"status", "id":3}),
            ]
        };
        for request in requests {
            writeln!(input, "{request}")?;
        }
        drop(input);
        assert!(
            child
                .0
                .wait_timeout(Duration::from_secs(20))?
                .is_some_and(|s| s.success())
        );
        let mut output = String::new();
        std::io::Read::read_to_string(&mut child.0.stdout.take().unwrap(), &mut output)?;
        let replies = output
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()?;
        let (refusal, status) = if mcp {
            assert_eq!(replies.len(), 3);
            assert_eq!(replies[1]["result"]["isError"], true);
            (
                &replies[1]["result"]["structuredContent"],
                &replies[2]["result"]["structuredContent"],
            )
        } else {
            assert_eq!(replies.len(), 2);
            assert_eq!(replies[0]["ok"], false);
            (&replies[0], &replies[1]["result"])
        };
        assert_eq!(refusal["error"]["kind"], "admission_busy");
        assert_eq!(status["open_attempts"], 0);
        assert_eq!(status["canonical_read_attempts"], 0);
        assert_eq!(status["reader_admission"]["lease_held"], false);
        assert!(!index.exists());
    }
    Ok(())
}

#[test]
fn service_memory_policy_is_enabled_in_both_transports_without_storage_access() -> anyhow::Result<()>
{
    for mcp in [false, true] {
        let temp = tempfile::tempdir()?;
        let index = temp.path().join("absent-index");
        let pool = temp.path().join("unopened-pool");
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        command
            .args(["serve", "--stdio", "--index"])
            .arg(&index)
            .arg("--admission-dir")
            .arg(&pool)
            .args(["--max-resident-mib", "2048"]);
        if mcp {
            command.arg("--mcp");
        }
        let mut child = DeadlineChild(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let mut stdout = child.0.stdout.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut output = String::new();
            std::io::Read::read_to_string(&mut stdout, &mut output).map(|_| output)
        });
        let mut input = child.0.stdin.take().unwrap();
        let requests = if mcp {
            vec![
                serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
                    "protocolVersion":"2025-11-25", "capabilities":{}, "clientInfo":{"name":"fixture","version":"1"}}}),
                serde_json::json!({"jsonrpc":"2.0", "method":"notifications/initialized"}),
                serde_json::json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{
                    "name":"cass_status", "arguments":{}}}),
            ]
        } else {
            vec![serde_json::json!({"op":"status", "id":2})]
        };
        for request in requests {
            writeln!(input, "{request}")?;
        }
        drop(input);
        let status = child.0.wait_timeout(Duration::from_secs(20))?;
        if status.is_none() {
            let _ = child.0.kill();
            let _ = child.0.wait();
        }
        let output = reader.join().expect("memory-status reader panicked")?;
        assert!(status.is_some_and(|s| s.success()));
        let replies = output
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()?;
        let status = if mcp {
            assert_eq!(replies.len(), 2);
            &replies[1]["result"]["structuredContent"]
        } else {
            assert_eq!(replies.len(), 1);
            &replies[0]["result"]
        };
        let memory = &status["memory_supervision"];
        assert_eq!(memory["enabled"], true);
        assert_eq!(memory["limit_bytes"], 2048_u64 * 1024 * 1024);
        assert_eq!(memory["kernel_enforced"], false);
        assert_eq!(memory["limit_exit_code"], 126);
        assert_eq!(memory["sample_interval_ms"], 100);
        assert!(memory["sampled_bytes"].as_u64().unwrap() > 0);
        assert_eq!(status["open_attempts"], 0);
        assert_eq!(status["canonical_read_attempts"], 0);
        assert!(!index.exists());
        assert!(!pool.exists());
    }
    Ok(())
}

#[test]
fn service_memory_configuration_and_initial_overage_fail_before_storage_access()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let index = temp.path().join("absent-index");
    let pool = temp.path().join("unopened-pool");
    for (value, expected_exit) in [
        ("0", 2),
        ("1048577", 2),
        ("18446744073709551616", 2),
        ("1", 126),
    ] {
        let mut child = DeadlineChild(
            Command::new(assert_cmd::cargo::cargo_bin!("cass"))
                .args(["serve", "--stdio", "--index"])
                .arg(&index)
                .arg("--admission-dir")
                .arg(&pool)
                .args(["--max-resident-mib", value])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let status = child.0.wait_timeout(Duration::from_secs(10))?;
        assert_eq!(
            status.and_then(|s| s.code()),
            Some(expected_exit),
            "{value}"
        );
        assert!(!index.exists());
        assert!(!pool.exists());
    }
    Ok(())
}

#[test]
fn binary_refinement_opt_in_is_lazy_and_mcp_rejects_invalid_work_before_access()
-> anyhow::Result<()> {
    use std::io::Read;

    for enabled in [false, true] {
        let temp = tempfile::tempdir()?;
        let index = temp.path().join("never-opened-index");
        let model = temp.path().join("never-opened-model");
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        command
            .args([
                "serve",
                "--stdio",
                "--mcp",
                "--request-timeout-ms",
                "5000",
                "--index",
            ])
            .arg(&index);
        if enabled {
            command.arg("--reranker-model").arg(&model);
        }
        let mut child = DeadlineChild(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        );
        // Drain both pipes while the process runs: an expanded tool catalog
        // must not deadlock the test at an operating-system pipe capacity.
        let mut stdout = child.0.stdout.take().unwrap();
        let output = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).map(|_| bytes)
        });
        let mut stderr = child.0.stderr.take().unwrap();
        let diagnostics = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).map(|_| bytes)
        });
        let mut input = child.0.stdin.take().unwrap();
        for request in [
            serde_json::json!({"jsonrpc": "2.0", "id": "initialize", "method": "initialize",
                "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                    "clientInfo": {"name": "fixture", "version": "1"}}}),
            serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            serde_json::json!({"jsonrpc": "2.0", "id": "catalog", "method": "tools/list"}),
            serde_json::json!({"jsonrpc": "2.0", "id": "refine", "method": "tools/call",
                "params": {"name": "cass_refine", "arguments": {
                    "query": "relevance", "lexical_query": "performance", "limit": 0}}}),
            serde_json::json!({"jsonrpc": "2.0", "id": "status", "method": "tools/call",
                "params": {"name": "cass_status", "arguments": {}}}),
        ] {
            writeln!(input, "{request}")?;
        }
        drop(input);
        let status = child.0.wait_timeout(Duration::from_secs(15))?;
        if status.is_none() {
            let _ = child.0.kill();
            let _ = child.0.wait();
        }
        let bytes = output.join().expect("stdout reader panicked")?;
        let stderr = diagnostics.join().expect("stderr reader panicked")?;
        assert!(
            status.is_some_and(|status| status.success()),
            "{}",
            String::from_utf8_lossy(&stderr)
        );
        let replies = std::str::from_utf8(&bytes)?
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(replies.len(), 4);
        assert_eq!(
            replies[1]["result"]["tools"].as_array().unwrap().len(),
            if enabled { 5 } else { 4 }
        );
        assert_eq!(replies[2]["id"], "refine");
        if enabled {
            assert_eq!(replies[2]["result"]["isError"], true);
            assert_eq!(
                replies[2]["result"]["structuredContent"]["error"]["kind"],
                "invalid_request"
            );
        } else {
            assert_eq!(replies[2]["error"]["code"], -32602);
        }
        let state = &replies[3]["result"]["structuredContent"];
        assert_eq!(state["refinement"]["enabled"], enabled);
        assert_eq!(state["refinement"]["load_attempts"], 0);
        assert_eq!(state["open_attempts"], 0);
        assert_eq!(state["models_loaded"], false);
        assert!(!index.exists());
        assert!(!model.exists());
    }
    Ok(())
}

// GH501 constructs the canonical archive and two distinct publications directly.
// No connector scan, model acquisition, or host session history is involved.
fn gh501_manifest_fixture(root: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
    use coding_agent_search::search::tantivy::{TantivyIndex, expected_index_dir};
    use coding_agent_search::storage::sqlite::FrankenStorage;
    use frankensearch::quill::cass::CassDocument;
    use serde_json::json;

    let storage = FrankenStorage::open(&root.join("agent_search.db"))?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    let path = expected_index_dir(root);
    let mut writer = TantivyIndex::open_or_create(&path)?;
    let now = chrono::Utc::now().timestamp_millis();
    for ordinal in 0..2 {
        let source_path = format!("/fixture/manifest-{ordinal}.jsonl");
        let content = format!("manifestneedle generation {ordinal}");
        let inserted = storage.insert_conversation_tree(
            agent,
            None,
            &Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: None,
                external_id: Some(format!("gh501-{ordinal}")),
                title: Some("manifest fixture".into()),
                source_path: source_path.clone().into(),
                started_at: Some(now),
                ended_at: Some(now),
                approx_tokens: None,
                metadata_json: json!({}),
                source_id: "local".into(),
                origin_host: None,
                messages: vec![Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::Agent,
                    author: None,
                    created_at: Some(now),
                    content: content.clone(),
                    extra_json: json!({}),
                    snippets: Vec::new(),
                }],
            },
        )?;
        writer.add_prebuilt_documents_slice(&[CassDocument {
            agent: "codex".into(),
            workspace: None,
            workspace_original: None,
            source_path,
            msg_idx: 0,
            created_at: Some(now),
            title: Some("manifest fixture".into()),
            content,
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
            conversation_id: Some(inserted.conversation_id),
        }])?;
        writer.commit()?;
    }
    storage.set_last_indexed_at(now)?;
    drop(writer);
    drop(storage);
    Ok(path)
}

fn gh501_command(root: &std::path::Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .current_dir(root)
        .env_remove("CASS_DB_PATH")
        .env_remove("CASS_DATA_DIR")
        .env_remove("CASS_ROBOT_FORMAT")
        .env_remove("CASS_QUILL_QUERY_FUEL_BUDGET")
        .env("HOME", root)
        .env("XDG_DATA_HOME", root.join(".local/share"))
        .env("XDG_CONFIG_HOME", root.join(".config"))
        .env("CODEX_HOME", root.join(".codex"))
        .env("CASS_CLOUDMCP_DATA_ROOT", root.join("cloudmcp-empty"))
        .env("CASS_OPENWEBUI_DATA_ROOT", root.join("openwebui-empty"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    command
}

fn gh501_run(mut command: Command, input: &str) -> anyhow::Result<std::process::Output> {
    use std::io::Read;
    let mut child = DeadlineChild(
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let mut stdout = child
        .0
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("child stdout"))?;
    let mut stderr = child
        .0
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("child stderr"))?;
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let mut stdin = child
        .0
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("child stdin"))?;
    stdin.write_all(input.as_bytes())?;
    drop(stdin);
    let status = child
        .0
        .wait_timeout(Duration::from_secs(60))?
        .ok_or_else(|| anyhow::anyhow!("GH501 child exceeded its deadline"))?;
    Ok(std::process::Output {
        status,
        stdout: out
            .join()
            .map_err(|_| anyhow::anyhow!("stdout reader panicked"))??,
        stderr: err
            .join()
            .map_err(|_| anyhow::anyhow!("stderr reader panicked"))??,
    })
}

fn gh501_advance_slot(path: &std::path::Path, state: usize) -> anyhow::Result<()> {
    let manifest = path.join("MANIFEST");
    match state {
        0 => {}
        1 => {
            let mut bytes = std::fs::read(&manifest)?;
            let middle = bytes.len() / 2;
            bytes[middle] ^= 0x80;
            std::fs::write(manifest, bytes)?;
        }
        2 => std::fs::rename(manifest, path.join("MANIFEST.fixture-corrupt"))?,
        _ => anyhow::bail!("unknown fixture slot state"),
    }
    Ok(())
}

const GH501_SOURCES: [&str; 3] = [
    "current",
    "previous_after_corrupt_current",
    "previous_after_missing_current",
];

#[test]
fn gh501_manifest_source_health_and_status_distinguish_all_durable_slots() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = gh501_manifest_fixture(temp.path())?;
    for (state, source) in GH501_SOURCES.iter().enumerate() {
        gh501_advance_slot(&path, state)?;
        let before = std::fs::read(path.join("MANIFEST")).ok();
        let previous = std::fs::read(path.join("MANIFEST.prev"))?;
        for operation in ["health", "status"] {
            let mut command = gh501_command(temp.path());
            command
                .args([operation, "--json", "--data-dir"])
                .arg(temp.path());
            let output = gh501_run(command, "")?;
            assert_eq!(
                output.status.code(),
                Some(if operation == "health" && state != 0 {
                    1
                } else {
                    0
                }),
                "{operation}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
            let index = if operation == "health" {
                &value["state"]["index"]
            } else {
                &value["index"]
            };
            assert_eq!(index["manifest_source"], *source, "{value}");
            assert_eq!(value["healthy"], state == 0, "{value}");
            if state != 0 {
                assert_eq!(
                    index["status"],
                    if state == 1 { "stale" } else { "missing" },
                    "{value}"
                );
                assert_eq!(index["fresh"], false);
                assert!(
                    value["recommended_action"]
                        .as_str()
                        .is_some_and(|action| action.contains("cass index"))
                );
                let reason = index["reason"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("manifest reason"))?;
                assert!(
                    reason.contains(if state == 1 {
                        "corrupt"
                    } else {
                        "metadata missing"
                    }),
                    "{reason}"
                );
                if state == 1 {
                    assert!(reason.contains("MANIFEST.prev"), "{reason}");
                }
                if state == 2 {
                    assert!(!reason.contains("is corrupt"), "{reason}");
                }
            }
        }
        assert_eq!(std::fs::read(path.join("MANIFEST")).ok(), before);
        assert_eq!(std::fs::read(path.join("MANIFEST.prev"))?, previous);
    }
    Ok(())
}

#[test]
fn gh501_manifest_source_search_robot_formats_report_the_served_generation() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = gh501_manifest_fixture(temp.path())?;
    for (state, source) in GH501_SOURCES[..2].iter().enumerate() {
        gh501_advance_slot(&path, state)?;
        let before = std::fs::read(path.join("MANIFEST"))?;
        for format in ["json", "jsonl", "compact", "toon"] {
            let mut command = gh501_command(temp.path());
            command
                .args([
                    "search",
                    "manifestneedle",
                    "--mode",
                    "lexical",
                    "--robot-meta",
                    "--no-maintenance",
                    "--limit",
                    "10",
                    "--robot-format",
                    format,
                    "--data-dir",
                ])
                .arg(temp.path());
            let output = gh501_run(command, "")?;
            assert!(
                output.status.success(),
                "{format}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if format == "toon" {
                let text = std::str::from_utf8(&output.stdout)?;
                assert!(
                    text.contains(&format!("manifest_source: {source}")),
                    "{text}"
                );
            } else {
                let text = std::str::from_utf8(&output.stdout)?;
                let first = if format == "jsonl" {
                    text.lines().next().unwrap_or_default()
                } else {
                    text
                };
                let value: serde_json::Value = serde_json::from_str(first)?;
                assert_eq!(value["_meta"]["manifest_source"], *source, "{value}");
                if format != "jsonl" {
                    assert_eq!(
                        value["hits"].as_array().map(Vec::len),
                        Some(if state == 0 { 2 } else { 1 }),
                        "{value}"
                    );
                }
            }
            assert_eq!(std::fs::read(path.join("MANIFEST"))?, before);
        }
    }
    Ok(())
}

#[test]
fn gh501_manifest_source_serve_reports_all_slots_without_repair() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = gh501_manifest_fixture(temp.path())?;
    for (state, source) in GH501_SOURCES.iter().enumerate() {
        gh501_advance_slot(&path, state)?;
        let before = std::fs::read(path.join("MANIFEST")).ok();
        let mut command = gh501_command(temp.path());
        command.args(["serve", "--stdio", "--index"]).arg(&path);
        let output = gh501_run(
            command,
            "{\"op\":\"search\",\"id\":1,\"query\":\"manifestneedle\",\"limit\":10}\n{\"op\":\"status\",\"id\":2}\n",
        )?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let replies: Vec<serde_json::Value> = std::str::from_utf8(&output.stdout)?
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0]["ok"], true, "{}", replies[0]);
        assert_eq!(
            replies[0]["result"]["count"],
            if state == 0 { 2 } else { 1 }
        );
        for snapshot in [&replies[0]["result"]["snapshot"], &replies[1]["result"]] {
            assert_eq!(snapshot["manifest_source"], *source, "{snapshot}");
            assert_eq!(snapshot["snapshot_manifest_source"], *source, "{snapshot}");
            assert_eq!(snapshot["maintenance_performed"], false);
        }
        assert_eq!(std::fs::read(path.join("MANIFEST")).ok(), before);
    }
    Ok(())
}
