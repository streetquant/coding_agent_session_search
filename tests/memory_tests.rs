//! Memory profiling tests for cass.
//!
//! These tests verify that repeated operations don't leak memory.
//!
//! IMPORTANT: Run with --test-threads=1 to avoid measurement interference:
//!   cargo test --test memory_tests --release -- --nocapture --test-threads=1
//!
//! For detailed profiling, use heaptrack:
//!   heaptrack cargo test --test memory_tests --release
//!   heaptrack_gui heaptrack.*.zst

use coding_agent_search::connectors::{NormalizedConversation, NormalizedMessage};
use coding_agent_search::indexer::persist::persist_conversation;
use coding_agent_search::search::query::{FieldMask, SearchClient, SearchFilters};
use coding_agent_search::search::tantivy::{TantivyIndex, index_dir};
use coding_agent_search::storage::sqlite::SqliteStorage;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tempfile::TempDir;

mod util;

/// These tests use RSS-based assertions and should not run concurrently.
static MEMORY_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn memory_test_guard() -> std::sync::MutexGuard<'static, ()> {
    MEMORY_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Generate a sample conversation for testing.
fn sample_conv(i: i64, msgs: i64) -> NormalizedConversation {
    let mut messages = Vec::new();
    for m in 0..msgs {
        messages.push(NormalizedMessage {
            idx: m,
            role: if m % 2 == 0 { "user" } else { "agent" }.into(),
            author: None,
            created_at: Some(1_700_000_000_000 + (i * 10 + m)),
            content: format!(
                "conversation {i} message {m} lorem ipsum dolor sit amet \
                 consectetur adipiscing elit sed do eiusmod tempor"
            ),
            extra: serde_json::json!({}),
            snippets: Vec::new(),
            invocations: Vec::new(),
        });
    }
    NormalizedConversation {
        agent_slug: "memory-test-agent".into(),
        external_id: Some(format!("mem-conv-{i}")),
        title: Some(format!("Memory Test Conversation {i}")),
        workspace: Some(PathBuf::from("/tmp/workspace")),
        source_path: PathBuf::from(format!("/tmp/memory-test/conv-{i}.jsonl")),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_000_000 + msgs),
        metadata: serde_json::json!({ "memory_test": true, "i": i }),
        messages,
    }
}

/// Set up a test index with sample data.
fn setup_test_index(conv_count: i64, msgs_per_conv: i64) -> (TempDir, SearchClient) {
    let temp = TempDir::new().expect("create tempdir");
    let data_dir = temp.path().to_path_buf();
    let db_path = data_dir.join("memory_test.db");
    let index_path = index_dir(&data_dir).expect("index path");

    let storage = SqliteStorage::open(&db_path).expect("open db");
    let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();

    for i in 0..conv_count {
        let conv = sample_conv(i, msgs_per_conv);
        persist_conversation(&storage, &mut t_index, &conv).expect("persist");
    }
    t_index.commit().unwrap();

    let client = SearchClient::open(&index_path, Some(&db_path))
        .expect("open client")
        .expect("client available");

    (temp, client)
}

/// Get current process memory usage (resident set size).
/// Returns 0 on unsupported platforms.
fn get_process_memory_bytes() -> usize {
    #[cfg(target_os = "linux")]
    {
        // Read /proc/self/statm: VmSize VmRSS VmShared ...
        // Second field is RSS in pages
        if let Ok(statm) = std::fs::read_to_string("/proc/self/statm")
            && let Some(rss_pages) = statm.split_whitespace().nth(1)
            && let Ok(pages) = rss_pages.parse::<usize>()
        {
            return pages * 4096; // Assume 4KB pages
        }
        0
    }

    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        // Use ps to get RSS in KB
        Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()
            .and_then(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .parse::<usize>()
                    .ok()
            })
            .map(|rss_kb| rss_kb * 1024)
            .unwrap_or(0)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0
    }
}

/// Test that repeated searches don't leak memory.
///
/// This test runs many searches and verifies that memory usage doesn't
/// grow unboundedly. Some growth is acceptable due to caching.
#[test]
#[ignore = "RSS profiling test; run manually with `cargo test --release --test memory_tests -- --nocapture --test-threads=1`"]
fn test_search_memory_no_leak() {
    let _guard = memory_test_guard();
    // Create index with 100 conversations
    let (_tmp, client) = setup_test_index(100, 10);
    let filters = SearchFilters::default();

    // Warm up - run a few searches to initialize caches
    for _ in 0..10 {
        let _ = client.search("lorem", filters.clone(), 20, 0, FieldMask::FULL);
    }

    // Get baseline memory after warmup
    let baseline = get_process_memory_bytes();

    // Skip test on platforms where we can't measure memory
    if baseline == 0 {
        println!("Skipping memory test - platform doesn't support memory measurement");
        return;
    }

    // Run many searches
    for i in 0..500 {
        let query = if i % 3 == 0 {
            "lorem"
        } else if i % 3 == 1 {
            "ipsum"
        } else {
            "dolor"
        };
        let _ = client.search(query, filters.clone(), 20, 0, FieldMask::FULL);
    }

    let after = get_process_memory_bytes();
    let growth = after.saturating_sub(baseline);

    // Allow up to 50MB growth (for caches, etc.)
    // This is generous but catches true leaks
    let max_allowed_growth = 50 * 1024 * 1024; // 50MB

    println!(
        "Memory: baseline={:.2}MB, after={:.2}MB, growth={:.2}MB",
        baseline as f64 / 1_048_576.0,
        after as f64 / 1_048_576.0,
        growth as f64 / 1_048_576.0
    );

    assert!(
        growth < max_allowed_growth,
        "Memory grew by {:.2}MB during search loop (max allowed: {:.2}MB). \
         This may indicate a memory leak.",
        growth as f64 / 1_048_576.0,
        max_allowed_growth as f64 / 1_048_576.0
    );
}

/// Test that repeated indexing operations don't leak memory.
#[test]
#[ignore = "RSS profiling test; run manually with `cargo test --release --test memory_tests -- --nocapture --test-threads=1`"]
fn test_indexing_memory_no_leak() {
    let _guard = memory_test_guard();
    let temp = TempDir::new().expect("tempdir");
    let data_dir = temp.path().to_path_buf();
    let db_path = data_dir.join("memory_index_test.db");
    let index_path = index_dir(&data_dir).expect("index path");

    let storage = SqliteStorage::open(&db_path).expect("open db");
    let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();

    // Warm up
    for i in 0..5 {
        let conv = sample_conv(i, 5);
        persist_conversation(&storage, &mut t_index, &conv).expect("persist");
    }
    t_index.commit().unwrap();

    let baseline = get_process_memory_bytes();

    if baseline == 0 {
        println!("Skipping memory test - platform doesn't support memory measurement");
        return;
    }

    // Index many conversations
    for i in 5..105 {
        let conv = sample_conv(i, 10);
        persist_conversation(&storage, &mut t_index, &conv).expect("persist");

        // Commit periodically
        if i % 20 == 0 {
            t_index.commit().unwrap();
        }
    }
    t_index.commit().unwrap();

    let after = get_process_memory_bytes();
    let growth = after.saturating_sub(baseline);

    // Allow up to 100MB growth for indexing (more data = more legitimate memory use)
    let max_allowed_growth = 100 * 1024 * 1024; // 100MB

    println!(
        "Indexing memory: baseline={:.2}MB, after={:.2}MB, growth={:.2}MB",
        baseline as f64 / 1_048_576.0,
        after as f64 / 1_048_576.0,
        growth as f64 / 1_048_576.0
    );

    assert!(
        growth < max_allowed_growth,
        "Memory grew by {:.2}MB during indexing (max allowed: {:.2}MB). \
         This may indicate a memory leak.",
        growth as f64 / 1_048_576.0,
        max_allowed_growth as f64 / 1_048_576.0
    );
}

/// Test that vector search operations don't leak memory.
#[test]
#[ignore = "RSS profiling test; run manually with `cargo test --release --test memory_tests -- --nocapture --test-threads=1`"]
fn test_vector_search_memory_no_leak() {
    let _guard = memory_test_guard();
    use coding_agent_search::search::vector_index::{Quantization, SemanticDocId, VectorIndex};

    let dimension = 384;
    let count = 10_000;

    fn normalize_in_place(vec: &mut [f32]) {
        let norm_sq: f32 = vec.iter().map(|v| v * v).sum();
        let norm = norm_sq.sqrt();
        if norm > 0.0 {
            for v in vec {
                *v /= norm;
            }
        }
    }

    // Build on-disk index (FSVI) so memory measurements reflect real behavior.
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("mem.fsvi");
    let mut writer = VectorIndex::create_with_revision(
        &path,
        "test-embedder",
        "rev1",
        dimension,
        Quantization::F16,
    )
    .expect("create writer");

    let mut vec_buf = vec![0.0f32; dimension];
    for idx in 0..count {
        for (d, slot) in vec_buf.iter_mut().enumerate() {
            *slot = ((idx + d * 31) % 997) as f32 / 997.0;
        }
        normalize_in_place(&mut vec_buf);
        let doc_id = SemanticDocId {
            message_id: idx as u64,
            chunk_idx: 0,
            agent_id: (idx % 8) as u32,
            workspace_id: 1,
            source_id: 1,
            role: 1,
            created_at_ms: idx as i64,
            content_hash: None,
        }
        .to_doc_id_string();
        writer
            .write_record(&doc_id, &vec_buf)
            .expect("write_record");
    }
    writer.finish().expect("finish");

    let index = VectorIndex::open(&path).expect("open");

    // Generate query vector
    let mut query: Vec<f32> = (0..dimension).map(|d| (d % 17) as f32 / 17.0).collect();
    normalize_in_place(&mut query);

    // Warm up
    for _ in 0..10 {
        let _ = index.search_top_k(&query, 25, None);
    }

    let baseline = get_process_memory_bytes();

    if baseline == 0 {
        println!("Skipping memory test - platform doesn't support memory measurement");
        return;
    }

    // Run many vector searches
    for _ in 0..500 {
        let _ = index.search_top_k(&query, 25, None);
    }

    let after = get_process_memory_bytes();
    let growth = after.saturating_sub(baseline);

    // Allow up to 20MB growth (vector search should be very memory-stable)
    let max_allowed_growth = 20 * 1024 * 1024; // 20MB

    println!(
        "Vector search memory: baseline={:.2}MB, after={:.2}MB, growth={:.2}MB",
        baseline as f64 / 1_048_576.0,
        after as f64 / 1_048_576.0,
        growth as f64 / 1_048_576.0
    );

    assert!(
        growth < max_allowed_growth,
        "Memory grew by {:.2}MB during vector search loop (max allowed: {:.2}MB). \
         This may indicate a memory leak.",
        growth as f64 / 1_048_576.0,
        max_allowed_growth as f64 / 1_048_576.0
    );
}

// The scale lane is separate from the small in-process leak probes above.
// Run only on an admitted Linux build host, inside a fresh cgroup with
// MemoryMax=4G and MemorySwapMax=0. All generated input and outputs are retained
// beneath the selected CASS_E2E_RUN_ID. The same assertions can be run against
// an explicitly identified older ELF as a negative control.
#[cfg(target_os = "linux")]
mod resident_cap {
    use anyhow::{Context, Result, ensure};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    use std::fs::{self, File, OpenOptions};
    use std::io::{BufWriter, Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use crate::util::e2e_log::{
        E2eError, E2eLogger, E2ePerformanceMetrics, E2eRunSummary, PhaseTracker,
    };

    const CAP: u64 = 4 * 1024 * 1024 * 1024;
    const RSS_LIMIT: u64 = CAP * 85 / 100;
    const MESSAGES: u64 = 318_800;
    // Python exposes Linux wait4 without adding application unsafe code or a
    // profiler dependency. The observer never imports or edits source files.
    const WAIT4: &str = r#"import json, os, subprocess, sys
child = subprocess.Popen(sys.argv[2:])
_, status, usage = os.wait4(child.pid, 0)
code = os.waitstatus_to_exitcode(status)
child.returncode = code
with open(sys.argv[1], "x") as output:
    json.dump({"peak_rss_bytes": usage.ru_maxrss * 1024, "exit_code": code}, output)
    output.flush()
    os.fsync(output.fileno())
sys.exit(code if code >= 0 else 128 - code)
"#;

    fn hash_file(path: &Path) -> Result<String> {
        let mut file = File::open(path)?;
        let mut hash = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hash.update(&buffer[..read]);
        }
        let mut encoded = String::with_capacity(64);
        for byte in hash.finalize().iter() {
            write!(&mut encoded, "{byte:02x}")?;
        }
        Ok(encoded)
    }

    fn write_json(path: &Path, value: &Value) -> Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        serde_json::to_writer_pretty(&mut file, value)?;
        writeln!(file)?;
        file.sync_all()?;
        Ok(())
    }

    fn cgroup() -> Result<PathBuf> {
        let membership = fs::read_to_string("/proc/self/cgroup")?;
        let relative = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .context("scale acceptance requires cgroup v2")?;
        let path = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
        ensure!(fs::read_to_string(path.join("memory.max"))?.trim() == CAP.to_string());
        ensure!(
            fs::read_to_string(path.join("memory.swap.max"))?.trim() == "0",
            "scale acceptance requires swap disabled for this cgroup"
        );
        Ok(path)
    }

    fn fixture(home: &Path) -> Result<Vec<Value>> {
        let sessions = home.join(".codex/sessions/2026/04/23");
        fs::create_dir_all(&sessions)?;
        let mut manifest = Vec::new();
        for conversation in 0..41 {
            let (messages, body_bytes, marker) = match conversation {
                0..36 => (250, 256, "rssqualificationsmall"),
                36..40 => (2_450, 512, "rssqualificationmedium"),
                _ => (300_000, 2_600, "rssqualificationgiant"),
            };
            let path = sessions.join(format!("rollout-rss-{conversation:02}.jsonl"));
            let mut output = BufWriter::new(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?,
            );
            serde_json::to_writer(
                &mut output,
                &json!({"timestamp":"2026-04-23T10:00:00Z","type":"session_meta",
                    "payload":{"id":format!("00000000-0000-4000-8000-{conversation:012}"),
                        "cwd":home.join("workspace"),"cli_version":"0.42.0"}}),
            )?;
            writeln!(output)?;
            let filler = "canonical indexing memory fixture content ".repeat(70);
            let mut observed_body_bytes = 0_u64;
            for message in 0..messages {
                let mut text = format!(
                    "{marker} conversation {conversation:02} message {message:06} {filler}"
                );
                text.truncate(body_bytes);
                observed_body_bytes += u64::try_from(text.len())?;
                let (role, kind) = if message % 2 == 0 {
                    ("user", "input_text")
                } else {
                    ("assistant", "text")
                };
                serde_json::to_writer(
                    &mut output,
                    &json!({"timestamp":"2026-04-23T10:00:01Z","type":"response_item",
                        "payload":{"type":"message","role":role,
                            "content":[{"type":kind,"text":text}]}}),
                )?;
                writeln!(output)?;
            }
            output.flush()?;
            manifest.push(json!({"path":path,"messages":messages,
                "message_body_bytes":observed_body_bytes,
                "source_bytes":fs::metadata(&path)?.len(),"sha256":hash_file(&path)?}));
        }
        Ok(manifest)
    }

    fn command(tracker: &PhaseTracker, home: &Path, mut command: Command) -> Command {
        command.env_clear();
        command.envs([
            ("PATH", "/usr/local/bin:/usr/bin:/bin"),
            ("LANG", "C.UTF-8"),
            ("CASS_AUTO_REFRESH", "0"),
            ("CASS_IGNORE_SOURCES_CONFIG", "1"),
            ("CASS_CODEX_MAX_SOURCE_BYTES", "1073741824"),
            ("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1"),
            ("RUST_MIN_STACK", "134217728"),
            ("NO_COLOR", "1"),
        ]);
        for (name, relative) in [
            ("XDG_DATA_HOME", "xdg-data"),
            ("XDG_CONFIG_HOME", "xdg-config"),
            ("XDG_CACHE_HOME", "xdg-cache"),
            ("CASS_CLOUDMCP_DATA_ROOT", "cloudmcp-empty"),
            ("CASS_OPENWEBUI_DATA_ROOT", "openwebui-empty"),
        ] {
            command.env(name, home.join(relative));
        }
        tracker
            .command_environment()
            .with_home(home)
            .with_codex_home(home.join(".codex"))
            .apply_to_std(&mut command);
        command.current_dir(home).stdin(Stdio::null());
        command
    }

    fn child_samples(parent: u32) -> Vec<Value> {
        let mut parents = vec![parent];
        let mut samples = Vec::new();
        // Resource observer -> timeout -> cass. Observe only owned descendants.
        for _ in 0..3 {
            let mut next = Vec::new();
            for parent in parents {
                let Ok(children) =
                    fs::read_to_string(format!("/proc/{parent}/task/{parent}/children"))
                else {
                    continue;
                };
                for child in children
                    .split_whitespace()
                    .filter_map(|pid| pid.parse::<u32>().ok())
                {
                    next.push(child);
                    if let Ok(status) = fs::read_to_string(format!("/proc/{child}/status")) {
                        let fields: Vec<_> = status
                            .lines()
                            .filter(|line| {
                                [
                                    "Name:", "Pid:", "PPid:", "VmRSS:", "VmHWM:", "RssAnon:",
                                    "RssFile:", "VmSwap:", "Threads:",
                                ]
                                .iter()
                                .any(|field| line.starts_with(field))
                            })
                            .collect();
                        samples.push(json!({"pid":child,"status":fields}));
                    }
                }
            }
            parents = next;
        }
        samples
    }

    fn run(tracker: &PhaseTracker) -> Result<()> {
        let group = cgroup()?;
        let root = fs::canonicalize(&tracker.artifacts().dir)?;
        let home = root.join("home");
        fs::create_dir(&home)?;
        let data = root.join("cass-data");
        let binary = dotenvy::var("CASS_MEMORY_E2E_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(assert_cmd::cargo::cargo_bin!("cass")));
        let binary = fs::canonicalize(binary)?;
        let binary_hash = hash_file(&binary)?;
        if let Ok(expected) = dotenvy::var("CASS_MEMORY_E2E_BIN_SHA256") {
            ensure!(binary_hash == expected, "reviewed ELF digest mismatch");
        } else {
            ensure!(
                dotenvy::var("CASS_MEMORY_E2E_BIN").is_err(),
                "an external ELF requires CASS_MEMORY_E2E_BIN_SHA256"
            );
        }
        let sources = tracker.phase(
            "generate_skew",
            "Generate native synthetic Codex sources",
            || fixture(&home),
        )?;
        ensure!(sources.len() == 41);
        let mut counts: Vec<_> = sources
            .iter()
            .map(|source| source["messages"].as_u64().context("fixture message count"))
            .collect::<Result<_>>()?;
        ensure!(counts.iter().sum::<u64>() == MESSAGES);
        counts.sort_unstable();
        ensure!(counts.get(20) == Some(&250) && counts.get(36) == Some(&2_450));
        ensure!(
            sources.iter().any(|source| {
                source["messages"]
                    .as_u64()
                    .is_some_and(|count| count >= 300_000)
                    && source["message_body_bytes"]
                        .as_u64()
                        .is_some_and(|bytes| bytes >= 700_000_000)
            }),
            "the real input must contain at least 300000 messages and 700 MB of text in one conversation"
        );
        write_json(
            &root.join("fixture.json"),
            &json!({"synthetic":true,
            "private_data":false,"conversations":41,"messages":MESSAGES,
            "median_messages":counts.get(20),"p90_messages":counts.get(36),
            "maximum_messages":counts.last(),
            "largest_message_body_bytes":sources.iter()
                .filter_map(|source| source["message_body_bytes"].as_u64()).max(),
            "sources":sources}),
        )?;
        let usage = root.join("index-usage.json");
        let mut index = command(tracker, &home, Command::new("/usr/bin/python3"));
        index.args(["-c", WAIT4]).arg(&usage);
        index.args([
            "/usr/bin/timeout",
            "--signal=TERM",
            "--kill-after=30s",
            "1800s",
        ]);
        index
            .arg(&binary)
            .args(["index", "--full", "--json", "--no-progress-events"]);
        index.arg("--data-dir").arg(&data);
        index.stdout(File::create_new(root.join("index.stdout.json"))?);
        index.stderr(File::create_new(root.join("index.stderr.log"))?);
        write_json(
            &root.join("invocation.json"),
            &json!({"binary":binary,
            "binary_sha256":binary_hash,"cgroup":group,"memory_max_bytes":CAP,
            "memory_swap_max_bytes":0,"rss_limit_bytes":RSS_LIMIT,
            "command":format!("{index:?}"),"class":"synthetic Linux memory acceptance"}),
        )?;
        let started = tracker.start(
            "index_full",
            Some("Index every source under the 4 GiB cgroup"),
        );
        let mut child = index.spawn()?;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            tracker.metrics(
                "index_cgroup_sample",
                &E2ePerformanceMetrics::new()
                    .with_duration(u64::try_from(started.elapsed().as_millis())?)
                    .with_custom("processes", child_samples(child.id()))
                    .with_custom(
                        "memory_current",
                        fs::read_to_string(group.join("memory.current"))?.trim(),
                    )
                    .with_custom(
                        "memory_peak",
                        fs::read_to_string(group.join("memory.peak"))?.trim(),
                    )
                    .with_custom(
                        "memory_events",
                        fs::read_to_string(group.join("memory.events"))?,
                    ),
            );
            tracker.flush();
            std::thread::sleep(Duration::from_secs(5));
        };
        tracker.end("index_full", Some("Actual process terminated"), started);
        let usage: Value = serde_json::from_slice(&fs::read(&usage)?)?;
        let peak = usage["peak_rss_bytes"]
            .as_u64()
            .context("Linux wait4 did not record an exact child peak")?;
        tracker.metrics(
            "index_peak",
            &E2ePerformanceMetrics::new()
                .with_peak_memory(peak)
                .with_custom("limit_bytes", RSS_LIMIT)
                .with_custom("exit_code", status.code()),
        );
        write_json(
            &root.join("memory-result.json"),
            &json!({"exit_code":status.code(),
            "wall_ms":started.elapsed().as_millis(),"peak_rss_bytes":peak,
            "rss_limit_bytes":RSS_LIMIT,"rss_within_limit":peak<=RSS_LIMIT}),
        )?;
        ensure!(
            status.success(),
            "full index failed; retained stdout/stderr and cgroup samples"
        );
        ensure!(usage["exit_code"] == 0, "measured child did not succeed");
        ensure!(
            peak > 0 && peak <= RSS_LIMIT,
            "peak RSS {peak} exceeds {RSS_LIMIT}"
        );
        let indexed: Value = serde_json::from_slice(&fs::read(root.join("index.stdout.json"))?)?;
        ensure!(indexed["success"] == true && indexed["conversations"] == 41);
        ensure!(indexed["messages"] == MESSAGES && indexed["quarantined_conversations"] == 0);
        ensure!(indexed["final_wal_checkpoint"]["status"] == "completed");
        let database = data.join("agent_search.db");
        let before = hash_file(&database)?;
        tracker.phase("canonical_integrity", "Check an independent quiescent database copy", || -> Result<()> {
            let copy = root.join("canonical-check.db");
            fs::copy(&database, &copy)?;
            let output = command(tracker, &home, Command::new("/usr/bin/timeout"))
                .args(["120s", "sqlite3", "-batch", "-noheader", "-readonly"])
                .arg(&copy).arg("PRAGMA quick_check; SELECT count(*) FROM conversations; SELECT count(*) FROM messages;").output()?;
            fs::write(root.join("canonical-check.stdout"), &output.stdout)?;
            fs::write(root.join("canonical-check.stderr"), &output.stderr)?;
            ensure!(output.status.success(), "stock SQLite integrity probe failed");
            ensure!(String::from_utf8(output.stdout)?.trim() == "ok\n41\n318800");
            Ok(())
        })?;
        tracker.phase("readiness_and_search", "Readiness plus all giant-message tokens remain searchable", || -> Result<()> {
            for (label, args) in [
                ("health", vec!["health", "--json"]),
                ("status", vec!["status", "--json"]),
                ("search", vec!["search", "rssqualificationgiant", "--robot", "--robot-meta", "--limit", "1", "--fields", "minimal"]),
            ] {
                let mut probe = command(tracker, &home, Command::new("/usr/bin/timeout"));
                probe.arg("120s").arg(&binary).args(args).arg("--data-dir").arg(&data);
                let output = probe.output()?;
                fs::write(root.join(format!("{label}.stdout.json")), &output.stdout)?;
                fs::write(root.join(format!("{label}.stderr.log")), &output.stderr)?;
                ensure!(output.status.success(), "{label} failed");
                let report: Value = serde_json::from_slice(&output.stdout)?;
                if label == "search" {
                    ensure!(report["total_matches"] == 300_000 && report["count"] == 1,
                        "every giant-conversation message must retain its searchable text: {report}");
                } else {
                    ensure!(report["healthy"] == true && report["status"] == "healthy");
                    if label == "status" {
                        ensure!(report["index"]["status"] == "ready");
                        ensure!(report["index"]["hollow"] == false);
                        ensure!(report["index"]["live_documents"] == MESSAGES);
                    }
                }
            }
            Ok(())
        })?;
        ensure!(
            before == hash_file(&database)?,
            "readiness probes changed canonical bytes"
        );
        ensure!(
            binary_hash == hash_file(&binary)?,
            "ELF changed during qualification"
        );
        for source in sources {
            let path = Path::new(source["path"].as_str().context("source path")?);
            ensure!(
                hash_file(path)? == source["sha256"],
                "fixture source changed"
            );
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires admitted Linux host, fresh 4 GiB cgroup/swap0, E2E_LOG=1 and CASS_E2E_RUN_ID; retains several GiB"]
    fn giant_native_source_completes_below_85_percent_of_four_gib() -> Result<()> {
        ensure!(dotenvy::var("E2E_LOG").ok().as_deref() == Some("1"));
        let run_id = dotenvy::var("CASS_E2E_RUN_ID").context("a unique run ID is required")?;
        let project = dotenvy::var("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir()?);
        ensure!(
            !project
                .join("test-results/e2e/runs")
                .join(&run_id)
                .join("memory_tests/giant_native_source_completes_below_85_percent_of_four_gib")
                .exists(),
            "this run already has memory acceptance artifacts; use a fresh run ID"
        );
        let logger = E2eLogger::new("rust")?;
        logger.run_start(None)?;
        logger.flush()?;
        let started = Instant::now();
        let tracker = PhaseTracker::new(
            "memory_tests",
            "giant_native_source_completes_below_85_percent_of_four_gib",
        );
        let result = run(&tracker);
        let failed = result.is_err();
        match &result {
            Ok(()) => tracker.complete(),
            Err(error) => tracker.fail(E2eError::with_type(
                format!("{error:#}"),
                "MEMORY_ACCEPTANCE_FAILED",
            )),
        }
        logger.run_end(
            E2eRunSummary {
                total: 1,
                passed: u32::from(!failed),
                failed: u32::from(failed),
                skipped: 0,
                flaky: None,
                duration_ms: u64::try_from(started.elapsed().as_millis())?,
            },
            i32::from(failed),
        )?;
        logger.flush()?;
        result
    }
}
