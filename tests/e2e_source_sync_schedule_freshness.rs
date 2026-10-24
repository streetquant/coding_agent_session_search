//! E2E coverage for source sync freshness and scheduled freshness.
//!
//! Bead: coding_agent_session_search-2l1b0.24 ("Prove source sync and
//! scheduled freshness without duplicate work or authority loss").
//!
//! Every fixture is disposable: a temp HOME, a temp config dir, a temp data
//! dir and — for the transport tests — a loopback sshd with a disposable host
//! key and a disposable authorized key. No fleet host, no real user session
//! and no real OS scheduler is touched.
//!
//! The transport tests are `#[ignore]`d because they need a local sshd
//! (`/usr/sbin/sshd`) plus `ssh-keygen`/`ssh-keyscan`. Run them explicitly:
//!
//! ```text
//! cargo test --test e2e_source_sync_schedule_freshness -- --ignored
//! ```

use serde_json::Value;
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Unique marker embedded in the fixture session, used to prove that a
/// remote-origin conversation is retrievable through the remote source scope
/// only.
const SESSION_MARKER: &str = "casssourcefreshnessmarker";

fn cass_bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_cass")
}

/// A cass invocation isolated from the developer's real environment.
fn cass_cmd(home: &Path) -> Command {
    let mut cmd = Command::new(cass_bin_path());
    isolate_env(&mut cmd, home);
    cmd
}

/// Apply the disposable fixture environment to any command.
fn isolate_env(cmd: &mut Command, home: &Path) {
    // Stop dotenvy's ancestor search at this disposable fixture home, even
    // when the test runner places it underneath the repository.
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(home.join(".env"))
    {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => panic!("create isolated fixture .env: {error}"),
    }
    cmd.env_clear();
    for key in ["PATH", "SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd.current_dir(home);
    cmd.env("HOME", home);
    cmd.env("USERPROFILE", home);
    cmd.env("XDG_CONFIG_HOME", home.join(".config"));
    cmd.env("XDG_DATA_HOME", home.join(".local/share"));
    cmd.env("XDG_CACHE_HOME", home.join(".cache"));
    cmd.env("CLAUDE_CONFIG_DIR", home.join(".claude"));
    cmd.env("CODEX_HOME", home.join(".codex"));
    cmd.env("RUST_MIN_STACK", "134217728");
    cmd.env("TUI_HEADLESS", "1");
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("CASS_RESPONSIVENESS_DISABLE", "1");
    cmd.env("CASS_AUTO_REFRESH", "0");
    // These connectors otherwise fall back to a machine-global capture root.
    cmd.env("CASS_OPENWEBUI_DATA_ROOT", home.join("openwebui"));
    cmd.env("CASS_CLOUDMCP_DATA_ROOT", home.join("cloudmcp"));
}

fn parse_json(stdout: &[u8], label: &str) -> Value {
    let text = String::from_utf8_lossy(stdout);
    serde_json::from_str::<Value>(text.trim())
        .unwrap_or_else(|e| panic!("{label}: stdout must be one JSON document ({e}); got:\n{text}"))
}

fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .unwrap_or_else(|e| panic!("chmod {} to {mode:o}: {e}", path.display()));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

/// One disposable workspace: a home (config lives here), a data dir and a
/// scratch root.
struct Fixture {
    root: PathBuf,
    home: PathBuf,
    data: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        // `.keep()` so the fixture never deletes anything it created.
        let root = tempfile::tempdir()
            .unwrap_or_else(|e| panic!("create fixture root: {e}"))
            .keep()
            .join(label);
        let home = root.join("home");
        let data = root.join("data");
        fs::create_dir_all(&home).expect("create fixture home");
        fs::create_dir_all(&data).expect("create fixture data dir");
        fs::write(home.join(".env"), "").expect("create fixture .env");
        Self { root, home, data }
    }

    fn write_sources_toml(&self, contents: &str) {
        let path = self.home.join(".config").join("cass").join("sources.toml");
        fs::create_dir_all(path.parent().expect("config parent")).expect("create config dir");
        fs::write(path, contents).expect("write sources.toml");
    }

    fn cmd(&self) -> Command {
        let mut cmd = cass_cmd(&self.home);
        cmd.env("CASS_DATA_DIR", &self.data);
        cmd
    }

    /// The durable sync ledger as it is on disk right now.
    fn ledger(&self) -> Value {
        let path = self.data.join("sync_status.json");
        let text = fs::read_to_string(&path).unwrap_or_default();
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    /// Rewrite the ledger's `last_sync` for one source, so a scheduled run
    /// can be observed at a chosen point in the source's schedule.
    fn age_ledger_entry(&self, source: &str, age_ms: i64) {
        let path = self.data.join("sync_status.json");
        let mut ledger: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read ledger"))
                .expect("ledger parses");
        ledger["sources"][source]["last_sync"] = Value::from(current_unix_ms() - age_ms);
        fs::write(
            path,
            serde_json::to_vec_pretty(&ledger).expect("ledger serializes"),
        )
        .expect("write ledger");
    }
}

/// The codex-shaped session fixture the connectors understand.
fn session_jsonl(marker: &str) -> String {
    format!(
        "{{\"timestamp\":\"2026-09-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"fixture-{marker}\",\"cwd\":\"/synthetic/project\",\"cli_version\":\"0.42.0\"}}}}\n\
         {{\"timestamp\":\"2026-09-01T00:00:01Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"{marker}\"}}]}}}}\n"
    )
}

// ---------------------------------------------------------------------------
// Durable ledger: no lost update, no duplicate work, no lost authority
// ---------------------------------------------------------------------------

/// Two writers of the durable sync ledger must not clobber each other.
///
/// A whole-file replace lets the later writer drop the earlier writer's
/// record: that source looks never-synced again, so the next scheduled tick
/// syncs it a second time (duplicate work), and its failure/backoff history
/// is gone (lost authority).
#[test]
fn durable_sync_ledger_keeps_a_concurrent_writers_record() {
    use coding_agent_search::sources::sync::{SourceSyncInfo, SyncResult, SyncStatus};

    let fixture = Fixture::new("ledger-merge");
    let info = |last_sync: i64| SourceSyncInfo {
        last_sync: Some(last_sync),
        last_result: SyncResult::Success,
        files_synced: 3,
        bytes_transferred: 42,
        duration_ms: 7,
        consecutive_failures: 0,
    };

    let mut first = SyncStatus::load(&fixture.data).expect("load empty ledger");
    first.set_info("alpha", info(1_000));
    first.save(&fixture.data).expect("write alpha");

    // Two writers hold the same snapshot. One of them records another source
    // and saves while the other is still holding its stale copy.
    let mut stale_snapshot = SyncStatus::load(&fixture.data).expect("load ledger");
    let mut other_writer = SyncStatus::load(&fixture.data).expect("load ledger");
    other_writer.set_info("beta", info(2_000));
    other_writer.save(&fixture.data).expect("write beta");

    // The stale snapshot writes afterwards. A whole-file replace would drop
    // "beta" right here.
    stale_snapshot.set_info("alpha", info(3_000));
    stale_snapshot
        .save(&fixture.data)
        .expect("write alpha again");

    let merged = SyncStatus::load(&fixture.data).expect("reload ledger");
    assert!(
        merged.get("alpha").is_some(),
        "the writer's own record must survive its own save"
    );
    assert!(
        merged.get("beta").is_some(),
        "a concurrent writer's record must survive a later save"
    );
    assert_eq!(
        merged.get("beta").and_then(|entry| entry.last_sync),
        Some(2_000),
        "the concurrent writer's record must be preserved verbatim"
    );

    // An explicit prune still removes the entry it names.
    let mut pruner = SyncStatus::load(&fixture.data).expect("load ledger");
    assert!(pruner.retain_sources(["alpha"]));
    pruner.save(&fixture.data).expect("write pruned ledger");
    let pruned = SyncStatus::load(&fixture.data).expect("reload pruned ledger");
    assert!(pruned.get("alpha").is_some());
    assert!(
        pruned.get("beta").is_none(),
        "a pruned source must not come back"
    );
}

/// A remote filesystem permission error is not an SSH authentication failure.
///
/// The SSH session succeeded and rsync could not read one remote path. The
/// source must stay retryable; classifying it as an authentication failure
/// parks it forever, so scheduled freshness silently dies for that source.
#[test]
fn remote_filesystem_permission_failure_is_not_an_authentication_failure() {
    use coding_agent_search::sources::config::{SourceDefinition, SyncSchedule};
    use coding_agent_search::sources::sync::{
        SourceHealthKind, SourceSyncInfo, SyncResult, SyncStatus, sync_failure_reason,
    };

    // The real rsync wording for "some files could not be transferred".
    let partial = "rsync partial transfer (exit 23): rsync: [sender] opendir \"/remote/locked\" \
                  failed: Permission denied (13)\nrsync error: some files/attrs were not \
                  transferred (see previous errors) (code 23) at main.c(1983)";
    assert_eq!(sync_failure_reason(partial), "partial_transfer");
    assert_eq!(
        sync_failure_reason(
            "Remote path permission denied: rsync: [sender] opendir \"/remote/locked\" failed: \
             Permission denied (13)"
        ),
        "remote_permission_denied"
    );
    // An SSH authentication failure keeps its historical reason.
    assert_eq!(
        sync_failure_reason("Permission denied (publickey)"),
        "authentication"
    );

    let now_ms = current_unix_ms();
    let mut source = SourceDefinition::ssh("fixture", "cass-fixture");
    source.paths = vec!["/remote/sessions".to_string()];
    source.sync_schedule = SyncSchedule::Hourly;

    let mut status = SyncStatus::default();
    status.set_info(
        "fixture",
        SourceSyncInfo {
            last_sync: Some(now_ms - 7_200_000),
            last_result: SyncResult::Failed(partial.to_string()),
            files_synced: 1,
            bytes_transferred: 8,
            duration_ms: 12,
            consecutive_failures: 1,
        },
    );

    let decision = status.decision_for_source_at(&source, now_ms, false);
    assert_ne!(
        decision.health,
        SourceHealthKind::AuthFailed,
        "a remote permission error must not be reported as an auth failure: {decision:?}"
    );
    assert_eq!(
        decision.action.as_str(),
        "sync",
        "a due source with a remote permission error must stay retryable: {decision:?}"
    );
}

// ---------------------------------------------------------------------------
// Scheduled freshness
// ---------------------------------------------------------------------------

/// A fresh remote source must not be synced again by the next scheduled run.
#[test]
fn scheduled_run_skips_a_fresh_remote_source_and_retries_a_due_one() {
    let fixture = Fixture::new("schedule-freshness");
    fixture.write_sources_toml(
        "[[sources]]\nname = \"fixture\"\ntype = \"ssh\"\nhost = \"cass-fixture\"\n\
         paths = [\"/remote/sessions\"]\nsync_schedule = \"hourly\"\n",
    );
    // The scheduled child must fail fast and locally: the fixture host is a
    // closed port on loopback, so no external host is contacted.
    let ssh_config = fixture.root.join("ssh_config");
    fs::write(
        &ssh_config,
        "Host cass-fixture\n  HostName 127.0.0.1\n  Port 1\n  User nobody\n  BatchMode yes\n",
    )
    .expect("write ssh config");

    // A fresh ledger entry: the source is not due, so the scheduler must not
    // start a sync for it (no duplicate work).
    let ledger = serde_json::json!({
        "sources": {
            "fixture": {
                "last_sync": current_unix_ms(),
                "last_result": "success",
                "files_synced": 3,
                "bytes_transferred": 128,
                "duration_ms": 42,
                "consecutive_failures": 0
            }
        }
    });
    fs::write(
        fixture.data.join("sync_status.json"),
        serde_json::to_vec_pretty(&ledger).expect("ledger serializes"),
    )
    .expect("write ledger");

    let fresh = run_scheduled_incremental(&fixture, &ssh_config);
    let steps = fresh["steps"].as_array().expect("steps array");
    let sync_step = steps
        .iter()
        .find(|step| {
            step["name"]
                .as_str()
                .unwrap_or_default()
                .starts_with("sources-sync")
        })
        .expect("a sources-sync step must be reported");
    assert_eq!(
        sync_step["skipped_reason"], "no remote source is due",
        "a fresh source must not be synced again: {sync_step}"
    );

    // The same source, one schedule period later, is due and must be synced.
    fixture.age_ledger_entry("fixture", 7_200_000);
    let stale = run_scheduled_incremental(&fixture, &ssh_config);
    let steps = stale["steps"].as_array().expect("steps array");
    let sync_step = steps
        .iter()
        .find(|step| step["name"] == "sources-sync:fixture")
        .expect("a due source must be synced by the scheduler");
    assert!(
        sync_step["skipped_reason"].is_null(),
        "a due source must not be skipped: {sync_step}"
    );
    assert!(
        sync_step["argv"]
            .as_array()
            .expect("argv array")
            .iter()
            .any(|arg| arg == "fixture"),
        "the scheduled sync must name the due source: {sync_step}"
    );
}

fn run_scheduled_incremental(fixture: &Fixture, ssh_config: &Path) -> Value {
    let output = fixture
        .cmd()
        .env("CASS_SSH_CONFIG", ssh_config)
        .args([
            "schedule",
            "run",
            "--job",
            "incremental",
            "--force",
            "--json",
        ])
        .output()
        .expect("run cass schedule run");
    parse_json(&output.stdout, "schedule run")
}

/// The rendered scheduler units must be real units for the native scheduler.
///
/// The install is only exercised with `--dry-run`: registering a real
/// launchd/systemd job from a test suite would leak state onto the machine
/// that runs it.
#[test]
fn schedule_install_dry_run_renders_units_without_executing_anything() {
    let fixture = Fixture::new("schedule-units");
    let output = fixture
        .cmd()
        .args(["schedule", "install", "--dry-run", "--json"])
        .output()
        .expect("run cass schedule install --dry-run");
    assert!(
        output.status.success(),
        "dry-run install must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = parse_json(&output.stdout, "schedule install --dry-run");
    assert_eq!(report["dry_run"], Value::Bool(true));
    let units = report["units"].as_array().expect("units array");
    assert!(!units.is_empty(), "the installer must render units");
    for unit in units {
        let path = unit["path"].as_str().expect("unit path");
        assert!(
            !Path::new(path).exists(),
            "dry-run must not write unit files: {path}"
        );
        let contents = unit["contents"].as_str().expect("unit contents");
        if path.ends_with(".timer") {
            let job = if path.contains("nightly") {
                "nightly"
            } else {
                "incremental"
            };
            assert!(
                contents.contains(&format!("Unit=cass-{job}.service")),
                "the timer must activate the {job} service: {contents}"
            );
            assert!(
                contents.contains("WantedBy=timers.target"),
                "the timer must be part of timers.target: {contents}"
            );
        } else {
            assert!(
                contents.contains("schedule") && contents.contains("--job"),
                "the unit must run the scheduler: {contents}"
            );
            assert!(
                contents.contains(fixture.data.to_str().unwrap_or_default()),
                "the unit must run against this data dir: {contents}"
            );
        }
    }
    for command in report["commands"].as_array().expect("commands array") {
        assert_eq!(
            command["executed"],
            Value::Bool(false),
            "dry-run must not execute scheduler commands: {command}"
        );
    }
}

/// Native Linux acceptance for the rendered systemd units.
///
/// `systemd-analyze verify` is systemd's own parser: it accepts the unit
/// files exactly as a user manager would load them. No timer is installed
/// and no user session is touched.
#[cfg(target_os = "linux")]
#[test]
fn rendered_systemd_units_are_accepted_by_systemd_analyze() {
    let fixture = Fixture::new("systemd-units");
    // The units log into <data>/schedule; make sure the directory exists so
    // the analyzer does not fail on the log path.
    fs::create_dir_all(fixture.data.join("schedule")).expect("create schedule dir");

    let output = fixture
        .cmd()
        .args(["schedule", "install", "--dry-run", "--json"])
        .output()
        .expect("run cass schedule install --dry-run");
    assert!(
        output.status.success(),
        "dry-run install must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = parse_json(&output.stdout, "schedule install --dry-run");
    let units = report["units"].as_array().expect("units array");

    let unit_dir = fixture.root.join("units");
    fs::create_dir_all(&unit_dir).expect("create unit dir");
    let mut services: Vec<PathBuf> = Vec::new();
    for unit in units {
        let path = unit["path"].as_str().expect("unit path");
        let name = Path::new(path)
            .file_name()
            .expect("unit file name")
            .to_string_lossy()
            .to_string();
        if !name.ends_with(".service") {
            continue;
        }
        let target = unit_dir.join(&name);
        fs::write(&target, unit["contents"].as_str().expect("unit contents"))
            .expect("write unit file");
        services.push(target);
    }
    assert!(
        !services.is_empty(),
        "the systemd service must be rendered on linux"
    );

    for service in &services {
        let base = service
            .file_stem()
            .expect("service stem")
            .to_string_lossy()
            .to_string();
        // The timer references its service, so both are verified together.
        let timer = unit_dir.join(format!("{base}.timer"));
        let mut argv: Vec<String> = vec![service.to_string_lossy().to_string()];
        if timer.exists() {
            argv.push(timer.to_string_lossy().to_string());
        }
        let verify = Command::new("systemd-analyze")
            .arg("verify")
            .args(&argv)
            .output()
            .expect("run systemd-analyze verify");
        assert!(
            verify.status.success(),
            "systemd-analyze must accept {base}: stdout={} stderr={}",
            String::from_utf8_lossy(&verify.stdout),
            String::from_utf8_lossy(&verify.stderr)
        );
    }
}

/// Split one `ExecStart=` value the way systemd's own parser does for the
/// quoted argv produced by `systemd_quote` (spaces and double quotes only).
fn split_systemd_exec(value: &str) -> Vec<String> {
    let mut argv = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => quoted = !quoted,
            '\\' if quoted => {
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            ' ' | '\t' if !quoted => {
                if !current.is_empty() {
                    argv.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        argv.push(current);
    }
    argv
}

/// The rendered systemd services must run the real incremental and nightly
/// jobs, at background priority, and record both outcomes.
///
/// The corpus is empty: this test proves the rendered unit body, not the
/// connector indexer. The ExecStart argv is executed exactly as systemd would
/// execute it. No timer is registered, so a developer's user manager is
/// untouched.
#[cfg(target_os = "linux")]
#[test]
fn rendered_systemd_services_execute_and_record_both_jobs() {
    let fixture = Fixture::new("systemd-exec");

    let output = fixture
        .cmd()
        .args([
            "schedule",
            "install",
            "--dry-run",
            "--no-semantic",
            "--json",
        ])
        .output()
        .expect("run cass schedule install --dry-run");
    assert!(
        output.status.success(),
        "dry-run install must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = parse_json(&output.stdout, "schedule install --dry-run");
    let units = report["units"].as_array().expect("units array");

    for job in ["incremental", "nightly"] {
        let name = format!("cass-{job}.service");
        let unit = units
            .iter()
            .find(|unit| {
                Path::new(unit["path"].as_str().expect("unit path"))
                    .file_name()
                    .is_some_and(|file| file == name.as_str())
            })
            .unwrap_or_else(|| panic!("the {job} service must be rendered: {report}"));
        let contents = unit["contents"].as_str().expect("unit contents");
        for expected in [
            "Nice=19",
            "IOSchedulingClass=idle",
            "CPUSchedulingPolicy=idle",
            "Environment=CASS_INDEX_NO_PROGRESS_EVENTS=1",
        ] {
            assert!(
                contents.contains(expected),
                "the {job} service must run at background priority ({expected}): {contents}"
            );
        }
        let log = format!(
            "StandardOutput=append:{}",
            fixture.data.join(format!("schedule/{job}.log")).display()
        );
        assert!(
            contents.contains(&log),
            "the {job} service must record into its own log: {contents}"
        );

        let exec_line = contents
            .lines()
            .find_map(|line| line.strip_prefix("ExecStart="))
            .unwrap_or_else(|| panic!("the {job} service must have ExecStart: {contents}"));
        let argv = split_systemd_exec(exec_line);
        assert!(Path::new(&argv[0]).is_absolute(), "{argv:?}");
        let expected = [
            "--color=never",
            "schedule",
            "run",
            "--job",
            job,
            "--data-dir",
            fixture.data.to_str().expect("fixture data path"),
            "--json",
            "--no-semantic",
        ];
        assert_eq!(argv.len(), expected.len() + 1, "{argv:?}");
        for (index, argument) in expected.iter().enumerate() {
            assert_eq!(
                &argv[index + 1],
                argument,
                "unexpected ExecStart argv: {argv:?}"
            );
        }

        let mut command = Command::new(&argv[0]);
        isolate_env(&mut command, &fixture.home);
        command.env("CASS_DATA_DIR", &fixture.data).args(&argv[1..]);
        let run = command
            .output()
            .expect("execute the rendered ExecStart argv");
        assert!(
            run.status.success(),
            "the {job} systemd ExecStart must succeed: stdout={} stderr={}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        let job_report = parse_json(&run.stdout, "scheduled ExecStart report");
        assert_eq!(job_report["job"], job, "{job_report}");
        assert_eq!(job_report["ok"], Value::Bool(true), "{job_report}");

        // The child index writer is a background-priority run, and a full
        // index is exactly the nightly job's work.
        let index_name = if job == "nightly" {
            "index-full"
        } else {
            "index"
        };
        let index = job_report["steps"]
            .as_array()
            .expect("steps array")
            .iter()
            .find(|step| step["name"] == index_name)
            .unwrap_or_else(|| panic!("the {job} run must record its index step"));
        let index_argv: Vec<&str> = index["argv"]
            .as_array()
            .expect("index argv")
            .iter()
            .map(|arg| arg.as_str().expect("index argument"))
            .collect();
        assert!(
            index_argv.contains(&"--background"),
            "the {job} index child must use the background policy: {index}"
        );
        assert_eq!(
            index_argv.contains(&"--full"),
            job == "nightly",
            "the {job} index child has the wrong rebuild mode: {index}"
        );
        assert_eq!(index["result"]["conversations"], 0, "{index}");
        assert_eq!(index["result"]["messages"], 0, "{index}");

        let log = fs::read_to_string(fixture.data.join(format!("schedule/{job}.log")))
            .expect("the scheduled run must write its job log");
        assert!(
            log.contains(&format!("cass schedule run --job {job}")),
            "the job log must name the run: {log}"
        );
    }

    // Both executions are durably recorded, once each.
    let state: Value = serde_json::from_str(
        &fs::read_to_string(fixture.data.join("schedule/state.json")).expect("read schedule state"),
    )
    .expect("schedule state parses");
    assert_eq!(
        state["last_incremental"]["ok"],
        Value::Bool(true),
        "{state}"
    );
    assert_eq!(state["last_nightly"]["ok"], Value::Bool(true), "{state}");
    let history = fs::read_to_string(fixture.data.join("schedule/runs.jsonl"))
        .expect("read schedule history");
    let jobs: Vec<String> = history
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).expect("history line parses")["job"]
                .as_str()
                .expect("history job")
                .to_string()
        })
        .collect();
    assert_eq!(jobs, ["incremental", "nightly"], "history is append-only");
}

/// Native systemd install/status/uninstall against a disposable user scope.
///
/// `systemctl` is a shim because the live user manager owns a real
/// `cass-incremental.timer`, and registering this fixture would start a
/// second real job. The unit directory, the unit files, the recorded argv and
/// the status decisions are the production code paths; `systemd-analyze
/// verify` above independently proves the rendered units parse natively.
#[cfg(target_os = "linux")]
#[test]
fn systemd_install_status_uninstall_in_a_disposable_user_scope() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new("systemd-install");
    let bin = fixture.root.join("bin");
    fs::create_dir_all(&bin).expect("create shim dir");
    let systemctl = bin.join("systemctl");
    fs::write(
        &systemctl,
        "#!/bin/sh\n\
         printf '%s\\n' \"$*\" >> \"$SYSTEMCTL_LOG\"\n\
         if [ \"$1\" = \"--user\" ] && [ \"$2\" = \"is-active\" ]; then\n\
           [ -f \"$SYSTEMCTL_ACTIVE\" ] && exit 0\n\
           exit 3\n\
         fi\n\
         exit 0\n",
    )
    .expect("write systemctl shim");
    fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755))
        .expect("make systemctl shim executable");
    let log = fixture.root.join("systemctl.log");
    let active = fixture.root.join("systemctl.active");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let run = |args: &[&str]| {
        let mut command = fixture.cmd();
        command
            .env("PATH", &path)
            .env("SYSTEMCTL_LOG", &log)
            .env("SYSTEMCTL_ACTIVE", &active)
            .args(args);
        command.output().expect("run cass")
    };

    let installed = run(&["schedule", "install", "--no-semantic", "--json"]);
    assert!(
        installed.status.success(),
        "install must succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&installed.stdout),
        String::from_utf8_lossy(&installed.stderr)
    );
    let report = parse_json(&installed.stdout, "schedule install");
    assert_eq!(report["platform"], "systemd");
    assert_eq!(report["dry_run"], Value::Bool(false));
    let unit_dir = fixture.home.join(".config/systemd/user");
    for job in ["incremental", "nightly"] {
        for extension in ["service", "timer"] {
            let unit = unit_dir.join(format!("cass-{job}.{extension}"));
            assert!(unit.is_file(), "install must publish {}", unit.display());
        }
    }
    let install_commands: Vec<String> = report["commands"]
        .as_array()
        .expect("commands array")
        .iter()
        .map(|command| {
            assert_eq!(command["executed"], Value::Bool(true), "{command}");
            assert_eq!(command["exit_code"], 0, "{command}");
            command["argv"]
                .as_array()
                .expect("command argv")
                .iter()
                .map(|arg| arg.as_str().expect("command argument"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    assert!(
        install_commands
            .iter()
            .any(|command| command == "systemctl --user daemon-reload"),
        "{install_commands:?}"
    );
    assert!(
        install_commands.iter().any(|command| command
            == "systemctl --user enable --now cass-incremental.timer cass-nightly.timer"),
        "{install_commands:?}"
    );

    // Before the timer is active, status must not claim it is loaded.
    let before = run(&["schedule", "status", "--json"]);
    assert!(
        before.status.success(),
        "status must succeed: {}",
        String::from_utf8_lossy(&before.stderr)
    );
    let before = parse_json(&before.stdout, "schedule status");
    assert_eq!(before["platform"], "systemd");
    for unit in before["units"].as_array().expect("units array") {
        assert_eq!(unit["installed"], Value::Bool(true), "{unit}");
        assert_eq!(unit["loaded"], Value::Bool(false), "{unit}");
        assert!(
            unit["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("exit 3"),
            "an inactive timer must report the scheduler's answer: {unit}"
        );
    }

    fs::write(&active, b"active").expect("mark the timers active");
    let active_status = run(&["schedule", "status", "--json"]);
    let active_status = parse_json(&active_status.stdout, "active schedule status");
    for unit in active_status["units"].as_array().expect("units array") {
        assert_eq!(unit["loaded"], Value::Bool(true), "{unit}");
    }

    let removed = run(&["schedule", "uninstall", "--json"]);
    assert!(
        removed.status.success(),
        "uninstall must succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&removed.stdout),
        String::from_utf8_lossy(&removed.stderr)
    );
    let removed = parse_json(&removed.stdout, "schedule uninstall");
    assert_eq!(
        removed["removed"].as_array().expect("removed array").len(),
        4,
        "uninstall must remove both jobs' service and timer: {removed}"
    );
    assert!(
        removed["missing"]
            .as_array()
            .expect("missing array")
            .is_empty(),
        "{removed}"
    );
    for job in ["incremental", "nightly"] {
        for extension in ["service", "timer"] {
            assert!(
                !unit_dir.join(format!("cass-{job}.{extension}")).exists(),
                "uninstall must remove cass-{job}.{extension}"
            );
        }
    }
    let recorded = fs::read_to_string(&log).expect("systemctl shim log");
    assert!(
        recorded.contains("--user disable --now cass-incremental.timer cass-nightly.timer"),
        "{recorded}"
    );
    assert!(
        recorded.contains("--user is-active cass-incremental.timer"),
        "{recorded}"
    );
    let after = run(&["schedule", "status", "--json"]);
    let after = parse_json(&after.stdout, "status after uninstall");
    for unit in after["units"].as_array().expect("units array") {
        assert_eq!(unit["installed"], Value::Bool(false), "{unit}");
    }
}

// ---------------------------------------------------------------------------
// Truthful source state
// ---------------------------------------------------------------------------

/// An unreachable host must be reported truthfully, and the attempt must be
/// recorded so the next tick can back off and retry.
#[test]
fn unreachable_remote_host_is_reported_truthfully() {
    let fixture = Fixture::new("unreachable-host");
    fixture.write_sources_toml(
        "[[sources]]\nname = \"fixture\"\ntype = \"ssh\"\nhost = \"cass-fixture\"\n\
         paths = [\"/remote/sessions\"]\nsync_schedule = \"hourly\"\n",
    );
    let ssh_config = fixture.root.join("ssh_config");
    fs::write(
        &ssh_config,
        "Host cass-fixture\n  HostName 127.0.0.1\n  Port 1\n  User nobody\n  BatchMode yes\n",
    )
    .expect("write ssh config");

    let output = fixture
        .cmd()
        .env("CASS_SSH_CONFIG", &ssh_config)
        .args([
            "sources",
            "sync",
            "--source",
            "fixture",
            "--no-index",
            "--json",
        ])
        .output()
        .expect("run cass sources sync");
    // The JSON document on stdout is the data surface; the process exit code
    // reports the failure to a shell.
    let report = parse_json(&output.stdout, "sources sync");
    assert_eq!(report["status"], "failed");
    let source = &report["sources"][0];
    let path = &source["paths"][0];
    assert_eq!(path["success"], Value::Bool(false));
    assert_eq!(
        path["failure_reason"], "connection_refused",
        "an unreachable host must be named truthfully: {path}"
    );

    // The durable ledger records the attempt: the next tick can back off
    // instead of forgetting that the source failed.
    let ledger = fixture.ledger();
    assert_eq!(
        ledger["sources"]["fixture"]["consecutive_failures"], 1,
        "the failed attempt must be recorded: {ledger}"
    );
    // A failed result is an externally tagged enum: the ledger records
    // {"failed": "<message>"}, not the string "failed".
    assert!(
        ledger["sources"]["fixture"]["last_result"]
            .get("failed")
            .is_some(),
        "the failure must be recorded: {ledger}"
    );
}

/// Two configured sources with the same identity must be refused, not synced
/// twice under one name.
#[test]
fn duplicate_source_identity_is_refused_instead_of_synced_twice() {
    let fixture = Fixture::new("duplicate-source");
    fixture.write_sources_toml(
        "[[sources]]\nname = \"fixture\"\ntype = \"ssh\"\nhost = \"cass-fixture\"\n\
         paths = [\"/remote/sessions\"]\n\
         \n[[sources]]\nname = \"fixture\"\ntype = \"ssh\"\nhost = \"cass-fixture\"\n\
         paths = [\"/remote/other\"]\n",
    );
    let output = fixture
        .cmd()
        .args(["sources", "sync", "--json"])
        .output()
        .expect("run cass sources sync");
    assert!(
        !output.status.success(),
        "a duplicate source identity must be refused: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        stderr.contains("duplicate"),
        "the refusal must name the duplicate identity: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Real SSH transport (loopback sshd)
// ---------------------------------------------------------------------------

/// A disposable loopback sshd with a disposable host key and client key.
///
/// This is a real, authorized SSH endpoint: the transport, the host key
/// check and the rsync-over-ssh path are the production ones. It is bound to
/// 127.0.0.1 and serves nothing but the fixture tree.
struct LocalSshServer {
    root: PathBuf,
    port: u16,
    alias: String,
    child: Option<Child>,
}

impl LocalSshServer {
    fn start(root: &Path) -> Self {
        let sshd = ["/usr/sbin/sshd", "/usr/local/sbin/sshd", "/usr/bin/sshd"]
            .iter()
            .copied()
            .find(|candidate| Path::new(candidate).exists())
            .unwrap_or_else(|| panic!("a local sshd is required for the real SSH transport test"));
        let user = current_user();
        let host_key = root.join("hostkey");
        let user_key = root.join("userkey");
        run_keygen(&host_key);
        run_keygen(&user_key);
        let authorized = root.join("authorized_keys");
        fs::copy(root.join("userkey.pub"), &authorized).expect("copy authorized key");
        set_mode(&authorized, 0o600);
        set_mode(&host_key, 0o600);
        set_mode(&user_key, 0o600);

        let port = free_port();
        let config = root.join("sshd_config");
        let host_key_display = host_key.display().to_string();
        let authorized_display = authorized.display().to_string();
        let pid_display = root.join("sshd.pid").display().to_string();
        fs::write(
            &config,
            format!(
                "Port {port}\n\
                 ListenAddress 127.0.0.1\n\
                 HostKey {host_key_display}\n\
                 PidFile {pid_display}\n\
                 UsePAM no\n\
                 PasswordAuthentication no\n\
                 KbdInteractiveAuthentication no\n\
                 PubkeyAuthentication yes\n\
                 PermitRootLogin prohibit-password\n\
                 StrictModes no\n\
                 AuthorizedKeysFile {authorized_display}\n\
                 AllowUsers {user}\n\
                 LogLevel ERROR\n\
                 PrintMotd no\n"
            ),
        )
        .expect("write sshd config");

        let log = fs::File::create(root.join("sshd.log")).expect("create sshd log");
        let child = Command::new(sshd)
            .args(["-D", "-e", "-f", config.to_str().expect("sshd config path")])
            .stdout(Stdio::from(log.try_clone().expect("clone sshd log")))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("spawn sshd");
        let server = Self {
            root: root.to_path_buf(),
            port,
            alias: "cass-fixture".to_string(),
            child: Some(child),
        };
        server.wait_until_listening();
        server.write_known_hosts();
        server
    }

    fn wait_until_listening(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let log = fs::read_to_string(self.root.join("sshd.log")).unwrap_or_default();
        panic!(
            "the local sshd did not start listening on 127.0.0.1:{}\n{log}",
            self.port
        );
    }

    fn write_known_hosts(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let output = Command::new("ssh-keyscan")
                .arg("-p")
                .arg(self.port.to_string())
                .arg("127.0.0.1")
                .output()
                .expect("run ssh-keyscan");
            if output.status.success() && String::from_utf8_lossy(&output.stdout).contains("ssh-") {
                fs::write(self.root.join("known_hosts"), output.stdout).expect("write known_hosts");
                return;
            }
            if Instant::now() >= deadline {
                panic!("ssh-keyscan did not reach the local sshd");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// The client-side ssh config for this fixture endpoint.
    fn ssh_config(&self) -> PathBuf {
        let path = self.root.join("ssh_config");
        let alias = self.alias.clone();
        let port = self.port;
        let user = current_user();
        let key_display = self.root.join("userkey").display().to_string();
        let known_display = self.root.join("known_hosts").display().to_string();
        fs::write(
            &path,
            format!(
                "Host {alias}\n\
                 \x20 HostName 127.0.0.1\n\
                 \x20 Port {port}\n\
                 \x20 User {user}\n\
                 \x20 IdentityFile {key_display}\n\
                 \x20 UserKnownHostsFile {known_display}\n\
                 \x20 StrictHostKeyChecking yes\n\
                 \x20 BatchMode yes\n\
                 \x20 ConnectTimeout 10\n\
                 \x20 ServerAliveInterval 15\n\
                 \x20 ServerAliveCountMax 3\n"
            ),
        )
        .expect("write ssh config");
        path
    }
}

impl Drop for LocalSshServer {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn current_user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var("LOGNAME")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "root".to_string())
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local address").port()
}

fn run_keygen(path: &Path) {
    let output = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-f"])
        .arg(path)
        .args(["-N", "", "-q"])
        .output()
        .expect("run ssh-keygen");
    assert!(
        output.status.success(),
        "ssh-keygen must succeed for {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The full chain over a real SSH transport: sync -> index -> scoped
/// retrieval, with the remote origin, the timestamps and the dedup intact.
#[test]
#[ignore = "requires a local sshd on 127.0.0.1 with a disposable key (real SSH transport)"]
fn local_ssh_sync_indexes_the_mirror_and_keeps_the_remote_origin() {
    use coding_agent_search::sources::sync::path_to_safe_dirname;

    let fixture = Fixture::new("ssh-sync");
    let server = LocalSshServer::start(&fixture.root);
    let ssh_config = server.ssh_config();

    let remote_sessions = fixture.root.join("remote").join("sessions");
    fs::create_dir_all(&remote_sessions).expect("create remote tree");
    let remote_session = remote_sessions.join("rollout-a.jsonl");
    fs::write(&remote_session, session_jsonl(SESSION_MARKER)).expect("write remote session");

    fixture.write_sources_toml(&format!(
        "[[sources]]\nname = \"fixture\"\ntype = \"ssh\"\nhost = \"{alias}\"\n\
         paths = [\"{path}\"]\nsync_schedule = \"manual\"\n\
         \n[[sources.path_mappings]]\nfrom = \"/synthetic/project\"\nto = \"/local/project\"\n",
        alias = server.alias,
        path = remote_sessions.display(),
    ));

    let synced = fixture
        .cmd()
        .env("CASS_SSH_CONFIG", &ssh_config)
        .args(["sources", "sync", "--source", "fixture", "--json"])
        .output()
        .expect("run cass sources sync");
    let report = parse_json(&synced.stdout, "sources sync");
    assert_eq!(
        report["status"], "complete",
        "the sync must complete: {report}"
    );
    assert_eq!(report["sources"][0]["status"], "success");
    assert_eq!(
        report["sources"][0]["paths"][0]["success"],
        Value::Bool(true)
    );
    assert!(
        report["total_files"].as_u64().unwrap_or_default() >= 1,
        "the transferred files must be counted: {report}"
    );
    assert!(
        report["total_bytes"].as_u64().unwrap_or_default() >= 1,
        "the transferred bytes must be counted: {report}"
    );
    assert_eq!(
        report["will_reindex"],
        Value::Bool(true),
        "landed files must trigger the index step: {report}"
    );

    // The mirror keeps the configured path mapping (the mirror directory is
    // keyed by the configured remote path, not by a host-chosen name).
    let mirror = fixture
        .data
        .join("remotes")
        .join("fixture")
        .join("mirror")
        .join(path_to_safe_dirname(&remote_sessions.to_string_lossy()));
    // The configured directory has no trailing slash, so rsync keeps its
    // basename inside the per-source path container.
    let mirrored_session = mirror.join("sessions").join("rollout-a.jsonl");
    assert!(
        mirrored_session.is_file(),
        "the session must be mirrored at {}",
        mirror.display()
    );

    // The remote timestamp is preserved by the transfer.
    let remote_modified = fs::metadata(&remote_session)
        .expect("remote metadata")
        .modified()
        .expect("remote mtime");
    let mirror_modified = fs::metadata(&mirrored_session)
        .expect("mirror metadata")
        .modified()
        .expect("mirror mtime");
    let delta = remote_modified
        .duration_since(mirror_modified)
        .or_else(|_| mirror_modified.duration_since(remote_modified))
        .expect("mtime delta");
    assert!(
        delta.as_secs() < 2,
        "the remote timestamp must be preserved (delta {delta:?})"
    );

    // The durable ledger records the transfer.
    let ledger = fixture.ledger();
    assert!(
        ledger["sources"]["fixture"]["files_synced"]
            .as_u64()
            .unwrap_or_default()
            >= 1,
        "the ledger must count the transferred files: {ledger}"
    );
    assert_eq!(ledger["sources"]["fixture"]["last_result"], "success");

    // Scoped retrieval: the conversation is reachable through the remote
    // source and only through it.
    let found = fixture
        .cmd()
        .env("CASS_SSH_CONFIG", &ssh_config)
        .args([
            "search",
            SESSION_MARKER,
            "--robot",
            "--mode",
            "lexical",
            "--source",
            "fixture",
            "--no-maintenance",
            "--no-daemon",
        ])
        .output()
        .expect("run cass search");
    assert!(
        found.status.success(),
        "search must succeed: {}",
        String::from_utf8_lossy(&found.stderr)
    );
    let searched = parse_json(&found.stdout, "search --source fixture");
    let hits = searched["hits"].as_array().expect("hits array");
    assert_eq!(
        hits.len(),
        1,
        "the session must be indexed once: {searched}"
    );
    assert_eq!(hits[0]["source_id"], "fixture");

    let local = fixture
        .cmd()
        .env("CASS_SSH_CONFIG", &ssh_config)
        .args([
            "search",
            SESSION_MARKER,
            "--robot",
            "--mode",
            "lexical",
            "--source",
            "local",
            "--no-maintenance",
            "--no-daemon",
        ])
        .output()
        .expect("run cass search --source local");
    let local_searched = parse_json(&local.stdout, "search --source local");
    assert!(
        local_searched["hits"]
            .as_array()
            .expect("hits array")
            .is_empty(),
        "the remote mirror must not leak into the local source scope: {local_searched}"
    );

    // A second sync must not repeat the transfer: rsync is incremental, so
    // nothing new lands and no duplicate conversation appears.
    let again = fixture
        .cmd()
        .env("CASS_SSH_CONFIG", &ssh_config)
        .args([
            "sources",
            "sync",
            "--source",
            "fixture",
            "--no-index",
            "--json",
        ])
        .output()
        .expect("run the second cass sources sync");
    let again_report = parse_json(&again.stdout, "second sources sync");
    assert_eq!(
        again_report["total_files"].as_u64().unwrap_or_default(),
        0,
        "a second sync must not transfer the same files again: {again_report}"
    );

    let reingest = fixture
        .cmd()
        .args(["sources", "reingest", "--from-mirror", "--json"])
        .output()
        .expect("run cass sources reingest");
    let reingested = parse_json(&reingest.stdout, "sources reingest");
    assert_eq!(
        reingested["status"], "complete",
        "the mirror must re-ingest cleanly: {reingested}"
    );

    let found = fixture
        .cmd()
        .env("CASS_SSH_CONFIG", &ssh_config)
        .args([
            "search",
            SESSION_MARKER,
            "--robot",
            "--mode",
            "lexical",
            "--source",
            "fixture",
            "--no-maintenance",
            "--no-daemon",
        ])
        .output()
        .expect("run cass search after reingest");
    let searched = parse_json(&found.stdout, "search after reingest");
    assert_eq!(
        searched["hits"].as_array().expect("hits array").len(),
        1,
        "a re-index must not duplicate the conversation: {searched}"
    );
}

/// An interrupted transfer must be reported truthfully and must not park the
/// source: the scheduler retries it and a later sync completes the work
/// without duplicating anything.
#[test]
#[ignore = "requires a local sshd on 127.0.0.1 with a disposable key (real SSH transport)"]
fn interrupted_transfer_is_reported_truthfully_and_resume_completes() {
    use coding_agent_search::sources::config::{SourceDefinition, SyncSchedule};
    use coding_agent_search::sources::sync::{SourceHealthKind, SyncStatus};

    let fixture = Fixture::new("partial-transfer");
    let server = LocalSshServer::start(&fixture.root);
    let ssh_config = server.ssh_config();

    let remote_sessions = fixture.root.join("remote").join("sessions");
    let locked = remote_sessions.join("locked");
    fs::create_dir_all(&locked).expect("create remote tree");
    fs::write(
        remote_sessions.join("rollout-a.jsonl"),
        session_jsonl(SESSION_MARKER),
    )
    .expect("write readable session");
    fs::write(
        locked.join("rollout-b.jsonl"),
        session_jsonl("casslockedmarker"),
    )
    .expect("write locked session");
    // The remote account cannot read this directory: rsync moves what it can
    // and exits 23 (partial transfer).
    set_mode(&locked, 0o000);

    fixture.write_sources_toml(&format!(
        "[[sources]]\nname = \"fixture\"\ntype = \"ssh\"\nhost = \"{alias}\"\n\
         paths = [\"{path}\"]\nsync_schedule = \"hourly\"\n",
        alias = server.alias,
        path = remote_sessions.display(),
    ));

    let interrupted = fixture
        .cmd()
        .env("CASS_SSH_CONFIG", &ssh_config)
        .args([
            "sources",
            "sync",
            "--source",
            "fixture",
            "--no-index",
            "--json",
        ])
        .output()
        .expect("run the interrupted cass sources sync");
    let report = parse_json(&interrupted.stdout, "interrupted sources sync");
    let path = &report["sources"][0]["paths"][0];
    assert_eq!(
        path["success"],
        Value::Bool(false),
        "an interrupted transfer must not report success: {path}"
    );
    assert_eq!(
        path["failure_reason"], "partial_transfer",
        "an interrupted transfer must be named truthfully: {path}"
    );
    assert!(
        path["files"].as_u64().unwrap_or_default() >= 1,
        "the files that did land must be counted: {path}"
    );
    assert!(
        report["total_files"].as_u64().unwrap_or_default() >= 1,
        "the transferred files must reach the report: {report}"
    );

    // The durable ledger keeps the counts and the failure.
    let ledger = fixture.ledger();
    assert!(
        ledger["sources"]["fixture"]["files_synced"]
            .as_u64()
            .unwrap_or_default()
            >= 1,
        "the ledger must count what landed: {ledger}"
    );
    assert_eq!(
        ledger["sources"]["fixture"]["consecutive_failures"], 1,
        "the failure must be recorded: {ledger}"
    );

    // The source must not be parked as an authentication failure.
    let status = SyncStatus::load(&fixture.data).expect("load the durable ledger");
    let mut source = SourceDefinition::ssh("fixture", &server.alias);
    source.paths = vec![remote_sessions.to_string_lossy().to_string()];
    source.sync_schedule = SyncSchedule::Hourly;
    let decision = status.decision_for_source_at(&source, current_unix_ms(), false);
    assert_ne!(
        decision.health,
        SourceHealthKind::AuthFailed,
        "a remote permission error must not be an auth failure: {decision:?}"
    );

    // One schedule period later the source is due and must be retried, not
    // deferred forever.
    fixture.age_ledger_entry("fixture", 7_200_000);
    let scheduled = run_scheduled_incremental(&fixture, &ssh_config);
    let steps = scheduled["steps"].as_array().expect("steps array");
    let sync_step = steps
        .iter()
        .find(|step| step["name"] == "sources-sync:fixture")
        .expect("a due source must be retried by the scheduler");
    assert!(
        sync_step["skipped_reason"].is_null(),
        "a due source must not be skipped: {sync_step}"
    );

    // The remote path becomes readable again: the resumed sync completes and
    // the failure history is cleared.
    set_mode(&locked, 0o755);
    let resumed = fixture
        .cmd()
        .env("CASS_SSH_CONFIG", &ssh_config)
        .args([
            "sources",
            "sync",
            "--source",
            "fixture",
            "--no-index",
            "--json",
        ])
        .output()
        .expect("run the resumed cass sources sync");
    let resumed_report = parse_json(&resumed.stdout, "resumed sources sync");
    assert_eq!(
        resumed_report["status"], "complete",
        "the resumed sync must complete: {resumed_report}"
    );
    assert!(
        resumed_report["total_files"].as_u64().unwrap_or_default() >= 1,
        "the resumed sync must transfer the remaining files: {resumed_report}"
    );
    let ledger = fixture.ledger();
    assert_eq!(
        ledger["sources"]["fixture"]["consecutive_failures"], 0,
        "a successful sync must clear the failure history: {ledger}"
    );

    // The already-transferred session is indexed once, not twice.
    let reingest = fixture
        .cmd()
        .args(["sources", "reingest", "--from-mirror", "--json"])
        .output()
        .expect("run cass sources reingest");
    let reingested = parse_json(&reingest.stdout, "sources reingest");
    assert_eq!(
        reingested["status"], "complete",
        "the mirror must re-ingest cleanly: {reingested}"
    );
    let found = fixture
        .cmd()
        .env("CASS_SSH_CONFIG", &ssh_config)
        .args([
            "search",
            SESSION_MARKER,
            "--robot",
            "--mode",
            "lexical",
            "--source",
            "fixture",
            "--no-maintenance",
            "--no-daemon",
        ])
        .output()
        .expect("run cass search");
    let searched = parse_json(&found.stdout, "search");
    assert_eq!(
        searched["hits"].as_array().expect("hits array").len(),
        1,
        "the resumed sync must not duplicate the conversation: {searched}"
    );
}
