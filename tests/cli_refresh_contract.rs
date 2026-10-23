use clap::Parser;
use coding_agent_search::{
    Cli, Commands, RobotFormat,
    doctor::{DoctorBackupCommand, DoctorCommandRequest, DoctorCommandSurface},
    raw_mirror::{
        RawMirrorCaptureInput, RawMirrorDbLink, RawMirrorPruneOptions, capture_source_file,
        merge_manifest_db_links, prune, storage_summary,
    },
};

fn parse(args: &[&str]) -> Result<Cli, String> {
    Cli::try_parse_from(args).map_err(|err| format!("parse cass CLI for {args:?}: {err}"))
}

#[test]
fn gh461_mirror_prune_cli_composes_provider_and_path_selectors() {
    let temp = tempfile::tempdir().expect("fixture");
    let data_dir = temp.path().join("data");
    let capture = |provider, name, bytes: &[u8]| {
        let source_path = temp.path().join(name);
        std::fs::write(&source_path, bytes).expect("source");
        capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider,
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("capture")
    };
    let selected = capture("opencode", "target.db", b"selected");
    let other_path = capture("opencode", "other.db", b"other path");
    let other_provider = capture("codex", "codex.jsonl", b"other provider");
    let run = |extra: &[&str]| {
        let mut command = assert_cmd::Command::new(env!("CARGO_BIN_EXE_cass"));
        command
            .args(["mirror", "prune", "--data-dir"])
            .arg(&data_dir)
            .args(["--older-than", "0s", "--safety-hold-down", "0s", "--json"])
            .args(extra)
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .timeout(std::time::Duration::from_secs(30));
        command.assert()
    };
    let result = run(&["--provider", "opencode", "--source-path", "*target.db"]).success();
    let report: serde_json::Value =
        serde_json::from_slice(&result.get_output().stdout).expect("prune JSON");
    assert_eq!(
        report["prune"]["providers"],
        serde_json::json!(["opencode"])
    );
    assert_eq!(report["prune"]["source_path"], "*target.db");
    assert_eq!(
        report["prune"]["scope_blob_bytes"],
        selected.source_size_bytes
    );
    let entries = report["prune"]["entries"].as_array().expect("entries");
    assert!(
        entries
            .iter()
            .any(|entry| entry["path"] == selected.manifest_relative_path)
    );
    assert!(
        !entries
            .iter()
            .any(|entry| entry["path"] == other_path.manifest_relative_path)
    );
    assert!(
        !entries
            .iter()
            .any(|entry| entry["path"] == other_provider.manifest_relative_path)
    );

    let result = run(&["--provider", "opencode", "--provider", "codex"]).success();
    let report: serde_json::Value =
        serde_json::from_slice(&result.get_output().stdout).expect("union JSON");
    assert_eq!(report["prune"]["planned_manifest_count"], 3);
    let result = run(&["--provider", "absent"]).success();
    let report: serde_json::Value =
        serde_json::from_slice(&result.get_output().stdout).expect("no matches JSON");
    assert_eq!(report["prune"]["planned_manifest_count"], 0);
    run(&["--source-path", "[invalid"]).code(2);
    run(&["--source-path", " "]).code(2);
    run(&["--provider", " "]).code(2);
    for capture in [selected, other_path, other_provider] {
        assert!(
            data_dir
                .join("raw-mirror/v1")
                .join(capture.manifest_relative_path)
                .exists()
        );
    }
}

fn run_on_large_stack<F>(f: F) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    let handle = std::thread::Builder::new()
        .name("cli-refresh-contract".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(f)
        .map_err(|err| format!("spawn large-stack CLI parser test: {err}"))?;

    match handle.join() {
        Ok(result) => result,
        Err(_) => Err("large-stack CLI parser test panicked".to_string()),
    }
}

#[test]
fn search_refresh_and_catch_up_alias_enable_incremental_preflight() -> Result<(), String> {
    run_on_large_stack(|| {
        for args in [
            ["cass", "search", "needle", "--refresh"],
            ["cass", "search", "needle", "--catch-up"],
        ] {
            let cli = parse(&args)?;
            match cli.command {
                Some(Commands::Search { refresh: true, .. }) => {}
                Some(Commands::Search { .. }) => {
                    return Err(format!("search should enable refresh for args {args:?}"));
                }
                other => {
                    return Err(format!(
                        "expected search command for args {args:?}: {other:?}"
                    ));
                }
            }
        }
        Ok(())
    })
}

#[test]
fn tui_refresh_and_catch_up_alias_enable_incremental_preflight() -> Result<(), String> {
    run_on_large_stack(|| {
        for args in [
            ["cass", "tui", "--once", "--refresh"],
            ["cass", "tui", "--once", "--catch-up"],
        ] {
            let cli = parse(&args)?;
            match cli.command {
                Some(Commands::Tui { refresh: true, .. }) => {}
                Some(Commands::Tui { .. }) => {
                    return Err(format!("tui should enable refresh for args {args:?}"));
                }
                other => return Err(format!("expected tui command for args {args:?}: {other:?}")),
            }
        }
        Ok(())
    })
}

#[test]
fn refresh_preflight_stays_opt_in_for_search_and_tui() -> Result<(), String> {
    run_on_large_stack(|| {
        let search = parse(&["cass", "search", "needle"])?;
        match search.command {
            Some(Commands::Search { refresh: false, .. }) => {}
            Some(Commands::Search { .. }) => {
                return Err("search refresh must stay opt-in".to_string());
            }
            other => return Err(format!("expected search command: {other:?}")),
        }

        let tui = parse(&["cass", "tui", "--once"])?;
        match tui.command {
            Some(Commands::Tui { refresh: false, .. }) => {}
            Some(Commands::Tui { .. }) => return Err("tui refresh must stay opt-in".to_string()),
            other => return Err(format!("expected tui command: {other:?}")),
        }
        Ok(())
    })
}

#[test]
fn refresh_preflight_remains_scoped_to_requested_data_dir() -> Result<(), String> {
    run_on_large_stack(|| {
        let search = parse(&[
            "cass",
            "search",
            "needle",
            "--refresh",
            "--data-dir",
            "/tmp/cass-refresh-contract",
            "--json",
        ])?;
        match search.command {
            Some(Commands::Search {
                refresh: true,
                data_dir: Some(data_dir),
                json: true,
                ..
            }) if data_dir.display().to_string() == "/tmp/cass-refresh-contract" => {}
            other => {
                return Err(format!(
                    "search refresh preflight must stay data-dir scoped: {other:?}"
                ));
            }
        }

        let tui = parse(&[
            "cass",
            "tui",
            "--once",
            "--catch-up",
            "--data-dir",
            "/tmp/cass-refresh-contract",
        ])?;
        match tui.command {
            Some(Commands::Tui {
                once: true,
                refresh: true,
                data_dir: Some(data_dir),
                ..
            }) if data_dir.display().to_string() == "/tmp/cass-refresh-contract" => Ok(()),
            other => Err(format!(
                "tui catch-up preflight must stay data-dir scoped: {other:?}"
            )),
        }
    })
}

#[test]
fn index_refresh_operator_controls_remain_parseable() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&[
            "cass",
            "index",
            "--full",
            "--force-rebuild",
            "--json",
            "--idempotency-key",
            "stale-refresh-001",
            "--progress-interval-ms",
            "250",
            "--no-progress-events",
        ])?;

        match cli.command {
            Some(Commands::Index {
                full: true,
                force_rebuild: true,
                json: true,
                idempotency_key: Some(key),
                progress_interval_ms: 250,
                no_progress_events: true,
                ..
            }) if key == "stale-refresh-001" => Ok(()),
            other => Err(format!(
                "expected full refresh operator controls to parse: {other:?}"
            )),
        }
    })
}

#[test]
fn index_refresh_robot_alias_keeps_global_format_contract() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&[
            "cass",
            "--robot-format",
            "jsonl",
            "index",
            "--full",
            "--robot",
            "--idempotency-key",
            "stale-refresh-jsonl-001",
            "--progress-interval-ms",
            "500",
        ])?;

        match cli {
            Cli {
                robot_format: Some(RobotFormat::Jsonl),
                command:
                    Some(Commands::Index {
                        full: true,
                        json: true,
                        idempotency_key: Some(key),
                        progress_interval_ms: 500,
                        ..
                    }),
                ..
            } if key == "stale-refresh-jsonl-001" => Ok(()),
            other => Err(format!(
                "index refresh robot alias must preserve global robot format: {other:?}"
            )),
        }
    })
}

#[test]
fn index_refresh_force_alias_stays_available_for_repair_scripts() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&["cass", "index", "--force"])?;

        match cli.command {
            Some(Commands::Index {
                force_rebuild: true,
                ..
            }) => Ok(()),
            other => Err(format!(
                "expected --force alias to map to force_rebuild: {other:?}"
            )),
        }
    })
}

#[test]
fn raw_mirror_and_doctor_modules_are_public_embedding_surfaces() -> Result<(), String> {
    let data_dir = tempfile::tempdir().map_err(|err| format!("temp data dir: {err}"))?;
    let source_path = data_dir.path().join("session.jsonl");
    std::fs::write(&source_path, b"{\"role\":\"user\",\"content\":\"hello\"}\n")
        .map_err(|err| format!("write source fixture: {err}"))?;

    let db_link = RawMirrorDbLink {
        conversation_id: Some(7),
        message_count: Some(1),
        source_path: Some(source_path.display().to_string()),
        started_at_ms: Some(123),
    };
    let captured = capture_source_file(RawMirrorCaptureInput {
        data_dir: data_dir.path(),
        provider: "contract-test",
        source_id: "session-1",
        origin_kind: "local",
        origin_host: None,
        source_path: &source_path,
        db_links: std::slice::from_ref(&db_link),
    })
    .map_err(|err| format!("capture raw mirror source: {err}"))?;
    merge_manifest_db_links(
        data_dir.path(),
        &captured.manifest_relative_path,
        &[db_link],
    )
    .map_err(|err| format!("merge raw mirror db links: {err}"))?;

    let summary = storage_summary(data_dir.path());
    assert_eq!(summary.manifest_count, 1);
    assert_eq!(summary.unique_blob_count, 1);

    let prune_report = prune(
        data_dir.path(),
        RawMirrorPruneOptions {
            older_than_ms: Some(i64::MAX),
            max_size_bytes: None,
            keep_tags: Vec::new(),
            safety_hold_down_ms: 0,
            apply: false,
            ..RawMirrorPruneOptions::default()
        },
    )
    .map_err(|err| format!("dry-run raw mirror prune: {err}"))?;
    assert_eq!(prune_report.mode, "dry-run");

    let doctor_request = DoctorCommandRequest::from_cli_flags_with_backups(
        Some(data_dir.path().to_path_buf()),
        None,
        Some(RobotFormat::Json),
        true,
        false,
        false,
        false,
        false,
        false,
        false,
        false,
        false,
        None,
        false,
        false,
        None,
        false,
        false,
        false,
    )
    .map_err(|err| format!("build public doctor check request: {err}"))?;
    assert_eq!(doctor_request.surface, DoctorCommandSurface::Check);
    assert_eq!(DoctorBackupCommand::List.stable_name(), "list");
    let _execute: fn(DoctorCommandRequest) -> coding_agent_search::CliResult<()> =
        coding_agent_search::doctor::execute_doctor_command;

    Ok(())
}

#[test]
fn index_watch_refresh_entrypoints_remain_parseable() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&[
            "cass",
            "index",
            "--watch",
            "--watch-interval",
            "7",
            "--watch-once",
            "/sessions/a.jsonl,/sessions/b.jsonl",
            "--watch-once",
            "/sessions/c.jsonl",
            "--json",
        ])?;

        match cli.command {
            Some(Commands::Index {
                watch: true,
                watch_interval: 7,
                watch_once: Some(paths),
                json: true,
                ..
            }) => {
                let rendered: Vec<String> = paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect();
                if rendered
                    == [
                        "/sessions/a.jsonl",
                        "/sessions/b.jsonl",
                        "/sessions/c.jsonl",
                    ]
                {
                    Ok(())
                } else {
                    Err(format!("watch-once paths parsed incorrectly: {rendered:?}"))
                }
            }
            other => Err(format!(
                "expected watch refresh entrypoint controls to parse: {other:?}"
            )),
        }
    })
}

#[test]
fn index_watch_refresh_defaults_stay_bounded() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&["cass", "index", "--watch"])?;

        match cli.command {
            Some(Commands::Index {
                watch: true,
                watch_interval: 30,
                watch_once: None,
                ..
            }) => Ok(()),
            other => Err(format!(
                "expected bounded watch refresh defaults to parse: {other:?}"
            )),
        }
    })
}

// Reuses the native journey from the unlanded GH514 candidate e6c8bc6db,
// with isolated connector roots, strict subprocess results, append coverage,
// and a negative case that cannot pass merely because cooldown is active.
mod gh514_native {
    use coding_agent_search::{indexer::background_refresh, storage::sqlite::FrankenStorage};
    use serde_json::{Value, json};
    use std::{
        fs,
        io::Write as _,
        path::PathBuf,
        process::Command,
        time::{Duration, Instant},
    };

    struct Fixture {
        _root: tempfile::TempDir,
        home: PathBuf,
        data: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            // Production deliberately refuses detached work in OS temp dirs.
            // Keep this fixture outside them without bypassing that policy.
            let cache = PathBuf::from(std::env::var_os("HOME").expect("test HOME"))
                .join(".cache/cass-test-scratch");
            fs::create_dir_all(&cache).unwrap();
            let root = tempfile::Builder::new()
                .prefix("gh514-")
                .tempdir_in(&cache)
                .unwrap();
            let root_path = root.path().canonicalize().unwrap();
            assert!(!root_path.starts_with(std::env::temp_dir()));
            assert!(!root_path.starts_with("/tmp"));
            assert!(!root_path.starts_with("/var/tmp"));
            let home = root_path.join("home");
            let data = root_path.join("data");
            fs::create_dir_all(&home).unwrap();
            Self {
                _root: root,
                home,
                data,
            }
        }

        fn command(&self, args: &[&str]) -> Command {
            let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
            cmd.env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &self.home)
                .env("USERPROFILE", &self.home)
                .env("CODEX_HOME", self.home.join(".codex"))
                .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
                .env("XDG_CONFIG_HOME", self.home.join(".config"))
                .env("XDG_DATA_HOME", self.home.join(".local/share"))
                .env("XDG_CACHE_HOME", self.home.join(".cache"))
                .env("CASS_CLOUDMCP_DATA_ROOT", self.home.join("cloudmcp"))
                .env("CASS_OPENWEBUI_DATA_ROOT", self.home.join("openwebui"))
                .env("CASS_IGNORE_SOURCES_CONFIG", "1")
                .env("CASS_SEMANTIC_ENABLED", "0")
                .env("CASS_RAW_MIRROR", "0")
                .env("CASS_AUTO_REFRESH", "1")
                .env("CASS_AUTO_REFRESH_COOLDOWN_SECS", "0")
                .env("RUST_MIN_STACK", "134217728")
                .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
                .env("NO_COLOR", "1")
                .current_dir(&self.home)
                .args(args)
                .arg("--data-dir")
                .arg(&self.data);
            cmd
        }

        fn run(&self, command: Command) -> Value {
            let output = assert_cmd::Command::from_std(command)
                .timeout(Duration::from_secs(90))
                .assert()
                .success()
                .get_output()
                .clone();
            serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
                panic!(
                    "native CLI did not emit JSON: {error}; stdout={} stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
            })
        }

        fn search_command(&self, marker: &str, interval: &str) -> Command {
            let mut cmd = self.command(&[
                "search",
                marker,
                "--json",
                "--robot-meta",
                "--mode",
                "lexical",
                "--timeout",
                "20000",
            ]);
            cmd.env("CASS_AUTO_REFRESH_SOURCE_SCAN_SECS", interval);
            cmd
        }

        fn read(&self, marker: &str, interval: &str) -> Value {
            self.run(self.search_command(marker, interval))
        }

        fn scan_watermark(&self) -> i64 {
            FrankenStorage::open_readonly(&self.data.join("agent_search.db"))
                .unwrap()
                .get_last_scan_ts()
                .unwrap()
                .expect("a completed scan records its watermark")
        }

        fn write_session(&self, name: &str, marker: &str) -> PathBuf {
            let sessions = self.home.join(".codex/sessions/2026/10/23");
            fs::create_dir_all(&sessions).unwrap();
            let path = sessions.join(format!("rollout-{name}.jsonl"));
            let stamp = chrono::Utc::now().to_rfc3339();
            fs::write(
                &path,
                format!(
                    "{}\n{}\n",
                    json!({"timestamp":stamp,"type":"session_meta","payload":{
                        "id":name,"cwd":self.home,"cli_version":"0.42.0"
                    }}),
                    message(marker)
                ),
            )
            .unwrap();
            path
        }

        fn await_catch_up(&self, marker: &str, previous_scan: i64) {
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                if fs::File::open(self.data.join("index-run.lock"))
                    .is_ok_and(|lock| fs2::FileExt::try_lock_shared(&lock).is_ok())
                    && self.scan_watermark() > previous_scan
                {
                    // Polls must never repair or ingest the source themselves:
                    // only the detached child may make this hit searchable.
                    let mut cmd = self.search_command(marker, "300");
                    cmd.arg("--no-maintenance");
                    let output = self.run(cmd);
                    assert!(
                        has_hit(&output),
                        "completed catch-up lost the hit: {output}"
                    );
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "detached catch-up did not complete; log={}",
                    fs::read_to_string(background_refresh::log_path(&self.data))
                        .unwrap_or_default()
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        fn trigger(&self, marker: &str) -> Value {
            // Exercise the real age gate, long before health's 30-minute age.
            std::thread::sleep(Duration::from_secs(2));
            let output = self.read(marker, "1");
            assert_eq!(
                output.pointer("/_meta/index_freshness/stale"),
                Some(&json!(false)),
                "{output}"
            );
            assert_eq!(
                output.pointer("/_meta/index_freshness/auto_refresh/outcome"),
                Some(&json!("spawned")),
                "{output}"
            );
            assert_eq!(
                output.pointer("/_meta/index_freshness/auto_refresh/trigger"),
                Some(&json!("source-scan-overdue")),
                "{output}"
            );
            output
        }

        fn assert_no_refresh(&self, marker: &str) {
            let before = fs::read(background_refresh::state_path(&self.data)).unwrap();
            let output = self.read(marker, "300");
            assert!(has_hit(&output), "{output}");
            // Reject every attempted outcome, including cooldown/active-run:
            // neither proves that the completed-scan age suppressed a spawn.
            assert!(
                output
                    .pointer("/_meta/index_freshness/auto_refresh")
                    .is_none_or(Value::is_null),
                "a current source scan must not request refresh: {output}"
            );
            assert_eq!(
                fs::read(background_refresh::state_path(&self.data)).unwrap(),
                before
            );
        }
    }

    fn message(marker: &str) -> Value {
        json!({"timestamp":chrono::Utc::now().to_rfc3339(),"type":"response_item",
        "payload":{"type":"message","role":"user","content":[{
            "type":"input_text","text":marker
        }]}})
    }

    fn has_hit(output: &Value) -> bool {
        !output["hits"]
            .as_array()
            .expect("robot search hits")
            .is_empty()
    }

    #[test]
    fn gh514_read_path_admits_new_files_appends_and_completed_noop_scans() {
        let fixture = Fixture::new();
        fixture.write_session("seed", "gh514seededproof4z");
        let index =
            fixture.run(fixture.command(&["index", "--full", "--json", "--no-progress-events"]));
        assert_eq!(index["success"], true, "{index}");
        assert_eq!(
            index["conversations"], 1,
            "isolated connector roots: {index}"
        );
        assert!(has_hit(&fixture.read("gh514seededproof4z", "300")));

        let scan = fixture.scan_watermark();
        let late = fixture.write_session("late", "gh514lateproof4z");
        assert!(!has_hit(&fixture.read("gh514lateproof4z", "300")));
        assert!(!has_hit(&fixture.trigger("gh514lateproof4z")));
        fixture.await_catch_up("gh514lateproof4z", scan);
        fixture.assert_no_refresh("gh514lateproof4z");

        let scan = fixture.scan_watermark();
        let mut source = fs::OpenOptions::new().append(true).open(&late).unwrap();
        writeln!(source, "{}", message("gh514appendproof4z")).unwrap();
        source.sync_all().unwrap();
        drop(source);
        assert!(!has_hit(&fixture.read("gh514appendproof4z", "300")));
        assert!(!has_hit(&fixture.trigger("gh514appendproof4z")));
        fixture.await_catch_up("gh514appendproof4z", scan);
        fixture.assert_no_refresh("gh514appendproof4z");

        // A real no-op scan must also advance source discovery and suppress
        // another request; no foreground index runs after initial setup.
        let scan = fixture.scan_watermark();
        let _ = fixture.trigger("gh514appendproof4z");
        fixture.await_catch_up("gh514appendproof4z", scan);
        fixture.assert_no_refresh("gh514appendproof4z");
    }
}
