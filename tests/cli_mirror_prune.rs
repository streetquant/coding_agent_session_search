//! Subprocess integration tests for `cass mirror prune`.
//!
//! Issue #253 reported that on v0.5.2 every documented invocation of
//! `cass mirror prune` exited 0 with empty stdout and stderr. The
//! `Commands::Mirror(..)` arm of cass's outer dispatcher was listed in the
//! pattern of the Index/Search/Pack/... branch, but the inner match inside
//! that branch had no `Commands::Mirror(..)` arm; the actual dispatch lived
//! in the sibling `_ =>` branch and was therefore unreachable for any
//! mirror invocation. The user-visible symptom was a clean exit with no
//! output for every flag combination.
//!
//! These tests pin the contract that the success path emits a plan/summary,
//! the no-args path errors out with the documented usage message, and the
//! `--json`/`--robot` paths produce a parseable JSON envelope.

use assert_cmd::assert::OutputAssertExt as _;
use coding_agent_search::raw_mirror::{RawMirrorCaptureInput, capture_source_file};
use predicates::str::contains;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// Build a `cass` command with an isolated data dir and no update prompts.
fn cmd_in(temp_home: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    let data_dir = temp_home.join("data");
    std::fs::create_dir_all(&data_dir).ok();
    cmd.env("HOME", temp_home);
    cmd.env("XDG_DATA_HOME", temp_home.join(".local/share"));
    cmd.env("CASS_DATA_DIR", &data_dir);
    cmd.env("XDG_CONFIG_HOME", temp_home.join(".config"));
    cmd.env("NO_COLOR", "1");
    cmd
}

/// The no-args invocation must reject with the documented usage message.
/// Regression guard for issue #253: previously this exited 0 silently.
#[test]
fn mirror_prune_without_retention_predicate_fails_with_usage_error() {
    let temp = TempDir::new().expect("tempdir");
    let mut cmd = cmd_in(temp.path());
    cmd.args(["mirror", "prune"]);
    cmd.assert().failure().code(2).stderr(contains(
        "cass mirror prune needs at least one retention predicate",
    ));
}

/// `--older-than` alone is a valid retention predicate; the dry-run path
/// must emit a human-readable plan to stdout and exit 0.
/// Regression guard for issue #253.
#[test]
fn mirror_prune_dry_run_emits_plan_to_stdout() {
    let temp = TempDir::new().expect("tempdir");
    let mut cmd = cmd_in(temp.path());
    cmd.args(["mirror", "prune", "--older-than", "90d", "--dry-run"]);
    let output = cmd.assert().success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout).into_owned();
    assert!(
        stdout.contains("Raw-mirror prune (dry-run)"),
        "stdout missing dry-run header; got: {stdout:?}"
    );
    assert!(
        stdout.contains("Manifests:"),
        "stdout missing manifest count line; got: {stdout:?}"
    );
    assert!(
        stdout.contains("Planned manifests:"),
        "stdout missing planned-manifest line; got: {stdout:?}"
    );
    assert!(stdout.contains("Audit ledger bytes before: 0"), "{stdout}");
    assert!(stdout.contains("Audit ledger bytes after: 0"), "{stdout}");
    assert!(stdout.contains("Preview details omitted: 0"), "{stdout}");
}

/// `--json` (or `--robot`) must emit a JSON envelope with a `prune` payload.
/// Regression guard for issue #253: previously the JSON path was also silent.
#[test]
fn mirror_prune_json_emits_envelope_with_prune_payload() {
    let temp = TempDir::new().expect("tempdir");
    let mut cmd = cmd_in(temp.path());
    cmd.args([
        "mirror",
        "prune",
        "--older-than",
        "90d",
        "--dry-run",
        "--json",
    ]);
    let output = cmd.assert().success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout).into_owned();
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).expect("prune JSON");
    assert_eq!(
        value.get("success"),
        Some(&serde_json::Value::Bool(true)),
        "envelope missing success=true: {value}"
    );
    let prune = value
        .get("prune")
        .expect("envelope missing `prune` payload");
    assert_eq!(
        prune.get("mode").and_then(|v| v.as_str()),
        Some("dry-run"),
        "expected mode=dry-run in payload; got {prune}"
    );
    assert!(
        prune.get("manifest_count").is_some(),
        "payload missing manifest_count: {prune}"
    );
}

/// GH488: populated previews return candidates without adding persistent audit
/// records. Exercise both an absent ledger and retained historical evidence.
#[test]
fn mirror_prune_repeated_previews_do_not_grow_the_archive() {
    let temp = TempDir::new().expect("tempdir");
    let data_dir = temp.path().join("data");
    for index in 0..32 {
        let source_path = temp.path().join(format!("session-{index}.jsonl"));
        std::fs::write(&source_path, format!("{{\"text\":\"session {index}\"}}\n"))
            .expect("write source");
        capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("capture real source");
    }
    let root = data_dir.join("raw-mirror/v1");
    let audit_path = root.join("pruned.jsonl");
    let snapshot = || {
        walkdir::WalkDir::new(&root)
            .into_iter()
            .map(|entry| entry.expect("mirror entry"))
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                let bytes = std::fs::read(entry.path()).expect("mirror bytes");
                (entry.into_path(), bytes)
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    for preview in 0..4 {
        if preview == 1 {
            std::fs::write(&audit_path, b"{\"mode\":\"apply\",\"applied\":true}\n")
                .expect("seed historical audit");
        }
        let before = snapshot();
        let mut cmd = cmd_in(temp.path());
        cmd.args([
            "mirror",
            "prune",
            "--max-size",
            "0",
            "--safety-hold-down",
            "0s",
            "--dry-run",
            "--json",
        ]);
        let output = cmd.assert().success();
        let value: serde_json::Value =
            serde_json::from_slice(&output.get_output().stdout).expect("prune JSON");
        let prune = &value["prune"];
        assert_eq!(value["success"], true);
        assert_eq!(prune["mode"], "dry-run");
        assert_eq!(prune["planned_manifest_count"], 32);
        assert_eq!(prune["planned_blob_count"], 32);
        assert_eq!(prune["entries"].as_array().expect("candidates").len(), 64);
        assert_eq!(prune["applied_reclaim_bytes"], 0);
        assert_eq!(prune["inventory_consistency"], "mutation-lock");
        let ledger_bytes = if preview == 0 { 0 } else { 32 };
        assert_eq!(prune["audit_log_bytes_before"], ledger_bytes);
        assert_eq!(prune["audit_log_bytes_after"], ledger_bytes);
        assert!(prune["audit_log_path"].is_null());
        assert_eq!(snapshot(), before, "preview {preview} changed mirror files");
        if preview == 0 {
            assert!(!audit_path.exists());
        }
    }
}

/// GH #522: `--keep-latest N` is a retention predicate on its own. It retires
/// the superseded capture of an appended transcript and keeps the newest one,
/// and the only capture of another session, through both preview and apply.
#[test]
fn mirror_prune_keep_latest_keeps_the_newest_capture_of_each_source() {
    let temp = TempDir::new().expect("tempdir");
    let data_dir = temp.path().join("data");
    let capture = |source_path: &Path| {
        capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path,
            db_links: &[],
        })
        .expect("capture real source")
    };
    let lone = temp.path().join("lone.jsonl");
    std::fs::write(&lone, "{\"text\":\"only copy\"}\n").expect("write lone session");
    let only_copy = capture(&lone);
    let active = temp.path().join("active.jsonl");
    std::fs::write(&active, "{\"text\":\"first run\"}\n").expect("write transcript");
    let superseded = capture(&active);
    // Capture times are milliseconds; keep the two captures apart.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(
        &active,
        "{\"text\":\"first run\"}\n{\"text\":\"second run\"}\n",
    )
    .expect("append to transcript");
    let newest = capture(&active);
    let root = data_dir.join("raw-mirror/v1");

    for mode in ["--dry-run", "--apply"] {
        let mut cmd = cmd_in(temp.path());
        cmd.args([
            "mirror",
            "prune",
            "--keep-latest",
            "1",
            "--safety-hold-down",
            "0s",
            mode,
            "--json",
        ]);
        let output = cmd.assert().success();
        let value: serde_json::Value =
            serde_json::from_slice(&output.get_output().stdout).expect("prune JSON");
        let report = &value["prune"];
        assert_eq!(report["keep_latest"], 1, "{report}");
        assert_eq!(report["planned_manifest_count"], 1, "{report}");
        let manifests = report["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .filter(|entry| entry["kind"] == "manifest")
            .collect::<Vec<_>>();
        assert_eq!(manifests.len(), 1, "{report}");
        assert_eq!(
            manifests[0]["path"].as_str(),
            Some(superseded.manifest_relative_path.as_str())
        );
        assert_eq!(
            manifests[0]["reason"],
            "superseded by the newest 1 captures of its source file"
        );
        assert_eq!(
            root.join(&superseded.manifest_relative_path).exists(),
            mode == "--dry-run"
        );
        assert!(root.join(&newest.manifest_relative_path).exists());
        assert!(root.join(&only_copy.manifest_relative_path).exists());
    }

    let mut cmd = cmd_in(temp.path());
    cmd.args(["mirror", "prune", "--keep-latest", "0", "--json"]);
    cmd.assert().failure().code(2);
    assert!(root.join(&newest.manifest_relative_path).exists());
}

/// Preview details are bounded, but apply must retain every audit entry.
#[test]
fn mirror_prune_caps_preview_details_without_truncating_apply() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("data/raw-mirror/v1");
    let mut expected_bytes = 0_u64;
    for index in 0..1_001 {
        let bytes = format!("orphan capture {index}");
        let digest = blake3::hash(bytes.as_bytes()).to_hex().to_string();
        let directory = root.join("blobs/blake3").join(&digest[..2]);
        std::fs::create_dir_all(&directory).expect("blob directory");
        std::fs::write(directory.join(format!("{digest}.raw")), &bytes).expect("orphan blob");
        expected_bytes += bytes.len() as u64;
    }
    let audit_path = root.join("pruned.jsonl");
    for mode in ["--dry-run", "--dry-run", "--apply"] {
        let mut cmd = cmd_in(temp.path());
        cmd.args([
            "mirror",
            "prune",
            "--max-size",
            "0",
            "--safety-hold-down",
            "0s",
            mode,
            "--json",
        ]);
        let output = cmd.assert().success();
        let value: serde_json::Value =
            serde_json::from_slice(&output.get_output().stdout).expect("prune JSON");
        let report = &value["prune"];
        assert_eq!(report["planned_blob_count"], 1_001);
        assert_eq!(report["planned_reclaim_bytes"], expected_bytes);
        let entries = report["entries"].as_array().expect("entries");
        if mode == "--dry-run" {
            assert_eq!(entries.len(), 1_000);
            assert_eq!(report["omitted_entry_count"], 1);
            assert_eq!(report["applied_blob_count"], 0);
            assert_eq!(report["inventory_consistency"], "unlocked-observation");
            assert_eq!(report["audit_log_bytes_before"], 0);
            assert_eq!(report["audit_log_bytes_after"], 0);
            assert!(!audit_path.exists());
            assert!(!root.join(".mutation.lock").exists());
        } else {
            assert_eq!(entries.len(), 1_001);
            assert_eq!(report["omitted_entry_count"], 0);
            assert_eq!(report["applied_blob_count"], 1_001);
            assert_eq!(report["applied_reclaim_bytes"], expected_bytes);
            assert!(entries.iter().all(|entry| entry["applied"] == true));
            let audit = std::fs::read_to_string(&audit_path).expect("applied audit");
            assert_eq!(report["audit_log_bytes_before"], 0);
            assert_eq!(report["audit_log_bytes_after"], audit.len() as u64);
            assert_eq!(
                audit.lines().count(),
                3_003,
                "one intent, canonical-removal result and reclamation per blob"
            );
        }
    }
}

/// A real read-only mirror must admit previews without chmod/creation. The
/// snapshot includes ctime and permissions, so byte preservation alone cannot
/// hide lock metadata writes.
#[test]
#[cfg(unix)]
fn mirror_prune_preview_reads_a_readonly_mirror_without_lock_metadata_writes() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let temp = TempDir::new().expect("tempdir");
    let data_dir = temp.path().join("data");
    let source_path = temp.path().join("session.jsonl");
    std::fs::write(&source_path, "read-only preview\n").expect("source");
    capture_source_file(RawMirrorCaptureInput {
        data_dir: &data_dir,
        provider: "codex",
        source_id: "local",
        origin_kind: "local",
        origin_host: None,
        source_path: &source_path,
        db_links: &[],
    })
    .expect("capture");
    let root = data_dir.join("raw-mirror/v1");
    let paths = walkdir::WalkDir::new(&root)
        .into_iter()
        .map(|entry| entry.expect("entry").into_path())
        .collect::<Vec<_>>();
    for path in paths.iter().rev() {
        let mode = if path.is_dir() { 0o555 } else { 0o444 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("readonly");
    }
    let snapshot = || {
        paths
            .iter()
            .map(|path| {
                let metadata = std::fs::metadata(path).expect("metadata");
                (
                    metadata.mode(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                    metadata.len(),
                )
            })
            .collect::<Vec<_>>()
    };
    let before = snapshot();
    let mut cmd = cmd_in(temp.path());
    cmd.args([
        "mirror",
        "prune",
        "--max-size",
        "0",
        "--safety-hold-down",
        "0s",
        "--json",
    ]);
    let output = cmd.output().expect("preview subprocess");
    let after = snapshot();
    // Restore only fixture permissions so TempDir can clean up normally.
    for path in &paths {
        let mode = if path.is_dir() { 0o700 } else { 0o600 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .expect("restore fixture");
    }
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(after, before, "preview changed mirror metadata");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert_eq!(value["prune"]["planned_manifest_count"], 1);
    assert_eq!(value["prune"]["inventory_consistency"], "mutation-lock");
    assert!(!root.join("pruned.jsonl").exists());
}

/// Re-measure the reported nine previews with the reported candidate and
/// ledger sizes. Every artifact belongs to this synthetic fixture.
#[test]
fn mirror_prune_reporter_scale_previews_preserve_114mb_of_evidence() {
    use std::io::Write;

    const MANIFESTS: usize = 18_976;
    const BLOBS: usize = 18_400;
    const LEDGER_BYTES: u64 = 114_147_826;
    const LEDGER_ROWS: u64 = 192_176;
    let temp = TempDir::new().expect("tempdir");
    let data_dir = temp.path().join("data");
    let started = std::time::Instant::now();
    let source = temp.path().join("session.jsonl");
    std::fs::write(&source, "{\"session\":0}\n").expect("seed source");
    let seed = capture_source_file(RawMirrorCaptureInput {
        data_dir: &data_dir,
        provider: "codex",
        source_id: "local",
        origin_kind: "local",
        origin_host: None,
        source_path: &source,
        db_links: &[],
    })
    .expect("real captured template");
    let root = data_dir.join("raw-mirror/v1");
    let template: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join(seed.manifest_relative_path)).expect("template bytes"),
    )
    .expect("template JSON");
    // Construct persisted-format inputs from one real capture. Calling capture
    // 19k times would benchmark its per-capture disk admission inventory, not
    // the preview journey. The CLI still validates every identity/checksum.
    fn ordered(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let sorted = map
                    .into_iter()
                    .map(|(key, value)| (key, ordered(value)))
                    .collect::<std::collections::BTreeMap<_, _>>();
                serde_json::Value::Object(sorted.into_iter().collect())
            }
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.into_iter().map(ordered).collect())
            }
            other => other,
        }
    }
    let canonical_hash = |prefix: &str, value| {
        let mut hash = blake3::Hasher::new();
        hash.update(prefix.as_bytes());
        hash.update(&[0]);
        hash.update(&serde_json::to_vec(&ordered(value)).expect("canonical fixture bytes"));
        format!("{prefix}-{}", hash.finalize().to_hex())
    };
    for index in 1..MANIFESTS {
        let bytes = format!("{{\"session\":{}}}\n", index % BLOBS);
        let digest = blake3::hash(bytes.as_bytes()).to_hex().to_string();
        let relative = format!("blobs/blake3/{}/{digest}.raw", &digest[..2]);
        if index < BLOBS {
            let path = root.join(&relative);
            std::fs::create_dir_all(path.parent().expect("blob parent")).expect("blob directory");
            std::fs::write(path, &bytes).expect("synthetic blob");
        }
        let mut manifest = template.clone();
        manifest["source_id"] = format!("synthetic-source-{index}").into();
        manifest["blob_relative_path"] = relative.into();
        manifest["blob_blake3"] = digest.clone().into();
        manifest["blob_size_bytes"] = bytes.len().into();
        manifest["source_size_bytes"] = bytes.len().into();
        manifest["verification"]["content_blake3"] = digest.into();
        let id = canonical_hash(
            "doctor-raw-mirror-manifest-id-v1",
            serde_json::json!({
                "provider":manifest["provider"], "source_id":manifest["source_id"],
                "origin_kind":manifest["origin_kind"], "origin_host":manifest["origin_host"],
                "original_path_blake3":manifest["original_path_blake3"], "blob_blake3":manifest["blob_blake3"],
            }),
        );
        manifest["manifest_id"] = id.clone().into();
        manifest
            .as_object_mut()
            .expect("manifest object")
            .remove("manifest_blake3");
        manifest["manifest_blake3"] =
            canonical_hash("doctor-raw-mirror-manifest-v1", manifest.clone()).into();
        std::fs::write(
            root.join(format!("manifests/{id}.json")),
            serde_json::to_vec(&manifest).expect("manifest JSON"),
        )
        .expect("synthetic manifest");
    }
    let audit = root.join("pruned.jsonl");
    let mut row = serde_json::to_vec(&serde_json::json!({
        "mode":"dry-run", "phase":"preview", "applied":false, "reason":"historical preview"
    }))
    .expect("historical record");
    // Whitespace before the newline keeps each historical line valid JSON.
    row.resize(592, b' ');
    row.push(b'\n');
    let long_rows = LEDGER_BYTES - LEDGER_ROWS * row.len() as u64;
    assert!(long_rows < LEDGER_ROWS);
    let mut writer =
        std::io::BufWriter::new(std::fs::File::create(&audit).expect("fixture ledger"));
    for index in 0..LEDGER_ROWS {
        if index < long_rows {
            writer.write_all(b" ").expect("extra byte");
        }
        writer.write_all(&row).expect("historical record");
    }
    writer.flush().expect("flush ledger");
    drop(writer);
    assert_eq!(
        std::fs::metadata(&audit).expect("ledger stat").len(),
        LEDGER_BYTES
    );
    let snapshot = || {
        walkdir::WalkDir::new(&root)
            .into_iter()
            .map(|entry| entry.expect("mirror entry"))
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                let mut hasher = blake3::Hasher::new();
                hasher
                    .update_reader(std::fs::File::open(entry.path()).expect("mirror file"))
                    .expect("hash mirror bytes");
                (entry.into_path(), hasher.finalize())
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let before = snapshot();
    let expected_reclaim_bytes = before
        .keys()
        .filter(|path| {
            path.starts_with(root.join("manifests")) || path.starts_with(root.join("blobs"))
        })
        .map(|path| std::fs::metadata(path).expect("candidate bytes").len())
        .sum::<u64>();
    let mut largest_output = 0;
    #[cfg(target_os = "linux")]
    let mut peak_planner_rss_kib = 0_u64;
    #[cfg(not(target_os = "linux"))]
    let peak_planner_rss_kib = 0_u64;
    for preview in 0..9 {
        let mut cmd = cmd_in(temp.path());
        cmd.args([
            "mirror",
            "prune",
            "--max-size",
            "0",
            "--safety-hold-down",
            "0s",
            "--json",
        ]);
        #[cfg(target_os = "linux")]
        let output = {
            let stdout_path = temp.path().join(format!("preview-{preview}.stdout"));
            let stderr_path = temp.path().join(format!("preview-{preview}.stderr"));
            let mut child = cmd
                .stdout(std::process::Stdio::from(
                    std::fs::File::create(&stdout_path).expect("stdout"),
                ))
                .stderr(std::process::Stdio::from(
                    std::fs::File::create(&stderr_path).expect("stderr"),
                ))
                .spawn()
                .expect("real preview process");
            let preview_started = std::time::Instant::now();
            loop {
                if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", child.id()))
                {
                    for line in status
                        .lines()
                        .filter(|line| line.starts_with("VmHWM:") || line.starts_with("VmRSS:"))
                    {
                        if let Some(kib) = line
                            .split_whitespace()
                            .nth(1)
                            .and_then(|value| value.parse::<u64>().ok())
                        {
                            peak_planner_rss_kib = peak_planner_rss_kib.max(kib);
                        }
                    }
                }
                if peak_planner_rss_kib > 512 * 1024 || preview_started.elapsed().as_secs() >= 120 {
                    let _ = child.kill();
                    let _ = child.wait();
                    assert!(
                        peak_planner_rss_kib <= 512 * 1024,
                        "planner exceeded 512 MiB: {peak_planner_rss_kib} KiB"
                    );
                    assert!(
                        preview_started.elapsed().as_secs() < 120,
                        "preview exceeded 120 seconds"
                    );
                }
                if child.try_wait().expect("preview state").is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            assert!(peak_planner_rss_kib > 0, "no CLI RSS measurement");
            std::process::Output {
                status: child.wait().expect("preview terminal status"),
                stdout: std::fs::read(stdout_path).expect("preview stdout"),
                stderr: std::fs::read(stderr_path).expect("preview stderr"),
            }
        };
        #[cfg(not(target_os = "linux"))]
        let output = cmd.output().expect("real preview process");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = &output.stdout;
        largest_output = largest_output.max(stdout.len());
        let value: serde_json::Value = serde_json::from_slice(stdout).expect("preview JSON");
        let report = &value["prune"];
        assert_eq!(report["manifest_count"], MANIFESTS);
        assert_eq!(report["unique_blob_count"], BLOBS);
        assert_eq!(report["planned_manifest_count"], MANIFESTS);
        assert_eq!(report["planned_blob_count"], BLOBS);
        assert_eq!(report["planned_reclaim_bytes"], expected_reclaim_bytes);
        assert_eq!(report["entries"].as_array().expect("details").len(), 1_000);
        assert_eq!(report["omitted_entry_count"], MANIFESTS + BLOBS - 1_000);
        assert_eq!(report["audit_log_bytes_before"], LEDGER_BYTES);
        assert_eq!(report["audit_log_bytes_after"], LEDGER_BYTES);
        assert_eq!(report["applied_reclaim_bytes"], 0);
        assert!(report["audit_log_path"].is_null());
        assert!(stdout.len() < 1_000_000, "preview output exceeded 1 MB");
        assert_eq!(
            snapshot(),
            before,
            "preview {preview} changed mirror evidence"
        );
    }
    writeln!(
        std::io::stderr(),
        "GH488_SCALE manifests={MANIFESTS} blobs={BLOBS} candidate_reclaim_bytes={expected_reclaim_bytes} ledger_rows={LEDGER_ROWS} ledger_bytes_before={LEDGER_BYTES} ledger_bytes_after={LEDGER_BYTES} previews=9 largest_output_bytes={largest_output} peak_planner_rss_kib={peak_planner_rss_kib} elapsed_ms={}",
        started.elapsed().as_millis()
    )
    .expect("scale receipt");
}

/// Kill the normal CLI during an actual applied plan, then restart it. Only a
/// durable result claims completion; interrupted intents remain unconfirmed.
#[test]
#[cfg(unix)]
fn mirror_prune_abrupt_process_restart_never_claims_surviving_targets() {
    use std::io::Write;

    const BLOBS: usize = 512;
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("data/raw-mirror/v1");
    let mut paths = Vec::new();
    for index in 0..BLOBS {
        let bytes = format!("abrupt restart orphan {index}");
        let digest = blake3::hash(bytes.as_bytes()).to_hex().to_string();
        let relative = format!("blobs/blake3/{}/{digest}.raw", &digest[..2]);
        let path = root.join(&relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, bytes).expect("blob");
        paths.push(relative);
    }
    let audit = root.join("pruned.jsonl");
    let mut cmd = cmd_in(temp.path());
    cmd.args([
        "mirror",
        "prune",
        "--max-size",
        "0",
        "--safety-hold-down",
        "0s",
        "--apply",
        "--json",
    ]);
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("apply process");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let bytes = std::fs::read(&audit).unwrap_or_default();
        let completed = bytes
            .split(|byte| *byte == b'\n')
            .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
            .any(|row| row["phase"] == "result" && row["applied"] == true);
        if completed {
            child
                .kill()
                .expect("kill active apply after a completed prefix");
            break;
        }
        if child.try_wait().expect("process state").is_some()
            || std::time::Instant::now() >= deadline
        {
            let _ = child.kill();
            let output = child.wait_with_output().expect("apply terminal output");
            assert!(
                completed,
                "apply did not expose a completed prefix before termination: {:?}; {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let terminal = child
        .wait_with_output()
        .expect("killed apply terminal output");
    assert!(!terminal.status.success());
    let interrupted = std::fs::read(&audit).expect("durable ledger");
    let rows = interrupted
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .collect::<Vec<_>>();
    let surviving = paths
        .iter()
        .filter(|path| root.join(path).exists())
        .collect::<Vec<_>>();
    assert!(!surviving.is_empty(), "the process must stop during apply");
    let confirmed = rows
        .iter()
        .filter(|row| row["phase"] == "result" && row["applied"] == true)
        .count();
    assert!(confirmed > 0);
    for path in &paths {
        let results = rows
            .iter()
            .filter(|row| {
                row["phase"] == "result" && row["path"] == *path && row["applied"] == true
            })
            .count();
        assert!(results <= 1, "duplicate completed result for {path}");
        if root.join(path).exists() {
            assert_eq!(results, 0, "surviving target claimed applied");
        }
    }
    let valid_tail = interrupted.last() == Some(&b'\n')
        && interrupted
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .all(|line| serde_json::from_slice::<serde_json::Value>(line).is_ok());
    let mut retry = cmd_in(temp.path());
    retry.args([
        "mirror",
        "prune",
        "--max-size",
        "0",
        "--safety-hold-down",
        "0s",
        "--apply",
        "--json",
    ]);
    if valid_tail {
        retry.assert().success();
        let completed = std::fs::read_to_string(&audit).expect("retry ledger");
        let completed_rows = completed
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("result row"))
            .collect::<Vec<_>>();
        for path in &paths {
            let before_count = rows
                .iter()
                .filter(|row| row["phase"] == "result" && row["path"] == *path)
                .count();
            let after_count = completed_rows
                .iter()
                .filter(|row| row["phase"] == "result" && row["path"] == *path)
                .count();
            assert_eq!(
                after_count, 1,
                "restart must confirm each removed action exactly once"
            );
            if before_count > 0 {
                assert_eq!(after_count, before_count);
            }
            assert!(!root.join(path).exists());
            assert_eq!(
                completed_rows
                    .iter()
                    .filter(|row| row["phase"] == "reclaimed" && row["path"] == *path)
                    .count(),
                1
            );
        }
        let mut empty_retry = cmd_in(temp.path());
        empty_retry.args([
            "mirror",
            "prune",
            "--max-size",
            "0",
            "--safety-hold-down",
            "0s",
            "--apply",
            "--json",
        ]);
        empty_retry.assert().success();
        assert_eq!(
            std::fs::read_to_string(&audit).expect("empty retry ledger"),
            completed
        );
    } else {
        retry
            .assert()
            .failure()
            .stderr(contains("torn, malformed or oversized record"));
        assert_eq!(
            std::fs::read(&audit).expect("preserved torn ledger"),
            interrupted
        );
        assert!(surviving.iter().all(|path| root.join(path).exists()));
    }
    let unconfirmed_removed = BLOBS - surviving.len() - confirmed;
    writeln!(
        std::io::stderr(),
        "GH488_RESTART killed=true confirmed_results={confirmed} surviving_at_restart={} unconfirmed_removed={unconfirmed_removed} torn_tail={}",
        surviving.len(),
        !valid_tail
    )
    .expect("restart receipt");
}

/// `--apply` on an empty data dir must still produce an "apply" summary
/// rather than silently exiting. The actual prune work is a no-op (no
/// captures present in the tempdir), but the user-facing summary is what
/// proves dispatch reached `run_mirror_prune`.
/// Regression guard for issue #253.
#[test]
fn mirror_prune_apply_emits_summary_even_with_empty_mirror() {
    let temp = TempDir::new().expect("tempdir");
    let mut cmd = cmd_in(temp.path());
    cmd.args([
        "mirror",
        "prune",
        "--older-than",
        "90d",
        "--apply",
        "--safety-hold-down",
        "0s",
    ]);
    let output = cmd.assert().success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout).into_owned();
    assert!(
        stdout.contains("Raw-mirror prune (apply)"),
        "stdout missing apply header; got: {stdout:?}"
    );
    assert!(
        stdout.contains("Applied manifests:"),
        "stdout missing applied-manifest line; got: {stdout:?}"
    );
}
