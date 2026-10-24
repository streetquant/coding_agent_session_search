//! The operator journey for GH #490 on real storage: repeated backfill passes
//! retire superseded scratch only after a durable checkpoint/publication
//! handoff, a killed run leaves orphans behind, and the approved orphan
//! recovery path reclaims exactly the previewed plan — never more.
//!
//! Hash vectors are a deterministic control, not a quality-model qualification.
//! The canonical archive is a real SQLite database, so the retirement and
//! recovery decisions are taken against real bytes on disk.

use super::*;
use anyhow::Context;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::franken_sync::compat::ConnectionExt;
use crate::franken_sync::params as fparams;
use crate::indexer::semantic::{
    EmbeddingInput, SemanticBackfillBatchOutcome, SemanticBackfillBatchPlan,
    SemanticBackfillStoragePlan, SemanticIndexer, packet_embedding_inputs_from_storage,
    semantic_doc_id_for_input,
};
use crate::indexer::semantic_progress::SemanticProgressSink;
use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use crate::search::embedder::Embedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::semantic_manifest::{SemanticManifest, TierKind};
use crate::storage::sqlite::FrankenStorage;
use frankensearch::index::VectorIndex;

/// Set on the helper child process. Absent in a normal run.
const CRASH_ROOT_ENV: &str = "CASS_TEST_SCRATCH_RETIREMENT_CRASH_ROOT";

/// Bytes of the orphan staging file the killed run leaves behind.
const KILLED_STAGING: &[u8] = b"killed run staging";
/// Bytes of the orphan reuse file the killed run leaves behind.
const KILLED_REUSE: &[u8] = b"killed run reuse";

fn conversation(name: &str) -> Conversation {
    Conversation {
        id: None,
        agent_slug: "codex".into(),
        workspace: None,
        external_id: Some(name.into()),
        title: Some(name.into()),
        source_path: PathBuf::from(format!("/gh490/{name}.jsonl")),
        started_at: Some(1_700_000_000_000),
        ended_at: None,
        approx_tokens: None,
        metadata_json: serde_json::json!({}),
        messages: vec![Message {
            id: None,
            idx: 0,
            role: MessageRole::User,
            author: None,
            created_at: Some(1_700_000_000_500),
            content: format!("compiler recovery evidence for {name}"),
            extra_json: serde_json::json!({}),
            snippets: Vec::new(),
        }],
        source_id: "local".into(),
        origin_host: None,
    }
}

fn archive(data: &Path) -> Result<(FrankenStorage, i64)> {
    let storage = FrankenStorage::open(&data.join("agent_search.db"))?;
    let agent_id = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    storage.insert_conversation_tree(agent_id, None, &conversation("first"))?;
    Ok((storage, agent_id))
}

/// One backfill pass over the real archive, exactly like a separate CLI pass.
/// The cap is the number of conversations a single pass may cover.
fn run(
    storage: &FrankenStorage,
    data: &Path,
    max_conversations: usize,
) -> Result<SemanticBackfillBatchOutcome> {
    let mut manifest = SemanticManifest::load(data)?.unwrap_or_default();
    SemanticIndexer::new("hash", None)?.run_capped_backfill_from_storage_with_sink(
        storage,
        data,
        &mut manifest,
        SemanticBackfillStoragePlan {
            tier: TierKind::Quality,
            db_fingerprint: crate::indexer::lexical_storage_fingerprint_for_storage(storage)?,
            model_revision: "hash".into(),
            max_conversations,
        },
        &SemanticProgressSink::disabled(),
    )
}

/// A published artifact with no scratch around it, for the approval-only
/// scenarios that do not need a backfill pass.
fn publish(data: &Path) -> Result<PathBuf> {
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    Ok(indexer
        .run_backfill_batch(
            &[EmbeddingInput::new(
                1,
                "compiler checkpoint recovery evidence",
            )],
            data,
            &mut manifest,
            SemanticBackfillBatchPlan {
                tier: TierKind::Quality,
                db_fingerprint: "content-v1:approval-live".into(),
                model_revision: "hash".into(),
                total_conversations: 1,
                conversations_in_batch: 1,
                last_offset: 1,
                cursor_exhausted: true,
            },
        )?
        .index_path)
}

/// Total logical bytes retained under the vector index.
fn retained_bytes(data: &Path) -> Result<u64> {
    let root = data.join(VECTOR_INDEX_DIR);
    let mut total = 0_u64;
    for entry in walkdir::WalkDir::new(&root).follow_links(false) {
        let entry = entry?;
        let metadata = entry.path().symlink_metadata()?;
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

/// The scratch entries the ownership classifier may ever consider.
fn scratch_entries(data: &Path) -> Result<Vec<String>> {
    let root = data.join(VECTOR_INDEX_DIR);
    let mut names = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".staging-") || name.starts_with(".backfill-reuse-") {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// The document identities a published artifact serves.
fn served_ids(path: &Path) -> Result<Vec<String>> {
    let reader = VectorIndex::open_read_only(path)?;
    let mut ids = Vec::new();
    for record in 0..reader.record_count() {
        ids.push(reader.doc_id_at(record)?.to_owned());
    }
    ids.sort();
    Ok(ids)
}

/// The retrieval answers of the published generation. The journey requires
/// real hits, not merely equal answers: an empty result set would satisfy an
/// equality assertion without proving anything.
fn hits(path: &Path) -> Result<Vec<String>> {
    let reader = VectorIndex::open_read_only(path)?;
    let query = HashEmbedder::default().embed_sync("compiler recovery evidence")?;
    Ok(reader
        .search_top_k(&query, 10, None)?
        .into_iter()
        .map(|hit| hit.doc_id.as_str().to_owned())
        .collect())
}

fn hold_lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    fs2::FileExt::try_lock_exclusive(&file)?;
    Ok(file)
}

/// The helper is selected explicitly in a child process. A normal test run
/// does nothing here; the parent kills the child so no destructor can clean up.
#[test]
fn killed_run_helper() -> Result<()> {
    let Ok(data) = std::env::var(CRASH_ROOT_ENV) else {
        return Ok(());
    };
    let data = PathBuf::from(data);
    let mut manifest = SemanticManifest::load(&data)?.context("child manifest missing")?;
    let indexer = SemanticIndexer::new("hash", None)?;
    indexer.with_backfill_artifacts(&data, &mut manifest, |_, _| {
        let root = data.join(VECTOR_INDEX_DIR);
        let staging = root.join(".staging-quality-minilm-384-deadbeef.fsvi");
        fs::write(&staging, KILLED_STAGING)?;
        let reuse = root.join(".backfill-reuse-Killed123");
        fs::create_dir_all(&reuse)?;
        fs::write(reuse.join("candidate.fsvi"), KILLED_REUSE)?;
        fs::write(data.join("child-ready"), b"orphans owned")?;
        loop {
            std::thread::park();
        }
    })?;
    Ok(())
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start a child that owns the artifact lease, leaves orphans behind and is
/// then killed outright.
fn spawn_killed_run(data: &Path) -> Result<KillOnDrop> {
    let helper = concat!(module_path!(), "::killed_run_helper");
    let (_, filter) = helper
        .split_once("::")
        .context("test module has no crate")?;
    let child = Command::new(std::env::current_exe()?)
        .arg("--exact")
        .arg(filter)
        .env(CRASH_ROOT_ENV, data)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(KillOnDrop(child))
}

/// The whole lifecycle in one journey on real storage: repeated passes retire
/// superseded scratch only after a durable handoff, a killed run leaves
/// orphans, and the operator path reclaims exactly the approved plan.
#[test]
fn scratch_retirement_and_approved_orphan_recovery_share_one_real_journey() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (storage, agent) = archive(data)?;

    // ---- Phase A: repeated passes retire superseded scratch ----
    // The first pass publishes the whole corpus.
    let published = run(&storage, data, 8)?;
    assert!(published.published, "the first complete pass must publish");
    let published_path = published.index_path.clone();
    let published_bytes = fs::read(&published_path)?;
    let mut retained = vec![retained_bytes(data)?];
    assert!(
        scratch_entries(data)?.is_empty(),
        "a published pass leaves no scratch behind"
    );

    // Two pending conversations keep the next pass a checkpoint pass.
    storage.insert_conversation_tree(agent, None, &conversation("second"))?;
    storage.insert_conversation_tree(agent, None, &conversation("third"))?;
    let first_checkpoint = run(&storage, data, 1)?;
    assert!(
        first_checkpoint.checkpoint_saved && !first_checkpoint.published,
        "an incomplete pass must checkpoint instead of publishing"
    );
    let first_checkpoint_path = first_checkpoint.index_path.clone();
    assert!(
        first_checkpoint_path.is_file(),
        "the durable checkpoint must exist before any retirement"
    );
    retained.push(retained_bytes(data)?);
    assert_eq!(
        fs::read(&published_path)?,
        published_bytes,
        "a checkpoint pass must not disturb the published generation"
    );

    // A changed corpus produces a new checkpoint. The superseded one retires
    // only after that replacement is durable.
    storage.insert_conversation_tree(agent, None, &conversation("fourth"))?;
    let second_checkpoint = run(&storage, data, 1)?;
    assert!(
        second_checkpoint.checkpoint_saved && !second_checkpoint.published,
        "an incomplete pass must checkpoint instead of publishing"
    );
    assert_ne!(
        second_checkpoint.index_path, first_checkpoint_path,
        "a changed corpus must checkpoint under a new identity"
    );
    assert!(
        !first_checkpoint_path.exists(),
        "the superseded checkpoint must be retired after the durable handoff"
    );
    retained.push(retained_bytes(data)?);
    assert_eq!(
        fs::read(&published_path)?,
        published_bytes,
        "a checkpoint pass must not disturb the published generation"
    );

    // The final pass publishes the whole corpus and leaves no scratch.
    let done = run(&storage, data, 8)?;
    assert!(done.published, "the completed corpus must publish");
    retained.push(retained_bytes(data)?);
    assert!(
        scratch_entries(data)?.is_empty(),
        "a completed publication leaves no scratch behind"
    );
    assert!(
        SemanticManifest::load(data)?.unwrap().checkpoint.is_none(),
        "a completed publication clears the resumable checkpoint"
    );
    let published_path = done.index_path.clone();
    let published_bytes = fs::read(&published_path)?;
    let bound = (published_bytes.len() as u64)
        .saturating_mul(4)
        .saturating_add(256 * 1024);
    for bytes in &retained {
        assert!(
            *bytes <= bound,
            "retained bytes must stay bounded across repeated passes: {bytes} > {bound}"
        );
    }
    let before_hits = hits(&published_path)?;
    assert!(
        !before_hits.is_empty(),
        "retrieval before recovery must return real hits"
    );

    // ---- Phase B: a killed run leaves orphans behind ----
    let mut child = spawn_killed_run(data)?;
    // The progress condition is the child's own marker file: it is written only
    // after the child holds the artifact lease and has created the orphans. The
    // deadline is a liveness backstop, not a performance assumption — the test
    // binary is large and a loaded host may spend a long time in the child's
    // startup before it reaches the closure. The only other failure condition
    // is the child exiting, which fails loudly instead of hanging.
    let deadline = Instant::now() + Duration::from_secs(300);
    while !data.join("child-ready").is_file() {
        assert!(
            child.0.try_wait()?.is_none(),
            "the child exited before it owned its scratch"
        );
        assert!(
            Instant::now() < deadline,
            "the child never signalled its scratch ownership"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // The live owner holds the artifact lease: the operator path must refuse
    // to plan while a writer is active.
    assert!(
        plan_backfill_artifacts(data).is_err(),
        "a live writer must block the operator preview"
    );
    child.0.kill()?;
    assert!(!child.0.wait()?.success());
    let root = data.join(VECTOR_INDEX_DIR);
    let staging = root.join(".staging-quality-minilm-384-deadbeef.fsvi");
    let reuse = root.join(".backfill-reuse-Killed123");
    assert!(
        staging.is_file() && reuse.is_dir(),
        "the kill must bypass cleanup"
    );

    // ---- Phase C: the approved orphan recovery path ----
    let listing_before = scratch_entries(data)?;
    let plan = plan_backfill_artifacts(data)?;
    assert_eq!(plan.candidates.len(), 2);
    assert_eq!(
        (plan.reclaimable_files, plan.reclaimable_directories),
        (1, 1)
    );
    assert_eq!(
        plan.reclaimable_bytes,
        (KILLED_STAGING.len() + KILLED_REUSE.len()) as u64
    );
    for candidate in &plan.candidates {
        assert!(candidate.size_bytes > 0, "size must be listed");
        assert!(!candidate.reason.is_empty(), "reason must be listed");
        assert!(!candidate.ownership.is_empty(), "ownership must be listed");
    }
    // The reason and owning lifecycle come from the shared classifier, not
    // from the entry's name: the two scratch kinds must be distinguished.
    let staging_candidate = plan
        .candidates
        .iter()
        .find(|candidate| candidate.path.ends_with(staging.file_name().unwrap()))
        .context("staging candidate must be listed")?;
    assert_eq!(
        staging_candidate.ownership, "semantic backfill artifact lease",
        "a staging file belongs to the backfill artifact lease"
    );
    assert!(
        staging_candidate
            .reason
            .contains("unreferenced staging index"),
        "a staging file's reason must not be inferred from its name: {}",
        staging_candidate.reason
    );
    let reuse_candidate = plan
        .candidates
        .iter()
        .find(|candidate| candidate.path.ends_with(reuse.file_name().unwrap()))
        .context("reuse candidate must be listed")?;
    assert_eq!(
        reuse_candidate.ownership, "semantic rebuild scratch lease",
        "a reuse directory belongs to the rebuild scratch lease"
    );
    assert!(
        reuse_candidate.reason.contains("abandoned reuse scratch"),
        "a reuse directory's reason must not be inferred from its name: {}",
        reuse_candidate.reason
    );
    // The preview is read-only: it lists without touching anything.
    assert_eq!(listing_before, scratch_entries(data)?);
    assert_eq!(fs::read(&staging)?, KILLED_STAGING);
    assert_eq!(fs::read(reuse.join("candidate.fsvi"))?, KILLED_REUSE);
    // An approval that is not the previewed plan removes nothing.
    assert!(apply_backfill_artifact_plan(data, &"0".repeat(64)).is_err());
    assert!(staging.is_file() && reuse.is_dir());
    // The approved, unchanged plan applies.
    let report = apply_backfill_artifact_plan(data, &plan.plan_fingerprint)?;
    assert_eq!((report.removed_files, report.removed_directories), (1, 1));
    assert_eq!(report.reclaimed_bytes, plan.reclaimable_bytes);
    assert!(report.failed_paths.is_empty());
    assert!(!staging.exists() && !reuse.exists());
    // The published generation and its retrieval are untouched.
    assert_eq!(fs::read(&published_path)?, published_bytes);
    let after_hits = hits(&published_path)?;
    assert!(
        !after_hits.is_empty(),
        "retrieval after recovery must return real hits"
    );
    assert_eq!(before_hits, after_hits);
    // The consumed approval is stale: it cannot be replayed.
    assert!(apply_backfill_artifact_plan(data, &plan.plan_fingerprint).is_err());
    Ok(())
}

/// A candidate path replaced by a different file of the same size must not be
/// removable under the approval that described the old file.
#[test]
fn a_replaced_candidate_invalidates_the_approval_without_deleting_the_replacement() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    publish(data)?;
    let staging = data
        .join(VECTOR_INDEX_DIR)
        .join(".staging-quality-minilm-384-deadbeef.fsvi");
    fs::write(&staging, b"AAAA")?;
    let plan = plan_backfill_artifacts(data)?;
    assert_eq!(plan.candidates.len(), 1);
    assert_eq!(plan.reclaimable_bytes, 4);
    // The path is taken over by another file of exactly the same size.
    fs::remove_file(&staging)?;
    fs::write(&staging, b"BBBB")?;
    assert!(
        apply_backfill_artifact_plan(data, &plan.plan_fingerprint).is_err(),
        "a replaced candidate must invalidate the approval"
    );
    assert_eq!(
        fs::read(&staging)?,
        b"BBBB",
        "the replacement must survive a stale approval"
    );
    Ok(())
}

/// A new owner that appears between the preview and the apply invalidates the
/// approval; the newly owned file must survive.
#[test]
fn a_new_owner_between_preview_and_apply_invalidates_the_approval() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    publish(data)?;
    let staging = data
        .join(VECTOR_INDEX_DIR)
        .join(".staging-quality-minilm-384-deadbeef.fsvi");
    fs::write(&staging, b"obsolete staging")?;
    let plan = plan_backfill_artifacts(data)?;
    assert_eq!(plan.candidates.len(), 1);
    let mut manifest = SemanticManifest::load(data)?.unwrap();
    let artifact = manifest.quality_tier.as_mut().unwrap();
    artifact.index_path = staging.strip_prefix(data)?.to_string_lossy().into_owned();
    // Not ready still owns its main/WAL pair.
    artifact.ready = false;
    manifest.save(data)?;
    assert!(apply_backfill_artifact_plan(data, &plan.plan_fingerprint).is_err());
    assert!(
        staging.is_file(),
        "a newly owned file must survive the stale approval"
    );
    Ok(())
}

/// A live writer owns the archive; neither the preview nor the apply may run.
#[test]
fn a_live_writer_blocks_the_approved_recovery() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    publish(data)?;
    let root = data.join(VECTOR_INDEX_DIR);
    let staging = root.join(".staging-quality-minilm-384-deadbeef.fsvi");
    let reuse = root.join(".backfill-reuse-Killed123");
    fs::write(&staging, b"obsolete staging")?;
    fs::create_dir_all(&reuse)?;
    fs::write(reuse.join("candidate.fsvi"), b"private copy")?;
    let plan = plan_backfill_artifacts(data)?;
    assert_eq!(plan.candidates.len(), 2);
    let owner = hold_lock(&data.join("index-run.lock"))?;
    assert!(
        plan_backfill_artifacts(data).is_err(),
        "a live writer must block the preview"
    );
    assert!(
        apply_backfill_artifact_plan(data, &plan.plan_fingerprint).is_err(),
        "a live writer must block the apply"
    );
    assert!(staging.is_file() && reuse.is_dir());
    drop(owner);
    let report = apply_backfill_artifact_plan(data, &plan.plan_fingerprint)?;
    assert_eq!((report.removed_files, report.removed_directories), (1, 1));
    assert!(!staging.exists() && !reuse.exists());
    Ok(())
}

/// A pass that embeds nothing is not proof that the selected corpus is
/// unchanged: a deleted conversation must be detected and dropped even when
/// the pass embeds zero documents.
#[test]
fn a_no_op_pass_after_a_deletion_is_not_proof_of_an_unchanged_corpus() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (storage, agent) = archive(data)?;
    let doomed = storage
        .insert_conversation_tree(agent, None, &conversation("second"))?
        .conversation_id;
    let mut published = None;
    for _ in 0..12 {
        let outcome = run(&storage, data, 1)?;
        if outcome.published {
            published = Some(outcome.index_path.clone());
            break;
        }
    }
    let published = published.context("the backfill must publish")?;
    let before = served_ids(&published)?;
    assert_eq!(before.len(), 2, "both conversations must be served");

    // Delete one conversation: the corpus shrinks, nothing new can be
    // embedded, and the pass must not report an unchanged corpus.
    storage.raw().execute_compat(
        "DELETE FROM messages WHERE conversation_id = ?1",
        fparams![doomed],
    )?;
    storage
        .raw()
        .execute_compat("DELETE FROM conversations WHERE id = ?1", fparams![doomed])?;

    let outcome = run(&storage, data, 1)?;
    assert!(
        !outcome.unchanged,
        "a changed corpus must not report an unchanged backfill"
    );
    assert_eq!(
        outcome.embedded_docs, 0,
        "a deletion embeds no document: the count alone proves nothing"
    );
    let mut expected: Vec<String> = packet_embedding_inputs_from_storage(&storage)?
        .iter()
        .filter_map(semantic_doc_id_for_input)
        .collect();
    expected.sort();
    assert_eq!(
        served_ids(&outcome.index_path)?,
        expected,
        "the deleted conversation must leave the selected corpus"
    );
    assert!(!hits(&outcome.index_path)?.is_empty());
    Ok(())
}
