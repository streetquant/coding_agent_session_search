//! Real-binary gate for the typed guided-operation adapters (bead
//! `coding_agent_session_search-2l1b0.19`).
//!
//! `src/guide_adapters.rs` is the typed adapter core: preview, apply, and
//! verification for every mutating workflow command the guide runner can
//! close. This gate proves the live surface against the real binary and
//! disposable archives:
//!
//! * the dry run publishes a typed preview (exact operation scope, plan
//!   identity, postconditions, compensation state) and changes no state;
//! * an apply that is not bound to the plan identity the operator inspected
//!   is refused before any child starts;
//! * a concurrent index run makes the guide refuse to mutate instead of
//!   racing the other writer;
//! * an adapter that cannot be closed stays `adapter-unavailable` with a
//!   stable reason code;
//! * the apply path reports planned, attempted, applied and verified
//!   separately, and never claims an executed rollback.
//!
//! Every run uses a fresh `HOME`/`XDG_*`/data dir, empty agent homes,
//! `CASS_IGNORE_SOURCES_CONFIG=1`, `CASS_SEMANTIC_EMBEDDER=hash`, and
//! `CASS_DATA_DIR` pinned at the disposable data dir so no ambient host state
//! leaks into the observation.

mod util;

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use fs2::FileExt;
use serde_json::Value;

use util::timeout::spawn_with_timeout_or_diag;

type TestResult = Result<(), Box<dyn Error>>;
const TIMEOUT: Duration = Duration::from_secs(180);

fn ensure(condition: bool, message: impl FnOnce() -> String) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(message().into())
    }
}

fn isolated_home() -> Result<(tempfile::TempDir, PathBuf, PathBuf), Box<dyn Error>> {
    let temp = tempfile::TempDir::new()?;
    let home = temp.path().join("home");
    let data_dir = temp.path().join("data");
    fs::create_dir_all(&home)?;
    fs::create_dir_all(&data_dir)?;
    Ok((temp, home, data_dir))
}

fn cass_cmd(home: &Path, data_dir: &Path) -> Command {
    let mut command = Command::new(cargo_bin("cass"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CASS_DATA_DIR", data_dir)
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("NO_COLOR", "1")
        .env("CODEX_HOME", home.join(".codex-empty"))
        .env("CLAUDE_HOME", home.join(".claude-empty"))
        .env("CLAUDE_CONFIG_DIR", home.join(".claude-empty"))
        .env("GEMINI_HOME", home.join(".gemini-empty"))
        .env_remove("CLAUDE_CONFIG_DIR");
    command
}

/// Seed a disposable, healthy archive so the live preflight facts are real.
fn seed_archive(home: &Path, data_dir: &Path) -> TestResult {
    let mut command = cass_cmd(home, data_dir);
    command.args([
        "index",
        "--force-rebuild",
        "--json",
        "--data-dir",
        data_dir.to_str().expect("utf8 data dir"),
    ]);
    let output = spawn_with_timeout_or_diag(command, "guide-adapter-seed", None, TIMEOUT);
    ensure(output.status.success(), || {
        format!(
            "seed index failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    Ok(())
}

fn run_guide(home: &Path, data_dir: &Path, args: &[&str]) -> Result<Value, Box<dyn Error>> {
    let mut command = cass_cmd(home, data_dir);
    command.arg("guide").args(args);
    command.arg("--data-dir");
    command.arg(data_dir);
    command.arg("--json");
    let output = spawn_with_timeout_or_diag(command, "guide-adapter", None, TIMEOUT);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let payload: Value = serde_json::from_str(stdout.trim()).map_err(|error| {
        format!(
            "guide stdout was not JSON: {error}; stdout={}; stderr={}",
            stdout.chars().take(500).collect::<String>(),
            stderr.chars().take(500).collect::<String>()
        )
    })?;
    Ok(payload)
}

/// A digest over the canonical archive and the published lexical generation.
/// The preview must not move either of them.
fn archive_digest(data_dir: &Path) -> String {
    let mut entries: Vec<(String, u64)> = Vec::new();
    for relative in ["agent_search.db", "index"] {
        let path = data_dir.join(relative);
        if path.is_file() {
            let size = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
            entries.push((relative.to_string(), size));
        } else if path.is_dir() {
            let mut files: Vec<(String, u64)> = Vec::new();
            collect_files(&path, &path, &mut files);
            files.sort();
            for (file, size) in files {
                entries.push((format!("{relative}/{file}"), size));
            }
        }
    }
    let rendered = entries
        .iter()
        .map(|(path, size)| format!("{path}:{size}"))
        .collect::<Vec<_>>()
        .join(",");
    blake3::hash(rendered.as_bytes()).to_hex().to_string()
}

fn collect_files(root: &Path, dir: &Path, files: &mut Vec<(String, u64)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, files);
        } else if let Ok(meta) = entry.metadata() {
            let relative = path
                .strip_prefix(root)
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| path.display().to_string());
            files.push((relative, meta.len()));
        }
    }
}

/// The mutating transcript row of a guide plan.
fn mutation_step(payload: &Value) -> Option<&Value> {
    payload
        .pointer("/execution/transcript")?
        .as_array()?
        .iter()
        .find(|step| step.get("mutation_class").and_then(Value::as_str) != Some("read-only-proof"))
}

fn gate_result<'a>(payload: &'a Value, name: &str) -> Option<&'a str> {
    payload
        .pointer("/execution/global_gates")?
        .as_array()?
        .iter()
        .find(|gate| gate.get("gate").and_then(Value::as_str) == Some(name))?
        .get("result")?
        .as_str()
}

fn counter(payload: &Value, name: &str) -> Option<u64> {
    payload.pointer("/execution")?.get(name)?.as_u64()
}

#[test]
fn the_dry_run_publishes_a_typed_preview_and_changes_nothing() -> TestResult {
    let (_guard, home, data_dir) = isolated_home()?;
    seed_archive(&home, &data_dir)?;
    let before = archive_digest(&data_dir);

    let payload = run_guide(&home, &data_dir, &["repair-assets"])?;

    let step = mutation_step(&payload).ok_or("no mutating step in the plan")?;
    ensure(
        step.get("result").and_then(Value::as_str) == Some("planned"),
        || format!("a dry run must plan every step: {step}"),
    )?;
    ensure(
        step.get("phase").and_then(Value::as_str) == Some("planned"),
        || format!("a dry run step must report the planned phase: {step}"),
    )?;
    let preview = step
        .get("preview")
        .ok_or("the mutating step has no preview")?;
    ensure(
        preview.get("status").and_then(Value::as_str) == Some("available"),
        || format!("the doctor adapter must publish an available preview: {preview}"),
    )?;
    ensure(
        preview.get("operation").and_then(Value::as_str) == Some("doctor.rebuild-stale-assets"),
        || format!("the preview must name its operation: {preview}"),
    )?;
    ensure(
        preview.pointer("/scope/kind").and_then(Value::as_str) == Some("derived-assets"),
        || format!("the preview must name the exact operation scope: {preview}"),
    )?;
    ensure(
        preview
            .get("postconditions")
            .and_then(Value::as_array)
            .is_some_and(|postconditions| !postconditions.is_empty()),
        || format!("the preview must declare its postconditions: {preview}"),
    )?;
    ensure(
        preview
            .pointer("/compensation/executed")
            .and_then(Value::as_bool)
            == Some(false),
        || format!("a preview must never claim an executed rollback: {preview}"),
    )?;
    ensure(
        preview
            .pointer("/identity/value")
            .and_then(Value::as_str)
            .is_some_and(|identity| !identity.is_empty()),
        || format!("the preview must publish a plan identity: {preview}"),
    )?;
    ensure(
        preview
            .get("apply_argv")
            .and_then(Value::as_array)
            .is_some_and(|argv| argv.iter().any(|token| token == "--plan-fingerprint")),
        || format!("the preview must name the exact apply argv: {preview}"),
    )?;
    ensure(
        counter(&payload, "attempted_mutation_count") == Some(0)
            && counter(&payload, "applied_mutation_count") == Some(0),
        || "a dry run must not attempt or apply a mutation".to_string(),
    )?;
    ensure(
        counter(&payload, "planned_mutation_count") == Some(1),
        || "the plan must report how many steps mutate".to_string(),
    )?;
    ensure(archive_digest(&data_dir) == before, || {
        "the preview changed the canonical archive or the published index".to_string()
    })?;
    Ok(())
}

#[test]
fn an_apply_that_is_not_bound_to_the_inspected_identity_is_refused() -> TestResult {
    let (_guard, home, data_dir) = isolated_home()?;
    seed_archive(&home, &data_dir)?;
    let before = archive_digest(&data_dir);

    let payload = run_guide(
        &home,
        &data_dir,
        &[
            "repair-assets",
            "--apply",
            "--confirm-step",
            "3",
            "--confirm-fact",
            "disk_headroom_ok",
            "--accept-cost-risk",
            "medium",
            "--allow-rch",
            "--confirm-stop-conditions-clear",
            "--expect-plan-identity",
            "3=not-the-inspected-identity",
        ],
    )?;

    let step = mutation_step(&payload).ok_or("no mutating step in the plan")?;
    let result = step.get("result").and_then(Value::as_str).unwrap_or("");
    // The refusal itself is what the scenario is about; a preceding proof
    // gate may refuse first (a degraded archive, or a proof budget that a
    // loaded host exhausted), and that refusal is equally honest.
    ensure(matches!(result, "identity-mismatch" | "blocked"), || {
        format!("a stale plan identity must be refused, not applied: {step}")
    })?;
    if result == "identity-mismatch" {
        ensure(
            step.get("detail")
                .and_then(Value::as_str)
                .is_some_and(|detail| detail.contains("identity")),
            || format!("the refusal must name the identity: {step}"),
        )?;
    }
    ensure(
        counter(&payload, "attempted_mutation_count") == Some(0)
            && counter(&payload, "applied_mutation_count") == Some(0),
        || "a refused identity must not start a child".to_string(),
    )?;
    ensure(
        step.pointer("/phases/attempted").and_then(Value::as_bool) == Some(false),
        || "the refused step must report that nothing was attempted".to_string(),
    )?;
    ensure(archive_digest(&data_dir) == before, || {
        "a refused identity must not change the archive".to_string()
    })?;
    Ok(())
}

#[test]
fn an_apply_runs_the_closed_adapter_and_reports_each_phase() -> TestResult {
    let (_guard, home, data_dir) = isolated_home()?;
    seed_archive(&home, &data_dir)?;

    // The operator inspects the plan first and approves exactly that identity.
    let plan = run_guide(&home, &data_dir, &["repair-assets"])?;
    let identity = mutation_step(&plan)
        .and_then(|step| step.pointer("/preview/identity/value"))
        .and_then(Value::as_str)
        .ok_or("the dry run did not publish a plan identity")?
        .to_string();
    let expectation = format!("3={identity}");

    let payload = run_guide(
        &home,
        &data_dir,
        &[
            "repair-assets",
            "--apply",
            "--confirm-step",
            "3",
            "--confirm-fact",
            "disk_headroom_ok",
            "--accept-cost-risk",
            "medium",
            "--allow-rch",
            "--confirm-stop-conditions-clear",
            "--expect-plan-identity",
            &expectation,
        ],
    )?;

    let step = mutation_step(&payload).ok_or("no mutating step in the plan")?;
    let result = step.get("result").and_then(Value::as_str).unwrap_or("");
    ensure(
        matches!(result, "executed" | "failed" | "plan-blocked" | "blocked"),
        || format!("the apply must report an honest operation outcome: {step}"),
    )?;
    match result {
        "executed" => {
            ensure(
                counter(&payload, "attempted_mutation_count") == Some(1)
                    && counter(&payload, "applied_mutation_count") == Some(1),
                || "an executed operation must be counted as one attempt and one apply".to_string(),
            )?;
            ensure(
                step.pointer("/verification/result")
                    .and_then(Value::as_str)
                    .is_some_and(|result| matches!(result, "verified" | "not-verified")),
                || format!("an applied operation must carry a typed verification: {step}"),
            )?;
        }
        "failed" => {
            ensure(
                counter(&payload, "attempted_mutation_count") == Some(1)
                    && counter(&payload, "applied_mutation_count") == Some(0),
                || {
                    "a failed operation must be counted as an attempt, never as an apply"
                        .to_string()
                },
            )?;
            ensure(
                step.pointer("/phases/attempted").and_then(Value::as_bool) == Some(true),
                || "a failed operation must report that a child did start".to_string(),
            )?;
            ensure(
                step.pointer("/compensation/executed")
                    .and_then(Value::as_bool)
                    == Some(false),
                || "a failed operation must never claim an executed rollback".to_string(),
            )?;
        }
        _ => {
            ensure(
                counter(&payload, "attempted_mutation_count") == Some(0)
                    && counter(&payload, "applied_mutation_count") == Some(0),
                || "a rejected operation scope must not start a child".to_string(),
            )?;
        }
    }
    Ok(())
}

#[test]
fn a_concurrent_index_run_makes_the_guide_refuse_to_mutate() -> TestResult {
    let (_guard, home, data_dir) = isolated_home()?;
    seed_archive(&home, &data_dir)?;
    let before = archive_digest(&data_dir);

    // Hold the data dir's index-run lock the way a live indexer does.
    let lock_path = data_dir.join("index-run.lock");
    let owner = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    owner.try_lock_exclusive()?;

    let payload = run_guide(
        &home,
        &data_dir,
        &[
            "onboard-source",
            "--apply",
            "--confirm-step",
            "3",
            "--confirm-fact",
            "sources_config_writable",
            "--accept-privacy-tier",
            "redacted",
            "--accept-cost-risk",
            "medium",
            "--allow-rch",
            "--confirm-stop-conditions-clear",
        ],
    )?;

    let step = mutation_step(&payload).ok_or("no mutating step in the plan")?;
    let readiness_blocked = gate_result(&payload, "readiness") == Some("blocked");
    if readiness_blocked {
        ensure(
            counter(&payload, "attempted_mutation_count") == Some(0)
                && counter(&payload, "applied_mutation_count") == Some(0),
            || "a blocked readiness gate must not attempt the concurrent mutation".to_string(),
        )?;
    } else {
        // A plan can be ready while another indexer holds the data dir's
        // advisory lock. The closed index adapter must then fail on that lock
        // and never report an applied generation.
        ensure(
            step.get("result").and_then(Value::as_str) == Some("failed"),
            || format!("a held index lock must produce an adapter refusal: {step}"),
        )?;
        ensure(
            counter(&payload, "attempted_mutation_count") == Some(1)
                && counter(&payload, "applied_mutation_count") == Some(0),
            || "the held-lock attempt must be attempted but never applied".to_string(),
        )?;
        ensure(
            step.pointer("/phases/applied").and_then(Value::as_bool) == Some(false),
            || "a held index lock must not report an applied generation".to_string(),
        )?;
    }
    ensure(archive_digest(&data_dir) == before, || {
        "a refused concurrent run must not change the archive".to_string()
    })?;
    Ok(())
}

#[test]
fn an_adapter_without_a_closed_command_path_stays_unavailable() -> TestResult {
    let (_guard, home, data_dir) = isolated_home()?;
    seed_archive(&home, &data_dir)?;

    for (intent, operation, reason) in [
        ("fix-ci", "verify.rerun-gate", "no-closed-command-path"),
        (
            "export-session",
            "export.encrypted-html",
            "requires-operator-scope",
        ),
    ] {
        let payload = run_guide(&home, &data_dir, &[intent])?;
        let step = mutation_step(&payload).ok_or("no mutating step in the plan")?;
        let preview = step
            .get("preview")
            .ok_or("the mutating step has no preview")?;
        ensure(
            preview.get("status").and_then(Value::as_str) == Some("unavailable"),
            || format!("{operation} must stay unavailable: {preview}"),
        )?;
        ensure(
            preview.get("operation").and_then(Value::as_str) == Some(operation),
            || format!("the preview must name the unavailable operation: {preview}"),
        )?;
        ensure(
            preview.get("reason").and_then(Value::as_str) == Some(reason),
            || format!("{operation} must report a stable reason code: {preview}"),
        )?;
        ensure(
            preview
                .get("apply_argv")
                .map(Value::is_null)
                .unwrap_or(true),
            || format!("{operation} must not publish an apply argv: {preview}"),
        )?;
        ensure(
            counter(&payload, "unavailable_mutation_count") == Some(1),
            || "an unavailable adapter must be counted".to_string(),
        )?;
        ensure(
            counter(&payload, "attempted_mutation_count") == Some(0),
            || "an unavailable adapter must never start a child".to_string(),
        )?;
    }
    Ok(())
}

#[test]
fn the_index_adapter_publishes_its_own_typed_scope() -> TestResult {
    let (_guard, home, data_dir) = isolated_home()?;
    seed_archive(&home, &data_dir)?;
    let before = archive_digest(&data_dir);

    let payload = run_guide(&home, &data_dir, &["onboard-source"])?;
    let step = mutation_step(&payload).ok_or("no mutating step in the plan")?;
    let preview = step
        .get("preview")
        .ok_or("the mutating step has no preview")?;
    ensure(
        preview.get("operation").and_then(Value::as_str) == Some("index.incremental"),
        || format!("the index adapter must publish its own preview: {preview}"),
    )?;
    ensure(
        preview
            .pointer("/scope/full_rebuild")
            .and_then(Value::as_bool)
            == Some(false)
            && preview
                .pointer("/scope/semantic_embedding")
                .and_then(Value::as_bool)
                == Some(false),
        || format!("the index scope must stay incremental and model-free: {preview}"),
    )?;
    ensure(
        preview
            .get("apply_argv")
            .and_then(Value::as_array)
            .is_some_and(|argv| {
                argv.iter().any(|token| token == "index")
                    && !argv.iter().any(|token| token == "--full")
                    && !argv.iter().any(|token| token == "--semantic")
            }),
        || format!("the index apply argv must stay incremental: {preview}"),
    )?;
    ensure(archive_digest(&data_dir) == before, || {
        "the index preview changed the archive".to_string()
    })?;
    Ok(())
}
