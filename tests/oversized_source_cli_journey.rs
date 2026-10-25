//! GH #484: the real-CLI journey for a rollout the per-source read budget
//! rejects, and for a rollout that changes while it is being read.
//!
//! The containment for an over-budget source already exists in the Codex
//! consumer; this file proves it end to end through the shipped cass binary
//! against a private archive, and it proves the accounting rather than the
//! plumbing: the rejected rollout is NAMED and EXCLUDED, the healthy
//! neighbours on both sides of it are still indexed and still searchable
//! from a fresh process, and the run reports partial coverage with an
//! actionable exit instead of a success receipt.
//!
//! Fixture economics: the cap is an ENFORCED limit with a real operator knob
//! (CASS_CODEX_MAX_SOURCE_BYTES, default 100 MiB, accepted range
//! 1..=1073741824), so the rejection path is driven with a few-KiB cap and
//! small synthetic rollouts. No 100 MiB fixture is written to a shared disk.

use assert_cmd::Command;
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// The enforced per-source read budget, lowered so the rejection path runs
/// against a few-KiB rollout instead of a 100 MiB one.
const CAP: &str = "4096";
/// The published default of the same budget.
const DEFAULT_CAP: &str = "104857600";

fn cass(home: &Path, cap: &str) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("CODEX_HOME", home.join(".codex"))
        // The OpenWebUI and CloudMCP connectors default to machine-global
        // directories (/warp/cass/raw-mirror/openwebui/v1 and
        // /warp/cloudmcp/contextos/transcripts/v1). Point them at empty
        // directories so the archive under test holds THIS fixture's
        // conversations and nothing else: the counts below are then counts of
        // the fixture, not of whatever the host happens to have indexed.
        .env("CASS_OPENWEBUI_DATA_ROOT", home.join("empty-openwebui"))
        .env("CASS_CLOUDMCP_DATA_ROOT", home.join("empty-cloudmcp"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_RAW_MIRROR", "0")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_CODEX_MAX_SOURCE_BYTES", cap)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("NO_COLOR", "1")
        .env_remove("CLAUDE_CONFIG_DIR")
        .timeout(Duration::from_secs(240));
    command
}

/// The private source tree of one journey: a Codex sessions directory plus
/// the two empty directories that keep the machine-global connectors out of
/// the archive.
fn private_home(home: &Path) -> PathBuf {
    let sessions = home.join(".codex/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(home.join("empty-openwebui")).unwrap();
    std::fs::create_dir_all(home.join("empty-cloudmcp")).unwrap();
    sessions
}

fn index(home: &Path, cap: &str, key: &str, full: bool) -> std::process::Output {
    let mut command = cass(home, cap);
    command.arg("index");
    if full {
        command.arg("--full");
    }
    let output = command
        .args(["--json", "--data-dir"])
        .arg(home.join("archive"))
        .args(["--idempotency-key", key])
        .output()
        .expect("run index command");
    output
}

fn json_output(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Search from a fresh process: the archive is reopened, so a hit proves the
/// conversation survived the index run and the process restart.
fn search(home: &Path, cap: &str, marker: &str) -> Value {
    let output = cass(home, cap)
        .args([
            "search",
            marker,
            "--json",
            "--mode",
            "lexical",
            "--no-maintenance",
            "--no-daemon",
            "--limit",
            "10",
            "--data-dir",
        ])
        .arg(home.join("archive"))
        .output()
        .expect("run search command");
    assert!(
        output.status.success(),
        "search {marker} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    json_output(&output)
}

/// The conversations the archive actually holds, with their message counts.
/// This is the count-and-exclusion proof: it is read back from the persisted
/// archive, never inferred from a discovered-file count.
fn indexed_conversations(home: &Path) -> Vec<(String, usize)> {
    let database = home.join("archive/agent_search.db");
    if !database.exists() {
        return Vec::new();
    }
    let storage = SqliteStorage::open_readonly(&database).expect("open archive read-only");
    let conversations = storage
        .list_conversations(1000, 0)
        .expect("list indexed conversations");
    let mut rows: Vec<(String, usize)> = conversations
        .iter()
        .map(|conversation| {
            let messages = storage
                .fetch_messages(conversation.id.expect("indexed conversation id"))
                .expect("fetch indexed messages")
                .len();
            (
                conversation.source_path.to_string_lossy().into_owned(),
                messages,
            )
        })
        .collect();
    rows.sort();
    rows
}

/// A Codex rollout in the shape the connector reads: one session record and
/// one user message carrying the marker.
fn rollout_bytes(id: &str, marker: &str) -> Vec<u8> {
    let rows = [
        json!({"type":"session_meta","payload":{"id":id,"cwd":"/project"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":marker}]}}),
    ];
    let mut bytes = Vec::new();
    for row in rows {
        bytes.extend_from_slice(row.to_string().as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

/// Grow a rollout past the enforced budget with VALID JSONL records, so the
/// refusal is a budget refusal and not a corrupt file in disguise.
fn grow_past_cap(bytes: &mut Vec<u8>, cap: u64) {
    let padding = format!(
        "{{\"type\":\"fixture_padding\",\"padding\":\"{}\"}}\n",
        "x".repeat(256)
    );
    while bytes.len() as u64 <= cap {
        bytes.extend_from_slice(padding.as_bytes());
    }
}

fn codex_stats(result: &Value) -> Value {
    result["indexing_stats"]["connectors"]
        .as_array()
        .expect("connector stats")
        .iter()
        .find(|connector| connector["name"] == "codex")
        .expect("codex connector stats")
        .clone()
}

/// Journey 1: one good source BEFORE the rejected one and one AFTER it.
///
/// The rejected rollout must be named and excluded; both neighbours must be
/// indexed (the scan continued past the rejection) and still searchable from a
/// fresh process; and the run must not report success.
#[test]
fn a_rejected_rollout_is_excluded_while_its_healthy_neighbours_survive() {
    let dir = tempfile::tempdir().expect("isolated archive");
    let home = dir.path();
    let sessions = private_home(home);

    let before = sessions.join("rollout-a.jsonl");
    let rejected = sessions.join("rollout-b.jsonl");
    let after = sessions.join("rollout-c.jsonl");
    let before_bytes = rollout_bytes("journey-before", "journeygoodbeforemarker");
    let mut rejected_bytes = rollout_bytes("journey-rejected", "journeyrejectedmarker");
    grow_past_cap(&mut rejected_bytes, CAP.parse().unwrap());
    let after_bytes = rollout_bytes("journey-after", "journeygoodaftermarker");
    std::fs::write(&before, &before_bytes).unwrap();
    std::fs::write(&rejected, &rejected_bytes).unwrap();
    std::fs::write(&after, &after_bytes).unwrap();

    let output = index(home, CAP, "journey-one", true);
    let result = json_output(&output);

    // Partial coverage plus an actionable exit, never a success receipt.
    assert_eq!(output.status.code(), Some(9), "{result}");
    assert_eq!(result["success"], false, "{result}");
    assert_eq!(result["partial"], true, "{result}");
    assert_eq!(result["coverage_status"], "incomplete", "{result}");
    assert_eq!(
        result["indexing_stats"]["scan_had_errors"], true,
        "{result}"
    );
    assert_eq!(result["failed_connectors"], json!(["codex"]), "{result}");
    assert_eq!(result["code"], 9, "{result}");
    assert_eq!(result["retryable"], true, "{result}");
    let hint = result["hint"].as_str().expect("actionable hint");
    assert!(hint.contains("indexing_stats.connectors"), "{hint}");
    assert!(hint.contains("rerun indexing"), "{hint}");

    // The rejected rollout is named with its reason.
    let codex = codex_stats(&result);
    let detail = codex["error"].as_str().expect("rejected source detail");
    assert!(
        detail.contains("enrichment_read_budget_exceeded"),
        "{detail}"
    );
    assert!(detail.contains("rollout-b.jsonl"), "{detail}");

    // COUNT, not "discovered files == conversations": three rollouts were
    // discovered, two conversations were published.
    assert_eq!(codex["conversations"], 2, "{codex}");
    let indexed = indexed_conversations(home);
    assert_eq!(
        indexed,
        vec![
            (before.to_string_lossy().into_owned(), 1),
            (after.to_string_lossy().into_owned(), 1),
        ],
        "the rejected rollout must be excluded from the archive"
    );

    // EXCLUSION at the search surface: the rejected rollout's content is not
    // reachable, while both healthy neighbours are, from a fresh process.
    let rejected_hits = search(home, CAP, "journeyrejectedmarker");
    assert!(
        rejected_hits["hits"].as_array().expect("hits").is_empty(),
        "{rejected_hits}"
    );
    for (marker, source) in [
        ("journeygoodbeforemarker", &before),
        ("journeygoodaftermarker", &after),
    ] {
        let hits = search(home, CAP, marker);
        let list = hits["hits"].as_array().expect("hits");
        assert_eq!(list.len(), 1, "{hits}");
        assert_eq!(
            list[0]["source_path"],
            source.to_string_lossy().as_ref(),
            "{hits}"
        );
    }

    // The rejected source is never modified.
    assert_eq!(std::fs::read(&before).unwrap(), before_bytes);
    assert_eq!(std::fs::read(&rejected).unwrap(), rejected_bytes);
    assert_eq!(std::fs::read(&after).unwrap(), after_bytes);
}

/// Journey 2: every source is rejected. Nothing may be published, nothing
/// may be searchable, every rejected rollout must be named, and the run must
/// still report partial coverage rather than an empty success.
#[test]
fn an_all_rejected_corpus_publishes_nothing_and_names_every_rollout() {
    let dir = tempfile::tempdir().expect("isolated archive");
    let home = dir.path();
    let sessions = private_home(home);

    let paths = [
        sessions.join("rollout-d.jsonl"),
        sessions.join("rollout-e.jsonl"),
    ];
    let markers = ["journeyalldmarkerone", "journeyalldmarkertwo"];
    let mut originals = Vec::new();
    for (path, marker) in paths.iter().zip(markers) {
        let mut bytes = rollout_bytes("journey-all", marker);
        grow_past_cap(&mut bytes, CAP.parse().unwrap());
        std::fs::write(path, &bytes).unwrap();
        originals.push(bytes);
    }

    let output = index(home, CAP, "journey-two", true);
    let result = json_output(&output);
    assert_eq!(output.status.code(), Some(9), "{result}");
    assert_eq!(result["success"], false, "{result}");
    assert_eq!(result["partial"], true, "{result}");
    assert_eq!(result["coverage_status"], "incomplete", "{result}");

    let codex = codex_stats(&result);
    let detail = codex["error"].as_str().expect("rejected source detail");
    assert!(
        detail.contains("enrichment_read_budget_exceeded"),
        "{detail}"
    );
    for path in &paths {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(detail.contains(&name), "{name} is not named: {detail}");
    }
    assert_eq!(codex["conversations"], 0, "{codex}");

    // Nothing was published, so nothing is searchable.
    assert!(indexed_conversations(home).is_empty());
    for marker in markers {
        let hits = search(home, CAP, marker);
        assert!(hits["hits"].as_array().expect("hits").is_empty(), "{hits}");
    }

    for (path, original) in paths.iter().zip(&originals) {
        assert_eq!(std::fs::read(path).unwrap(), *original);
    }
}

/// Replace the target path with the mutated bytes for as long as the stop
/// flag is clear. The rename is atomic, so a reader always sees one complete
/// generation; only the generation identity changes.
fn rewrite_until_stopped(
    path: PathBuf,
    staging: PathBuf,
    mutated: Vec<u8>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if std::fs::write(&staging, &mutated).is_ok() {
                let _ = std::fs::rename(&staging, &path);
            }
            std::thread::sleep(Duration::from_micros(100));
        }
    })
}

/// Journey 3: a source that changes while it is being read.
///
/// The changed rollout must not publish a mixed history, its healthy
/// neighbour must still be indexed, and the failure must be reported as a
/// change (not as an unreadable file an operator should chase permissions
/// for). A later run, once the source is stable, must pick up its current
/// content.
#[test]
fn a_source_that_changes_while_it_is_read_is_excluded_and_its_neighbour_is_indexed() {
    let dir = tempfile::tempdir().expect("isolated archive");
    let home = dir.path();
    let sessions = private_home(home);

    let changing = sessions.join("rollout-f.jsonl");
    let stable = sessions.join("rollout-g.jsonl");
    let stable_bytes = rollout_bytes("journey-stable", "journeystablemarker");
    std::fs::write(&stable, &stable_bytes).unwrap();

    // A rollout big enough that the connector's two reads (the primary parse
    // and the enrichment pass) span a visible interval, so the replacement
    // lands between them. The bytes stay small: a few hundred KiB, not 100 MiB.
    let mut changing_bytes = rollout_bytes("journey-changing", "journeychangingmarkerone");
    let padding = format!(
        "{{\"type\":\"fixture_padding\",\"padding\":\"{}\"}}\n",
        "x".repeat(200)
    );
    while changing_bytes.len() < 256 * 1024 {
        changing_bytes.extend_from_slice(padding.as_bytes());
    }
    std::fs::write(&changing, &changing_bytes).unwrap();

    // The replacement has the same length (so the fixture's parse cost does not
    // change) but different content, so a mixed history would be visible if
    // the connector published one.
    let mut mutated = changing_bytes.clone();
    let needle = b"journeychangingmarkerone";
    let position = mutated
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("marker in fixture");
    mutated[position..position + needle.len()].copy_from_slice(b"journeychangingmarkertwo");

    let stop = Arc::new(AtomicBool::new(false));
    let rewriter = rewrite_until_stopped(
        changing.clone(),
        home.join("staging.partial"),
        mutated.clone(),
        stop.clone(),
    );

    let output = index(home, DEFAULT_CAP, "journey-three", true);
    stop.store(true, Ordering::Relaxed);
    rewriter.join().expect("rewriter thread");
    let result = json_output(&output);

    assert_eq!(output.status.code(), Some(9), "{result}");
    assert_eq!(result["success"], false, "{result}");
    assert_eq!(result["partial"], true, "{result}");
    assert_eq!(result["coverage_status"], "incomplete", "{result}");

    let codex = codex_stats(&result);
    let detail = codex["error"].as_str().expect("changed source detail");
    assert!(detail.contains("source_changed"), "{detail}");
    assert!(detail.contains("rollout-f.jsonl"), "{detail}");

    // The changed rollout published nothing; the healthy neighbour survived.
    assert_eq!(codex["conversations"], 1, "{codex}");
    let indexed = indexed_conversations(home);
    assert_eq!(
        indexed,
        vec![(stable.to_string_lossy().into_owned(), 1)],
        "the changed rollout must not publish a mixed history"
    );
    for marker in ["journeychangingmarkerone", "journeychangingmarkertwo"] {
        let hits = search(home, DEFAULT_CAP, marker);
        assert!(hits["hits"].as_array().expect("hits").is_empty(), "{hits}");
    }
    let hits = search(home, DEFAULT_CAP, "journeystablemarker");
    let list = hits["hits"].as_array().expect("hits");
    assert_eq!(list.len(), 1, "{hits}");
    assert_eq!(
        list[0]["source_path"],
        stable.to_string_lossy().as_ref(),
        "{hits}"
    );

    // The change is reported as a change: the source was readable, so the
    // operator is not sent to check permissions.
    let diagnostics = result["indexing_stats"]["connector_diagnostics"]
        .as_array()
        .expect("connector diagnostics");
    let changed = diagnostics
        .iter()
        .find(|diagnostic| diagnostic["failure_kind"] == "source-changed-during-read")
        .unwrap_or_else(|| panic!("no source-changed-during-read diagnostic: {diagnostics:?}"));
    assert_eq!(changed["provider"], "codex", "{changed}");
    assert_eq!(changed["retryable"], true, "{changed}");
    assert!(
        !changed["safe_next_action"]
            .as_str()
            .expect("safe next action")
            .contains("check permissions"),
        "{changed}"
    );

    // Once the source is stable again, the next run indexes its CURRENT
    // content: the action the diagnostic asks for.
    std::fs::write(&changing, &mutated).unwrap();
    let output = index(home, DEFAULT_CAP, "journey-three-retry", false);
    let result = json_output(&output);
    assert!(
        output.status.success(),
        "the stable retry must succeed: {result} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(
        result["indexing_stats"]["scan_had_errors"], false,
        "{result}"
    );
    let hits = search(home, DEFAULT_CAP, "journeychangingmarkertwo");
    let list = hits["hits"].as_array().expect("hits");
    assert_eq!(list.len(), 1, "{hits}");
    assert_eq!(
        list[0]["source_path"],
        changing.to_string_lossy().as_ref(),
        "{hits}"
    );
}
