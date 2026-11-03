//! Metamorphic contract check for `cass introspect --json`.
//!
//! `coding_agent_session_search-eq69o`: `response_schemas` is a
//! hand-written schema registry. Golden tests pin the registry and
//! several runtime payloads independently, but they did not prove the
//! registry still describes the JSON emitted by the corresponding
//! runtime commands. Compare live values recursively, including every array
//! element and typed dynamic property. Keep negative controls independent of
//! the production schema builder so a permissive validator cannot pass silently.

use assert_cmd::Command;
use serde_json::{Map, Value, json};
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[allow(deprecated)]
fn cass_cmd(test_home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("cass").expect("cass binary");
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home)
        .env("XDG_CONFIG_HOME", test_home.join(".config"))
        .env("HOME", test_home)
        .env("CODEX_HOME", test_home.join(".codex"))
        .env("CLAUDE_HOME", test_home.join(".claude"))
        .env("GEMINI_HOME", test_home.join(".gemini"))
        .env("OPENCODE_STORAGE_ROOT", test_home.join(".opencode"))
        .env("CASS_AIDER_DATA_ROOT", test_home.join(".aider-missing"))
        .env("PI_SESSIONS_DIR", test_home.join(".pi-sessions-missing"))
        .env("PI_CODING_AGENT_DIR", test_home.join(".pi-agent-missing"))
        .env(
            "PI_CODING_AGENT_SESSION_DIR",
            test_home.join(".pi-coding-agent-sessions-missing"),
        )
        .env_remove("PI_CONFIG_DIR")
        .env_remove("PI_PROFILE")
        .env_remove("CASS_DATA_DIR")
        .env_remove("CASS_DB_PATH")
        .env_remove("CASS_EXCLUDE_PATHS")
        .env_remove("CASS_OUTPUT_FORMAT")
        .env_remove("TOON_DEFAULT_FORMAT")
        .env_remove("RUST_LOG")
        .env("CASS_AUTO_REFRESH", "0")
        .current_dir(test_home)
        .env("CASS_IGNORE_SOURCES_CONFIG", "1");
    cmd
}

fn fixture_path(parts: &[&str]) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for part in parts {
        path.push(part);
    }
    path
}

fn isolated_search_demo_data(test_home: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let dst_root = test_home.join("search_demo_data");
    let sessions = test_home.join(".codex/sessions/2025/11/25");
    fs::create_dir_all(&sessions)?;
    fs::copy(
        fixture_path(&[
            "codex_real",
            "sessions",
            "2025",
            "11",
            "25",
            "rollout-test.jsonl",
        ]),
        sessions.join("rollout-test.jsonl"),
    )?;
    // Exercise live response shapes from a current archive and publication.
    // The frozen search_demo_data database is a legacy migration fixture with
    // duplicate FTS schema rows; full indexing correctly refuses to replace it.
    cass_cmd(test_home)
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&dst_root)
        .assert()
        .success();
    // Exercise the same publication path that build-hnsw consumes, including
    // its semantic manifest. Legacy vector files alone are not a publication.
    cass_cmd(test_home)
        .args(["models", "backfill", "--tier", "fast", "--embedder", "hash"])
        .args(["--json", "--data-dir"])
        .arg(&dst_root)
        .assert()
        .success();
    Ok(dst_root)
}

fn seed_current_archive_backup(data_dir: &Path) -> Result<(), Box<dyn Error>> {
    // Copy the quiescent, freshly indexed archive and its sidecars. The frozen
    // legacy database fixture must not be used to make a current backup valid.
    let backup_dir = data_dir.join("doctor/candidate-promotions/schema-runtime-backup/backup");
    let live_dir = backup_dir.join("live");
    fs::create_dir_all(&live_dir)?;
    let mut artifacts = Vec::new();
    for (suffix, kind) in [
        ("", "prior_live_archive_db_backup"),
        ("-wal", "prior_live_archive_wal_backup"),
        ("-shm", "prior_live_archive_shm_backup"),
    ] {
        let name = format!("agent_search.db{suffix}");
        let source = data_dir.join(&name);
        if !source.exists() {
            continue;
        }
        let destination = live_dir.join(&name);
        fs::copy(&source, &destination)?;
        artifacts.push(json!({
            "artifact_kind": kind,
            "asset_class": "backup_bundle",
            "source_path": source,
            "backup_path": destination,
            "target_path": source,
            "size_bytes": fs::metadata(&destination)?.len(),
            "checksum_blake3": blake3::hash(&fs::read(&destination)?).to_hex().to_string(),
            "copied_to_backup": true,
            "promoted_to_live": false
        }));
    }
    fs::write(
        backup_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "manifest_kind": "cass_doctor_candidate_promotion_backup_manifest_v1",
            "promotion_id": "schema-runtime-backup",
            "backup_dir": backup_dir,
            "artifacts": artifacts
        }))?,
    )?;
    Ok(())
}

fn runtime_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) => {
            if number.is_f64() {
                "number"
            } else {
                "integer"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[derive(Clone, Copy)]
enum ExpectStatus {
    ExitOk,
    ExitAny,
}

fn run_json(test_home: &Path, args: &[String], expect_status: ExpectStatus) -> Value {
    let output = cass_cmd(test_home)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("run cass {args:?}: {err}"));
    if matches!(expect_status, ExpectStatus::ExitOk) {
        assert!(
            output.status.success(),
            "cass {args:?} exited non-zero: status={:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "cass {args:?} stdout is not JSON: {err}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    })
}

fn advertised_types(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn schema_allows_type(schema: &Value, actual_type: &str) -> bool {
    let advertised = advertised_types(schema);
    !schema
        .as_object()
        .is_some_and(|schema| schema.contains_key("type"))
        || advertised.contains(&actual_type)
        || (actual_type == "integer" && advertised.contains(&"number"))
}

fn collect_runtime_shape_gaps(
    surface: &str,
    path: &str,
    runtime: &Value,
    advertised: &Value,
    gaps: &mut Vec<String>,
) {
    if advertised == &Value::Bool(true) {
        return;
    }
    if advertised == &Value::Bool(false) {
        gaps.push(format!(
            "{surface}{path}: runtime value is forbidden by schema"
        ));
        return;
    }
    let runtime_type = runtime_type(runtime);
    if !schema_allows_type(advertised, runtime_type) {
        gaps.push(format!(
            "{surface}{path}: runtime type {runtime_type:?} is not allowed by introspect schema {advertised}"
        ));
        return;
    }

    if let Some(allowed) = advertised.get("enum").and_then(Value::as_array)
        && !allowed.contains(runtime)
    {
        gaps.push(format!(
            "{surface}{path}: runtime value {runtime} is outside enum {allowed:?}"
        ));
    }
    if let Some(expected) = advertised.get("const")
        && runtime != expected
    {
        gaps.push(format!(
            "{surface}{path}: runtime value {runtime} differs from const {expected}"
        ));
    }

    match runtime {
        Value::Object(runtime_props) => {
            let advertised_props = advertised.get("properties").and_then(Value::as_object);
            if let Some(required) = advertised.get("required").and_then(Value::as_array) {
                for field in required.iter().filter_map(Value::as_str) {
                    if !runtime_props.contains_key(field) {
                        gaps.push(format!("{surface}{path}.{field}: required field is absent"));
                    }
                }
            }
            for (key, runtime_child) in runtime_props {
                let child_path = format!("{path}.{key}");
                if let Some(advertised_child) = advertised_props.and_then(|props| props.get(key)) {
                    collect_runtime_shape_gaps(
                        surface,
                        &child_path,
                        runtime_child,
                        advertised_child,
                        gaps,
                    );
                } else {
                    match advertised.get("additionalProperties") {
                        Some(Value::Bool(true)) => {}
                        Some(Value::Object(_)) => collect_runtime_shape_gaps(
                            surface,
                            &child_path,
                            runtime_child,
                            &advertised["additionalProperties"],
                            gaps,
                        ),
                        // A type-only object schema (e.g. introspect's map of
                        // JSON Schemas) constrains the value's type alone.
                        None if advertised_props.is_none() => {}
                        _ => gaps.push(format!(
                            "{surface}{child_path}: runtime field is missing from introspect schema"
                        )),
                    }
                }
            }
        }
        Value::Array(runtime_items) => {
            if let Some(advertised_items) = advertised.get("items") {
                for (index, item) in runtime_items.iter().enumerate() {
                    collect_runtime_shape_gaps(
                        surface,
                        &format!("{path}[{index}]"),
                        item,
                        advertised_items,
                        gaps,
                    );
                }
            } else if !runtime_items.is_empty() && advertised.get("type").is_some() {
                gaps.push(format!("{surface}{path}: array schema missing items"));
            }
        }
        _ => {}
    }
}

fn runtime_gaps(runtime: &Value, advertised: &Value) -> Vec<String> {
    let mut gaps = Vec::new();
    collect_runtime_shape_gaps("control", "$", runtime, advertised, &mut gaps);
    gaps
}

fn collect_doctor_failure_schema_gaps(
    test_home: &Path,
    data_dir: &Path,
    schemas: &Map<String, Value>,
    gaps: &mut Vec<String>,
) -> Result<(), Box<dyn Error>> {
    let blocked_data = test_home.join("doctor-blocked-data");
    let lock_dir = blocked_data.join("doctor/locks");
    fs::create_dir_all(&lock_dir)?;
    let mut lock = fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(lock_dir.join("doctor-repair.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)?;
    writeln!(
        lock,
        "schema_version=1\npid={}\nmode=safe_auto_run\ncommand=cass doctor --fix",
        std::process::id()
    )?;
    lock.flush()?;
    let blocked = cass_cmd(test_home)
        .args(["doctor", "--fix", "--json", "--data-dir"])
        .arg(&blocked_data)
        .output()?;
    assert_eq!(blocked.status.code(), Some(7));
    let payload: Value = serde_json::from_slice(&blocked.stdout)?;
    assert_eq!(payload["operation_outcome"]["kind"], "repair-blocked");
    assert_eq!(payload["auto_fix_applied"], false);
    assert_ne!(payload["authority_status"], "selected");
    assert_eq!(payload["coverage_delta"]["status"], "unknown");
    collect_runtime_shape_gaps(
        "doctor-safe-auto-run-blocked",
        "$",
        &payload,
        &schemas["doctor-safe-auto-run"],
        gaps,
    );
    let context_path = payload["failure_context"]["path"]
        .as_str()
        .expect("blocked doctor captures a real failure artifact");
    let context: Value = serde_json::from_slice(&fs::read(context_path)?)?;
    assert_eq!(
        context["context_kind"],
        "cass_doctor_lock_contention_failure_context"
    );
    collect_runtime_shape_gaps(
        "doctor-failure-context",
        "$",
        &context,
        &schemas["doctor-failure-context"],
        gaps,
    );

    let stale_plan = cass_cmd(test_home)
        .args([
            "doctor",
            "archive-normalize",
            "--yes",
            "--plan-fingerprint",
            "schema-stale-fingerprint",
            "--json",
            "--data-dir",
        ])
        .arg(data_dir)
        .output()?;
    assert_eq!(stale_plan.status.code(), Some(2));
    let refused: Value = serde_json::from_slice(&stale_plan.stdout)?;
    assert_eq!(refused["status"], "fingerprint_mismatch");
    assert_eq!(refused["outcome_kind"], "blocked");
    assert_eq!(refused["auto_fix_applied"], false);
    assert_eq!(refused["issues_fixed"], 0);
    collect_runtime_shape_gaps(
        "doctor-archive-normalize-stale-fingerprint",
        "$",
        &refused,
        &schemas["doctor-archive-normalize"],
        gaps,
    );
    let error: Value = serde_json::from_slice(&stale_plan.stderr)?;
    assert_eq!(error["err"]["kind"], "usage");
    assert_eq!(error["err"]["code"], 2);
    assert_eq!(error["err"]["surface"], "archive-normalize");
    assert_eq!(error["err"]["mode"], "archive-normalize-apply");
    assert_eq!(error["err"]["asset_class"], "unknown");
    collect_runtime_shape_gaps(
        "doctor-error-envelope-stale-fingerprint",
        "$",
        &error,
        &schemas["doctor-error-envelope"],
        gaps,
    );

    for (args, surface, mode) in [
        (
            ["doctor", "backups", "verify", "schema-missing-backup"],
            "backups",
            "read-only-check",
        ),
        (
            ["doctor", "baseline", "diff", "schema-missing-baseline"],
            "baseline-diff",
            "baseline-diff",
        ),
    ] {
        let missing = cass_cmd(test_home)
            .args(args)
            .args(["--json", "--data-dir"])
            .arg(data_dir)
            .output()?;
        assert_eq!(missing.status.code(), Some(13));
        assert!(missing.stdout.is_empty(), "fatal error belongs on stderr");
        let error: Value = serde_json::from_slice(&missing.stderr)?;
        assert_eq!(error["err"]["code"], 13);
        assert_eq!(error["err"]["kind"], "not-found");
        assert_eq!(error["err"]["retryable"], false);
        assert_eq!(error["err"]["surface"], surface);
        assert_eq!(error["err"]["mode"], mode);
        assert_eq!(error["err"]["risk_level"], "unknown");
        assert_eq!(error["err"]["failure_context_path"], Value::Null);
        assert_eq!(error["err"]["event_log_path"], Value::Null);
        collect_runtime_shape_gaps(
            "doctor-error-envelope",
            "$",
            &error,
            &schemas["doctor-error-envelope"],
            gaps,
        );
    }
    let search = cass_cmd(test_home)
        .args(["search", "matrix", "--json", "--data-dir"])
        .arg(test_home.join("missing-search-data"))
        .output()?;
    assert!(!search.status.success());
    let error: Value = serde_json::from_slice(&search.stderr)?;
    assert!(error["error"].is_object(), "non-doctor error: {error}");
    assert!(error.get("err").is_none(), "non-doctor error: {error}");
    Ok(())
}

fn collect_doctor_noop_apply_schema_gaps(
    test_home: &Path,
    schemas: &Map<String, Value>,
    gaps: &mut Vec<String>,
) -> Result<(), Box<dyn Error>> {
    for (command, schema) in [
        ("repair", "doctor-repair-receipt"),
        ("cleanup", "doctor-cleanup-apply"),
    ] {
        let home = test_home.join(format!("{command}-apply-home"));
        let data = home.join("data");
        fs::create_dir_all(&home)?;
        let data_arg = data.to_string_lossy().into_owned();
        run_json(
            &home,
            &[
                "index".to_string(),
                "--force-rebuild".to_string(),
                "--json".to_string(),
                "--data-dir".to_string(),
                data_arg.clone(),
            ],
            ExpectStatus::ExitOk,
        );
        let plan = run_json(
            &home,
            &[
                "doctor".to_string(),
                command.to_string(),
                "--dry-run".to_string(),
                "--json".to_string(),
                "--data-dir".to_string(),
                data_arg.clone(),
            ],
            ExpectStatus::ExitOk,
        );
        let fingerprint = if command == "repair" {
            assert_eq!(
                plan["repair_plan"]["planned_action_count"], 0,
                "no-op repair fixture: {plan}"
            );
            &plan["repair_plan"]["plan_fingerprint"]
        } else {
            let cleanup = &plan["quarantine"]["lexical_cleanup_dry_run"];
            assert!(
                cleanup["reclaim_candidates"]
                    .as_array()
                    .expect("real cleanup candidates")
                    .is_empty(),
                "no-op cleanup fixture: {plan}"
            );
            &cleanup["approval_fingerprint"]
        }
        .as_str()
        .expect("live dry-run fingerprint");
        let applied = run_json(
            &home,
            &[
                "doctor".to_string(),
                command.to_string(),
                "--yes".to_string(),
                "--plan-fingerprint".to_string(),
                fingerprint.to_string(),
                "--json".to_string(),
                "--data-dir".to_string(),
                data_arg,
            ],
            ExpectStatus::ExitOk,
        );
        if command == "repair" {
            assert_eq!(applied["repair_plan"]["apply_authorized"], true);
        } else {
            let cleanup = &applied["cleanup_apply"];
            assert_eq!(cleanup["requested"], true);
            assert_eq!(cleanup["approval_fingerprint"], fingerprint);
            assert_eq!(cleanup["outcome_kind"], "no_op");
            assert_eq!(cleanup["applied"], false);
            assert_eq!(cleanup["pruned_asset_count"], 0);
            assert_eq!(cleanup["reclaimed_bytes"], 0);
            assert!(
                cleanup["planned_actions"]
                    .as_array()
                    .expect("real cleanup plan actions")
                    .is_empty()
            );
        }
        assert_eq!(applied["auto_fix_applied"], false);
        assert_eq!(applied["issues_fixed"], 0);
        assert_eq!(applied["asset_class"], "unknown");
        assert_ne!(applied["authority_status"], "selected");
        assert_eq!(applied["coverage_delta"]["status"], "unknown");
        collect_runtime_shape_gaps(schema, "$", &applied, &schemas[schema], gaps);
    }
    Ok(())
}

fn collect_doctor_component_schema_gaps(
    test_home: &Path,
    data_dir: &Path,
    schemas: &Map<String, Value>,
    gaps: &mut Vec<String>,
) {
    let doctor = run_json(
        test_home,
        &[
            "doctor".to_string(),
            "--json".to_string(),
            "--data-dir".to_string(),
            data_dir.to_string_lossy().into_owned(),
        ],
        ExpectStatus::ExitOk,
    );
    // These three schema-first envelopes have no standalone producer in the
    // parsed doctor command model. Verify their implemented component reports
    // against actual stdout; their future common envelopes are not live proof.
    for (registry_key, fields) in [
        (
            "doctor-reconstruct-dry-run",
            &["candidate_staging", "source_authority"][..],
        ),
        (
            "doctor-remote-source-sync-gaps",
            &["source_inventory", "remote_source_sync", "source_authority"][..],
        ),
        (
            "doctor-semantic-model-fallback",
            &["semantic", "derived_semantic_assets"][..],
        ),
    ] {
        for field in fields {
            let actual = doctor
                .get(*field)
                .unwrap_or_else(|| panic!("doctor omitted live {field}"));
            let declared = schemas[registry_key]["properties"]
                .get(*field)
                .unwrap_or_else(|| panic!("{registry_key} omitted {field} schema"));
            collect_runtime_shape_gaps(registry_key, &format!("$.{field}"), actual, declared, gaps);
        }
    }
}

#[test]
fn runtime_validator_checks_every_array_item_and_nested_key() {
    let schema = json!({
        "type": "array",
        "items": {"type": "object", "properties": {"count": {"type": "integer"}}}
    });
    assert!(runtime_gaps(&json!([]), &schema).is_empty());
    assert!(runtime_gaps(&json!([{"count": 1}, {"count": 2}]), &schema).is_empty());
    let gaps = runtime_gaps(
        &json!([{"count": 1}, {"count": "two", "fleet_id": 3}]),
        &schema,
    );
    assert_eq!(gaps.len(), 2, "{gaps:?}");
    assert!(
        gaps.iter().any(|gap| gap.contains("$[1].count")),
        "{gaps:?}"
    );
    assert!(
        gaps.iter().any(|gap| gap.contains("$[1].fleet_id")),
        "{gaps:?}"
    );
}

#[test]
fn runtime_validator_checks_typed_additional_properties_alongside_declared_keys() {
    let schema = json!({
        "type": "object",
        "properties": {"version": {"type": "string"}},
        "additionalProperties": {
            "type": "object",
            "properties": {"count": {"type": "integer"}},
            "required": ["count"]
        }
    });
    assert!(runtime_gaps(&json!({"version": "v1", "codex": {"count": 1}}), &schema).is_empty());
    let gaps = runtime_gaps(
        &json!({"version": "v1", "codex": {"count": "one"}}),
        &schema,
    );
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert!(gaps[0].contains("$.codex.count"), "{gaps:?}");
    let gaps = runtime_gaps(&json!({"version": 1, "codex": {}}), &schema);
    assert_eq!(gaps.len(), 2, "{gaps:?}");
    assert!(
        gaps.iter()
            .any(|gap| gap.contains("required field is absent")),
        "{gaps:?}"
    );
    let integer_map = json!({"type": "object", "additionalProperties": {"type": "integer"}});
    assert!(runtime_gaps(&json!({"codex": 2}), &integer_map).is_empty());
    assert_eq!(
        runtime_gaps(&json!({"codex": "two"}), &integer_map).len(),
        1
    );
}

#[test]
fn runtime_validator_distinguishes_optional_required_and_nullable_values() {
    let schema = json!({
        "type": "object",
        "properties": {
            "ready": {"type": "boolean"},
            "hint": {"type": ["string", "null"]},
            "count": {"type": "integer"},
            "ratio": {"type": "number"}
        },
        "required": ["ready"]
    });
    for runtime in [
        json!({"ready": true}),
        json!({"ready": false, "hint": null, "count": 1, "ratio": 2}),
    ] {
        assert!(runtime_gaps(&runtime, &schema).is_empty(), "{runtime}");
    }
    let gaps = runtime_gaps(&json!({"ready": null, "count": 1.5, "ratio": "2"}), &schema);
    assert_eq!(gaps.len(), 3, "{gaps:?}");
    let gaps = runtime_gaps(&json!({"hint": "index first"}), &schema);
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert!(
        gaps[0].contains("$.ready: required field is absent"),
        "{gaps:?}"
    );
}

#[test]
fn runtime_validator_enforces_enum_const_and_explicit_open_objects() {
    let schema = json!({
        "type": "object",
        "properties": {
            "mode": {"type": "string", "enum": ["lexical", "hybrid"]},
            "schema_version": {"type": "integer", "const": 1},
            "opaque": {"type": "object", "additionalProperties": true}
        }
    });
    assert!(
        runtime_gaps(
            &json!({"mode": "lexical", "schema_version": 1, "opaque": {"any": [null, 2]}}),
            &schema
        )
        .is_empty()
    );
    let gaps = runtime_gaps(
        &json!({"mode": "unknown", "schema_version": 2, "fleet_id": 3}),
        &schema,
    );
    assert_eq!(gaps.len(), 3, "{gaps:?}");
    assert!(
        gaps.iter().any(|gap| gap.contains("outside enum")),
        "{gaps:?}"
    );
    assert!(
        gaps.iter().any(|gap| gap.contains("differs from const")),
        "{gaps:?}"
    );
    assert!(runtime_gaps(&json!({"anything": [1, "two"]}), &json!({})).is_empty());
    assert_eq!(
        runtime_gaps(
            &json!({"anything": true}),
            &json!({"type": "object", "additionalProperties": false})
        )
        .len(),
        1
    );
}

fn surface_command(
    surface: &str,
    test_home: &Path,
    demo_data: &Path,
) -> Option<(Vec<String>, ExpectStatus)> {
    let demo_data = demo_data.to_str().expect("utf8 demo data");
    let session = fixture_path(&["html_export", "real_sessions", "claude_code_auth_fix.jsonl"]);
    let session = session.to_str().expect("utf8 session path");
    let empty_data_dir = test_home.join(format!("{surface}-data"));
    let empty_data_dir = empty_data_dir.to_str().expect("utf8 data dir");

    let doctor_args: Option<&[&str]> = match surface {
        "doctor-archive-scan" => Some(&["doctor", "archive-scan"]),
        "doctor-archive-normalize" => Some(&["doctor", "archive-normalize", "--dry-run"]),
        "doctor-backups-list" => Some(&["doctor", "backups", "list"]),
        "doctor-backups-verify" => Some(&["doctor", "backups", "verify", "schema-runtime-backup"]),
        "doctor-restore-rehearsal" => {
            Some(&["doctor", "backups", "restore", "schema-runtime-backup"])
        }
        "doctor-baseline-save" => Some(&["doctor", "baseline", "save", "schema-save"]),
        "doctor-baseline-update" => Some(&["doctor", "baseline", "update", "schema-runtime"]),
        "doctor-baseline-diff" => Some(&["doctor", "baseline", "diff", "schema-runtime"]),
        "doctor-check" => Some(&["doctor", "check"]),
        "doctor-cleanup-dry-run" => Some(&["doctor", "cleanup", "--dry-run"]),
        "doctor-repair-dry-run" => Some(&["doctor", "repair", "--dry-run"]),
        "doctor-support-bundle" => Some(&["doctor", "support-bundle"]),
        _ => None,
    };
    if let Some(args) = doctor_args {
        return Some((
            args.iter()
                .map(|arg| (*arg).to_string())
                .chain([
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    demo_data.to_string(),
                ])
                .collect(),
            ExpectStatus::ExitOk,
        ));
    }

    let args = match surface {
        "analytics-incidents" => {
            return Some((
                vec![
                    "analytics".to_string(),
                    "incidents".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    demo_data.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "api-version" => vec!["api-version", "--json"],
        "capabilities" => vec!["capabilities", "--json"],
        "diag" => vec!["diag", "--json"],
        "doctor" => vec!["doctor", "--json", "--data-dir", demo_data],
        "health" => {
            return Some((
                vec![
                    "health".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    empty_data_dir.to_string(),
                ],
                ExpectStatus::ExitAny,
            ));
        }
        "index" => {
            return Some((
                vec![
                    "index".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    empty_data_dir.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "introspect" => vec!["introspect", "--json"],
        "selftest" => vec!["selftest", "--json"],
        "models-build-hnsw" => {
            return Some((
                vec![
                    "models".to_string(),
                    "build-hnsw".to_string(),
                    "--check".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    demo_data.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "models-check-update" => vec!["models", "check-update", "--json"],
        "models-status" => vec!["models", "status", "--json"],
        "models-verify" => vec!["models", "verify", "--json"],
        "pack" => {
            return Some((
                vec![
                    "pack".to_string(),
                    "matrix".to_string(),
                    "--json".to_string(),
                    "--limit".to_string(),
                    "2".to_string(),
                    "--max-evidence".to_string(),
                    "2".to_string(),
                    "--max-tokens".to_string(),
                    "8000".to_string(),
                    "--data-dir".to_string(),
                    demo_data.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "search" => {
            return Some((
                vec![
                    "search".to_string(),
                    "matrix".to_string(),
                    "--json".to_string(),
                    "--limit".to_string(),
                    "2".to_string(),
                    "--robot-meta".to_string(),
                    "--aggregate".to_string(),
                    "agent".to_string(),
                    "--data-dir".to_string(),
                    demo_data.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "sessions" => {
            return Some((
                vec![
                    "sessions".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    demo_data.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "state" => {
            return Some((
                vec![
                    "state".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    empty_data_dir.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "stats" => {
            return Some((
                vec![
                    "stats".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    demo_data.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "status" => {
            return Some((
                vec![
                    "status".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    empty_data_dir.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "triage" => {
            return Some((
                vec![
                    "triage".to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    empty_data_dir.to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        "view" => {
            return Some((
                vec![
                    "view".to_string(),
                    session.to_string(),
                    "-n".to_string(),
                    "1".to_string(),
                    "--json".to_string(),
                ],
                ExpectStatus::ExitOk,
            ));
        }
        _ => return None,
    }
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();

    Some((args, ExpectStatus::ExitOk))
}

#[test]
fn introspect_response_schemas_cover_runtime_json_shapes() -> Result<(), Box<dyn Error>> {
    let test_home = tempfile::tempdir().expect("create temp home");
    let demo_data = isolated_search_demo_data(test_home.path())?;
    seed_current_archive_backup(&demo_data)?;
    let introspect = run_json(
        test_home.path(),
        &["introspect".to_string(), "--json".to_string()],
        ExpectStatus::ExitOk,
    );
    let response_schemas = introspect["response_schemas"]
        .as_object()
        .expect("introspect.response_schemas is an object");
    let mut gaps = Vec::new();

    run_json(
        test_home.path(),
        &[
            "doctor".to_string(),
            "baseline".to_string(),
            "save".to_string(),
            "schema-runtime".to_string(),
            "--json".to_string(),
            "--data-dir".to_string(),
            demo_data.to_string_lossy().into_owned(),
        ],
        ExpectStatus::ExitOk,
    );

    for (surface, advertised_schema) in response_schemas {
        let Some((args, expect_status)) = surface_command(surface, test_home.path(), &demo_data)
        else {
            match surface.as_str() {
                // Real embedded summaries, artifacts, fatal errors and guarded
                // applies are exercised by the explicit collectors below.
                "doctor-health-summary" | "doctor-status-summary" |
                "doctor-safe-auto-run" | "doctor-failure-context" |
                "doctor-error-envelope" | "doctor-cleanup-apply" |
                "doctor-repair-receipt" |
                // Implemented components of three schema-first envelopes.
                "doctor-reconstruct-dry-run" | "doctor-remote-source-sync-gaps" |
                "doctor-semantic-model-fallback" => continue,
                _ => panic!("no runtime command sample mapped for introspect response schema {surface}"),
            }
        };
        let payload = run_json(test_home.path(), &args, expect_status);
        if matches!(
            surface.as_str(),
            "doctor-check"
                | "doctor-archive-scan"
                | "doctor-archive-normalize"
                | "doctor-backups-list"
                | "doctor-backups-verify"
                | "doctor-restore-rehearsal"
                | "doctor-repair-dry-run"
                | "doctor-cleanup-dry-run"
        ) {
            assert_eq!(payload["contract_provenance"], "runtime");
            // These samples have one bounded observation, not a measured
            // before/after comparison or an authority-selection receipt.
            assert_eq!(payload["coverage_delta"]["status"], "unknown");
            assert_ne!(payload["authority_status"], "selected");
        }
        let sampled_items = match surface.as_str() {
            "search" => Some("hits"),
            "pack" => Some("evidence"),
            "sessions" => Some("sessions"),
            "doctor-backups-list" => Some("backups"),
            "doctor-support-bundle" => Some("included_artifacts"),
            _ => None,
        };
        if let Some(field) = sampled_items {
            assert!(
                payload[field]
                    .as_array()
                    .is_some_and(|items| !items.is_empty()),
                "{surface} must exercise nonempty {field} item schemas: {payload}"
            );
        }
        if surface == "doctor-backups-verify" {
            assert_eq!(payload["backup_verification"]["status"], "verified");
            assert_eq!(
                payload["backup_verification"]["restore_rehearsal_allowed"],
                true
            );
        }
        if surface == "doctor-restore-rehearsal" {
            assert_eq!(payload["restore_rehearsal"]["status"], "passed");
            assert_eq!(payload["restore_rehearsal"]["live_archive_untouched"], true);
        }
        collect_runtime_shape_gaps(surface, "$", &payload, advertised_schema, &mut gaps);
        if let Some(registry_key) = match surface.as_str() {
            "health" => Some("doctor-health-summary"),
            "status" | "state" => Some("doctor-status-summary"),
            _ => None,
        } {
            let summary = payload.get("doctor_summary").expect("live doctor summary");
            assert_eq!(summary["contract_provenance"], "runtime");
            collect_runtime_shape_gaps(
                registry_key,
                "$",
                summary,
                &response_schemas[registry_key],
                &mut gaps,
            );
        }
    }

    // State-dependent fields must be sampled in both a missing archive and
    // a real populated archive. In particular memory telemetry, canonical
    // session identity, and the chosen lexical manifest are absent on first use.
    for surface in ["doctor", "health", "status", "state"] {
        for (variant, data_dir) in [
            ("empty", test_home.path().join(format!("{surface}-empty"))),
            ("populated", demo_data.clone()),
        ] {
            let payload = run_json(
                test_home.path(),
                &[
                    surface.to_string(),
                    "--json".to_string(),
                    "--data-dir".to_string(),
                    data_dir.to_string_lossy().into_owned(),
                ],
                if surface == "health" {
                    ExpectStatus::ExitAny
                } else {
                    ExpectStatus::ExitOk
                },
            );
            collect_runtime_shape_gaps(
                &format!("{surface}-{variant}"),
                "$",
                &payload,
                &response_schemas[surface],
                &mut gaps,
            );
        }
    }

    collect_uninspected_triage_schema_gaps(
        test_home.path(),
        &demo_data,
        response_schemas,
        &mut gaps,
    );
    collect_doctor_failure_schema_gaps(test_home.path(), &demo_data, response_schemas, &mut gaps)?;
    collect_doctor_noop_apply_schema_gaps(test_home.path(), response_schemas, &mut gaps)?;
    collect_doctor_component_schema_gaps(test_home.path(), &demo_data, response_schemas, &mut gaps);
    assert!(
        gaps.is_empty(),
        "runtime payloads are not covered by introspect schemas:\n{}",
        gaps.join("\n")
    );
    Ok(())
}

fn collect_uninspected_triage_schema_gaps(
    test_home: &Path,
    demo_data: &Path,
    response_schemas: &Map<String, Value>,
    gaps: &mut Vec<String>,
) {
    // One millisecond is accepted by the CLI and below its 25 ms response
    // reserve, so readiness probes are deterministically left uninspected.
    let uninspected = run_json(
        test_home,
        &[
            "triage".to_string(),
            "--json".to_string(),
            "--timeout".to_string(),
            "1".to_string(),
            "--data-dir".to_string(),
            demo_data.to_string_lossy().into_owned(),
        ],
        ExpectStatus::ExitOk,
    );
    assert_eq!(uninspected["budget"]["budget_ms"], 1);
    assert_eq!(uninspected["budget"]["timed_out"], true);
    assert_eq!(uninspected["search_completeness"]["inspected"], false);
    assert_eq!(uninspected["root_cause"]["inspected"], false);
    assert_eq!(
        uninspected["search_completeness"]["quarantine_status"],
        "not_inspected"
    );
    for field in [
        "quarantined_conversations",
        "complete",
        "can_search",
        "coverage_suspect",
    ] {
        assert_eq!(
            uninspected["search_completeness"][field],
            Value::Null,
            "{field}"
        );
    }
    for section in [
        "index",
        "database",
        "pending",
        "rebuild",
        "rebuild_progress",
        "semantic",
        "ingest_quarantine",
    ] {
        assert_eq!(
            uninspected["readiness"][section]["inspected"], false,
            "{section}"
        );
    }
    for section in ["index", "database"] {
        assert_eq!(
            uninspected["readiness"][section]["exists"],
            Value::Null,
            "{section}"
        );
    }
    let triage_schema = &response_schemas["triage"];
    assert!(triage_schema["properties"]["search_completeness"]["properties"]["quarantine_status"]["enum"]
        .as_array().expect("triage quarantine enum")
        .contains(&uninspected["search_completeness"]["quarantine_status"]));
    collect_runtime_shape_gaps(
        "triage-uninspected-budget",
        "$",
        &uninspected,
        triage_schema,
        gaps,
    );
    // Budget fallback nulls belong to triage, not observed status verdicts.
    let status_completeness = &response_schemas["status"]["properties"]["search_completeness"];
    assert!(!schema_allows_type(
        &status_completeness["properties"]["complete"],
        "null"
    ));
    assert_eq!(
        status_completeness["properties"]["quarantine_status"]["enum"],
        json!(["ok", "degraded"])
    );
}
