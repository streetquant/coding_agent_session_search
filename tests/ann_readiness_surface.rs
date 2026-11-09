//! Fresh-process E2E for the shared ANN readiness surface (wfm4e).
//!
//! The readiness surfaces (status, health, doctor, TUI) and the serving path
//! (an approximate semantic search) must agree about one cohort on disk. Every
//! case here runs the real `cass` binary against a real indexed archive in an
//! isolated HOME: nothing is mocked, both sides read the same bytes, and the
//! test proves that the state token, the reason code and the artifacts agree.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{SecondsFormat, Utc};
use coding_agent_search::indexer::semantic::HASH_VECTOR_SPACE_REVISION;
use frankensearch::index::{HnswConfig, HnswIndex as FsHnswIndex, VectorIndex};
use serde_json::json;

mod util;
use util::e2e_log::{E2eCommandEnvironment, PhaseTracker};

const HASH_EMBEDDER_ID: &str = "fnv1a-384";
const HASH_DIMENSION: usize = 384;
const SESSION_COUNT: usize = 6;
const QUERY: &str = "readiness corpus";

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("ann_readiness_surface", test_name)
}

fn codex_iso_timestamp(ts_millis: u64) -> String {
    chrono::DateTime::<Utc>::from_timestamp_millis(ts_millis as i64)
        .expect("valid millis timestamp for codex fixture")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Create a Codex session fixture.
fn make_codex_session(root: &Path, date_path: &str, filename: &str, content: &str, ts: u64) {
    let sessions = root.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).expect("create session directory");
    let workspace = root.to_string_lossy();
    let lines = [
        json!({
            "timestamp": codex_iso_timestamp(ts),
            "type": "session_meta",
            "payload": {
                "id": filename,
                "cwd": workspace,
                "cli_version": "0.42.0"
            }
        }),
        json!({
            "timestamp": codex_iso_timestamp(ts + 1_000),
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": content }]
            }
        }),
        json!({
            "timestamp": codex_iso_timestamp(ts + 2_000),
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "text", "text": format!("{content}_response") }]
            }
        }),
    ];
    let mut sample = String::new();
    for line in lines {
        sample.push_str(&serde_json::to_string(&line).expect("serialize session line"));
        sample.push('\n');
    }
    // Publish the fixture atomically: a reader never sees a half-written
    // transcript, exactly like the connector's own session writer.
    let target = sessions.join(filename);
    let staging = target.with_extension("jsonl.staging");
    fs::write(&staging, sample).expect("write session fixture");
    fs::rename(&staging, &target).expect("publish session fixture");
}

/// One real archive with a real exact FSVI and a real native ANN sidecar.
struct Corpus {
    _root: tempfile::TempDir,
    home: PathBuf,
    codex_home: PathBuf,
    data_dir: PathBuf,
}

impl Corpus {
    /// The isolated environment every `cass` invocation runs in.
    ///
    /// The machine-local connector roots are redirected into the temp home so
    /// the fixture indexes only its own sessions: a readiness test must not
    /// depend on whatever else lives on the host.
    fn env(&self, tracker: &PhaseTracker) -> E2eCommandEnvironment {
        tracker
            .command_environment()
            .with_home(&self.home)
            .with_codex_home(&self.codex_home)
            .with_var("XDG_DATA_HOME", self.home.join(".local/share"))
            .with_var("XDG_CONFIG_HOME", self.home.join(".config"))
            .with_var("CASS_IGNORE_SOURCES_CONFIG", "1")
            .with_var("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .with_var("CASS_SEMANTIC_EMBEDDER", "hash")
            .with_var("CASS_OPENWEBUI_DATA_ROOT", self.home.join("no-openwebui"))
            .with_var("CASS_CLOUDMCP_DATA_ROOT", self.home.join("no-cloudmcp"))
    }

    fn data_dir(&self) -> &str {
        self.data_dir.to_str().expect("utf-8 data dir")
    }

    /// The exact FSVI of the published hash generation.
    fn fsvi(&self) -> PathBuf {
        self.data_dir
            .join("vector_index")
            .join(format!("index-{HASH_EMBEDDER_ID}.fsvi"))
    }

    /// The ANN sidecar metadata of the published hash generation.
    fn sidecar(&self) -> PathBuf {
        self.data_dir
            .join("vector_index")
            .join(format!("hnsw-{HASH_EMBEDDER_ID}.chsw"))
    }
}

/// Index a real corpus with a real semantic generation and a real native
/// ANN sidecar.
fn build_corpus(tracker: &PhaseTracker) -> Corpus {
    let root = tempfile::tempdir().expect("corpus temp dir");
    let home = root.path().join("home");
    let codex_home = home.join(".codex");
    let data_dir = root.path().join("cass_data");
    fs::create_dir_all(&data_dir).expect("create data dir");
    for index in 0..SESSION_COUNT {
        make_codex_session(
            &codex_home,
            &format!("2024/11/{:02}", 15 + index),
            &format!("rollout-{index}.jsonl"),
            &format!("ann readiness corpus session {index} token{index}"),
            1_731_628_800_000 + (index as u64 * 86_400_000),
        );
    }
    let corpus = Corpus {
        _root: root,
        home,
        codex_home,
        data_dir,
    };
    corpus
        .env(tracker)
        .cass_assert_command()
        .args([
            "index",
            "--full",
            "--semantic",
            "--build-hnsw",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&corpus.data_dir)
        .assert()
        .success();
    // A fresh `cass index` seals a certified `vector_index/served/` copy and
    // records a coverage certificate. Certified owners deliberately serve
    // exact native vectors with *no* paired ANN sidecar, so the ANN surface
    // is only exercised by the canonical legacy-manifest cohort. Drop the
    // certificate exactly like the other real-binary ANN fixtures do; the
    // published fast-tier artifact and its sidecar stay untouched.
    let manifest_path = corpus.data_dir.join("vector_index/semantic_manifest.json");
    let bytes = fs::read(&manifest_path).expect("read semantic manifest");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&bytes).expect("parse semantic manifest");
    let coverage = manifest
        .as_object_mut()
        .expect("semantic manifest must be an object")
        .remove("coverage");
    assert!(
        coverage.is_some(),
        "a fresh semantic index must certify its served generation"
    );
    fs::write(
        &manifest_path,
        serde_json::to_vec(&manifest).expect("encode legacy semantic manifest"),
    )
    .expect("write legacy semantic manifest");
    corpus
}

/// Run `cass` in the isolated environment and parse its JSON payload.
///
/// The helper is fallible on purpose: a readiness fixture that cannot even
/// produce a JSON envelope is a fixture failure, not a reason to abort the
/// process from a panic. The error carries the whole command output so the
/// failure is still diagnosable.
fn cass_json(env: &E2eCommandEnvironment, args: &[&str]) -> Result<serde_json::Value, String> {
    let output = env
        .cass_assert_command()
        .args(args)
        .output()
        .map_err(|err| format!("cass {args:?} could not run: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        return Err(format!(
            "cass {args:?} failed ({}): {stdout}{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_str(&stdout)
        .map_err(|err| format!("cass {args:?} did not print JSON: {err}: {stdout}"))
}

/// Every file under `root` with its bytes, mtime and mode.
#[derive(Debug, PartialEq, Eq)]
struct ArtifactSnapshot {
    entries: Vec<(PathBuf, Vec<u8>, SystemTime, u32)>,
}

fn artifact_snapshot(root: &Path) -> ArtifactSnapshot {
    let mut entries = Vec::new();
    collect_snapshot(root, root, &mut entries);
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    ArtifactSnapshot { entries }
}

fn collect_snapshot(
    root: &Path,
    dir: &Path,
    entries: &mut Vec<(PathBuf, Vec<u8>, SystemTime, u32)>,
) {
    for entry in fs::read_dir(dir).expect("read directory") {
        let entry = entry.expect("read directory entry");
        let path = entry.path();
        let metadata = entry.metadata().expect("entry metadata");
        if metadata.is_dir() {
            collect_snapshot(root, &path, entries);
            continue;
        }
        #[cfg(unix)]
        let mode = std::os::unix::fs::PermissionsExt::mode(&metadata.permissions());
        #[cfg(not(unix))]
        let mode = 0;
        entries.push((
            path.strip_prefix(root)
                .expect("relative path")
                .to_path_buf(),
            fs::read(&path).expect("read file"),
            metadata.modified().expect("modified time"),
            mode,
        ));
    }
}

/// The ANN verdict every surface must agree on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Verdict {
    /// The shared state token.
    state: &'static str,
    /// The doctor component status.
    doctor_status: &'static str,
    /// Whether a real approximate search may use native ANN.
    native: bool,
    /// The stable reason code a refused cohort reports.
    reason_code: Option<&'static str>,
}

const NATIVE: Verdict = Verdict {
    state: "native_ready",
    doctor_status: "ready",
    native: true,
    reason_code: None,
};

const ABSENT: Verdict = Verdict {
    state: "absent",
    doctor_status: "not-present",
    native: false,
    reason_code: Some("ann_sidecar_missing"),
};

const REJECTED_OPEN_FAILED: Verdict = Verdict {
    state: "rejected",
    doctor_status: "rejected",
    native: false,
    reason_code: Some("ann_sidecar_open_failed"),
};

const REJECTED_NOT_NATIVE: Verdict = Verdict {
    state: "rejected",
    doctor_status: "rejected",
    native: false,
    reason_code: Some("ann_sidecar_not_native"),
};

/// Assert that the deep health, the doctor, the human surface and a real
/// approximate search all report the same ANN verdict for one cohort.
fn assert_surfaces_agree(
    corpus: &Corpus,
    tracker: &PhaseTracker,
    expected: &Verdict,
) -> Result<(), String> {
    let env = corpus.env(tracker);
    let data_dir = corpus.data_dir();

    // Deep health: the exact native admission against the selected owner.
    let health = cass_json(
        &env,
        &["health", "--json", "--deep", "--data-dir", data_dir],
    )?;
    let semantic = health
        .pointer("/state/semantic")
        .ok_or_else(|| format!("deep health has no semantic block: {health}"))?;
    assert_eq!(
        semantic["hnsw_state"].as_str(),
        Some(expected.state),
        "deep health state: {health}"
    );
    assert_eq!(
        semantic["hnsw_ready"].as_bool(),
        Some(expected.native),
        "deep health readiness: {health}"
    );
    assert_eq!(
        semantic["hnsw_reason_code"].as_str(),
        expected.reason_code,
        "deep health reason: {health}"
    );

    // Doctor: the same cohort, the same vocabulary, the same reason code.
    let doctor = cass_json(
        &env,
        &["doctor", "--json", "--deep", "--data-dir", data_dir],
    )?;
    let hnsw = doctor
        .pointer("/derived_semantic_assets/hnsw_index")
        .ok_or_else(|| format!("doctor has no HNSW component: {doctor}"))?;
    assert_eq!(
        hnsw["status"].as_str(),
        Some(expected.doctor_status),
        "doctor component status: {doctor}"
    );
    assert_eq!(
        hnsw["reason_code"].as_str(),
        expected.reason_code,
        "doctor component reason: {doctor}"
    );

    // The human surface names the same token and the same reason code.
    let human = env
        .cass_assert_command()
        .args(["health", "--deep", "--data-dir", data_dir])
        .output()
        .expect("run human health");
    let human_text = String::from_utf8_lossy(&human.stdout);
    let expected_line = match expected.reason_code {
        Some(code) => format!("ANN: {} ({code})", expected.state),
        None => format!("ANN: {}", expected.state),
    };
    assert!(
        human_text.contains(&expected_line),
        "the human health surface must name {expected_line:?}: {human_text}"
    );

    // A real approximate search: the serving decision the readiness surfaces
    // must agree with, on the very same bytes.
    let search = cass_json(
        &env,
        &[
            "search",
            QUERY,
            "--robot",
            "--robot-meta",
            "--mode",
            "semantic",
            "--model",
            "hash",
            "--approximate",
            "--data-dir",
            data_dir,
        ],
    )?;
    let meta = search
        .pointer("/_meta")
        .ok_or_else(|| format!("search has no _meta block: {search}"))?;
    assert_eq!(
        meta["ann_unavailable_reason"].as_str(),
        expected.reason_code,
        "search fallback reason: {search}"
    );
    assert_eq!(
        meta["ann_stats"].is_object(),
        expected.native,
        "search ANN stats: {search}"
    );
    // Every case still answers the query: a refused accelerator degrades to
    // exact retrieval, it never fails the search.
    assert!(
        search["hits"]
            .as_array()
            .is_some_and(|hits| !hits.is_empty()),
        "the exact fallback must still answer: {search}"
    );
    Ok(())
}

/// Move a file aside instead of deleting it, so a mutated fixture keeps its
/// original bytes for inspection.
fn move_aside(path: &Path) -> PathBuf {
    let moved = path.with_extension("moved");
    fs::rename(path, &moved).expect("move fixture aside");
    moved
}

/// Read the ANN sidecar metadata as JSON.
fn read_sidecar_metadata(path: &Path) -> serde_json::Value {
    let bytes = fs::read(path).expect("read sidecar metadata");
    serde_json::from_slice(&bytes).expect("parse sidecar metadata")
}

/// Rewrite the exact FSVI with the same document IDs and different vectors.
///
/// This is the stale-sidecar case: the documents are identical, so only the
/// content fingerprint can tell that the persisted graph no longer describes
/// the exact owner.
fn rewrite_exact_with_stale_vectors(source: &Path, replacement: &Path) {
    let index = VectorIndex::open_read_only(source).expect("open exact FSVI");
    let mut writer = VectorIndex::create_with_revision(
        replacement,
        index.embedder_id(),
        index.embedder_revision(),
        index.dimension(),
        index.quantization(),
    )
    .expect("create replacement FSVI");
    for position in 0..index.record_count() {
        let doc_id = index.doc_id_at(position).expect("document id");
        let mut vector = index.vector_at_f32(position).expect("vector");
        assert!(
            vector.iter().any(|value| *value != 0.0),
            "the stale fixture needs a non-zero source vector"
        );
        for value in &mut vector {
            *value = -*value;
        }
        writer
            .write_record(doc_id, &vector)
            .expect("write replacement record");
    }
    writer.finish().expect("finish replacement FSVI");
}

#[test]
fn native_ann_is_admitted_and_agrees_with_an_approximate_search() -> Result<(), String> {
    let tracker = tracker_for("native_ann_is_admitted_and_agrees_with_an_approximate_search");
    let corpus = build_corpus(&tracker);

    // The probe never writes: bytes, mtimes, modes, permissions and the
    // directory inventory of the semantic artifacts survive the readiness
    // probe and the serving search unchanged.
    let before = artifact_snapshot(&corpus.data_dir.join("vector_index"));
    assert_surfaces_agree(&corpus, &tracker, &NATIVE)?;
    assert_eq!(
        artifact_snapshot(&corpus.data_dir.join("vector_index")),
        before,
        "the readiness probe and the search must not touch the semantic artifacts"
    );
    tracker.complete();
    Ok(())
}

#[test]
fn bounded_surfaces_report_a_present_sidecar_as_unverified() -> Result<(), String> {
    let tracker = tracker_for("bounded_surfaces_report_a_present_sidecar_as_unverified");
    let corpus = build_corpus(&tracker);
    let env = corpus.env(&tracker);
    let data_dir = corpus.data_dir();

    // The default health probe is bounded: it never deserializes a graph, so
    // it reports presence without ever claiming native readiness.
    let health = cass_json(&env, &["health", "--json", "--data-dir", data_dir])?;
    let semantic = health
        .pointer("/state/semantic")
        .ok_or_else(|| format!("bounded health has no semantic block: {health}"))?;
    assert_eq!(
        semantic["hnsw_present"].as_bool(),
        Some(true),
        "bounded health presence: {health}"
    );
    assert_eq!(
        semantic["hnsw_state"].as_str(),
        Some("present_unverified"),
        "bounded health state: {health}"
    );
    assert_eq!(
        semantic["hnsw_ready"].as_bool(),
        Some(false),
        "bounded health readiness: {health}"
    );
    assert_eq!(
        semantic["hnsw_reason_code"].as_str(),
        None,
        "an unprobed sidecar has no reason code: {health}"
    );

    // The default status probe is bounded the same way.
    let status = cass_json(&env, &["status", "--json", "--data-dir", data_dir])?;
    let status_semantic = status
        .pointer("/semantic")
        .ok_or_else(|| format!("bounded status has no semantic block: {status}"))?;
    assert_eq!(
        status_semantic["hnsw_present"].as_bool(),
        Some(true),
        "bounded status presence: {status}"
    );
    assert_eq!(
        status_semantic["hnsw_state"].as_str(),
        Some("present_unverified"),
        "bounded status state: {status}"
    );
    assert_eq!(
        status_semantic["hnsw_ready"].as_bool(),
        Some(false),
        "bounded status readiness: {status}"
    );

    // The human surface repeats the same token.
    let human = env
        .cass_assert_command()
        .args(["status", "--data-dir", data_dir])
        .output()
        .expect("run human status");
    let human_text = String::from_utf8_lossy(&human.stdout);
    assert!(
        human_text.contains("ANN: present_unverified"),
        "the human status surface must name the bounded token: {human_text}"
    );
    tracker.complete();
    Ok(())
}

#[test]
fn a_missing_sidecar_is_absent_and_falls_back_to_exact() -> Result<(), String> {
    let tracker = tracker_for("a_missing_sidecar_is_absent_and_falls_back_to_exact");
    let corpus = build_corpus(&tracker);
    move_aside(&corpus.sidecar());
    assert_surfaces_agree(&corpus, &tracker, &ABSENT)?;
    tracker.complete();
    Ok(())
}

#[test]
fn corrupt_sidecar_metadata_is_rejected_with_the_serving_reason() -> Result<(), String> {
    let tracker = tracker_for("corrupt_sidecar_metadata_is_rejected_with_the_serving_reason");
    let corpus = build_corpus(&tracker);
    fs::write(corpus.sidecar(), b"not an hnsw sidecar").expect("corrupt sidecar metadata");
    assert_surfaces_agree(&corpus, &tracker, &REJECTED_OPEN_FAILED)?;
    tracker.complete();
    Ok(())
}

#[test]
fn an_incomplete_native_sidecar_is_not_admitted() -> Result<(), String> {
    let tracker = tracker_for("an_incomplete_native_sidecar_is_not_admitted");
    let corpus = build_corpus(&tracker);
    let metadata = read_sidecar_metadata(&corpus.sidecar());
    let generation = metadata
        .pointer("/sidecar_generation")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("sidecar metadata has no generation: {metadata}"))?;
    let basename = metadata
        .pointer("/sidecar_basename")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("sidecar metadata has no basename: {metadata}"))?;
    // The metadata still points at a generation whose native graph is gone:
    // the sidecar is present, but it cannot be admitted.
    move_aside(
        &corpus
            .data_dir
            .join("vector_index")
            .join(generation)
            .join(format!("{basename}.hnsw.graph")),
    );
    assert_surfaces_agree(&corpus, &tracker, &REJECTED_NOT_NATIVE)?;
    tracker.complete();
    Ok(())
}

#[test]
fn a_stale_sidecar_behind_matching_documents_is_not_admitted() -> Result<(), String> {
    let tracker = tracker_for("a_stale_sidecar_behind_matching_documents_is_not_admitted");
    let corpus = build_corpus(&tracker);
    // Same document IDs, different vectors: only the content fingerprint can
    // tell that the persisted graph no longer describes the exact owner.
    let replacement = corpus
        .data_dir
        .join("vector_index")
        .join("index-stale.fsvi");
    rewrite_exact_with_stale_vectors(&corpus.fsvi(), &replacement);
    fs::rename(&replacement, corpus.fsvi()).expect("publish the stale exact artifact");
    assert_surfaces_agree(&corpus, &tracker, &REJECTED_NOT_NATIVE)?;
    tracker.complete();
    Ok(())
}

#[test]
fn a_sidecar_with_the_wrong_dimension_is_rejected() -> Result<(), String> {
    let tracker = tracker_for("a_sidecar_with_the_wrong_dimension_is_rejected");
    let corpus = build_corpus(&tracker);
    let mut metadata = read_sidecar_metadata(&corpus.sidecar());
    metadata
        .as_object_mut()
        .ok_or_else(|| "sidecar metadata is not an object".to_string())?
        .insert("dimension".to_owned(), json!(HASH_DIMENSION + 1));
    fs::write(
        corpus.sidecar(),
        serde_json::to_vec(&metadata).expect("serialize sidecar metadata"),
    )
    .expect("rewrite sidecar metadata");
    assert_surfaces_agree(&corpus, &tracker, &REJECTED_OPEN_FAILED)?;
    tracker.complete();
    Ok(())
}

#[test]
fn a_sidecar_bound_to_another_source_identity_is_not_admitted() -> Result<(), String> {
    let tracker = tracker_for("a_sidecar_bound_to_another_source_identity_is_not_admitted");
    let corpus = build_corpus(&tracker);
    let mut metadata = read_sidecar_metadata(&corpus.sidecar());
    // The sidecar claims a generation identity the live exact owner does not
    // have. A published artifact's identity is not a path and not a name.
    let mut identity = json!({
        "generation": {
            "schema_version": 1,
            "sequence": 7,
            "nonce": [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        }
    });
    // The digest fields are fixed-width byte arrays; a JSON array of numbers
    // is their wire form.
    for digest in [
        "generation_fingerprint",
        "identity_bundle_fingerprint",
        "space_fingerprint",
        "storage_fingerprint",
        "ordered_live_docset_digest",
    ] {
        identity[digest] = json!(vec![0u8; 32]);
    }
    metadata
        .as_object_mut()
        .ok_or_else(|| "sidecar metadata is not an object".to_string())?
        .insert("source_identity".to_owned(), identity);
    fs::write(
        corpus.sidecar(),
        serde_json::to_vec(&metadata).expect("serialize sidecar metadata"),
    )
    .expect("rewrite sidecar metadata");
    assert_surfaces_agree(&corpus, &tracker, &REJECTED_NOT_NATIVE)?;
    tracker.complete();
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_symlinked_sidecar_is_rejected() -> Result<(), String> {
    let tracker = tracker_for("a_symlinked_sidecar_is_rejected");
    let corpus = build_corpus(&tracker);
    // A final-component symlink is never followed into the accelerator: the
    // sidecar is not the artifact it names.
    let real = move_aside(&corpus.sidecar());
    std::os::unix::fs::symlink(&real, corpus.sidecar()).expect("link the sidecar");
    assert_surfaces_agree(&corpus, &tracker, &REJECTED_OPEN_FAILED)?;
    tracker.complete();
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_cross_role_alias_sidecar_is_rejected() -> Result<(), String> {
    let tracker = tracker_for("a_cross_role_alias_sidecar_is_rejected");
    let corpus = build_corpus(&tracker);
    // The exact artifact may never double as its own ANN sidecar.
    move_aside(&corpus.sidecar());
    fs::hard_link(corpus.fsvi(), corpus.sidecar()).expect("alias the exact artifact");
    assert_surfaces_agree(&corpus, &tracker, &REJECTED_OPEN_FAILED)?;
    tracker.complete();
    Ok(())
}

#[test]
fn a_multiple_exact_shard_cohort_is_admitted_and_agrees_with_a_search() -> Result<(), String> {
    let tracker = tracker_for("a_multiple_exact_shard_cohort_is_admitted_and_agrees_with_a_search");
    let corpus = build_corpus(&tracker);
    let env = corpus.env(&tracker);

    // Publish a real two-shard generation the production selection picks up,
    // so the multi-shard cohort is exercised end to end.
    let status = cass_json(&env, &["status", "--json", "--data-dir", corpus.data_dir()])?;
    let fingerprint = status
        .pointer("/index/fingerprint/current_db_fingerprint")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("status has no current database fingerprint: {status}"))?
        .to_string();
    publish_two_shard_generation(&corpus, &fingerprint);

    // The sharded cohort is native: the deep probe admits it and a real
    // approximate search uses it.
    assert_surfaces_agree(&corpus, &tracker, &NATIVE)?;
    tracker.complete();
    Ok(())
}

/// Publish a two-shard semantic generation with a native ANN sidecar per
/// shard, and record it in the shard manifest the production selection reads.
fn publish_two_shard_generation(corpus: &Corpus, db_fingerprint: &str) {
    let vector_dir = corpus.data_dir.join("vector_index");
    let source = VectorIndex::open_read_only(&corpus.fsvi()).expect("open exact FSVI");
    let mut records = Vec::new();
    for position in 0..source.record_count() {
        records.push((
            source.doc_id_at(position).expect("document id"),
            source.vector_at_f32(position).expect("vector"),
        ));
    }
    assert!(
        records.len() >= 2,
        "the fixture needs at least two vectors to shard"
    );
    let midpoint = records.len() / 2;
    let shard_dir = vector_dir.join("shards").join("hash");
    fs::create_dir_all(&shard_dir).expect("create shard directory");

    let mut shard_records = Vec::new();
    for (shard_index, chunk) in [records[..midpoint].to_vec(), records[midpoint..].to_vec()]
        .into_iter()
        .enumerate()
    {
        let fsvi = shard_dir.join(format!("shard-{shard_index}.fsvi"));
        let ann = shard_dir.join(format!("shard-{shard_index}.chsw"));
        let mut writer = VectorIndex::create_with_revision(
            &fsvi,
            HASH_EMBEDDER_ID,
            HASH_VECTOR_SPACE_REVISION,
            HASH_DIMENSION,
            source.quantization(),
        )
        .expect("create shard FSVI");
        for (doc_id, vector) in &chunk {
            writer
                .write_record(doc_id, vector)
                .expect("write shard record");
        }
        writer.finish().expect("finish shard FSVI");
        let shard_index_reader = VectorIndex::open_read_only(&fsvi).expect("open shard FSVI");
        let graph =
            FsHnswIndex::build_from_vector_index(&shard_index_reader, HnswConfig::default())
                .expect("build shard graph");
        graph.save(&ann).expect("save shard graph");
        let ann_size = fs::metadata(&ann).expect("shard sidecar metadata").len();
        shard_records.push(json!({
            "tier": "fast",
            "embedder_id": HASH_EMBEDDER_ID,
            "model_revision": "hash",
            "schema_version": 1,
            "chunking_version": 2,
            "dimension": HASH_DIMENSION,
            "shard_index": shard_index,
            "shard_count": 2,
            "doc_count": chunk.len(),
            "total_conversations": SESSION_COUNT,
            "db_fingerprint": db_fingerprint,
            "index_path": format!("vector_index/shards/hash/shard-{shard_index}.fsvi"),
            "quantization": "f16",
            "mmap_ready": true,
            "ann_index_path": Some(format!("vector_index/shards/hash/shard-{shard_index}.chsw")),
            "ann_size_bytes": ann_size,
            "ann_ready": true,
            "size_bytes": fs::metadata(&fsvi).expect("shard size").len(),
            "started_at_ms": 0,
            "completed_at_ms": 0,
            "ready": true
        }));
    }
    let manifest = json!({
        "manifest_version": 2,
        "shards": shard_records,
        "updated_at_ms": 0
    });
    fs::write(
        vector_dir.join("semantic_shards.json"),
        serde_json::to_vec(&manifest).expect("serialize shard manifest"),
    )
    .expect("write shard manifest");
}
