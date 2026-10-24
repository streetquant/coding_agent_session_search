//! Typed guided-operation adapters: preview, apply, and verification.
//!
//! Bead `coding_agent_session_search-2l1b0.19` ("Complete remaining
//! guided-operation adapters with typed preview, apply and verification").
//!
//! A workflow macro stores structured command identifiers, not shell snippets.
//! The guide runner resolves those identifiers through a closed allowlist and
//! refuses anything it cannot execute. This module owns the *adapter* half of
//! that contract: for every mutating step it can close it answers three typed
//! questions.
//!
//! 1. **preview** — a read-only, typed description of the exact operation
//!    scope, the identity the operation is bound to, the postconditions that
//!    must hold afterwards, and what a failure would leave behind.
//! 2. **apply** — the exact tokenized argv the runner executes (never a shell
//!    string), whether the attempt actually started, and whether it applied.
//! 3. **verification** — the read-only observation that decides whether the
//!    applied state is verified, which is deliberately distinct from "the
//!    command exited zero".
//!
//! An adapter that cannot be closed stays `adapter-unavailable` with a stable
//! reason code. A generated plan is never reported as an executed repair, and
//! a compensation plan is never reported as an executed rollback.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Schema identifier for the typed adapter payloads.
pub const SCHEMA_VERSION: &str = "cass.guide.adapters.v1";

/// Fingerprint algorithm for guide-computed plan identities.
///
/// Deliberately distinct from the doctor surface's own
/// `blake3-canonical-json-v1`: this one hashes the sorted-key JSON of the
/// precondition facts it names, with no prefix and no doctor-specific
/// normalization, so a guide identity can never be mistaken for a doctor
/// plan fingerprint.
pub const IDENTITY_ALGORITHM: &str = "blake3-sorted-json-v1";

/// The mutating workflow command that rebuilds stale derived assets.
pub const OP_DOCTOR_REBUILD: &str = "doctor.rebuild-stale-assets";

/// The mutating workflow command that runs an incremental lexical index.
pub const OP_INDEX_INCREMENTAL: &str = "index.incremental";

/// The mutating workflow command that produces a redacted support capsule.
pub const OP_SUPPORT_CAPSULE: &str = "support.produce-capsule";

/// The read-only workflow command that previews a remote source sync.
pub const OP_SOURCES_DRY_SYNC: &str = "sources.dry-sync";

/// Reason code: no closed, parameter-complete `cass` command path exists for
/// this structured command.
pub const REASON_NO_CLOSED_COMMAND_PATH: &str = "no-closed-command-path";

/// Reason code: the command needs an operator-selected target (a session, a
/// key) that the guide plan deliberately does not carry.
pub const REASON_REQUIRES_OPERATOR_SCOPE: &str = "requires-operator-scope";

/// Budget for one read-only adapter preview command.
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(120);

/// Budget for one mutating adapter command.
const MUTATION_TIMEOUT: Duration = Duration::from_secs(600);

/// Budget for one post-apply verification command.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(120);

/// The identity a mutating step must be bound to before it may run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanIdentity {
    /// Fingerprint algorithm that produced `value`.
    pub algorithm: String,
    /// The fingerprint itself.
    pub value: String,
    /// Which surface produced it.
    pub source: String,
}

impl PlanIdentity {
    /// The typed JSON projection of an identity.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "algorithm": self.algorithm,
            "value": self.value,
            "source": self.source,
        })
    }
}

/// A typed, read-only description of exactly one mutating operation.
#[derive(Debug, Clone)]
pub struct OperationPreview {
    /// The structured workflow command this preview belongs to.
    pub operation: String,
    /// `available` | `unavailable`.
    pub status: String,
    /// The read-only command that produced the preview, when one ran.
    pub preview_argv: Option<Vec<String>>,
    /// The exact command the apply would run, when it is fully determined.
    pub apply_argv: Option<Vec<String>>,
    /// The exact operation scope: what changes, and what it does not.
    pub scope: Value,
    /// Typed postconditions that must hold after a successful apply.
    pub postconditions: Value,
    /// What a failure would leave behind, and the fact that nothing is rolled
    /// back by the guide runner.
    pub compensation: Value,
    /// The identity the apply must match, when the adapter has one.
    pub identity: Option<PlanIdentity>,
    /// Read-only observations the preview was derived from.
    pub observation: Option<Value>,
    /// Stable reason code when the adapter is not closed.
    pub reason: Option<String>,
    /// Human-readable, non-sensitive detail.
    pub detail: String,
}

impl OperationPreview {
    /// The typed JSON projection of a preview.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "schema_version": SCHEMA_VERSION,
            "operation": self.operation,
            "status": self.status,
            "preview_argv": self.preview_argv,
            "apply_argv": self.apply_argv,
            "scope": self.scope,
            "postconditions": self.postconditions,
            "compensation": self.compensation,
            "identity": self.identity.as_ref().map(PlanIdentity::to_json),
            "observation": self.observation,
            "reason": self.reason,
            "detail": self.detail,
        })
    }

    /// Whether the preview describes an operation that may be applied.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.status == "available"
    }
}

/// The typed outcome of an apply attempt.
#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    /// `executed` | `failed` | `adapter-unavailable` | `identity-missing` |
    /// `identity-mismatch` | `plan-blocked`.
    pub status: String,
    /// `applied` | `failed` | `not-run`.
    pub phase: String,
    /// Whether a child process was started. A started attempt may have left
    /// side effects behind even when it failed.
    pub started: bool,
    /// Non-sensitive detail. Never carries source text, session text, or an
    /// absolute path.
    pub detail: String,
    /// Typed observations from the applied command.
    pub observation: Option<Value>,
}

impl ApplyOutcome {
    /// The typed JSON projection of an apply outcome.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "status": self.status,
            "phase": self.phase,
            "started": self.started,
            "detail": self.detail,
            "observation": self.observation,
        })
    }
}

/// The typed outcome of verifying an applied operation.
#[derive(Debug, Clone)]
pub struct VerificationOutcome {
    /// `verified` | `not-verified` | `failed` | `not-run`.
    pub result: String,
    /// Which surface produced the verdict.
    pub source: String,
    /// Non-sensitive detail.
    pub detail: String,
    /// Typed observations from the verification command.
    pub observation: Option<Value>,
}

impl VerificationOutcome {
    /// The typed JSON projection of a verification outcome.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "result": self.result,
            "source": self.source,
            "detail": self.detail,
            "observation": self.observation,
        })
    }
}

/// Whether a command may already have produced side effects is independent of
/// its exit status. In particular, a timeout after spawn is not a read-only
/// run.
#[derive(Debug, Clone, Copy)]
pub struct CommandFailure {
    /// Whether the child process was started.
    pub started: bool,
    /// Stable, non-sensitive reason.
    pub detail: &'static str,
}

impl CommandFailure {
    /// A failure that happened before any child was started.
    #[must_use]
    pub fn before_start(detail: &'static str) -> Self {
        Self {
            started: false,
            detail,
        }
    }

    /// A failure that happened after a child was started; side effects are
    /// possible and must not be reported as a clean refusal.
    #[must_use]
    pub fn after_start(detail: &'static str) -> Self {
        Self {
            started: true,
            detail,
        }
    }
}

/// Whether a token contains a character a shell would interpret.
#[must_use]
pub fn has_shell_metacharacter(token: &str) -> bool {
    token.chars().any(|ch| {
        matches!(
            ch,
            ';' | '|' | '&' | '>' | '<' | '`' | '$' | '\n' | '\r' | '\0'
        )
    })
}

/// Run the installed binary, never another `cass` selected by PATH. The shared
/// supervisor bounds both captured streams and terminates timed-out children.
pub fn run_bounded_cass(argv: &[String], timeout: Duration) -> Result<Output, CommandFailure> {
    run_bounded_cass_with_env(argv, timeout, &[])
}

/// Run the installed binary with extra environment overrides.
///
/// The overrides are the only way a nested command can be pointed at the
/// selected data dir when its own surface has no data-dir flag.
pub fn run_bounded_cass_with_env(
    argv: &[String],
    timeout: Duration,
    env: &[(&str, &str)],
) -> Result<Output, CommandFailure> {
    let args = argv
        .get(1..)
        .filter(|args| !args.is_empty())
        .ok_or_else(|| CommandFailure::before_start("missing argv"))?;
    if argv.first().map(String::as_str) != Some("cass") || timeout.is_zero() {
        return Err(CommandFailure::before_start(
            "invalid command or exhausted command budget",
        ));
    }
    #[cfg(target_os = "linux")]
    let mut child = Command::new("/proc/self/exe");
    #[cfg(not(target_os = "linux"))]
    let mut child =
        Command::new(std::env::current_exe().map_err(|_| {
            CommandFailure::before_start("cannot resolve the running cass executable")
        })?);
    child
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CASS_AUTO_REFRESH", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    for (key, value) in env {
        child.env(key, value);
    }
    crate::sources::configure_child_process_group(&mut child);
    let child = child
        .spawn()
        .map_err(|_| CommandFailure::before_start("could not start cass command"))?;
    crate::sources::wait_for_child_output_with_limit(child, timeout, Some(2 * 1024 * 1024))
        .map_err(|_| CommandFailure::after_start("cass command failed to capture bounded output"))?
        .ok_or_else(|| CommandFailure::after_start("cass command deadline exceeded"))
}

/// Replace the selected data-dir prefix with a stable placeholder so a receipt
/// never carries an absolute path.
fn redact_data_dir(text: &str, data_dir: &Path) -> String {
    let prefix = data_dir.display().to_string();
    if prefix.is_empty() {
        return text.to_string();
    }
    text.replace(&prefix, "<data-dir>")
}

/// A closed argv builder: every token is checked for shell metacharacters
/// before the command may run.
fn closed_argv(tokens: &[&str]) -> Option<Vec<String>> {
    let argv = tokens
        .iter()
        .map(|token| (*token).to_string())
        .collect::<Vec<_>>();
    if argv.iter().any(|token| has_shell_metacharacter(token)) {
        return None;
    }
    Some(argv)
}

/// The read-only doctor repair-plan command for a data dir.
fn doctor_repair_preview_argv(data_dir: &Path) -> Option<Vec<String>> {
    closed_argv(&[
        "cass",
        "doctor",
        "--repair",
        "--dry-run",
        "--json",
        "--data-dir",
        &data_dir.display().to_string(),
    ])
}

/// The fingerprint-guarded doctor repair apply command for a data dir.
fn doctor_repair_apply_argv(data_dir: &Path, fingerprint: &str) -> Option<Vec<String>> {
    closed_argv(&[
        "cass",
        "doctor",
        "--repair",
        "--yes",
        "--plan-fingerprint",
        fingerprint,
        "--json",
        "--data-dir",
        &data_dir.display().to_string(),
    ])
}

/// The read-only status command used to observe index preconditions.
fn status_argv(data_dir: &Path) -> Option<Vec<String>> {
    closed_argv(&[
        "cass",
        "status",
        "--data-dir",
        &data_dir.display().to_string(),
        "--json",
    ])
}

/// The incremental lexical index command. Never `--full`, never `--semantic`:
/// the adapter must not download a model or rebuild the whole corpus.
fn index_apply_argv(data_dir: &Path) -> Option<Vec<String>> {
    closed_argv(&[
        "cass",
        "index",
        "--data-dir",
        &data_dir.display().to_string(),
        "--json",
    ])
}

/// The read-only doctor truth command used to verify a repair.
fn doctor_check_argv(data_dir: &Path) -> Option<Vec<String>> {
    closed_argv(&[
        "cass",
        "doctor",
        "--check",
        "--json",
        "--data-dir",
        &data_dir.display().to_string(),
    ])
}

/// The read-only remote source dry-sync command.
///
/// The source sync surface has no `--data-dir` flag of its own: it resolves
/// the data dir from `CASS_DATA_DIR`, which the adapter sets for the child.
fn sources_dry_sync_argv() -> Option<Vec<String>> {
    closed_argv(&["cass", "sources", "sync", "--dry-run", "--json"])
}

/// Hash a JSON value with sorted object keys. `serde_json` keeps object keys
/// in a `BTreeMap` here, so the serialized form is already key-sorted.
fn sorted_json_fingerprint(value: &Value) -> String {
    let encoded = serde_json::to_vec(value).unwrap_or_default();
    blake3::hash(&encoded).to_hex().to_string()
}

/// Project the index preconditions an incremental index depends on.
fn index_precondition_facts(status: &Value) -> Value {
    let index = status.get("index").cloned().unwrap_or(Value::Null);
    json!({
        "operation": OP_INDEX_INCREMENTAL,
        "index_status": index.get("status").cloned().unwrap_or(Value::Null),
        "index_fresh": index.get("fresh").cloned().unwrap_or(Value::Null),
        "index_documents": index.get("documents").cloned().unwrap_or(Value::Null),
        "index_partial": index.get("partial").cloned().unwrap_or(Value::Null),
        "current_db_fingerprint": index
            .pointer("/fingerprint/current_db_fingerprint")
            .cloned()
            .unwrap_or(Value::Null),
        "checkpoint_fingerprint": index
            .pointer("/fingerprint/checkpoint_fingerprint")
            .cloned()
            .unwrap_or(Value::Null),
        "matches_current_db_fingerprint": index
            .pointer("/fingerprint/matches_current_db_fingerprint")
            .cloned()
            .unwrap_or(Value::Null),
        "checkpoint_completed": index
            .pointer("/checkpoint/completed")
            .cloned()
            .unwrap_or(Value::Null),
        "checkpoint_db_matches": index
            .pointer("/checkpoint/db_matches")
            .cloned()
            .unwrap_or(Value::Null),
        "checkpoint_schema_matches": index
            .pointer("/checkpoint/schema_matches")
            .cloned()
            .unwrap_or(Value::Null),
    })
}

/// A read-only status observation reduced to typed, non-path measurements.
fn index_observation(status: &Value) -> Value {
    let index = status.get("index").cloned().unwrap_or(Value::Null);
    json!({
        "index_status": index.get("status").cloned().unwrap_or(Value::Null),
        "index_fresh": index.get("fresh").cloned().unwrap_or(Value::Null),
        "index_documents": index.get("documents").cloned().unwrap_or(Value::Null),
        "index_hollow": index.get("hollow").cloned().unwrap_or(Value::Null),
        "index_partial": index.get("partial").cloned().unwrap_or(Value::Null),
        "rebuild_active": status
            .pointer("/rebuild_progress/active")
            .cloned()
            .unwrap_or(Value::Null),
    })
}

/// The typed preview of the incremental index adapter.
fn index_preview(data_dir: &Path, deadline: Option<Instant>) -> OperationPreview {
    let apply_argv = index_apply_argv(data_dir);
    let scope = json!({
        "kind": "lexical-index",
        "data_dir": "<data-dir>",
        "mode": "incremental",
        "full_rebuild": false,
        "semantic_embedding": false,
        "model_download": false,
        "deletes_source_evidence": false,
    });
    let postconditions = json!([
        "cass status reports index.status == \"ready\"",
        "the published generation is fresh against the current archive fingerprint",
        "no source or archive bytes were deleted"
    ]);
    let compensation = json!({
        "state": "not-applicable",
        "kind": "derived-index-generation",
        "reason": "an incremental index publishes a new derived generation and never removes source evidence, so there is nothing for the guide runner to roll back",
        "executed": false
    });
    let Some(deadline) = deadline else {
        return OperationPreview {
            operation: OP_INDEX_INCREMENTAL.to_string(),
            status: "available".to_string(),
            preview_argv: status_argv(data_dir),
            apply_argv,
            scope,
            postconditions,
            compensation,
            identity: None,
            observation: None,
            reason: None,
            detail: "static preview; the live precondition identity is computed from the read-only status surface".to_string(),
        };
    };
    let Some(argv) = status_argv(data_dir) else {
        return unavailable_preview(
            OP_INDEX_INCREMENTAL,
            REASON_NO_CLOSED_COMMAND_PATH,
            "no safe argv for the read-only index precondition probe",
        );
    };
    let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
        return unavailable_preview(
            OP_INDEX_INCREMENTAL,
            REASON_NO_CLOSED_COMMAND_PATH,
            "shared adapter preview deadline exhausted; no child started",
        );
    };
    let output = match run_bounded_cass(&argv, timeout.min(PREVIEW_TIMEOUT)) {
        Ok(output) => output,
        Err(error) => {
            return unavailable_preview(
                OP_INDEX_INCREMENTAL,
                REASON_NO_CLOSED_COMMAND_PATH,
                error.detail,
            );
        }
    };
    let status: Value = match serde_json::from_slice(&output.stdout) {
        Ok(status) => status,
        Err(_) => {
            return unavailable_preview(
                OP_INDEX_INCREMENTAL,
                REASON_NO_CLOSED_COMMAND_PATH,
                "the read-only index precondition probe did not return JSON",
            );
        }
    };
    let facts = index_precondition_facts(&status);
    let identity = PlanIdentity {
        algorithm: IDENTITY_ALGORITHM.to_string(),
        value: sorted_json_fingerprint(&facts),
        source: "guide-precondition-fingerprint".to_string(),
    };
    OperationPreview {
        operation: OP_INDEX_INCREMENTAL.to_string(),
        status: "available".to_string(),
        preview_argv: Some(argv),
        apply_argv,
        scope,
        postconditions,
        compensation,
        identity: Some(identity),
        observation: Some(index_observation(&status)),
        reason: None,
        detail: "incremental lexical index over the selected data dir; the identity binds the apply to the observed index preconditions".to_string(),
    }
}

/// The typed preview of the doctor derived-asset repair adapter.
fn doctor_rebuild_preview(data_dir: &Path, deadline: Option<Instant>) -> OperationPreview {
    let scope = json!({
        "kind": "derived-assets",
        "data_dir": "<data-dir>",
        "rebuilds_derived_assets_only": true,
        "replaces_canonical_archive": false,
        "deletes_source_evidence": false,
    });
    let postconditions = json!([
        "cass doctor --check reports no failing derived-asset check",
        "the canonical archive database is unchanged"
    ]);
    let compensation = json!({
        "state": "not-executed",
        "kind": "doctor-run-undo",
        "reason": "a failed doctor apply leaves the doctor run journal and any partial derived assets in place; `cass doctor undo <run-id>` is the operator recovery path and the guide runner never executes it",
        "command": "cass doctor undo latest",
        "executed": false
    });
    let Some(deadline) = deadline else {
        return OperationPreview {
            operation: OP_DOCTOR_REBUILD.to_string(),
            status: "available".to_string(),
            preview_argv: doctor_repair_preview_argv(data_dir),
            apply_argv: None,
            scope,
            postconditions,
            compensation,
            identity: None,
            observation: None,
            reason: None,
            detail: "static preview; the concrete action set and the plan identity come from the read-only doctor repair plan".to_string(),
        };
    };
    let Some(argv) = doctor_repair_preview_argv(data_dir) else {
        return unavailable_preview(
            OP_DOCTOR_REBUILD,
            REASON_NO_CLOSED_COMMAND_PATH,
            "no safe argv for the read-only doctor repair plan",
        );
    };
    let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
        return unavailable_preview(
            OP_DOCTOR_REBUILD,
            REASON_NO_CLOSED_COMMAND_PATH,
            "shared adapter preview deadline exhausted; no child started",
        );
    };
    let output = match run_bounded_cass(&argv, timeout.min(PREVIEW_TIMEOUT)) {
        Ok(output) => output,
        Err(error) => {
            return unavailable_preview(
                OP_DOCTOR_REBUILD,
                REASON_NO_CLOSED_COMMAND_PATH,
                error.detail,
            );
        }
    };
    let payload: Value = match serde_json::from_slice(&output.stdout) {
        Ok(payload) => payload,
        Err(_) => {
            return unavailable_preview(
                OP_DOCTOR_REBUILD,
                REASON_NO_CLOSED_COMMAND_PATH,
                "the read-only doctor repair plan did not return JSON",
            );
        }
    };
    let Some(plan) = payload.get("repair_plan") else {
        return unavailable_preview(
            OP_DOCTOR_REBUILD,
            REASON_NO_CLOSED_COMMAND_PATH,
            "the doctor repair plan did not report a typed repair_plan block",
        );
    };
    let Some(fingerprint) = plan.get("plan_fingerprint").and_then(Value::as_str) else {
        return unavailable_preview(
            OP_DOCTOR_REBUILD,
            REASON_NO_CLOSED_COMMAND_PATH,
            "the doctor repair plan did not report a plan fingerprint",
        );
    };
    let actions = plan
        .get("planned_actions")
        .and_then(Value::as_array)
        .map(|actions| {
            actions
                .iter()
                .map(|action| {
                    json!({
                        "action_kind": action.get("action_kind").cloned().unwrap_or(Value::Null),
                        "asset_class": action.get("asset_class").cloned().unwrap_or(Value::Null),
                        "authority_decision": action.get("authority_decision").cloned().unwrap_or(Value::Null),
                        "planned_bytes": action.get("planned_bytes").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let blocked_reasons = plan
        .get("blocked_reasons")
        .and_then(Value::as_array)
        .map(|reasons| {
            reasons
                .iter()
                .filter_map(Value::as_str)
                .map(|reason| redact_data_dir(reason, data_dir))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let planned_action_count = plan
        .get("planned_action_count")
        .and_then(Value::as_u64)
        .unwrap_or(actions.len() as u64);
    let identity = PlanIdentity {
        algorithm: plan
            .get("fingerprint_algorithm")
            .and_then(Value::as_str)
            .unwrap_or("blake3-canonical-json-v1")
            .to_string(),
        value: fingerprint.to_string(),
        source: "doctor-plan-fingerprint".to_string(),
    };
    let scope = json!({
        "kind": "derived-assets",
        "data_dir": "<data-dir>",
        "rebuilds_derived_assets_only": true,
        "replaces_canonical_archive": false,
        "deletes_source_evidence": !plan
            .get("never_prunes_source_evidence")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        "planned_action_count": planned_action_count,
        "planned_actions": actions,
        "blocked_reasons": blocked_reasons,
        "post_repair_probes_required": plan
            .get("post_repair_probes_required")
            .cloned()
            .unwrap_or(Value::Null),
    });
    let apply_argv = doctor_repair_apply_argv(data_dir, fingerprint);
    OperationPreview {
        operation: OP_DOCTOR_REBUILD.to_string(),
        status: "available".to_string(),
        preview_argv: Some(argv),
        apply_argv,
        scope,
        postconditions,
        compensation,
        identity: Some(identity),
        observation: Some(json!({
            "plan_kind": plan.get("plan_kind").cloned().unwrap_or(Value::Null),
            "mode": plan.get("mode").cloned().unwrap_or(Value::Null),
            "will_mutate": plan.get("will_mutate").cloned().unwrap_or(Value::Null),
            "apply_authorized": plan.get("apply_authorized").cloned().unwrap_or(Value::Null),
        })),
        reason: None,
        detail: "doctor repair plan over the selected data dir; the apply is bound to the plan fingerprint the operator inspected".to_string(),
    }
}

/// The typed preview of the support-capsule adapter.
fn support_capsule_preview(data_dir: &Path) -> OperationPreview {
    OperationPreview {
        operation: OP_SUPPORT_CAPSULE.to_string(),
        status: "available".to_string(),
        preview_argv: None,
        apply_argv: closed_argv(&[
            "cass",
            "doctor",
            "--support-bundle",
            "--data-dir",
            &data_dir.display().to_string(),
            "--json",
        ]),
        scope: json!({
            "kind": "support-bundle",
            "root": "<data-dir>/doctor/support",
            "contains_session_text": false,
            "contains_secrets": false,
            "deletes_source_evidence": false,
        }),
        postconditions: json!([
            "the bundle manifest exists inside the selected data dir",
            "the manifest is a readable regular file"
        ]),
        compensation: json!({
            "state": "not-executed",
            "kind": "derived-artifact",
            "reason": "a produced capsule is a new bundle file; the guide runner never deletes bundle files, so a failure leaves the partial bundle in place for inspection",
            "executed": false
        }),
        identity: None,
        observation: None,
        reason: None,
        detail: "redacted support bundle under the selected data dir; this adapter has no fingerprinted plan, so the explicit step confirmation is its approval".to_string(),
    }
}

/// A typed preview for an adapter that cannot be closed.
fn unavailable_preview(operation: &str, reason: &str, detail: &str) -> OperationPreview {
    OperationPreview {
        operation: operation.to_string(),
        status: "unavailable".to_string(),
        preview_argv: None,
        apply_argv: None,
        scope: json!({"kind": "unavailable"}),
        postconditions: json!([]),
        compensation: json!({
            "state": "not-executed",
            "kind": "none",
            "reason": "no adapter ran, so nothing was applied and nothing needs compensating",
            "executed": false
        }),
        identity: None,
        observation: None,
        reason: Some(reason.to_string()),
        detail: detail.to_string(),
    }
}

/// The stable reason a structured command has no closed adapter.
#[must_use]
pub fn unavailable_reason(operation: &str) -> &'static str {
    match operation {
        "verify.rerun-gate" => REASON_NO_CLOSED_COMMAND_PATH,
        "export.encrypted-html" => REASON_REQUIRES_OPERATOR_SCOPE,
        _ => REASON_NO_CLOSED_COMMAND_PATH,
    }
}

/// Build the typed preview for one structured workflow command.
///
/// `deadline` is `None` for a static preview (no child process is started),
/// which is what fixture mode and other offline contexts use.
#[must_use]
pub fn preview_operation(
    operation: &str,
    data_dir: &Path,
    deadline: Option<Instant>,
) -> OperationPreview {
    match operation {
        OP_DOCTOR_REBUILD => doctor_rebuild_preview(data_dir, deadline),
        OP_INDEX_INCREMENTAL => index_preview(data_dir, deadline),
        OP_SUPPORT_CAPSULE => support_capsule_preview(data_dir),
        other => unavailable_preview(
            other,
            unavailable_reason(other),
            "no closed, parameter-complete adapter exists for this structured command",
        ),
    }
}

/// Whether the preview's own plan is blocked and must not be applied.
fn plan_is_blocked(preview: &OperationPreview) -> bool {
    preview
        .scope
        .get("blocked_reasons")
        .and_then(Value::as_array)
        .is_some_and(|reasons| !reasons.is_empty())
}

/// Apply one structured workflow command through its closed adapter.
///
/// `expected_identity` is the identity the operator approved. When the
/// adapter has an identity, the apply is refused unless the operator's
/// approval matches the identity observed now: a stale plan identity is a
/// refusal, never a silent re-plan.
pub fn apply_operation(
    operation: &str,
    preview: &OperationPreview,
    expected_identity: Option<&str>,
    deadline: Instant,
) -> ApplyOutcome {
    if !preview.is_available() {
        return ApplyOutcome {
            status: "adapter-unavailable".to_string(),
            phase: "not-run".to_string(),
            started: false,
            detail: preview
                .reason
                .clone()
                .unwrap_or_else(|| "no closed adapter for this operation".to_string()),
            observation: None,
        };
    }
    if let Some(identity) = preview.identity.as_ref() {
        match expected_identity {
            None => {
                return ApplyOutcome {
                    status: "identity-missing".to_string(),
                    phase: "not-run".to_string(),
                    started: false,
                    detail: "the operator approval is not bound to a plan identity; re-run the dry run and pass --expect-plan-identity for this step".to_string(),
                    observation: Some(json!({
                        "identity_algorithm": identity.algorithm,
                        "identity_source": identity.source,
                    })),
                };
            }
            Some(expected) if expected != identity.value => {
                return ApplyOutcome {
                    status: "identity-mismatch".to_string(),
                    phase: "not-run".to_string(),
                    started: false,
                    detail: "the approved plan identity does not match the plan observed now; the plan is stale and was not applied".to_string(),
                    observation: Some(json!({
                        "identity_algorithm": identity.algorithm,
                        "identity_source": identity.source,
                        "approved_identity_present": true,
                    })),
                };
            }
            Some(_) => {}
        }
    }
    if plan_is_blocked(preview) {
        return ApplyOutcome {
            status: "plan-blocked".to_string(),
            phase: "not-run".to_string(),
            started: false,
            detail: "the read-only plan reports blocked reasons; the operation scope was rejected before any child started".to_string(),
            observation: Some(json!({
                "blocked_reason_count": preview
                    .scope
                    .get("blocked_reasons")
                    .and_then(Value::as_array)
                    .map(Vec::len),
            })),
        };
    }
    let Some(argv) = preview.apply_argv.clone() else {
        return ApplyOutcome {
            status: "adapter-unavailable".to_string(),
            phase: "not-run".to_string(),
            started: false,
            detail: "the adapter has no complete apply argv".to_string(),
            observation: None,
        };
    };
    let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
        return ApplyOutcome {
            status: "failed".to_string(),
            phase: "not-run".to_string(),
            started: false,
            detail: "shared adapter apply deadline exhausted; no child started".to_string(),
            observation: None,
        };
    };
    let output = match run_bounded_cass(&argv, timeout.min(MUTATION_TIMEOUT)) {
        Ok(output) => output,
        Err(error) => {
            return ApplyOutcome {
                status: "failed".to_string(),
                phase: "failed".to_string(),
                started: error.started,
                detail: error.detail.to_string(),
                observation: None,
            };
        }
    };
    classify_applied_output(
        operation,
        output.status.code(),
        &output.stdout,
        &output.stderr,
    )
}

/// Classify a finished apply command. Only typed measurements are kept: the
/// exit code and a machine-readable error kind, never a message that could
/// carry a path or source text.
fn classify_applied_output(
    operation: &str,
    exit_code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
) -> ApplyOutcome {
    if exit_code != Some(0) {
        let error_kind = typed_error_kind(stderr);
        return ApplyOutcome {
            status: "failed".to_string(),
            phase: "failed".to_string(),
            started: true,
            detail: match error_kind.as_deref() {
                Some("index-busy") => {
                    "another index run holds the data dir lock; the operation was not applied"
                        .to_string()
                }
                Some(_) => {
                    "the operation command reported a typed error envelope; it was not applied"
                        .to_string()
                }
                None => {
                    "the operation command did not exit successfully; partial effects may remain"
                        .to_string()
                }
            },
            observation: Some(json!({
                "exit_code": exit_code,
                "error_kind": error_kind,
            })),
        };
    }
    let observation = match operation {
        OP_INDEX_INCREMENTAL => serde_json::from_slice::<Value>(stdout)
            .ok()
            .map(|payload| {
                json!({
                    "indexed": payload.get("indexed").cloned().unwrap_or(Value::Null),
                    "conversations": payload.get("conversations").cloned().unwrap_or(Value::Null),
                    "messages": payload.get("messages").cloned().unwrap_or(Value::Null),
                    "exit_code": exit_code,
                })
            })
            .unwrap_or_else(|| json!({"exit_code": exit_code})),
        OP_DOCTOR_REBUILD => serde_json::from_slice::<Value>(stdout)
            .ok()
            .map(|payload| {
                let plan = payload.get("repair_plan");
                json!({
                    "plan_fingerprint": plan
                        .and_then(|plan| plan.get("plan_fingerprint"))
                        .cloned()
                        .unwrap_or(Value::Null),
                    "applied_action_count": plan
                        .and_then(|plan| plan.get("planned_action_count"))
                        .cloned()
                        .unwrap_or(Value::Null),
                    "will_mutate": plan
                        .and_then(|plan| plan.get("will_mutate"))
                        .cloned()
                        .unwrap_or(Value::Null),
                    "exit_code": exit_code,
                })
            })
            .unwrap_or_else(|| json!({"exit_code": exit_code})),
        _ => json!({"exit_code": exit_code}),
    };
    ApplyOutcome {
        status: "executed".to_string(),
        phase: "applied".to_string(),
        started: true,
        detail: "the operation command completed successfully".to_string(),
        observation: Some(observation),
    }
}

/// Extract a machine-readable error kind from a command's stderr without
/// keeping the message, which can carry a path.
fn typed_error_kind(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    for line in text.lines().rev() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(kind) = payload.pointer("/error/kind").and_then(Value::as_str) {
            return Some(kind.to_string());
        }
    }
    None
}

/// Verify an applied operation against its typed postconditions.
pub fn verify_operation(
    operation: &str,
    data_dir: &Path,
    deadline: Instant,
) -> VerificationOutcome {
    match operation {
        OP_DOCTOR_REBUILD => verify_doctor_rebuild(data_dir, deadline),
        OP_INDEX_INCREMENTAL => verify_index_incremental(data_dir, deadline),
        _ => VerificationOutcome {
            result: "not-run".to_string(),
            source: "none".to_string(),
            detail: "this adapter declares no verification surface".to_string(),
            observation: None,
        },
    }
}

/// Verify a doctor repair: the read-only doctor truth surface must report no
/// failing derived-asset check.
fn verify_doctor_rebuild(data_dir: &Path, deadline: Instant) -> VerificationOutcome {
    let Some(argv) = doctor_check_argv(data_dir) else {
        return VerificationOutcome {
            result: "failed".to_string(),
            source: "doctor-check".to_string(),
            detail: "no safe argv for the read-only doctor verification".to_string(),
            observation: None,
        };
    };
    let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
        return VerificationOutcome {
            result: "not-run".to_string(),
            source: "doctor-check".to_string(),
            detail: "shared verification deadline exhausted; no child started".to_string(),
            observation: None,
        };
    };
    let output = match run_bounded_cass(&argv, timeout.min(VERIFY_TIMEOUT)) {
        Ok(output) => output,
        Err(error) => {
            return VerificationOutcome {
                result: "failed".to_string(),
                source: "doctor-check".to_string(),
                detail: error.detail.to_string(),
                observation: None,
            };
        }
    };
    let exit_code = output.status.code();
    let payload: Value = match serde_json::from_slice(&output.stdout) {
        Ok(payload) => payload,
        Err(_) => {
            return VerificationOutcome {
                result: "failed".to_string(),
                source: "doctor-check".to_string(),
                detail: "the read-only doctor verification did not return JSON".to_string(),
                observation: Some(json!({"exit_code": exit_code})),
            };
        }
    };
    let checks = payload.get("checks").and_then(Value::as_array);
    let failing = checks
        .map(|checks| {
            checks
                .iter()
                .filter(|check| check.get("status").and_then(Value::as_str) == Some("fail"))
                .count()
        })
        .unwrap_or(0);
    let healthy = payload.get("healthy").and_then(Value::as_bool);
    let needs_rebuild = payload.get("needs_rebuild").and_then(Value::as_bool);
    let verified = healthy == Some(true) && failing == 0 && needs_rebuild != Some(true);
    VerificationOutcome {
        result: if verified {
            "verified".to_string()
        } else {
            "not-verified".to_string()
        },
        source: "doctor-check".to_string(),
        detail: if verified {
            "the read-only doctor truth surface reports a healthy archive with no failing check"
                .to_string()
        } else {
            "the read-only doctor truth surface did not confirm the repaired state".to_string()
        },
        observation: Some(json!({
            "exit_code": exit_code,
            "healthy": healthy,
            "failing_check_count": failing,
            "needs_rebuild": needs_rebuild,
        })),
    }
}

/// Verify an incremental index: the read-only status surface must report a
/// ready, fresh index.
fn verify_index_incremental(data_dir: &Path, deadline: Instant) -> VerificationOutcome {
    let Some(argv) = status_argv(data_dir) else {
        return VerificationOutcome {
            result: "failed".to_string(),
            source: "status".to_string(),
            detail: "no safe argv for the read-only index verification".to_string(),
            observation: None,
        };
    };
    let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
        return VerificationOutcome {
            result: "not-run".to_string(),
            source: "status".to_string(),
            detail: "shared verification deadline exhausted; no child started".to_string(),
            observation: None,
        };
    };
    let output = match run_bounded_cass(&argv, timeout.min(VERIFY_TIMEOUT)) {
        Ok(output) => output,
        Err(error) => {
            return VerificationOutcome {
                result: "failed".to_string(),
                source: "status".to_string(),
                detail: error.detail.to_string(),
                observation: None,
            };
        }
    };
    let exit_code = output.status.code();
    let payload: Value = match serde_json::from_slice(&output.stdout) {
        Ok(payload) => payload,
        Err(_) => {
            return VerificationOutcome {
                result: "failed".to_string(),
                source: "status".to_string(),
                detail: "the read-only index verification did not return JSON".to_string(),
                observation: Some(json!({"exit_code": exit_code})),
            };
        }
    };
    let mut observation = index_observation(&payload);
    let index_status = payload
        .pointer("/index/status")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let fresh = payload.pointer("/index/fresh").and_then(Value::as_bool);
    let verified = index_status == "ready" && fresh == Some(true);
    if let Some(map) = observation.as_object_mut() {
        map.insert("exit_code".to_string(), json!(exit_code));
        map.insert("index_status".to_string(), json!(index_status));
        map.insert("index_fresh".to_string(), json!(fresh));
    }
    VerificationOutcome {
        result: if verified {
            "verified".to_string()
        } else {
            "not-verified".to_string()
        },
        source: "status".to_string(),
        detail: if verified {
            "the read-only status surface reports a ready, fresh index".to_string()
        } else {
            "the read-only status surface did not confirm a ready, fresh index".to_string()
        },
        observation: Some(observation),
    }
}

/// Observe the read-only remote source dry-sync as a typed proof.
///
/// The dry sync is the read-only half of the remote source path: it reports
/// what a sync would transfer without transferring anything. A run with no
/// configured remote source is a typed "nothing to transfer" observation, not
/// a silent pass and not a failure.
pub fn observe_sources_dry_sync(data_dir: &Path, deadline: Instant) -> Value {
    let Some(argv) = sources_dry_sync_argv() else {
        return json!({
            "result": "not-run",
            "source": "sources-dry-sync",
            "detail": "no safe argv for the read-only source dry sync",
        });
    };
    let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
        return json!({
            "result": "not-run",
            "source": "sources-dry-sync",
            "detail": "shared proof deadline exhausted; no child started",
        });
    };
    let data_dir_value = data_dir.display().to_string();
    let output = match run_bounded_cass_with_env(
        &argv,
        timeout.min(PREVIEW_TIMEOUT),
        &[("CASS_DATA_DIR", data_dir_value.as_str())],
    ) {
        Ok(output) => output,
        Err(error) => {
            return json!({
                "result": "not-run",
                "source": "sources-dry-sync",
                "detail": error.detail,
            });
        }
    };
    let exit_code = output.status.code();
    if exit_code != Some(0) {
        return json!({
            "result": "failed",
            "source": "sources-dry-sync",
            "detail": "the read-only source dry sync did not exit successfully",
            "observation": {"exit_code": exit_code, "error_kind": typed_error_kind(&output.stderr)},
        });
    }
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    let status = payload
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let detail = match status.as_str() {
        "no_sources" => {
            "no remote source is configured, so the dry sync had nothing to transfer".to_string()
        }
        "no_match" => {
            "the source filter matched no configured remote source, so nothing would transfer"
                .to_string()
        }
        _ => "the read-only source dry sync completed and reported what it would transfer"
            .to_string(),
    };
    json!({
        "result": "passed",
        "source": "sources-dry-sync",
        "detail": detail,
        "observation": {"exit_code": exit_code, "sync_status": status},
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::{
        IDENTITY_ALGORITHM, OP_DOCTOR_REBUILD, OP_INDEX_INCREMENTAL, OP_SUPPORT_CAPSULE,
        REASON_REQUIRES_OPERATOR_SCOPE, apply_operation, has_shell_metacharacter,
        index_precondition_facts, preview_operation, sorted_json_fingerprint, typed_error_kind,
    };

    fn preview(operation: &str) -> super::OperationPreview {
        preview_operation(operation, Path::new("/tmp/cass-guide-adapter-test"), None)
    }

    #[test]
    fn enabled_adapters_expose_a_typed_scope_and_a_non_executed_compensation() {
        for operation in [OP_DOCTOR_REBUILD, OP_INDEX_INCREMENTAL, OP_SUPPORT_CAPSULE] {
            let preview = preview(operation);
            assert_eq!(preview.status, "available", "{operation}");
            assert_eq!(preview.operation, operation);
            assert!(
                preview
                    .scope
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    .is_some(),
                "{operation} must name its operation scope"
            );
            assert!(
                !preview
                    .postconditions
                    .as_array()
                    .map(Vec::is_empty)
                    .unwrap_or(true),
                "{operation} must declare its postconditions"
            );
            assert_eq!(
                preview
                    .compensation
                    .get("executed")
                    .and_then(serde_json::Value::as_bool),
                Some(false),
                "{operation} must never claim an executed rollback"
            );
        }
    }

    #[test]
    fn unsupported_adapters_stay_unavailable_with_a_stable_reason() {
        let preview = preview("export.encrypted-html");
        assert_eq!(preview.status, "unavailable");
        assert_eq!(
            preview.reason.as_deref(),
            Some(REASON_REQUIRES_OPERATOR_SCOPE)
        );
        assert!(preview.apply_argv.is_none());
    }

    #[test]
    fn the_index_identity_is_bound_to_the_observed_preconditions() {
        let facts = index_precondition_facts(&json!({
            "index": {"status": "ready", "fresh": true, "documents": 3,
                "fingerprint": {"current_db_fingerprint": "abc",
                    "checkpoint_fingerprint": "abc",
                    "matches_current_db_fingerprint": true},
                "checkpoint": {"completed": true, "db_matches": true, "schema_matches": true}}
        }));
        let first = sorted_json_fingerprint(&facts);
        let mut changed = facts.clone();
        changed["index_documents"] = json!(4);
        assert_ne!(first, sorted_json_fingerprint(&changed));
        assert_eq!(IDENTITY_ALGORITHM, "blake3-sorted-json-v1");
    }

    #[test]
    fn a_missing_or_stale_approval_is_refused_before_any_child_starts() {
        let preview = preview(OP_INDEX_INCREMENTAL);
        let with_identity = super::OperationPreview {
            identity: Some(super::PlanIdentity {
                algorithm: IDENTITY_ALGORITHM.to_string(),
                value: "observed".to_string(),
                source: "guide-precondition-fingerprint".to_string(),
            }),
            ..preview
        };
        let missing = apply_operation(
            OP_INDEX_INCREMENTAL,
            &with_identity,
            None,
            std::time::Instant::now(),
        );
        assert_eq!(missing.status, "identity-missing");
        assert!(!missing.started);
        let stale = apply_operation(
            OP_INDEX_INCREMENTAL,
            &with_identity,
            Some("approved-elsewhere"),
            std::time::Instant::now(),
        );
        assert_eq!(stale.status, "identity-mismatch");
        assert!(!stale.started);
    }

    #[test]
    fn an_unavailable_adapter_never_reports_an_applied_repair() {
        let outcome = apply_operation(
            "export.encrypted-html",
            &preview("export.encrypted-html"),
            None,
            std::time::Instant::now(),
        );
        assert_eq!(outcome.status, "adapter-unavailable");
        assert_eq!(outcome.phase, "not-run");
        assert!(!outcome.started);
    }

    #[test]
    fn shell_metacharacters_and_typed_error_kinds_are_handled() {
        assert!(has_shell_metacharacter("$(touch-nope)"));
        assert!(!has_shell_metacharacter("cass doctor --check"));
        let stderr =
            b"{\"error\":{\"code\":7,\"kind\":\"index-busy\",\"message\":\"/private/path\"}}";
        assert_eq!(
            typed_error_kind(stderr).as_deref(),
            Some("index-busy"),
            "a typed error kind must survive without the message"
        );
        assert!(typed_error_kind(b"not json").is_none());
    }

    #[test]
    fn a_failed_apply_is_reported_as_failed_after_it_started() {
        let stderr =
            b"{\"error\":{\"code\":7,\"kind\":\"index-busy\",\"message\":\"/private/path\"}}";
        let outcome = super::classify_applied_output(OP_INDEX_INCREMENTAL, Some(7), b"", stderr);
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.phase, "failed");
        assert!(outcome.started, "a failed apply may have left side effects");
        assert_eq!(
            outcome
                .observation
                .as_ref()
                .and_then(|o| o.get("error_kind"))
                .and_then(serde_json::Value::as_str),
            Some("index-busy")
        );
        assert!(
            !outcome.detail.contains("/private/path"),
            "a failure receipt must not carry a private path: {}",
            outcome.detail
        );
    }

    #[test]
    fn a_successful_apply_is_applied_and_is_not_yet_verified() {
        let outcome = super::classify_applied_output(OP_INDEX_INCREMENTAL, Some(0), b"{}", b"");
        assert_eq!(outcome.status, "executed");
        assert_eq!(outcome.phase, "applied");
        assert!(outcome.started);
    }

    #[test]
    fn an_exhausted_deadline_never_starts_a_child() {
        let preview = preview(OP_INDEX_INCREMENTAL);
        let outcome = apply_operation(
            OP_INDEX_INCREMENTAL,
            &preview,
            None,
            std::time::Instant::now() - std::time::Duration::from_secs(1),
        );
        assert_eq!(outcome.status, "failed");
        assert!(
            !outcome.started,
            "an exhausted deadline must not start a child"
        );
    }
}
