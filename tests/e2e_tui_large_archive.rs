//! GH #395 / coding_agent_session_search-2l1b0.22: prove large-archive first
//! frame and the core TUI journeys on the real Linux terminal stack.
//!
//! Every assertion here runs the shipped `cass` binary inside a real PTY:
//! kernel winsize negotiation, the ftui native backend, the byte parser, the
//! real search engine and the real archive. Nothing is headless and no TUI
//! component is replaced.
//!
//! Journeys covered by `tui_large_archive_first_frame_and_core_journeys`:
//! first usable frame, initial lexical search, selection navigation, detail
//! citation, resize, help overlay and clean cancellation at a small and a
//! large terminal size, with a large indexed archive and no semantic model.
//!
//! `tui_large_archive_missing_index_does_not_block_first_frame` starts the
//! TUI on the same large corpus with no derived index or archive; the shipped
//! binary's first-run path spawns the real detached `cass index` writer, and
//! the test proves the first frame and key handling do not wait for it and
//! that ForceQuit returns without waiting for the writer.
//!
//! Run with:
//! `cargo test --test e2e_tui_large_archive -- --nocapture --test-threads=1`

#![cfg(unix)]

use portable_pty::{CommandBuilder, ExitStatus, PtyPair, PtySize, native_pty_system};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

mod util;
use util::e2e_log::{E2ePerformanceMetrics, PhaseTracker};

const TUI_READY_MARKER: &str = "Search sessions, messages, code";
const HELP_MARKER: &str = "Quick Start & Shortcuts";
const DETAIL_MARKER: &str = "Detail [Messages]";
const NARROW_QUERY: &str = "lighthousekeeper";
const NARROW_RESULT_MARKER: &str = "lhalpha";
const BROAD_QUERY: &str = "routine";
const BROAD_RESULT_MARKER: &str = "engineering note";

fn detail_visible(rendered: &str) -> bool {
    rendered.contains(DETAIL_MARKER)
        || (rendered.contains("Detail") && rendered.contains("Messages"))
}

const FIRST_FRAME_BUDGET_MS: u64 = 5_000;
const SEARCH_BUDGET_MS: u64 = 4_000;
const NAV_BUDGET_MS: u64 = 1_500;
const DETAIL_BUDGET_MS: u64 = 4_000;
const RESIZE_BUDGET_MS: u64 = 2_500;
const HELP_BUDGET_MS: u64 = 2_500;
const CANCEL_BUDGET_MS: u64 = 3_000;

/// Terminal shapes exercised by the proof: a tight 100x30 and a roomy 200x60.
const SMALL_SIZE: PtySize = size(100, 30);
const LARGE_SIZE: PtySize = size(200, 60);

const fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn tui_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    match LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn cass_bin() -> &'static str {
    env!("CARGO_BIN_EXE_cass")
}

// =============================================================================
// Fixtures
// =============================================================================

/// A large Codex corpus. The data dir lives outside the OS temp dir because
/// the TUI deliberately skips background indexing for scratch temp dirs.
struct Fixture {
    root: tempfile::TempDir,
    home: PathBuf,
    xdg_data: PathBuf,
    xdg_config: PathBuf,
    data_dir: PathBuf,
    codex_home: PathBuf,
    cloudmcp_root: PathBuf,
    openwebui_root: PathBuf,
    child_tmp: PathBuf,
    sessions: usize,
    messages_per_session: usize,
}

impl Fixture {
    fn create(sessions: usize, messages_per_session: usize) -> Self {
        // The corpus must not live under the OS temp dir: the TUI skips
        // first-run/background indexing for scratch dirs (`/tmp`, `/var/tmp`).
        // Prefer /dev/shm so the index headroom check is not starved by a full
        // workspace disk; fall back to the repo for hosts without it.
        let mut root = tempfile::Builder::new()
            .prefix("gh395-tui-")
            .tempdir_in("/dev/shm");
        if root.is_err() {
            let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".local-state");
            fs::create_dir_all(&base).expect("create fallback fixture base");
            root = tempfile::Builder::new()
                .prefix("gh395-tui-")
                .tempdir_in(&base);
        }
        let root = root.expect("create fixture root outside the OS temp dir");
        let home = root.path().join("home");
        let xdg_data = root.path().join("xdg-data");
        let xdg_config = root.path().join("xdg-config");
        let data_dir = root.path().join("cass_data");
        let codex_home = root.path().join("codex_home");
        let cloudmcp_root = root.path().join("cloudmcp-empty");
        let openwebui_root = root.path().join("openwebui-empty");
        let child_tmp = root.path().join("tmp");
        for dir in [
            &home,
            &xdg_data,
            &xdg_config,
            &data_dir,
            &codex_home,
            &cloudmcp_root,
            &openwebui_root,
            &child_tmp,
        ] {
            fs::create_dir_all(dir).expect("create fixture dir");
        }
        fs::write(
            data_dir.join("tui_state.json"),
            r#"{"version":1,"has_seen_help":true,"help_pinned":false}"#,
        )
        .expect("seed TUI state");
        write_codex_corpus(&codex_home, sessions, messages_per_session);
        Self {
            root,
            home,
            xdg_data,
            xdg_config,
            data_dir,
            codex_home,
            cloudmcp_root,
            openwebui_root,
            child_tmp,
            sessions,
            messages_per_session,
        }
    }

    fn index_dir(&self) -> PathBuf {
        coding_agent_search::search::tantivy::expected_index_dir(&self.data_dir)
    }

    fn db_path(&self) -> PathBuf {
        self.data_dir.join("agent_search.db")
    }

    fn model_dir(&self) -> PathBuf {
        self.data_dir.join("models")
    }

    fn archive_summary(&self) -> serde_json::Value {
        let db_bytes = fs::metadata(self.db_path()).map(|m| m.len()).unwrap_or(0);
        let index_bytes = dir_size(&self.data_dir.join("index"));
        serde_json::json!({
            "sessions": self.sessions,
            "messages_per_session": self.messages_per_session,
            "expected_messages": self.sessions * self.messages_per_session,
            "source_bytes": dir_size(&self.codex_home),
            "db_bytes": db_bytes,
            "index_bytes": index_bytes,
            "index_present": self.index_dir().exists(),
            "semantic_model_present": self.model_dir().exists(),
        })
    }

    fn apply_env(&self, cmd: &mut Command) {
        cmd.env("HOME", &self.home)
            .env("XDG_DATA_HOME", &self.xdg_data)
            .env("XDG_CONFIG_HOME", &self.xdg_config)
            .env("CASS_DATA_DIR", &self.data_dir)
            .env("CODEX_HOME", &self.codex_home)
            .env("CASS_CLOUDMCP_DATA_ROOT", &self.cloudmcp_root)
            .env("CASS_OPENWEBUI_DATA_ROOT", &self.openwebui_root)
            .env("TMPDIR", &self.child_tmp)
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("NO_COLOR", "1")
            .env("CASS_RESPECT_NO_COLOR", "1")
            .env("TERM", "xterm-256color")
            .env("CASS_TUI_RUNTIME", "ftui")
            .env_remove("TUI_HEADLESS")
            .env_remove("CASS_AUTO_REFRESH")
            .env_remove("CASS_DB_PATH")
            .env_remove("CASS_TUI_KITTY_KEYBOARD")
            .current_dir(&self.home);
    }

    fn apply_pty_env(&self, cmd: &mut CommandBuilder) {
        cmd.env("HOME", self.home.to_string_lossy().as_ref());
        cmd.env("XDG_DATA_HOME", self.xdg_data.to_string_lossy().as_ref());
        cmd.env(
            "XDG_CONFIG_HOME",
            self.xdg_config.to_string_lossy().as_ref(),
        );
        cmd.env("CASS_DATA_DIR", self.data_dir.to_string_lossy().as_ref());
        cmd.env("CODEX_HOME", self.codex_home.to_string_lossy().as_ref());
        cmd.env(
            "CASS_CLOUDMCP_DATA_ROOT",
            self.cloudmcp_root.to_string_lossy().as_ref(),
        );
        cmd.env(
            "CASS_OPENWEBUI_DATA_ROOT",
            self.openwebui_root.to_string_lossy().as_ref(),
        );
        cmd.env("TMPDIR", self.child_tmp.to_string_lossy().as_ref());
        cmd.env("CASS_IGNORE_SOURCES_CONFIG", "1");
        cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
        cmd.env("NO_COLOR", "1");
        cmd.env("CASS_RESPECT_NO_COLOR", "1");
        cmd.env("TERM", "xterm-256color");
        cmd.env("CASS_TUI_RUNTIME", "ftui");
        cmd.env_remove("TUI_HEADLESS");
        cmd.env_remove("CASS_AUTO_REFRESH");
        cmd.env_remove("CASS_DB_PATH");
        cmd.env_remove("CASS_TUI_KITTY_KEYBOARD");
        cmd.cwd(self.home.to_string_lossy().as_ref());
    }

    fn cass_command(&self) -> Command {
        let mut cmd = Command::new(cass_bin());
        self.apply_env(&mut cmd);
        cmd
    }

    fn index(&self, tracker: &PhaseTracker) -> serde_json::Value {
        let start = tracker.start(
            "index_large_corpus",
            Some("Build the derived lexical index"),
        );
        let output = self
            .cass_command()
            .args(["index", "--full", "--json", "--no-progress-events"])
            .arg("--data-dir")
            .arg(&self.data_dir)
            .output()
            .expect("spawn cass index for large fixture");
        tracker.end(
            "index_large_corpus",
            Some("Large fixture index complete"),
            start,
        );
        assert!(
            output.status.success(),
            "large fixture index failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("parse index json")
    }

    fn wait_for_index_lock(&self, timeout: Duration) -> bool {
        let lock = self.data_dir.join("index-run.lock");
        let started = Instant::now();
        while started.elapsed() < timeout {
            if let Ok(file) = fs::OpenOptions::new().read(true).write(true).open(&lock) {
                // The writer holds an exclusive flock; a successful try_lock
                // here means no index run owns the lock.
                if file.try_lock().is_err()
                    && fs::metadata(&lock).map(|m| m.len() > 0).unwrap_or(false)
                {
                    return true;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            entry
                .metadata()
                .map(|meta| {
                    if meta.is_dir() {
                        dir_size(&entry.path())
                    } else {
                        meta.len()
                    }
                })
                .unwrap_or(0)
        })
        .sum()
}

/// Write a Codex rollout corpus with one conversation per file. Conversations
/// 0 and 1 carry exactly one `lighthousekeeper` message each so a narrow query
/// has a deterministic two-hit (one per conversation) result set, and every
/// message in those conversations repeats a short `lhalpha`/`lhbeta` marker so
/// the detail view can attribute the selected conversation. Every conversation
/// carries a shared `routine engineering note` phrase for the broad query.
fn write_codex_corpus(codex_home: &Path, sessions: usize, messages_per_session: usize) {
    for session in 0..sessions {
        let day = 1 + (session % 27);
        let dir = codex_home
            .join("sessions/2026/09")
            .join(format!("{day:02}"));
        fs::create_dir_all(&dir).expect("create codex session dir");
        let path = dir.join(format!("rollout-{session:05}.jsonl"));
        let mut body = String::with_capacity(256 * messages_per_session);
        body.push_str(&format!(
            "{{\"type\":\"session_meta\",\"timestamp\":{},\"payload\":{{\"cwd\":\"/warp/work/workspace-{:02}\"}}}}\n",
            1_760_000_000_000_i64 + session as i64 * 10_000,
            session % 32
        ));
        for message in 0..messages_per_session {
            let role = if message % 2 == 0 {
                "user"
            } else {
                "assistant"
            };
            let content = match session {
                0 if message == 0 => {
                    "lhalpha lighthousekeeper conversation conv-0000 query hit".to_string()
                }
                1 if message == 0 => {
                    "lhbeta lighthousekeeper conversation conv-0001 query hit".to_string()
                }
                0 => format!("lhalpha conversation conv-{session:04} marker message {message}"),
                1 => format!("lhbeta conversation conv-{session:04} marker message {message}"),
                _ if message == 0 || message % 3 == 0 => format!(
                    "routine engineering note conversation conv-{session:04} workspace {:02}",
                    session % 32
                ),
                _ => format!(
                    "conversation conv-{session:04} follow-up detail {message} workspace {:02}",
                    session % 32
                ),
            };
            let timestamp = 1_760_000_000_000_i64 + session as i64 * 10_000 + message as i64 * 100;
            if role == "user" {
                body.push_str(&format!(
                    "{{\"type\":\"event_msg\",\"timestamp\":{timestamp},\"payload\":{{\"type\":\"user_message\",\"message\":\"{content}\"}}}}\n"
                ));
            } else {
                body.push_str(&format!(
                    "{{\"type\":\"response_item\",\"timestamp\":{timestamp},\"payload\":{{\"role\":\"assistant\",\"content\":\"{content}\"}}}}\n"
                ));
            }
        }
        fs::write(&path, body).expect("write codex rollout");
    }
}

// =============================================================================
// PTY session
// =============================================================================

struct TuiSession {
    pair: PtyPair,
    writer: Box<dyn Write + Send>,
    captured: Arc<Mutex<Vec<u8>>>,
    reader: Option<thread::JoinHandle<()>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    spawn_at: Instant,
}

impl TuiSession {
    fn spawn(
        fixture: &Fixture,
        size: PtySize,
        latency_trace: Option<&Path>,
        memory_limit_kib: Option<u64>,
    ) -> Self {
        let pair = native_pty_system()
            .openpty(size)
            .expect("open PTY for cass TUI");
        let reader = pair.master.try_clone_reader().expect("clone PTY reader");
        let writer = pair.master.take_writer().expect("take PTY writer");
        let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
        let reader_captured = Arc::clone(&captured);
        let reader_thread = thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => reader_captured
                        .lock()
                        .expect("capture lock")
                        .extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
        });

        let mut cmd = if let Some(kib) = memory_limit_kib {
            // Constrained-memory proof: RLIMIT_AS on the actual TUI process.
            let mut cmd = CommandBuilder::new("/bin/sh");
            cmd.arg("-c");
            cmd.arg(format!("ulimit -v {kib}; exec \"$1\" tui"));
            cmd.arg("cass-pty");
            cmd.arg(cass_bin());
            cmd
        } else {
            let mut cmd = CommandBuilder::new(cass_bin());
            cmd.arg("tui");
            cmd
        };
        fixture.apply_pty_env(&mut cmd);
        if let Some(trace) = latency_trace {
            cmd.env(
                "CASS_TUI_LATENCY_TRACE_FILE",
                trace.to_string_lossy().as_ref(),
            );
        }
        let spawn_at = Instant::now();
        let child = pair
            .slave
            .spawn_command(cmd)
            .expect("spawn cass tui inside PTY");
        Self {
            pair,
            writer,
            captured,
            reader: Some(reader_thread),
            child,
            spawn_at,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write to PTY");
        self.writer.flush().expect("flush PTY");
    }

    fn captured_bytes(&self) -> Vec<u8> {
        self.captured.lock().expect("capture lock").clone()
    }

    fn captured_len(&self) -> usize {
        self.captured.lock().expect("capture lock").len()
    }

    fn rendered_since(&self, since: usize) -> String {
        let data = self.captured.lock().expect("capture lock");
        let tail = data.get(since..).unwrap_or_default();
        strip_terminal_control_sequences(tail)
    }

    fn wait_render_since(
        &self,
        since: usize,
        timeout: Duration,
        predicate: impl Fn(&str) -> bool,
    ) -> bool {
        let started = Instant::now();
        loop {
            if predicate(&self.rendered_since(since)) {
                return true;
            }
            if started.elapsed() >= timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_growth(&self, since: usize, min_delta: usize, timeout: Duration) -> bool {
        let started = Instant::now();
        loop {
            if self.captured_len() >= since.saturating_add(min_delta) {
                return true;
            }
            if started.elapsed() >= timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn resize(&self, size: PtySize) {
        self.pair.master.resize(size).expect("resize PTY");
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let started = Instant::now();
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {
                    if started.elapsed() >= timeout {
                        let _ = self.child.kill();
                        return None;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return None,
            }
        }
    }

    /// Restore-check with `stty -a` on the same slave after the TUI exits, then
    /// collect the PTY capture.
    fn finish(mut self) -> (Vec<u8>, String, bool) {
        let mut stty = CommandBuilder::new("stty");
        stty.arg("-a");
        let mut restore_ok = false;
        if let Ok(mut stty_child) = self.pair.slave.spawn_command(stty) {
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(3) {
                match stty_child.try_wait() {
                    Ok(Some(status)) => {
                        restore_ok = status.success();
                        break;
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
            let _ = stty_child.wait();
        }
        thread::sleep(Duration::from_millis(50));
        let raw = self.captured.lock().expect("capture lock").clone();
        let rendered = strip_terminal_control_sequences(&raw);
        drop(self.writer);
        drop(self.pair);
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
        (raw, rendered, restore_ok)
    }
}

fn strip_terminal_control_sequences(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    let _ = chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    let _ = chars.next();
                    let mut prev = '\0';
                    for c in chars.by_ref() {
                        if c == '\u{7}' || (prev == '\u{1b}' && c == '\\') {
                            break;
                        }
                        prev = c;
                    }
                }
                Some('P') | Some('^') | Some('_') => {
                    let _ = chars.next();
                    let mut prev = '\0';
                    for c in chars.by_ref() {
                        if prev == '\u{1b}' && c == '\\' {
                            break;
                        }
                        prev = c;
                    }
                }
                Some(_) => {
                    let _ = chars.next();
                }
                None => {}
            }
            continue;
        }
        if ch.is_control() && ch != '\n' && ch != '\t' {
            continue;
        }
        out.push(ch);
    }
    out
}

// =============================================================================
// Evidence
// =============================================================================

fn artifact_dir() -> PathBuf {
    let dir = PathBuf::from("test-results/e2e/tui_large_archive");
    fs::create_dir_all(&dir).expect("create artifact dir");
    dir
}

fn save_artifact(name: &str, trace_id: &str, bytes: &[u8]) -> PathBuf {
    let path = artifact_dir().join(format!("{trace_id}_{name}"));
    fs::write(&path, bytes).expect("write artifact");
    eprintln!("[gh395-tui] artifact {}", path.display());
    path
}

fn git_head() -> String {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn platform() -> serde_json::Value {
    serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "term": "xterm-256color",
    })
}

fn directory_has_entries(path: &Path) -> bool {
    fs::read_dir(path)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

// =============================================================================
// Test 1: large indexed archive, strict terminal sizes, no model
// =============================================================================

#[test]
fn tui_large_archive_first_frame_and_core_journeys() {
    let _guard = tui_guard();
    let tracker = PhaseTracker::new(
        "e2e_tui_large_archive",
        "tui_large_archive_first_frame_and_core_journeys",
    );
    let trace_id = tracker.trace_id().to_string();
    let fixture = Fixture::create(400, 16);
    let index = fixture.index(&tracker);
    let corpus = fixture.archive_summary();
    assert!(
        index["success"].as_bool().unwrap_or(false),
        "index json reported failure: {index}"
    );
    assert!(
        index["messages"].as_u64().unwrap_or(0) >= 6_400,
        "large corpus was not fully indexed: {index}"
    );
    assert!(
        index["conversations"].as_u64().unwrap_or(0) >= 400,
        "large corpus conversations missing: {index}"
    );
    assert!(
        fixture.index_dir().exists(),
        "derived index missing after a successful index run"
    );
    assert!(
        !fixture.model_dir().exists(),
        "fixture must start without a semantic model: {}",
        fixture.model_dir().display()
    );
    assert!(
        directory_has_entries(&fixture.index_dir()),
        "derived index directory is empty"
    );

    let mut runs = Vec::new();
    for (label, terminal_size) in [("small", SMALL_SIZE), ("large", LARGE_SIZE)] {
        let latency_trace = fixture.data_dir.join(format!("latency-{label}.json"));
        let mut session = TuiSession::spawn(&fixture, terminal_size, Some(&latency_trace), None);

        // ---- first usable frame ------------------------------------------
        assert!(
            session.wait_render_since(
                0,
                Duration::from_millis(FIRST_FRAME_BUDGET_MS),
                |rendered| rendered.contains(TUI_READY_MARKER),
            ),
            "[{label}] no usable first frame within {FIRST_FRAME_BUDGET_MS}ms; capture tail: {}",
            tail(&session.rendered_since(0), 2000)
        );
        let first_frame_ms = session.spawn_at.elapsed().as_millis() as u64;
        assert!(
            first_frame_ms <= FIRST_FRAME_BUDGET_MS,
            "[{label}] first frame {first_frame_ms}ms > {FIRST_FRAME_BUDGET_MS}ms"
        );

        // ---- initial lexical search (narrow, citation-deterministic) ------
        // Live search: the first keystroke starts the budget, and the visible
        // result proves the debounce, engine and render completed.
        let narrow_base = session.captured_len();
        let search_start = Instant::now();
        session.write(NARROW_QUERY.as_bytes());
        assert!(
            session.wait_render_since(
                narrow_base,
                Duration::from_millis(SEARCH_BUDGET_MS),
                |rendered| rendered.contains(NARROW_RESULT_MARKER),
            ),
            "[{label}] initial lexical search produced no visible hit within {SEARCH_BUDGET_MS}ms; tail: {}",
            tail(&session.rendered_since(narrow_base), 2000)
        );
        let search_ms = search_start.elapsed().as_millis() as u64;
        assert!(
            search_ms <= SEARCH_BUDGET_MS,
            "[{label}] key-to-result {search_ms}ms > {SEARCH_BUDGET_MS}ms"
        );
        // Let the result frame and its hit list settle before navigating.
        thread::sleep(Duration::from_millis(40));

        // ---- selection navigation + detail/citation ----------------------
        // Search results carry no selection until the user navigates, and
        // NO_COLOR selection changes may not emit bytes of their own. Prove
        // navigation by opening the cited hit: the visible conversation
        // repeats the selected fixture token once per message, while the
        // result list shows each token at most once.
        let first_detail_base = session.captured_len();
        let first_detail_start = Instant::now();
        session.write(b"\x1b[B\r"); // Down selects the first hit, Enter opens it
        assert!(
            session.wait_render_since(
                first_detail_base,
                Duration::from_millis(DETAIL_BUDGET_MS),
                detail_visible,
            ),
            "[{label}] detail modal did not open within {DETAIL_BUDGET_MS}ms; tail: {}",
            tail(&session.rendered_since(first_detail_base), 2000)
        );
        let detail_ms = first_detail_start.elapsed().as_millis() as u64;
        assert!(
            detail_ms <= DETAIL_BUDGET_MS,
            "[{label}] detail latency {detail_ms}ms > {DETAIL_BUDGET_MS}ms"
        );
        let first_detail = session.rendered_since(first_detail_base);
        // The fixture session cwd is `/warp/work/workspace-<session%32>`, so
        // the detail header workspace chip attributes the selected hit
        // independently of the result-list rows (whose narrow titles carry no
        // workspace). Each fixture message then repeats its conversation
        // marker; both signals must agree.
        let first_ws0 = first_detail.matches("workspace-00").count();
        let first_ws1 = first_detail.matches("workspace-01").count();
        assert!(
            first_ws0 != first_ws1,
            "[{label}] detail header did not attribute exactly one selected hit (ws0={first_ws0}, ws1={first_ws1}); tail: {}",
            tail(&first_detail, 2000)
        );
        let first_alpha = first_detail.matches("lhalpha").count();
        let first_beta = first_detail.matches("lhbeta").count();
        assert!(
            first_alpha != first_beta,
            "[{label}] detail did not cite exactly one selected fixture conversation (alpha={first_alpha}, beta={first_beta}); tail: {}",
            tail(&first_detail, 2000)
        );
        assert!(
            first_alpha.max(first_beta) >= 2,
            "[{label}] detail rendered too few selected-conversation messages (alpha={first_alpha}, beta={first_beta})"
        );
        let first_is_alpha = first_ws0 > first_ws1;
        assert!(
            (first_alpha > first_beta) == first_is_alpha,
            "[{label}] detail header workspace and cited message content disagree (ws0={first_ws0}, ws1={first_ws1}, alpha={first_alpha}, beta={first_beta})"
        );
        let expected_conv = if first_is_alpha {
            "conv-0000"
        } else {
            "conv-0001"
        };
        assert!(
            first_detail.contains(expected_conv),
            "[{label}] detail did not cite the selected conversation ({expected_conv}); tail: {}",
            tail(&first_detail, 2000)
        );
        assert!(
            first_detail.contains("rollout-0000")
                || first_detail.contains("sessions/2026")
                || first_detail.contains("/warp/work/workspace-"),
            "[{label}] detail did not cite the selected hit's source path; tail: {}",
            tail(&first_detail, 2000)
        );

        // Close the modal; the query surface must come back.
        let close_base = session.captured_len();
        session.write(b"\x1b");
        assert!(
            session.wait_growth(close_base, 4, Duration::from_millis(RESIZE_BUDGET_MS)),
            "[{label}] closing the detail modal produced no frame"
        );
        thread::sleep(Duration::from_millis(150));

        // Up moves from the last hit back to the first (ftui clamps Down at
        // the end of a two-hit list); opening it must flip the cited
        // conversation. The measured span covers selection + detail render.
        let nav_base = session.captured_len();
        let nav_start = Instant::now();
        session.write(b"\x1b[A\r"); // Up moves to the other hit, Enter opens it
        assert!(
            session.wait_render_since(
                nav_base,
                Duration::from_millis(NAV_BUDGET_MS),
                detail_visible,
            ),
            "[{label}] selecting and opening the next hit produced no detail within {NAV_BUDGET_MS}ms; tail: {}",
            tail(&session.rendered_since(nav_base), 2000)
        );
        let nav_ms = nav_start.elapsed().as_millis() as u64;
        assert!(
            nav_ms <= NAV_BUDGET_MS,
            "[{label}] navigation latency {nav_ms}ms > {NAV_BUDGET_MS}ms"
        );
        let second_detail = session.rendered_since(nav_base);
        let second_ws0 = second_detail.matches("workspace-00").count();
        let second_ws1 = second_detail.matches("workspace-01").count();
        let second_alpha = second_detail.matches("lhalpha").count();
        let second_beta = second_detail.matches("lhbeta").count();
        assert!(
            second_ws0 != second_ws1 && (second_ws0 > second_ws1) != first_is_alpha,
            "[{label}] Up did not move the selection to the other fixture hit (first alpha={first_is_alpha}; second ws0={second_ws0}, ws1={second_ws1}); tail: {}",
            tail(&second_detail, 2000)
        );
        assert!(
            (second_alpha > second_beta) == (second_ws0 > second_ws1),
            "[{label}] second detail header workspace and cited content disagree (ws0={second_ws0}, ws1={second_ws1}, alpha={second_alpha}, beta={second_beta})"
        );
        assert!(
            second_alpha.max(second_beta) >= 2,
            "[{label}] second hit detail rendered too few messages (alpha={second_alpha}, beta={second_beta})"
        );
        // The child must still be live after two modal cycles.
        assert!(
            session
                .child
                .try_wait()
                .expect("poll after navigation")
                .is_none(),
            "[{label}] TUI exited during navigation"
        );

        // Close the second modal before continuing the journey.
        let close_base = session.captured_len();
        session.write(b"\x1b");
        assert!(
            session.wait_growth(close_base, 4, Duration::from_millis(RESIZE_BUDGET_MS)),
            "[{label}] closing the second detail modal produced no frame"
        );
        thread::sleep(Duration::from_millis(150));

        // ---- resize ------------------------------------------------------
        for (resize_label, resize_size) in [
            (
                "grow",
                size(cols(terminal_size) + 60, rows(terminal_size) + 16),
            ),
            (
                "shrink",
                size(cols(terminal_size) - 12, rows(terminal_size) - 6),
            ),
        ] {
            let base = session.captured_len();
            let started = Instant::now();
            session.resize(resize_size);
            assert!(
                session.wait_growth(base, 4, Duration::from_millis(RESIZE_BUDGET_MS)),
                "[{label}] {resize_label} resize produced no frame within {RESIZE_BUDGET_MS}ms"
            );
            let resize_ms = started.elapsed().as_millis() as u64;
            assert!(
                resize_ms <= RESIZE_BUDGET_MS,
                "[{label}] {resize_label} resize {resize_ms}ms > {RESIZE_BUDGET_MS}ms"
            );
            assert!(
                session
                    .child
                    .try_wait()
                    .expect("poll after resize")
                    .is_none(),
                "[{label}] TUI exited after resize"
            );
        }
        session.resize(terminal_size); // restore the declared terminal shape

        // ---- broad lexical search on the large result set -----------------
        save_artifact(
            &format!("{label}_before_broad.raw"),
            &trace_id,
            &session.captured_bytes(),
        );
        session.write(b"\x0c"); // Ctrl+L clears the query
        thread::sleep(Duration::from_millis(80));
        let broad_base = session.captured_len();
        let broad_start = Instant::now();
        session.write(BROAD_QUERY.as_bytes());
        assert!(
            session.wait_render_since(
                broad_base,
                Duration::from_millis(SEARCH_BUDGET_MS),
                |rendered| rendered.contains(BROAD_RESULT_MARKER),
            ),
            "[{label}] broad key-to-result produced no visible hit within {SEARCH_BUDGET_MS}ms; tail: {}",
            tail(&session.rendered_since(broad_base), 2000)
        );
        let broad_ms = broad_start.elapsed().as_millis() as u64;
        assert!(
            broad_ms <= SEARCH_BUDGET_MS,
            "[{label}] broad key-to-result {broad_ms}ms > {SEARCH_BUDGET_MS}ms"
        );

        // ---- help overlay -------------------------------------------------
        let help_base = session.captured_len();
        let help_start = Instant::now();
        session.write(b"\x1bOP"); // F1
        assert!(
            session.wait_render_since(
                help_base,
                Duration::from_millis(HELP_BUDGET_MS),
                |rendered| rendered.contains(HELP_MARKER),
            ),
            "[{label}] help overlay did not open within {HELP_BUDGET_MS}ms; tail: {}",
            tail(&session.rendered_since(help_base), 2000)
        );
        let help_ms = help_start.elapsed().as_millis() as u64;
        assert!(
            help_ms <= HELP_BUDGET_MS,
            "[{label}] help latency {help_ms}ms > {HELP_BUDGET_MS}ms"
        );
        // F1 toggles deterministically; ESC would be ambiguous with a following
        // control byte until the terminal parser's escape timeout elapses.
        let help_close_base = session.captured_len();
        session.write(b"\x1bOP"); // F1 closes
        assert!(
            session.wait_render_since(
                help_close_base,
                Duration::from_millis(HELP_BUDGET_MS),
                |rendered| rendered.contains(BROAD_RESULT_MARKER),
            ),
            "[{label}] help overlay did not close back to the results within {HELP_BUDGET_MS}ms; tail: {}",
            tail(&session.rendered_since(help_close_base), 2000)
        );

        // ---- cancellation while the large archive is still live -----------
        let cancel_start = Instant::now();
        session.write(b"\x03");
        let status = session.wait_exit(Duration::from_millis(CANCEL_BUDGET_MS));
        let cancel_ms = cancel_start.elapsed().as_millis() as u64;
        assert!(
            status.is_some(),
            "[{label}] TUI did not exit within {CANCEL_BUDGET_MS}ms of Ctrl+C"
        );
        let status = status.expect("status");
        assert!(
            status.success(),
            "[{label}] TUI exited unsuccessfully: {status:?}"
        );
        assert!(
            cancel_ms <= CANCEL_BUDGET_MS,
            "[{label}] cancellation took {cancel_ms}ms > {CANCEL_BUDGET_MS}ms"
        );

        // Keyboard mode and cursor must be restored on the same slave after
        // the alternate-screen TUI exits.
        let (raw, rendered, restore_ok) = session.finish();
        assert!(restore_ok, "[{label}] stty -a failed after TUI exit");
        assert!(
            rendered.contains("icanon"),
            "[{label}] terminal was not restored to canonical mode: {}",
            tail(&rendered, 800)
        );
        assert!(
            raw.windows(b"\x1b[?1049l".len())
                .any(|w| w == b"\x1b[?1049l"),
            "[{label}] TUI did not leave the alternate screen"
        );
        assert!(
            raw.windows(b"\x1b[?25h".len()).any(|w| w == b"\x1b[?25h"),
            "[{label}] TUI did not restore the cursor"
        );

        // The model-side trace must independently confirm the live-search
        // input-to-first-visible budget.
        let trace_bytes = fs::read(&latency_trace).expect("read TUI latency trace");
        save_artifact(
            &format!("{label}_latency_trace.json"),
            &trace_id,
            &trace_bytes,
        );
        let trace_json: serde_json::Value =
            serde_json::from_slice(&trace_bytes).expect("parse latency trace");
        let narrow_samples: Vec<&serde_json::Value> = trace_json["samples"]
            .as_array()
            .map(|samples| {
                samples
                    .iter()
                    .filter(|sample| sample["query"].as_str() == Some(NARROW_QUERY))
                    .collect()
            })
            .unwrap_or_default();
        let trace_visible_ms = narrow_samples
            .iter()
            .filter_map(|sample| sample["input_to_first_visible_us"].as_u64())
            .map(|us| us / 1000)
            .max();
        assert!(
            trace_visible_ms.is_some_and(|ms| ms <= SEARCH_BUDGET_MS),
            "[{label}] model-side input-to-visible latency missing or over budget: {trace_visible_ms:?}ms; trace: {trace_json}"
        );

        save_artifact(&format!("{label}_pty_output.raw"), &trace_id, &raw);
        let run = serde_json::json!({
            "size": { "cols": cols(terminal_size), "rows": rows(terminal_size) },
            "first_frame_ms": first_frame_ms,
            "initial_search_ms": search_ms,
            "trace_input_to_visible_ms": trace_visible_ms,
            "navigation_ms": nav_ms,
            "detail_ms": detail_ms,
            "help_ms": help_ms,
            "broad_search_ms": broad_ms,
            "cancel_ms": cancel_ms,
            "terminal_exit": format!("{status:?}"),
            "terminal_restored": restore_ok,
            "capture_bytes": raw.len(),
        });
        eprintln!("[gh395-tui] run {label}: {run}");
        tracker.metrics(
            &format!("tui_large_archive_{label}"),
            &E2ePerformanceMetrics::new()
                .with_duration(first_frame_ms + search_ms)
                .with_custom("first_frame_ms", first_frame_ms)
                .with_custom("initial_search_ms", search_ms)
                .with_custom("navigation_ms", nav_ms)
                .with_custom("cancel_ms", cancel_ms),
        );
        runs.push(run);
    }

    let summary = serde_json::json!({
        "schema_version": 1,
        "bead": "coding_agent_session_search-2l1b0.22",
        "issue": "GH#395",
        "trace_id": trace_id,
        "git_head": git_head(),
        "binary": cass_bin(),
        "command": "cass tui (PTY, CASS_TUI_RUNTIME=ftui, NO_COLOR=1, CASS_IGNORE_SOURCES_CONFIG=1)",
        "platform": platform(),
        "corpus": corpus,
        "index_json": index,
        "model_present_after": fixture.model_dir().exists(),
        "budgets_ms": {
            "first_frame": FIRST_FRAME_BUDGET_MS,
            "initial_search": SEARCH_BUDGET_MS,
            "navigation": NAV_BUDGET_MS,
            "detail": DETAIL_BUDGET_MS,
            "resize": RESIZE_BUDGET_MS,
            "help": HELP_BUDGET_MS,
            "cancel": CANCEL_BUDGET_MS,
        },
        "runs": runs,
    });
    save_artifact(
        "summary.json",
        &trace_id,
        serde_json::to_string_pretty(&summary)
            .expect("serialize summary")
            .as_bytes(),
    );
    eprintln!("[gh395-tui] summary: {summary}");
    tracker.complete();
}

// =============================================================================
// Test 2: missing derived index + TUI-spawned writer + constrained memory
// =============================================================================

#[test]
fn tui_large_archive_missing_index_does_not_block_first_frame() {
    let _guard = tui_guard();
    let tracker = PhaseTracker::new(
        "e2e_tui_large_archive",
        "tui_large_archive_missing_index_does_not_block_first_frame",
    );
    let trace_id = tracker.trace_id().to_string();
    let fixture = Fixture::create(600, 20);
    assert!(
        !fixture.index_dir().exists(),
        "fixture must start without a derived index"
    );
    assert!(
        !fixture.db_path().exists(),
        "fixture must start without an archive"
    );
    assert!(
        !fixture.model_dir().exists(),
        "fixture must start without a semantic model"
    );

    // The TUI's own first-run path spawns the real `cass index --background`
    // child while no archive or index exists. That detached writer is the
    // active writer for this proof: it is started by the shipped binary, not
    // by the harness, and the TUI must stay interactive while it runs.
    let mut session = TuiSession::spawn(
        &fixture,
        LARGE_SIZE,
        None,
        Some(2 * 1024 * 1024), // RLIMIT_AS = 2 GiB
    );

    assert!(
        session.wait_render_since(
            0,
            Duration::from_millis(FIRST_FRAME_BUDGET_MS),
            |rendered| { rendered.contains(TUI_READY_MARKER) }
        ),
        "constrained-memory TUI produced no first frame within {FIRST_FRAME_BUDGET_MS}ms; tail: {}",
        tail(&session.rendered_since(0), 2000)
    );
    let first_frame_ms = session.spawn_at.elapsed().as_millis() as u64;
    assert!(
        first_frame_ms <= FIRST_FRAME_BUDGET_MS,
        "first frame {first_frame_ms}ms > {FIRST_FRAME_BUDGET_MS}ms"
    );
    assert!(
        session.wait_render_since(
            0,
            Duration::from_millis(FIRST_FRAME_BUDGET_MS),
            |rendered| { rendered.contains("Indexing your agent history in the background") }
        ),
        "TUI did not surface its detached first-run indexer without blocking; capture: {}",
        tail(&session.rendered_since(0), 2000)
    );
    let launch_text = session.rendered_since(0);
    let index_pid = status_index_pid(&launch_text).unwrap_or_else(|| {
        panic!(
            "could not read the background indexer pid from the TUI status; capture: {}",
            tail(&launch_text, 2000)
        )
    });
    assert!(
        wait_for_indexer_process(index_pid, Duration::from_secs(5)),
        "TUI status pid {index_pid} is not a live indexer process"
    );
    let writer_running_at_first_frame = process_alive(index_pid);
    assert!(
        writer_running_at_first_frame,
        "background indexer {index_pid} already exited during the first-frame proof"
    );
    assert!(
        fixture.wait_for_index_lock(Duration::from_secs(20)),
        "the TUI-spawned background indexer never acquired index-run.lock within 20s"
    );

    // Event loop stays live while the writer runs: help, close, resize.
    let help_base = session.captured_len();
    let help_start = Instant::now();
    session.write(b"\x1bOP");
    assert!(
        session.wait_render_since(
            help_base,
            Duration::from_millis(HELP_BUDGET_MS),
            |rendered| rendered.contains(HELP_MARKER),
        ),
        "help did not open while the background index ran; tail: {}",
        tail(&session.rendered_since(help_base), 2000)
    );
    let help_ms = help_start.elapsed().as_millis() as u64;
    let help_close_base = session.captured_len();
    session.write(b"\x1b");
    assert!(
        session.wait_growth(help_close_base, 4, Duration::from_millis(HELP_BUDGET_MS)),
        "help did not close while the background index ran"
    );

    let resize_base = session.captured_len();
    let resize_start = Instant::now();
    session.resize(SMALL_SIZE);
    assert!(
        session.wait_growth(resize_base, 4, Duration::from_millis(RESIZE_BUDGET_MS)),
        "resize produced no frame while the background index ran"
    );
    let resize_ms = resize_start.elapsed().as_millis() as u64;
    session.resize(LARGE_SIZE);

    // Cancel the TUI while the writer is still running. The TUI must not wait
    // for background indexing to finish.
    let writer_running_before_quit = process_alive(index_pid);
    let cancel_start = Instant::now();
    session.write(b"\x03");
    let status = session.wait_exit(Duration::from_millis(CANCEL_BUDGET_MS));
    let cancel_ms = cancel_start.elapsed().as_millis() as u64;
    assert!(
        status.is_some(),
        "TUI did not exit within {CANCEL_BUDGET_MS}ms while the background index ran"
    );
    let status = status.expect("status");
    assert!(status.success(), "TUI exited unsuccessfully: {status:?}");
    let writer_running_after_quit = process_alive(index_pid);
    assert!(
        !writer_running_before_quit || writer_running_after_quit,
        "background writer {index_pid} vanished before cancellation, so the no-wait proof is inconclusive"
    );
    assert!(
        cancel_ms <= CANCEL_BUDGET_MS,
        "cancellation while indexing took {cancel_ms}ms > {CANCEL_BUDGET_MS}ms"
    );

    let (raw, rendered, restore_ok) = session.finish();
    assert!(
        restore_ok,
        "stty -a failed after constrained-memory TUI exit"
    );
    assert!(
        rendered.contains("icanon"),
        "terminal was not restored to canonical mode: {}",
        tail(&rendered, 800)
    );
    assert!(
        !fixture.model_dir().exists(),
        "TUI acquired a semantic model while none was installed"
    );

    // The first-run indexer is detached from the TUI; stop it before the
    // fixture disappears so the proof leaves no orphaned writer behind.
    stop_process(index_pid);
    let writer_stopped = !process_alive(index_pid);

    let summary = serde_json::json!({
        "schema_version": 1,
        "bead": "coding_agent_session_search-2l1b0.22",
        "scenario": "large corpus, no derived index, TUI-spawned detached writer, RLIMIT_AS=2GiB",
        "trace_id": trace_id,
        "git_head": git_head(),
        "binary": cass_bin(),
        "command": "cass tui (PTY 200x60, sh -c 'ulimit -v 2097152; exec cass tui')",
        "platform": platform(),
        "corpus": fixture.archive_summary(),
        "indexer_pid": index_pid,
        "writer_running_at_first_frame": writer_running_at_first_frame,
        "writer_running_before_cancel": writer_running_before_quit,
        "writer_running_at_cancel": writer_running_after_quit,
        "writer_stopped_by_test": writer_stopped,
        "first_frame_ms": first_frame_ms,
        "help_ms": help_ms,
        "resize_ms": resize_ms,
        "cancel_ms": cancel_ms,
        "terminal_exit": format!("{status:?}"),
        "terminal_restored": restore_ok,
        "capture_bytes": raw.len(),
        "budgets_ms": {
            "first_frame": FIRST_FRAME_BUDGET_MS,
            "help": HELP_BUDGET_MS,
            "resize": RESIZE_BUDGET_MS,
            "cancel": CANCEL_BUDGET_MS,
        },
    });
    save_artifact(
        "missing_index_summary.json",
        &trace_id,
        serde_json::to_string_pretty(&summary)
            .expect("serialize missing-index summary")
            .as_bytes(),
    );
    save_artifact("missing_index_pty_output.raw", &trace_id, &raw);
    eprintln!("[gh395-tui] missing-index summary: {summary}");
    tracker.metrics(
        "tui_missing_index_first_frame",
        &E2ePerformanceMetrics::new()
            .with_duration(first_frame_ms)
            .with_custom("first_frame_ms", first_frame_ms)
            .with_custom("help_ms", help_ms)
            .with_custom("cancel_ms", cancel_ms),
    );
    tracker.complete();
}

/// Extract the pid the TUI reports for its detached first-run indexer.
fn status_index_pid(rendered: &str) -> Option<u32> {
    let rest = rendered.split("(pid ").nth(1)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn process_is_indexer(pid: u32) -> bool {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|cmdline| String::from_utf8_lossy(&cmdline).contains("index"))
        .unwrap_or(false)
}

fn wait_for_indexer_process(pid: u32, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if process_is_indexer(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

/// Stop a detached indexer, escalating to SIGKILL if SIGTERM is ignored.
fn stop_process(pid: u32) {
    for signal in ["-TERM", "-KILL"] {
        let _ = Command::new("kill")
            .arg(signal)
            .arg(pid.to_string())
            .status();
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(10) {
            if !process_alive(pid) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn cols(size: PtySize) -> u16 {
    size.cols
}

fn rows(size: PtySize) -> u16 {
    size.rows
}

fn tail(text: &str, max_chars: usize) -> String {
    let mut start = text.len().saturating_sub(max_chars);
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    text[start..].to_string()
}
