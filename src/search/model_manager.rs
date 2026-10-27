//! Semantic model management (local-only detection).
//!
//! This module wires the pure-Rust native MiniLM embedder into semantic search by:
//! - validating the local model files
//! - loading the vector index
//! - building filter maps from the SQLite database
//! - detecting model version mismatches
//!
//! It does **not** download models. Missing files are surfaced as availability
//! states so the UI can guide the user. Downloads are handled by [`model_download`].

mod external;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::franken_sync::compat::RowExt;
use crate::indexer::semantic::expected_vector_space_revision;
use crate::search::ann_index::hnsw_index_path;
use crate::search::embedder::{CassPassageProducer, Embedder};
use crate::search::fastembed_embedder::{FastEmbedder, LazyFastEmbedder};
use crate::search::hash_embedder::HashEmbedder;
use crate::search::model_download::{
    ModelAcquisitionPolicy, ModelCacheState, ModelManifest, classify_model_cache,
    classify_model_cache_metadata,
};
use crate::search::policy::{CliSemanticOverrides, SemanticPolicy};
use crate::search::semantic_manifest::selection::SemanticSelectionMetadata;
use crate::search::semantic_manifest::source_snapshot::{
    SemanticSourceContract, SemanticSourceSnapshot, SemanticSourceSnapshotError,
    semantic_source_certificate_in_transaction,
};
use crate::search::semantic_manifest::{
    SemanticArtifactRole, SemanticCurrentPointerV1, SemanticGenerationError,
    SemanticGenerationManifestV1, SemanticGenerationPurpose, SemanticShardManifest,
    SemanticShardRecord, TierKind, semantic_shard_artifact_path_is_safe,
};
use crate::search::semantic_reader::ann::AnnAdmissionBudget;
use crate::search::semantic_reader::publication::text::hydration::{
    PassageProjection, SemanticReadSnapshot, snapshot_seal,
};
use crate::search::semantic_reader::publication::{
    SelectedSemanticGeneration, SemanticSelectionBudget, SemanticSelectionError,
};
use crate::search::vector_index::{
    ROLE_ASSISTANT, ROLE_USER, SemanticFilterMaps, SemanticIndexArtifact,
    SemanticProgressiveUnavailableReason, VectorIndex, vector_index_path,
};
use crate::storage::sqlite::FrankenStorage;
use anyhow::anyhow;

/// Unified TUI state machine for semantic search availability.
///
/// This enum tracks the full lifecycle of semantic search from the user's perspective:
/// - Model installation flow (NotInstalled → NeedsConsent → Downloading → Verifying → Ready)
/// - Index building flow (Ready → IndexBuilding → Ready)
/// - Explicit user preferences (HashFallback, Disabled)
/// - Error states (LoadFailed, ModelMissing, etc.)
#[derive(Debug, Clone)]
pub enum SemanticAvailability {
    /// Model is ready for use.
    Ready { embedder_id: String },

    // =========================================================================
    // TUI-centric states for user flow
    // =========================================================================
    /// Model not installed - semantic not available.
    /// TUI should show an option to download or explicitly choose hash mode.
    NotInstalled,

    /// User needs to consent before downloading model.
    /// TUI should show consent dialog.
    NeedsConsent,

    /// Model download in progress.
    Downloading {
        /// Progress percentage (0-100).
        progress_pct: u8,
        /// Bytes downloaded so far.
        bytes_downloaded: u64,
        /// Total bytes to download.
        total_bytes: u64,
    },

    /// Verifying downloaded model (SHA256 check).
    Verifying,

    /// Index is being built or rebuilt.
    IndexBuilding {
        embedder_id: String,
        /// Optional progress percentage (0-100).
        progress_pct: Option<u8>,
        /// Number of items indexed so far.
        items_indexed: u64,
        /// Total items to index.
        total_items: u64,
    },

    /// Vector assets load, but the semantic manifest says they do not describe
    /// the current database (stale generation, or a backfill that has not yet
    /// published a queryable tier). Serving them would return nearest
    /// neighbours of an outdated corpus, so the query path must not consult
    /// them (GH #404). `reason` carries the manifest-derived verdict.
    IndexStale { embedder_id: String, reason: String },

    /// User explicitly opted for hash-based degraded mode (no ML model).
    HashFallback,

    /// Semantic search disabled by policy or user.
    Disabled { reason: String },

    // =========================================================================
    // Diagnostic states for troubleshooting
    // =========================================================================
    /// Model files are missing.
    ModelMissing {
        embedder_id: String,
        model_dir: PathBuf,
        missing_files: Vec<String>,
    },

    /// Vector index is missing.
    IndexMissing { index_path: PathBuf },

    /// Database is unavailable.
    DatabaseUnavailable { db_path: PathBuf, error: String },

    /// Failed to load semantic context.
    LoadFailed { context: String },

    /// Model update available - index rebuild needed.
    UpdateAvailable {
        embedder_id: String,
        current_revision: String,
        latest_revision: String,
    },
}

impl SemanticAvailability {
    /// Check if semantic search is ready to use.
    pub fn is_ready(&self) -> bool {
        matches!(self, SemanticAvailability::Ready { .. })
    }

    /// Check if a model update is available.
    pub fn has_update(&self) -> bool {
        matches!(self, SemanticAvailability::UpdateAvailable { .. })
    }

    /// Check if the index is being rebuilt.
    pub fn is_building(&self) -> bool {
        matches!(self, SemanticAvailability::IndexBuilding { .. })
    }

    /// Check if loadable vector assets were refused because they do not
    /// describe the current database (GH #404).
    pub fn is_index_stale(&self) -> bool {
        matches!(self, SemanticAvailability::IndexStale { .. })
    }

    /// Check if a download is in progress.
    pub fn is_downloading(&self) -> bool {
        matches!(self, SemanticAvailability::Downloading { .. })
    }

    /// Check if user consent is needed.
    pub fn needs_consent(&self) -> bool {
        matches!(self, SemanticAvailability::NeedsConsent)
    }

    /// Check if explicit hash mode is active.
    pub fn is_hash_fallback(&self) -> bool {
        matches!(self, SemanticAvailability::HashFallback)
    }

    /// Check if semantic search is disabled.
    pub fn is_disabled(&self) -> bool {
        matches!(self, SemanticAvailability::Disabled { .. })
    }

    /// Check if the model is not installed.
    pub fn is_not_installed(&self) -> bool {
        matches!(
            self,
            SemanticAvailability::NotInstalled | SemanticAvailability::ModelMissing { .. }
        )
    }

    /// Check if any error state is active.
    pub fn is_error(&self) -> bool {
        matches!(
            self,
            SemanticAvailability::LoadFailed { .. }
                | SemanticAvailability::DatabaseUnavailable { .. }
        )
    }

    /// Check if vector search can be used (native semantic or explicit hash mode).
    pub fn can_search(&self) -> bool {
        matches!(
            self,
            SemanticAvailability::Ready { .. } | SemanticAvailability::HashFallback
        )
    }

    /// Get download progress if downloading.
    pub fn download_progress(&self) -> Option<(u8, u64, u64)> {
        match self {
            SemanticAvailability::Downloading {
                progress_pct,
                bytes_downloaded,
                total_bytes,
            } => Some((*progress_pct, *bytes_downloaded, *total_bytes)),
            _ => None,
        }
    }

    /// Get index building progress if building.
    pub fn index_progress(&self) -> Option<(Option<u8>, u64, u64)> {
        match self {
            SemanticAvailability::IndexBuilding {
                progress_pct,
                items_indexed,
                total_items,
                ..
            } => Some((*progress_pct, *items_indexed, *total_items)),
            _ => None,
        }
    }

    /// Get a short status label for display in status bar.
    pub fn status_label(&self) -> &'static str {
        match self {
            SemanticAvailability::Ready { .. } => "SEM",
            SemanticAvailability::HashFallback => "SEM*",
            SemanticAvailability::NotInstalled => "LEX",
            SemanticAvailability::NeedsConsent => "LEX",
            SemanticAvailability::Downloading { .. } => "DL...",
            SemanticAvailability::Verifying => "VFY...",
            SemanticAvailability::IndexBuilding { .. } => "IDX...",
            SemanticAvailability::IndexStale { .. } => "STALE",
            SemanticAvailability::Disabled { .. } => "OFF",
            SemanticAvailability::ModelMissing { .. } => "NOMODEL",
            SemanticAvailability::IndexMissing { .. } => "NOIDX",
            SemanticAvailability::DatabaseUnavailable { .. } => "NODB",
            SemanticAvailability::LoadFailed { .. } => "ERR",
            SemanticAvailability::UpdateAvailable { .. } => "UPD",
        }
    }

    /// Get a detailed summary for display.
    pub fn summary(&self) -> String {
        match self {
            SemanticAvailability::Ready { embedder_id } => {
                format!("semantic ready ({embedder_id})")
            }
            SemanticAvailability::NotInstalled => "model not installed".to_string(),
            SemanticAvailability::NeedsConsent => "consent required for model download".to_string(),
            SemanticAvailability::Downloading {
                progress_pct,
                bytes_downloaded,
                total_bytes,
            } => {
                let mb_done = *bytes_downloaded as f64 / 1_048_576.0;
                let mb_total = *total_bytes as f64 / 1_048_576.0;
                format!("downloading model: {progress_pct}% ({mb_done:.1}/{mb_total:.1} MB)")
            }
            SemanticAvailability::Verifying => "verifying model checksum".to_string(),
            SemanticAvailability::IndexBuilding {
                items_indexed,
                total_items,
                progress_pct,
                ..
            } => {
                if let Some(pct) = progress_pct {
                    format!("building index: {pct}% ({items_indexed}/{total_items})")
                } else {
                    format!("building index: {items_indexed}/{total_items}")
                }
            }
            SemanticAvailability::IndexStale { reason, .. } => {
                format!("semantic assets not current: {reason}")
            }
            SemanticAvailability::HashFallback => "using explicit hash mode".to_string(),
            SemanticAvailability::Disabled { reason } => {
                format!("semantic disabled: {reason}")
            }
            SemanticAvailability::ModelMissing { model_dir, .. } => {
                format!("model missing at {}", model_dir.display())
            }
            SemanticAvailability::IndexMissing { index_path } => {
                format!("vector index missing at {}", index_path.display())
            }
            SemanticAvailability::DatabaseUnavailable { error, .. } => {
                format!("db unavailable ({error})")
            }
            SemanticAvailability::LoadFailed { context } => {
                format!("semantic load failed ({context})")
            }
            SemanticAvailability::UpdateAvailable {
                current_revision,
                latest_revision,
                ..
            } => {
                format!("update available: {current_revision} -> {latest_revision}")
            }
        }
    }
}

pub struct SemanticContext {
    pub embedder: Arc<dyn Embedder>,
    /// These owners may come from the monolithic writer path or the prototype
    /// shard manifest. Exact search consumes the retained owners directly.
    /// Progressive/two-tier serving remains fail-closed for every source,
    /// because its current FrankenSearch constructors reopen paths. Do not
    /// populate these owners by reopening
    /// `ValidatedSemanticGeneration::artifact_paths`: that map is diagnostic
    /// resolution only until the immutable-generation validator can hand
    /// serving code retained FSVI witnesses/owners and FrankenSearch can
    /// consume those owners without reopening.
    pub artifacts: Vec<SemanticIndexArtifact>,
    pub quality_artifact: Option<SemanticIndexArtifact>,
    pub filter_maps: SemanticFilterMaps,
    pub roles: Option<HashSet<u8>>,
}

/// Actual producer owners selected by the caller's explicit retrieval policy.
/// Missing tiers stay missing; a quality producer is never replaced by hash.
#[derive(Clone)]
pub struct RetainedSemanticProducers {
    pub fast: Option<Arc<CassPassageProducer>>,
    pub quality: Option<Arc<CassPassageProducer>>,
    pub primary: TierKind,
}

impl RetainedSemanticProducers {
    fn validate_manifest(
        &self,
        manifest: &SemanticGenerationManifestV1,
        contract: SemanticSourceContract,
    ) -> Result<(), RetainedSemanticLoadError> {
        let primary = match self.primary {
            TierKind::Fast => &self.fast,
            TierKind::Quality => &self.quality,
        };
        if primary.is_none() {
            return Err(RetainedSemanticLoadError::MissingPrimaryProducer);
        }
        for (tier, role, producer) in [
            (TierKind::Fast, SemanticArtifactRole::FastVector, &self.fast),
            (
                TierKind::Quality,
                SemanticArtifactRole::QualityVector,
                &self.quality,
            ),
        ] {
            let Some(producer) = producer else {
                continue;
            };
            if producer.source_contract() != contract {
                return Err(RetainedSemanticLoadError::ProducerSourceContract { tier });
            }
            let identity = producer
                .identity()
                .map_err(|_| RetainedSemanticLoadError::ProducerUnavailable { tier })?;
            let mut selected = false;
            for artifact in manifest.artifacts.iter().filter(|item| item.role == role) {
                selected = true;
                let expected = &artifact.embedding_identity.identity;
                expected
                    .verify_exact_producer_with(identity)
                    .map_err(|_| RetainedSemanticLoadError::ProducerMismatch { tier })?;
                if producer.dimension() != expected.space.dimension as usize {
                    return Err(RetainedSemanticLoadError::ProducerMismatch { tier });
                }
            }
            if !selected {
                return Err(RetainedSemanticLoadError::UnselectedProducer { tier });
            }
        }
        Ok(())
    }
}

/// Explicit source projection and admission limits; none are inferred from a
/// selected vector's name or rewritten to match an available model.
#[derive(Debug, Clone, Copy)]
pub struct RetainedSemanticLoadPolicy {
    pub source_contract: SemanticSourceContract,
    pub vectors: SemanticSelectionBudget,
    pub ann: AnnAdmissionBudget,
}

/// A source-bound empty publication has no embedding or vector identity.
#[derive(Clone)]
pub struct RetainedEmptySemanticContext {
    source: SemanticSourceSnapshot,
    pointer: SemanticCurrentPointerV1,
    manifest: SemanticGenerationManifestV1,
}

impl std::fmt::Debug for RetainedEmptySemanticContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetainedEmptySemanticContext")
            .finish_non_exhaustive()
    }
}

impl RetainedEmptySemanticContext {
    pub fn source_snapshot(&self) -> &SemanticSourceSnapshot {
        &self.source
    }

    pub fn pointer(&self) -> &SemanticCurrentPointerV1 {
        &self.pointer
    }

    pub fn manifest(&self) -> &SemanticGenerationManifestV1 {
        &self.manifest
    }
}

/// Empty and populated publications retain distinct kinds of authority.
#[derive(Debug, Clone)]
pub enum RetainedSemanticLoadOutcome {
    Empty(Arc<RetainedEmptySemanticContext>),
    Populated(Arc<RetainedSemanticContext>),
}

impl RetainedSemanticLoadOutcome {
    pub fn source_snapshot(&self) -> &SemanticSourceSnapshot {
        match self {
            Self::Empty(context) => context.source_snapshot(),
            Self::Populated(context) => context.source_snapshot(),
        }
    }

    /// Revalidate the actual query snapshot before model or ANN work. The
    /// caller must use this same transaction for filtering and hydration.
    pub fn verify_source_in_transaction<R: SemanticReadSnapshot>(
        &self,
        transaction: &R,
    ) -> Result<(), RetainedSemanticLoadError> {
        let retained = self.source_snapshot();
        let current = semantic_source_certificate_in_transaction(transaction, retained.contract())
            .map_err(RetainedSemanticLoadError::Source)?;
        if current.identity() != retained.identity() {
            return Err(RetainedSemanticLoadError::SourceChanged);
        }
        Ok(())
    }
}

/// An independently certified source, retained publication, and actual models.
/// This is distinct from the path-based SemanticContext: no dummy legacy
/// artifact vector can represent an immutable publication.
#[derive(Clone)]
pub struct RetainedSemanticContext {
    selected: SelectedSemanticGeneration,
    source: SemanticSourceSnapshot,
    producers: RetainedSemanticProducers,
    filter_maps: SemanticFilterMaps,
    roles: Option<HashSet<u8>>,
    ann_budget: AnnAdmissionBudget,
}

impl std::fmt::Debug for RetainedSemanticContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetainedSemanticContext")
            .field("primary", &self.producers.primary)
            .field("fast_shards", &self.selected.shard_count(TierKind::Fast))
            .field(
                "quality_shards",
                &self.selected.shard_count(TierKind::Quality),
            )
            .finish_non_exhaustive()
    }
}

impl RetainedSemanticContext {
    pub fn selected(&self) -> &SelectedSemanticGeneration {
        &self.selected
    }

    pub fn source_snapshot(&self) -> &SemanticSourceSnapshot {
        &self.source
    }

    pub fn producers(&self) -> &RetainedSemanticProducers {
        &self.producers
    }

    pub fn projection(&self) -> PassageProjection {
        self.source.contract().projection()
    }

    pub fn filter_maps(&self) -> &SemanticFilterMaps {
        &self.filter_maps
    }

    pub fn roles(&self) -> Option<&HashSet<u8>> {
        self.roles.as_ref()
    }

    pub fn ann_budget(&self) -> AnnAdmissionBudget {
        self.ann_budget
    }
}

/// Stable displays contain no model diagnostics, archive text or artifact paths.
/// The typed underlying publication failure is retained for deliberate diagnosis.
#[derive(Debug, thiserror::Error)]
pub enum RetainedSemanticLoadError {
    #[error("semantic current pointer could not be inspected")]
    PointerProbe(#[source] std::io::Error),
    #[error("semantic source certificate admission failed: {0}")]
    Source(#[source] SemanticSourceSnapshotError),
    #[error("semantic filter metadata could not be read from the source snapshot")]
    FilterMaps(#[source] anyhow::Error),
    #[error("semantic publication metadata admission failed")]
    Publication(#[source] SemanticGenerationError),
    #[error("semantic retained owner admission failed")]
    Selection(#[source] SemanticSelectionError),
    #[error("semantic primary producer is unavailable")]
    MissingPrimaryProducer,
    #[error("semantic {tier:?} producer is unavailable")]
    ProducerUnavailable { tier: TierKind },
    #[error("semantic {tier:?} producer differs from the selected identity")]
    ProducerMismatch { tier: TierKind },
    #[error("semantic {tier:?} producer differs from the independent source contract")]
    ProducerSourceContract { tier: TierKind },
    #[error("semantic {tier:?} producer has no selected vector tier")]
    UnselectedProducer { tier: TierKind },
    #[error("semantic publication changed during producer admission")]
    SelectionChanged,
    #[error("semantic source differs from the retained query context")]
    SourceChanged,
}

impl RetainedSemanticLoadError {
    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::PointerProbe(_) => "semantic_pointer_probe_failed",
            Self::Source(source) => source.reason_code(),
            Self::FilterMaps(_) => "semantic_filter_metadata_rejected",
            Self::Publication(_) => "semantic_publication_metadata_rejected",
            Self::Selection(_) => "semantic_retained_owner_rejected",
            Self::MissingPrimaryProducer => "semantic_primary_producer_missing",
            Self::ProducerUnavailable { .. } => "semantic_producer_unavailable",
            Self::ProducerMismatch { .. } => "semantic_producer_mismatch",
            Self::ProducerSourceContract { .. } => "semantic_producer_source_contract_mismatch",
            Self::UnselectedProducer { .. } => "semantic_producer_tier_unselected",
            Self::SelectionChanged => "semantic_selection_changed",
            Self::SourceChanged => "semantic_source_context_changed",
        }
    }
}

/// Admit CURRENT using an independent certificate from the caller's actual read
/// snapshot, then load actual producers and retain every selected vector owner.
///
/// None means CURRENT is absent. A present but rejected publication is an error;
/// callers must not fall through to conventional vector filenames in that case.
/// The factory receives metadata and the explicit source contract, not authority
/// to copy expected identities onto unbound embeddings. It must return actual
/// producer owners. Constructing a CassPassageProducer measures its conformance
/// outputs through actual inference; the caller must admit that factory work.
/// An explicitly empty publication returns before the factory, retaining its
/// certified source and selected metadata without fabricated model identities.
/// The adapter performs no query inference, download, graph load, archive write,
/// or transaction lifecycle operation.
pub fn load_retained_semantic_context<R, F>(
    data_dir: &Path,
    transaction: &R,
    policy: RetainedSemanticLoadPolicy,
    roles: Option<HashSet<u8>>,
    producer_factory: F,
) -> Result<Option<RetainedSemanticLoadOutcome>, RetainedSemanticLoadError>
where
    R: SemanticReadSnapshot,
    F: FnOnce(
        &SemanticGenerationManifestV1,
        SemanticSourceContract,
    ) -> Result<RetainedSemanticProducers, RetainedSemanticLoadError>,
{
    match std::fs::symlink_metadata(SemanticCurrentPointerV1::path(data_dir)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(RetainedSemanticLoadError::PointerProbe(error)),
        Ok(_) => {}
    }
    let source = semantic_source_certificate_in_transaction(transaction, policy.source_contract)
        .map_err(RetainedSemanticLoadError::Source)?;
    let metadata = SemanticSelectionMetadata::read(data_dir, Some(source.identity()))
        .map_err(RetainedSemanticLoadError::Publication)?;
    policy
        .vectors
        .check(&metadata.manifest)
        .map_err(RetainedSemanticLoadError::Selection)?;
    if metadata.manifest.purpose == SemanticGenerationPurpose::EmptyCorpus {
        return Ok(Some(RetainedSemanticLoadOutcome::Empty(Arc::new(
            RetainedEmptySemanticContext {
                source,
                pointer: metadata.pointer,
                manifest: metadata.manifest,
            },
        ))));
    }
    let filter_maps = SemanticFilterMaps::from_read_snapshot(transaction)
        .map_err(RetainedSemanticLoadError::FilterMaps)?;
    let producers = producer_factory(&metadata.manifest, source.contract())?;
    producers.validate_manifest(&metadata.manifest, source.contract())?;
    let selected =
        SelectedSemanticGeneration::open_current(data_dir, source.identity(), policy.vectors)
            .map_err(RetainedSemanticLoadError::Selection)?;
    if selected.selection().pointer() != &metadata.pointer
        || selected.selection().manifest() != &metadata.manifest
    {
        return Err(RetainedSemanticLoadError::SelectionChanged);
    }
    producers.validate_manifest(selected.selection().manifest(), source.contract())?;
    Ok(Some(RetainedSemanticLoadOutcome::Populated(Arc::new(
        RetainedSemanticContext {
            selected,
            source,
            producers,
            filter_maps,
            roles,
            ann_budget: policy.ann,
        },
    ))))
}

pub struct SemanticSetup {
    pub availability: SemanticAvailability,
    pub context: Option<SemanticContext>,
}

const SEMANTIC_POINTER_PROBE_FAILED_REASON: &str = "semantic_pointer_probe_failed";

fn selected_generation_owner_requirement(data_dir: &Path) -> Option<SemanticAvailability> {
    match std::fs::symlink_metadata(SemanticCurrentPointerV1::path(data_dir)) {
        Ok(_) => Some(SemanticAvailability::LoadFailed {
            context: SemanticProgressiveUnavailableReason::OwnerBackedReaderRequired
                .code()
                .to_string(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(SemanticAvailability::LoadFailed {
            context: SEMANTIC_POINTER_PROBE_FAILED_REASON.to_string(),
        }),
    }
}

fn semantic_sidecar_path(data_dir: &Path, recorded_path: &str) -> Option<PathBuf> {
    semantic_shard_artifact_path_is_safe(recorded_path).then(|| data_dir.join(recorded_path))
}

fn validate_vector_index_contract(
    index: &VectorIndex,
    expected_embedder_id: &str,
    expected_dimension: usize,
) -> Result<(), String> {
    let expected_revision =
        expected_vector_space_revision(expected_embedder_id).ok_or_else(|| {
            format!("no current vector-space revision for embedder {expected_embedder_id}")
        })?;
    if index.embedder_id() != expected_embedder_id
        || index.dimension() != expected_dimension
        || index.embedder_revision() != expected_revision
    {
        return Err(format!(
            "incompatible vector index: expected embedder {expected_embedder_id} revision {expected_revision} dimension {expected_dimension}, found embedder {} revision {} dimension {}; rebuild semantic vectors",
            index.embedder_id(),
            index.embedder_revision(),
            index.dimension()
        ));
    }
    Ok(())
}

fn open_validated_semantic_artifact(
    index_path: &Path,
    ann_path: Option<PathBuf>,
    expected_embedder_id: &str,
    expected_dimension: usize,
) -> Result<SemanticIndexArtifact, String> {
    let artifact = SemanticIndexArtifact::open(index_path, ann_path)
        .map_err(|error| format!("vector index: {error}"))?;
    validate_vector_index_contract(artifact.index(), expected_embedder_id, expected_dimension)?;
    Ok(artifact)
}

fn semantic_artifact_candidate_exists(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "inspect semantic artifact candidate {}: {error}",
            path.display()
        )),
    }
}

fn matching_complete_shard_records(
    data_dir: &Path,
    tier: TierKind,
    embedder_id: &str,
    db_fingerprint: &str,
) -> Result<Option<Vec<SemanticShardRecord>>, String> {
    let manifest = match SemanticShardManifest::load(data_dir) {
        Ok(Some(manifest)) => manifest,
        Ok(None) => return Ok(None),
        Err(err) => return Err(format!("semantic shard manifest: {err}")),
    };
    let summary = manifest.summary(tier, embedder_id, db_fingerprint);
    if !summary.complete {
        return Ok(None);
    }

    let mut records = manifest
        .shards
        .into_iter()
        .filter(|shard| shard.matches_generation(tier, embedder_id, db_fingerprint))
        .collect::<Vec<_>>();
    records.sort_by_key(|shard| shard.shard_index);
    if records.len() != usize::try_from(summary.shard_count).unwrap_or(usize::MAX) {
        return Ok(None);
    }

    let Some(first) = records.first() else {
        return Ok(None);
    };
    for (expected_index, shard) in records.iter().enumerate() {
        if shard.shard_index != u32::try_from(expected_index).unwrap_or(u32::MAX)
            || !shard.ready
            || !shard.mmap_ready
            || shard.model_revision != first.model_revision
            || shard.schema_version != crate::search::policy::SEMANTIC_SCHEMA_VERSION
            || shard.chunking_version != crate::search::policy::CHUNKING_STRATEGY_VERSION
            || shard.dimension == 0
            || shard.dimension != first.dimension
            || shard.total_conversations != first.total_conversations
        {
            return Ok(None);
        }
        let Some(path) = semantic_sidecar_path(data_dir, &shard.index_path) else {
            return Ok(None);
        };
        if !semantic_artifact_candidate_exists(&path)? {
            return Ok(None);
        }
    }

    Ok(Some(records))
}

fn load_complete_shard_artifacts(
    data_dir: &Path,
    embedder_id: &str,
    expected_dimension: usize,
    db_fingerprint: &str,
) -> Result<Option<Vec<SemanticIndexArtifact>>, String> {
    for tier in [TierKind::Quality, TierKind::Fast] {
        let Some(records) =
            matching_complete_shard_records(data_dir, tier, embedder_id, db_fingerprint)?
        else {
            continue;
        };

        let mut artifacts = Vec::with_capacity(records.len());
        for shard in records {
            let Some(path) = semantic_sidecar_path(data_dir, &shard.index_path) else {
                return Ok(None);
            };
            let ann_path = if shard.ann_ready {
                shard
                    .ann_index_path
                    .as_deref()
                    .and_then(|recorded| semantic_sidecar_path(data_dir, recorded))
                    .and_then(|candidate| {
                        let selected = select_existing_ann_candidate(candidate.clone());
                        if selected.is_none() {
                            tracing::warn!(
                                path = %candidate.display(),
                                embedder = embedder_id,
                                shard = shard.shard_index,
                                "semantic shard manifest marks ANN ready but its exact sidecar is missing"
                            );
                        }
                        selected
                    })
            } else {
                None
            };
            let artifact = SemanticIndexArtifact::open(&path, ann_path)
                .map_err(|err| format!("semantic shard vector index {}: {err}", path.display()))?;
            if shard.dimension != expected_dimension {
                return Err(format!(
                    "semantic shard manifest {} declares dimension {}, expected {expected_dimension}",
                    path.display(),
                    shard.dimension
                ));
            }
            validate_vector_index_contract(artifact.index(), embedder_id, expected_dimension)
                .map_err(|error| {
                    format!("semantic shard vector index {}: {error}", path.display())
                })?;
            artifacts.push(artifact);
        }
        if !artifacts.is_empty() {
            tracing::info!(
                tier = tier.as_str(),
                embedder = embedder_id,
                shard_count = artifacts.len(),
                "loaded complete semantic shard generation"
            );
            return Ok(Some(artifacts));
        }
    }

    Ok(None)
}

fn select_existing_ann_candidate(path: PathBuf) -> Option<PathBuf> {
    match std::fs::symlink_metadata(&path) {
        Ok(_) => Some(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(path),
    }
}

/// Resolve the production-selected exact cohort and its explicitly paired ANN
/// sidecars for a readiness probe.
///
/// This mirrors the serving selection exactly: the complete shard generation
/// named by the shard manifest when it matches the current database,
/// otherwise the monolithic artifact with its paired sidecar. It never loads
/// an embedding model, never writes, and never rediscovers an artifact beside
/// a vector the way a filename guess would.
pub(crate) fn selected_semantic_ann_cohort(
    data_dir: &Path,
    embedder_id: &str,
    expected_dimension: usize,
    db_fingerprint: Option<&str>,
) -> Vec<SemanticIndexArtifact> {
    if let Some(fingerprint) = db_fingerprint
        && let Some(artifacts) =
            load_complete_shard_artifacts(data_dir, embedder_id, expected_dimension, fingerprint)
                .ok()
                .flatten()
    {
        return artifacts;
    }
    // Monolithic fallback: the conventional sidecar path is the pairing the
    // serving path uses for a non-sharded generation.
    let index_path = vector_index_path(data_dir, embedder_id);
    let ann_path = select_existing_ann_candidate(hnsw_index_path(data_dir, embedder_id));
    open_validated_semantic_artifact(&index_path, ann_path, embedder_id, expected_dimension)
        .map(|artifact| vec![artifact])
        .unwrap_or_default()
}

fn complete_shard_generation_candidate_exists(data_dir: &Path, embedder_id: &str) -> bool {
    let manifest = match SemanticShardManifest::load(data_dir) {
        Ok(Some(manifest)) => manifest,
        Ok(None) => return false,
        Err(err) => {
            tracing::debug!(
                error = %err,
                embedder = embedder_id,
                "semantic shard candidate probe could not load manifest"
            );
            return false;
        }
    };

    let mut candidates = std::collections::HashSet::new();
    for shard in manifest
        .shards
        .iter()
        .filter(|shard| shard.embedder_id == embedder_id)
    {
        candidates.insert((shard.tier, shard.db_fingerprint.as_str()));
    }

    candidates
        .into_iter()
        .any(|(tier, db_fingerprint)| manifest.summary(tier, embedder_id, db_fingerprint).complete)
}

/// Adapter for legacy callers that already own a `FrankenStorage` handle.
struct ConnectionReadSnapshot<'a>(&'a crate::franken_sync::Connection);

impl snapshot_seal::Sealed for ConnectionReadSnapshot<'_> {}

impl SemanticReadSnapshot for ConnectionReadSnapshot<'_> {
    fn query_with_params(
        &self,
        sql: &str,
        params: &[crate::franken_sync::SqliteValue],
    ) -> Result<Vec<crate::franken_sync::Row>, crate::franken_sync::FrankenError> {
        self.0.query_with_params(sql, params)
    }
}

/// The lexical storage fingerprint, computed on a caller-owned snapshot.
///
/// Keep this byte-identical to
/// `crate::indexer::lexical_storage_fingerprint_for_storage`; a fingerprint
/// difference would make every semantic artifact look stale.
fn lexical_fingerprint_from_snapshot(
    snapshot: &dyn SemanticReadSnapshot,
) -> anyhow::Result<String> {
    let count_row = snapshot
        .query_with_params("SELECT COUNT(*) FROM conversations", &[])
        .map_err(|error| anyhow!("counting canonical conversations: {error}"))?;
    let total_conversations: i64 = count_row
        .first()
        .ok_or_else(|| anyhow!("conversation count query returned no row"))?
        .get_typed(0)
        .map_err(|error| anyhow!("conversation count was not an integer: {error}"))?;
    let total_conversations = usize::try_from(total_conversations.max(0)).unwrap_or(usize::MAX);
    let max_conversation_id: i64 = snapshot
        .query_with_params("SELECT COALESCE(MAX(id), 0) FROM conversations", &[])
        .map_err(|error| anyhow!("computing conversation fingerprint: {error}"))?
        .first()
        .ok_or_else(|| anyhow!("conversation fingerprint query returned no row"))?
        .get_typed(0)
        .map_err(|error| anyhow!("conversation fingerprint was not an integer: {error}"))?;
    let max_message_id: i64 = snapshot
        .query_with_params("SELECT COALESCE(MAX(id), 0) FROM messages", &[])
        .map_err(|error| anyhow!("computing message fingerprint: {error}"))?
        .first()
        .ok_or_else(|| anyhow!("message fingerprint query returned no row"))?
        .get_typed(0)
        .map_err(|error| anyhow!("message fingerprint was not an integer: {error}"))?;
    Ok(format!(
        "content-v1:{total_conversations}:{max_conversation_id}:{max_message_id}"
    ))
}

fn load_complete_shard_artifacts_for_current_db(
    data_dir: &Path,
    db_path: &Path,
    embedder_id: &str,
    expected_dimension: usize,
    context_label: &'static str,
    strict_read_only: bool,
    snapshot: Option<&dyn SemanticReadSnapshot>,
) -> Option<Vec<SemanticIndexArtifact>> {
    load_complete_shard_artifacts_with_fingerprint(
        data_dir,
        embedder_id,
        expected_dimension,
        context_label,
        || match snapshot {
            Some(snapshot) => lexical_fingerprint_from_snapshot(snapshot),
            None if strict_read_only => {
                crate::indexer::lexical_storage_fingerprint_for_db_strict(db_path)
            }
            None => crate::indexer::lexical_storage_fingerprint_for_db(db_path),
        },
    )
}

fn load_complete_shard_artifacts_with_fingerprint(
    data_dir: &Path,
    embedder_id: &str,
    expected_dimension: usize,
    context_label: &'static str,
    fingerprint: impl FnOnce() -> anyhow::Result<String>,
) -> Option<Vec<SemanticIndexArtifact>> {
    // GH #452: a monolithic FSVI is not evidence that shards exist. Probe
    // metadata before opening the archive solely to fingerprint a possible
    // shard generation; the normal staleness/filter-map path opens it later.
    if !complete_shard_generation_candidate_exists(data_dir, embedder_id) {
        return None;
    }
    let db_fingerprint = match fingerprint() {
        Ok(fingerprint) => fingerprint,
        Err(err) => {
            tracing::debug!(
                error = %format!("{err:#}"),
                embedder = embedder_id,
                context = context_label,
                "semantic shard context unavailable: failed to fingerprint current DB"
            );
            return None;
        }
    };

    match load_complete_shard_artifacts(data_dir, embedder_id, expected_dimension, &db_fingerprint)
    {
        Ok(artifacts) => artifacts,
        Err(err) => {
            tracing::debug!(
                error = %err,
                embedder = embedder_id,
                context = context_label,
                "semantic shard context unavailable"
            );
            None
        }
    }
}

/// Load semantic context with optional version mismatch checking.
///
/// If `check_for_updates` is true, this function will check if the installed
/// model version matches the manifest and return `UpdateAvailable` if they differ.
pub fn load_semantic_context(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    load_semantic_context_for_embedder(data_dir, db_path, active_policy_embedder_name())
}

/// Strict read-only counterpart used by `search --no-maintenance`.
pub fn load_semantic_context_strict(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    load_semantic_context_inner(
        data_dir,
        db_path,
        true,
        active_policy_embedder_name(),
        false,
        true,
        None,
    )
}

/// Load the active policy context without initializing its local model yet.
pub fn load_semantic_context_deferred(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    load_semantic_context_for_embedder_deferred(data_dir, db_path, active_policy_embedder_name())
}

/// Strict read-only daemon-first counterpart used by
/// `search --no-maintenance`.
pub fn load_semantic_context_deferred_strict(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    load_semantic_context_inner(
        data_dir,
        db_path,
        true,
        active_policy_embedder_name(),
        true,
        true,
        None,
    )
}

pub fn load_semantic_context_for_embedder(
    data_dir: &Path,
    db_path: &Path,
    embedder_name: &str,
) -> SemanticSetup {
    load_semantic_context_inner(data_dir, db_path, true, embedder_name, false, false, None)
}

pub fn load_semantic_context_for_embedder_strict(
    data_dir: &Path,
    db_path: &Path,
    embedder_name: &str,
) -> SemanticSetup {
    load_semantic_context_inner(data_dir, db_path, true, embedder_name, false, true, None)
}

/// Load index/filter metadata now while deferring the local model itself.
///
/// This is the daemon-first CLI path: the returned embedder preserves the
/// vector index's stable id/dimension contract, but initializes local inference
/// only if the daemon wrapper needs its fallback (#347).
pub fn load_semantic_context_for_embedder_deferred(
    data_dir: &Path,
    db_path: &Path,
    embedder_name: &str,
) -> SemanticSetup {
    load_semantic_context_inner(data_dir, db_path, true, embedder_name, true, false, None)
}

pub fn load_semantic_context_for_embedder_deferred_strict(
    data_dir: &Path,
    db_path: &Path,
    embedder_name: &str,
) -> SemanticSetup {
    load_semantic_context_inner(data_dir, db_path, true, embedder_name, true, true, None)
}

/// Load the policy context from a caller-owned archive snapshot.
pub fn load_semantic_context_with_snapshot(
    data_dir: &Path,
    db_path: &Path,
    snapshot: &dyn SemanticReadSnapshot,
) -> SemanticSetup {
    load_semantic_context_inner(
        data_dir,
        db_path,
        true,
        active_policy_embedder_name(),
        false,
        true,
        Some(snapshot),
    )
}

/// Deferred-model counterpart of [`load_semantic_context_with_snapshot`].
pub fn load_semantic_context_deferred_with_snapshot(
    data_dir: &Path,
    db_path: &Path,
    snapshot: &dyn SemanticReadSnapshot,
) -> SemanticSetup {
    load_semantic_context_inner(
        data_dir,
        db_path,
        true,
        active_policy_embedder_name(),
        true,
        true,
        Some(snapshot),
    )
}

/// Load an explicitly requested embedder from a caller-owned archive snapshot.
pub fn load_semantic_context_for_embedder_with_snapshot(
    data_dir: &Path,
    db_path: &Path,
    embedder_name: &str,
    snapshot: &dyn SemanticReadSnapshot,
) -> SemanticSetup {
    load_semantic_context_inner(
        data_dir,
        db_path,
        true,
        embedder_name,
        false,
        true,
        Some(snapshot),
    )
}

/// Deferred-model counterpart of
/// [`load_semantic_context_for_embedder_with_snapshot`].
pub fn load_semantic_context_for_embedder_deferred_with_snapshot(
    data_dir: &Path,
    db_path: &Path,
    embedder_name: &str,
    snapshot: &dyn SemanticReadSnapshot,
) -> SemanticSetup {
    load_semantic_context_inner(
        data_dir,
        db_path,
        true,
        embedder_name,
        true,
        true,
        Some(snapshot),
    )
}

/// Probe semantic availability without loading the embedder or DB-backed
/// filter maps. The selected FSVI is opened through the same exact-artifact
/// policy as serving so status cannot advertise an artifact a query rejects.
pub(crate) fn probe_semantic_availability(data_dir: &Path) -> SemanticAvailability {
    probe_semantic_availability_for_embedder(data_dir, active_policy_embedder_name())
}

pub(crate) fn probe_semantic_availability_for_embedder(
    data_dir: &Path,
    embedder_name: &str,
) -> SemanticAvailability {
    if embedder_name
        .trim()
        .eq_ignore_ascii_case(crate::search::external_embedder::EXTERNAL_EMBEDDER)
    {
        return external::probe(data_dir);
    }
    if let Some(availability) = selected_generation_owner_requirement(data_dir) {
        return availability;
    }
    let Some(canonical_name) = FastEmbedder::canonical_name(embedder_name) else {
        return SemanticAvailability::LoadFailed {
            context: format!(
                "unsupported semantic embedder: {embedder_name}; supported: minilm, multilingual-minilm"
            ),
        };
    };
    let Some(config) = FastEmbedder::config_for(canonical_name) else {
        return SemanticAvailability::LoadFailed {
            context: format!("unknown semantic embedder: {embedder_name}"),
        };
    };
    let Some(model_dir) = FastEmbedder::runtime_model_dir_for(data_dir, canonical_name) else {
        return SemanticAvailability::LoadFailed {
            context: format!("no model directory mapping for semantic embedder: {embedder_name}"),
        };
    };
    let manifest =
        ModelManifest::for_embedder(canonical_name).unwrap_or_else(ModelManifest::minilm_v2);
    let semantic_policy = SemanticPolicy::resolve(&CliSemanticOverrides::default());
    let acquisition_policy = ModelAcquisitionPolicy::from_semantic_policy(&semantic_policy);
    let cache_report = classify_model_cache_metadata(&model_dir, &manifest, &acquisition_policy);

    if let Some(availability) = semantic_availability_from_cache_state(
        &config.embedder_id,
        &model_dir,
        &cache_report.state,
        true,
    ) {
        return availability;
    }

    let index_path = vector_index_path(data_dir, &config.embedder_id);
    match semantic_artifact_candidate_exists(&index_path) {
        Ok(true) => {}
        Ok(false) => return SemanticAvailability::IndexMissing { index_path },
        Err(context) => return SemanticAvailability::LoadFailed { context },
    }

    if let Err(context) =
        open_validated_semantic_artifact(&index_path, None, &config.embedder_id, config.dimension)
    {
        return SemanticAvailability::LoadFailed { context };
    }

    SemanticAvailability::Ready {
        embedder_id: config.embedder_id,
    }
}

/// Probe hash semantic availability without opening the DB. The selected FSVI
/// is admitted through the same exact-artifact policy as serving.
pub(crate) fn probe_hash_semantic_availability(data_dir: &Path) -> SemanticAvailability {
    if let Some(availability) = selected_generation_owner_requirement(data_dir) {
        return availability;
    }
    let embedder = HashEmbedder::default();
    let index_path = vector_index_path(data_dir, embedder.id());
    match semantic_artifact_candidate_exists(&index_path) {
        Ok(false) => SemanticAvailability::IndexMissing { index_path },
        Err(context) => SemanticAvailability::LoadFailed { context },
        Ok(true) => match open_validated_semantic_artifact(
            &index_path,
            None,
            embedder.id(),
            embedder.dimension(),
        ) {
            Ok(_) => SemanticAvailability::HashFallback,
            Err(context) => SemanticAvailability::LoadFailed { context },
        },
    }
}

/// Load hash-based semantic context (no model download required).
pub fn load_hash_semantic_context(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    load_hash_semantic_context_inner(data_dir, db_path, false, None)
}

pub fn load_hash_semantic_context_strict(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    load_hash_semantic_context_inner(data_dir, db_path, true, None)
}

/// Load the hash-vector context from a caller-owned archive snapshot.
///
/// The snapshot serves staleness admission, filter maps and the artifact
/// fingerprint, so the caller's later hydration reuses the same archive
/// handle instead of opening the database again.
pub fn load_hash_semantic_context_with_snapshot(
    data_dir: &Path,
    db_path: &Path,
    snapshot: &dyn SemanticReadSnapshot,
) -> SemanticSetup {
    load_hash_semantic_context_inner(data_dir, db_path, true, Some(snapshot))
}

fn load_hash_semantic_context_inner(
    data_dir: &Path,
    db_path: &Path,
    strict_read_only: bool,
    snapshot: Option<&dyn SemanticReadSnapshot>,
) -> SemanticSetup {
    if let Some(availability) = selected_generation_owner_requirement(data_dir) {
        return SemanticSetup {
            availability,
            context: None,
        };
    }
    let embedder = HashEmbedder::default();
    let index_path = vector_index_path(data_dir, embedder.id());
    let monolithic_present = match semantic_artifact_candidate_exists(&index_path) {
        Ok(present) => present,
        Err(context) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed { context },
                context: None,
            };
        }
    };
    let shard_artifacts = load_complete_shard_artifacts_for_current_db(
        data_dir,
        db_path,
        embedder.id(),
        embedder.dimension(),
        "hash semantic",
        strict_read_only,
        snapshot,
    );
    if !monolithic_present && shard_artifacts.is_none() {
        return SemanticSetup {
            availability: SemanticAvailability::IndexMissing { index_path },
            context: None,
        };
    }

    let owned_storage;
    let storage_snapshot;
    let admission: &dyn SemanticReadSnapshot = match snapshot {
        Some(snapshot) => snapshot,
        None => {
            owned_storage = match if strict_read_only {
                FrankenStorage::open_strict_readonly(db_path)
            } else {
                FrankenStorage::open_readonly(db_path)
            } {
                Ok(storage) => storage,
                Err(err) => {
                    return SemanticSetup {
                        availability: SemanticAvailability::DatabaseUnavailable {
                            db_path: db_path.to_path_buf(),
                            error: err.to_string(),
                        },
                        context: None,
                    };
                }
            };
            storage_snapshot = ConnectionReadSnapshot(owned_storage.raw());
            &storage_snapshot
        }
    };

    if let Some(availability) = refuse_stale_semantic_assets(
        data_dir,
        admission,
        embedder.id(),
        &SemanticAvailability::HashFallback,
        crate::search::asset_state::SemanticPreference::HashFallback,
    ) {
        return SemanticSetup {
            availability,
            context: None,
        };
    }

    let filter_maps = match SemanticFilterMaps::from_read_snapshot(admission) {
        Ok(maps) => maps,
        Err(err) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed {
                    context: format!("filter maps: {err}"),
                },
                context: None,
            };
        }
    };

    let artifacts = if let Some(artifacts) = shard_artifacts {
        artifacts
    } else {
        let ann_path = hnsw_index_path(data_dir, embedder.id());
        let ann_path = select_existing_ann_candidate(ann_path);
        match open_validated_semantic_artifact(
            &index_path,
            ann_path,
            embedder.id(),
            embedder.dimension(),
        ) {
            Ok(artifact) => vec![artifact],
            Err(context) => {
                return SemanticSetup {
                    availability: SemanticAvailability::LoadFailed { context },
                    context: None,
                };
            }
        }
    };

    let roles = Some(HashSet::from([ROLE_USER, ROLE_ASSISTANT]));
    let embedder = Arc::new(embedder) as Arc<dyn Embedder>;

    SemanticSetup {
        availability: SemanticAvailability::HashFallback,
        context: Some(SemanticContext {
            embedder,
            artifacts,
            quality_artifact: None,
            filter_maps,
            roles,
        }),
    }
}

/// Load semantic context without version checking.
///
/// Use this when you've already acknowledged an update and want to load
/// the model anyway.
pub fn load_semantic_context_no_version_check(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    load_semantic_context_inner(
        data_dir,
        db_path,
        false,
        active_policy_embedder_name(),
        false,
        false,
        None,
    )
}

fn load_semantic_context_inner(
    data_dir: &Path,
    db_path: &Path,
    check_for_updates: bool,
    embedder_name: &str,
    defer_embedder_load: bool,
    strict_read_only: bool,
    snapshot: Option<&dyn SemanticReadSnapshot>,
) -> SemanticSetup {
    if embedder_name
        .trim()
        .eq_ignore_ascii_case(crate::search::external_embedder::EXTERNAL_EMBEDDER)
    {
        // The local deferred/daemon fallback is not an external provider. This
        // path admits the external artifact first, then runs its own preflight.
        return external::load(data_dir, db_path, strict_read_only);
    }
    if let Some(availability) = selected_generation_owner_requirement(data_dir) {
        return SemanticSetup {
            availability,
            context: None,
        };
    }
    let Some(canonical_name) = FastEmbedder::canonical_name(embedder_name) else {
        return SemanticSetup {
            availability: SemanticAvailability::LoadFailed {
                context: format!(
                    "unsupported semantic embedder: {embedder_name}; supported: minilm, multilingual-minilm"
                ),
            },
            context: None,
        };
    };
    let Some(config) = FastEmbedder::config_for(canonical_name) else {
        return SemanticSetup {
            availability: SemanticAvailability::LoadFailed {
                context: format!("unknown semantic embedder: {embedder_name}"),
            },
            context: None,
        };
    };
    let Some(model_dir) = FastEmbedder::runtime_model_dir_for(data_dir, canonical_name) else {
        return SemanticSetup {
            availability: SemanticAvailability::LoadFailed {
                context: format!(
                    "no model directory mapping for semantic embedder: {embedder_name}"
                ),
            },
            context: None,
        };
    };
    let manifest =
        ModelManifest::for_embedder(canonical_name).unwrap_or_else(ModelManifest::minilm_v2);
    let semantic_policy = SemanticPolicy::resolve(&CliSemanticOverrides::default());
    let acquisition_policy = ModelAcquisitionPolicy::from_semantic_policy(&semantic_policy);
    let cache_report = classify_model_cache(&model_dir, &manifest, &acquisition_policy);

    if let Some(availability) = semantic_availability_from_cache_state(
        &config.embedder_id,
        &model_dir,
        &cache_report.state,
        check_for_updates,
    ) {
        return SemanticSetup {
            availability,
            context: None,
        };
    }

    let index_path = vector_index_path(data_dir, &config.embedder_id);
    let monolithic_present = match semantic_artifact_candidate_exists(&index_path) {
        Ok(present) => present,
        Err(context) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed { context },
                context: None,
            };
        }
    };
    let shard_artifacts = load_complete_shard_artifacts_for_current_db(
        data_dir,
        db_path,
        &config.embedder_id,
        config.dimension,
        "semantic",
        strict_read_only,
        snapshot,
    );
    if !monolithic_present && shard_artifacts.is_none() {
        return SemanticSetup {
            availability: SemanticAvailability::IndexMissing { index_path },
            context: None,
        };
    }

    let owned_storage;
    let storage_snapshot;
    let admission: &dyn SemanticReadSnapshot = match snapshot {
        Some(snapshot) => snapshot,
        None => {
            owned_storage = match if strict_read_only {
                FrankenStorage::open_strict_readonly(db_path)
            } else {
                FrankenStorage::open_readonly(db_path)
            } {
                Ok(storage) => storage,
                Err(err) => {
                    return SemanticSetup {
                        availability: SemanticAvailability::DatabaseUnavailable {
                            db_path: db_path.to_path_buf(),
                            error: err.to_string(),
                        },
                        context: None,
                    };
                }
            };
            storage_snapshot = ConnectionReadSnapshot(owned_storage.raw());
            &storage_snapshot
        }
    };

    if let Some(availability) = refuse_stale_semantic_assets(
        data_dir,
        admission,
        &config.embedder_id,
        &SemanticAvailability::Ready {
            embedder_id: config.embedder_id.clone(),
        },
        crate::search::asset_state::SemanticPreference::DefaultModel,
    ) {
        return SemanticSetup {
            availability,
            context: None,
        };
    }

    let filter_maps = match SemanticFilterMaps::from_read_snapshot(admission) {
        Ok(maps) => maps,
        Err(err) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed {
                    context: format!("filter maps: {err}"),
                },
                context: None,
            };
        }
    };

    let artifacts = if let Some(artifacts) = shard_artifacts {
        artifacts
    } else {
        let ann_path = hnsw_index_path(data_dir, &config.embedder_id);
        let ann_path = select_existing_ann_candidate(ann_path);
        match open_validated_semantic_artifact(
            &index_path,
            ann_path,
            &config.embedder_id,
            config.dimension,
        ) {
            Ok(artifact) => vec![artifact],
            Err(context) => {
                return SemanticSetup {
                    availability: SemanticAvailability::LoadFailed { context },
                    context: None,
                };
            }
        }
    };

    let embedder: Arc<dyn Embedder> = match if defer_embedder_load {
        LazyFastEmbedder::new(data_dir, canonical_name)
            .map(|embedder| Arc::new(embedder) as Arc<dyn Embedder>)
    } else {
        FastEmbedder::load_by_name(data_dir, canonical_name)
            .map(|embedder| Arc::new(embedder) as Arc<dyn Embedder>)
    } {
        Ok(embedder) => embedder,
        Err(err) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed {
                    context: format!("model load: {err}"),
                },
                context: None,
            };
        }
    };

    let roles = Some(HashSet::from([ROLE_USER, ROLE_ASSISTANT]));

    SemanticSetup {
        availability: SemanticAvailability::Ready {
            embedder_id: embedder.id().to_string(),
        },
        context: Some(SemanticContext {
            embedder,
            artifacts,
            quality_artifact: None,
            filter_maps,
            roles,
        }),
    }
}

/// GH #404: refuse vector assets that load but do not describe the current
/// database.
///
/// `cass status` already computes a readiness verdict (`can_search`,
/// `fallback_mode`) from the semantic manifest against the live DB
/// fingerprint, but the query path used to admit any artifact that merely
/// parsed. A stale or mid-backfill FSVI/HNSW has no relevance floor, so an
/// out-of-vocabulary query returned `k` arbitrary neighbours while status
/// said `fallback_mode: "lexical"`. This applies the same verdict before a
/// context is handed to `SearchClient`, reusing the read-only handle the
/// caller already opened for filter maps (no second archive open).
///
/// Returns `Some(unavailable)` when serving must be refused; `None` when the
/// assets are current (or when there is no manifest to contradict them, which
/// preserves legacy manifest-less installs).
fn refuse_stale_semantic_assets(
    data_dir: &Path,
    snapshot: &dyn SemanticReadSnapshot,
    served_embedder_id: &str,
    availability_if_served: &SemanticAvailability,
    preference: crate::search::asset_state::SemanticPreference,
) -> Option<SemanticAvailability> {
    // Schema keys owned by `storage::sqlite::SemanticIdentityTier::meta_key`;
    // mirrored here because the snapshot reader is intentionally independent
    // of an open `FrankenStorage`. Keep these strings in lockstep.
    let identity_meta_key = if served_embedder_id == HashEmbedder::default().id() {
        "semantic_fast_identity_rebuild_v1"
    } else {
        "semantic_quality_identity_rebuild_v1"
    };
    let identity_marker = snapshot
        .query_with_params(
            "SELECT value FROM meta WHERE key = ?1",
            &[crate::franken_sync::SqliteValue::Text(
                identity_meta_key.into(),
            )],
        )
        .map_err(|error| {
            format!("checking canonical semantic identity invalidation marker: {error}")
        })
        .and_then(|rows| {
            let Some(row) = rows.first() else {
                return Ok(None);
            };
            row.get_typed::<String>(0)
                .map(Some)
                .map_err(|error| format!("reading canonical semantic identity marker: {error}"))
        });
    match identity_marker {
        Ok(Some(value)) if value != "complete" => {
            return Some(SemanticAvailability::IndexStale {
                embedder_id: served_embedder_id.to_string(),
                reason: "canonical agent/source identity changed; run 'cass index --semantic' to rebuild semantic filter metadata"
                    .to_string(),
            });
        }
        Ok(_) => {}
        Err(err) => {
            return Some(SemanticAvailability::LoadFailed { context: err });
        }
    }
    let db_fingerprint = match lexical_fingerprint_from_snapshot(snapshot) {
        Ok(fingerprint) => fingerprint,
        Err(err) => {
            // The archive answered the open but not the fingerprint queries;
            // the filter-map load right after this will surface the real
            // error, so do not invent a staleness verdict here.
            tracing::debug!(
                error = %format!("{err:#}"),
                embedder = served_embedder_id,
                "semantic staleness gate skipped: could not fingerprint current DB"
            );
            return None;
        }
    };
    let state = crate::search::asset_state::semantic_state_from_availability(
        data_dir,
        availability_if_served,
        preference,
        Some(&db_fingerprint),
    );
    // The tier that actually backs the artifact being served. The overall
    // verdict can be `can_search` on the strength of the *other* tier (e.g.
    // a current hash fast tier while the MiniLM quality tier is stale), which
    // is fine for status but not for handing out this specific artifact.
    let served_tier = if served_embedder_id == HashEmbedder::default().id() {
        &state.fast_tier
    } else {
        &state.quality_tier
    };
    // Only a record for *this* embedder can vouch for or refuse this artifact;
    // a tier record built by some other embedder says nothing about it.
    let served_tier_describes_artifact =
        served_tier.present && served_tier.embedder_id.as_deref() == Some(served_embedder_id);
    let served_tier_not_current = served_tier_describes_artifact
        && (!served_tier.ready || served_tier.current_db_matches == Some(false));
    if state.can_search && !served_tier_not_current {
        return None;
    }
    let reason = match (&state.hint, served_tier_not_current) {
        (_, true) if state.can_search => format!(
            "published {} tier for {served_embedder_id} does not match the current database; run 'cass index --semantic' to refresh it",
            if served_embedder_id == HashEmbedder::default().id() {
                "fast"
            } else {
                "quality"
            }
        ),
        (Some(hint), _) => format!("{}; {hint}", state.summary),
        (None, _) => state.summary.clone(),
    };
    tracing::info!(
        embedder = served_embedder_id,
        status = state.status,
        fallback_mode = state.fallback_mode,
        "refusing semantic assets that do not describe the current database (GH #404)"
    );
    Some(SemanticAvailability::IndexStale {
        embedder_id: served_embedder_id.to_string(),
        reason,
    })
}

fn active_policy_embedder_name() -> &'static str {
    let semantic_policy = SemanticPolicy::resolve(&CliSemanticOverrides::default());
    FastEmbedder::canonical_name(&semantic_policy.quality_tier_embedder).unwrap_or("unsupported")
}

fn semantic_availability_from_cache_state(
    embedder_id: &str,
    model_dir: &Path,
    state: &ModelCacheState,
    check_for_updates: bool,
) -> Option<SemanticAvailability> {
    match state {
        ModelCacheState::Acquired { .. }
        | ModelCacheState::PreseededLocal { .. }
        | ModelCacheState::MirrorSourced { .. } => None,
        ModelCacheState::IncompatibleVersion {
            current_revision,
            expected_revision,
        } if check_for_updates => Some(SemanticAvailability::UpdateAvailable {
            embedder_id: embedder_id.to_string(),
            current_revision: current_revision.clone(),
            latest_revision: expected_revision.clone(),
        }),
        ModelCacheState::IncompatibleVersion { .. } => None,
        ModelCacheState::NotAcquired {
            missing_files,
            needs_consent,
        } => {
            if *needs_consent {
                Some(SemanticAvailability::NeedsConsent)
            } else {
                Some(SemanticAvailability::ModelMissing {
                    embedder_id: embedder_id.to_string(),
                    model_dir: model_dir.to_path_buf(),
                    missing_files: missing_files.clone(),
                })
            }
        }
        ModelCacheState::Acquiring {
            bytes_present,
            total_bytes,
            ..
        } => {
            let progress_pct = if *total_bytes == 0 {
                0
            } else {
                ((*bytes_present as f64 / *total_bytes as f64) * 100.0).min(100.0) as u8
            };
            Some(SemanticAvailability::Downloading {
                progress_pct,
                bytes_downloaded: *bytes_present,
                total_bytes: *total_bytes,
            })
        }
        ModelCacheState::ChecksumMismatch {
            file,
            expected,
            actual,
        } => Some(SemanticAvailability::LoadFailed {
            context: format!(
                "model checksum mismatch for {file}: expected {expected}, got {actual}"
            ),
        }),
        ModelCacheState::DisabledByPolicy { reason } => Some(SemanticAvailability::Disabled {
            reason: reason.clone(),
        }),
        ModelCacheState::BudgetBlocked {
            required_bytes,
            max_bytes,
        } => Some(SemanticAvailability::Disabled {
            reason: format!(
                "semantic model requires {required_bytes} bytes but policy allows {max_bytes}"
            ),
        }),
        ModelCacheState::QuarantinedCorrupt {
            marker_path,
            reason,
        } => Some(SemanticAvailability::LoadFailed {
            context: format!(
                "model cache quarantined at {}: {reason}",
                marker_path.display()
            ),
        }),
        ModelCacheState::OfflineBlocked { missing_files } => Some(SemanticAvailability::Disabled {
            reason: format!(
                "offline and semantic model is not acquired: missing {}",
                missing_files.join(", ")
            ),
        }),
    }
}

/// Check if the vector index needs rebuilding after a model upgrade.
///
/// This compares the embedder ID, vector-space revision, and dimension in the
/// vector index header with the current native MiniLM contract. Shape equality
/// alone cannot prove compatibility across inference-engine generations.
///
/// A selected generation requires the owner-backed admission path; this legacy
/// probe cannot certify it by inspecting a conventional filename. Such a
/// selection (including an unreadable pointer) returns `true` without opening
/// legacy assets. This function never repairs or replays a writable index.
pub fn needs_index_rebuild(data_dir: &Path) -> bool {
    if selected_generation_owner_requirement(data_dir).is_some() {
        return true;
    }
    let index_path = vector_index_path(data_dir, FastEmbedder::embedder_id_static());

    if !index_path.is_file() {
        // Index doesn't exist, so it needs to be built (not rebuilt)
        return false;
    }

    // Diagnostics must not replay/repair a journal or create writable sidecars.
    match VectorIndex::open_read_only(&index_path) {
        Ok(index) => {
            validate_vector_index_contract(&index, FastEmbedder::embedder_id_static(), 384).is_err()
        }
        Err(_) => {
            // Index is corrupted or unreadable, needs rebuild
            true
        }
    }
}

/// Delete the vector index to force a rebuild.
///
/// Call this after a model upgrade when the user has consented to rebuilding
/// the semantic index. The next index run will rebuild from scratch.
///
/// # Returns
///
/// `Ok(true)` if the index was deleted.
/// `Ok(false)` if the index didn't exist.
/// `Err(_)` if deletion failed.
pub fn delete_vector_index_for_rebuild(data_dir: &Path) -> std::io::Result<bool> {
    let index_path = vector_index_path(data_dir, FastEmbedder::embedder_id_static());

    if index_path.is_file() {
        std::fs::remove_file(&index_path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Get the model directory path for the default MiniLM model.
pub fn default_model_dir(data_dir: &Path) -> PathBuf {
    FastEmbedder::default_model_dir(data_dir)
}

/// Get the model manifest for the default MiniLM model.
pub fn default_model_manifest() -> ModelManifest {
    ModelManifest::minilm_v2()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::franken_sync::compat::TransactionExt;
    use crate::storage::sqlite::SemanticIdentityTier;
    use tempfile::tempdir;

    type AvailabilityTuiCase = (
        SemanticAvailability,
        &'static str,
        fn(&SemanticAvailability) -> bool,
    );

    #[test]
    fn retained_loader_absent_pointer_does_not_require_source_or_model() -> anyhow::Result<()> {
        let root = tempdir()?;
        let connection = crate::franken_sync::Connection::open(":memory:")?;
        let mut transaction = connection.transaction()?;
        let context = load_retained_semantic_context(
            root.path(),
            &transaction,
            RetainedSemanticLoadPolicy {
                source_contract: SemanticSourceContract::new(PassageProjection::RawPassagesV2),
                vectors: SemanticSelectionBudget::default(),
                ann: AnnAdmissionBudget::default(),
            },
            None,
            |_, _| panic!("an absent publication must not initialize a model"),
        )?;
        assert!(context.is_none());
        transaction.rollback()?;
        Ok(())
    }

    #[test]
    fn retained_loader_rejects_source_before_publication_or_model() -> anyhow::Result<()> {
        let root = tempdir()?;
        let pointer = SemanticCurrentPointerV1::path(root.path());
        std::fs::create_dir_all(pointer.parent().expect("current pointer has a parent"))?;
        std::fs::write(pointer, b"publication metadata must not be read yet")?;
        let connection = crate::franken_sync::Connection::open(":memory:")?;
        let mut transaction = connection.transaction()?;
        let error = load_retained_semantic_context(
            root.path(),
            &transaction,
            RetainedSemanticLoadPolicy {
                source_contract: SemanticSourceContract::new(PassageProjection::RawPassagesV2),
                vectors: SemanticSelectionBudget::default(),
                ann: AnnAdmissionBudget::default(),
            },
            None,
            |_, _| panic!("an uncertified source must not initialize a model"),
        )
        .expect_err("a present publication requires independent source authority");
        assert!(matches!(error, RetainedSemanticLoadError::Source(_)));
        transaction.rollback()?;
        Ok(())
    }

    #[test]
    fn retained_loader_rejects_bad_publication_after_actual_source_certificate()
    -> anyhow::Result<()> {
        use crate::search::semantic_manifest::source_snapshot::{
            persist_semantic_source_certificate_in_transaction,
            semantic_source_snapshot_in_transaction,
        };

        let root = tempdir()?;
        let database = root.path().join("retained-loader-source.db");
        let storage = FrankenStorage::open(&database)?;
        storage.raw().execute_batch(
            "INSERT INTO agents(id, slug, name, kind, created_at, updated_at)
                 VALUES(9101, 'loader', 'loader', 'cli', 1, 1);
             INSERT INTO conversations(id, agent_id, source_path)
                 VALUES(9102, 9101, '/loader.jsonl');
             INSERT INTO messages(id, conversation_id, idx, role, content)
                 VALUES(9103, 9102, 0, 'user', 'actual loader source certificate');",
        )?;
        let contract = SemanticSourceContract::new(PassageProjection::RawPassagesV2);
        let mut transaction = storage.raw().transaction()?;
        let source =
            semantic_source_snapshot_in_transaction(&transaction, contract, Default::default())?;
        persist_semantic_source_certificate_in_transaction(&transaction, &source)?;
        transaction.commit()?;
        drop(transaction);
        storage.close()?;

        let pointer = SemanticCurrentPointerV1::path(root.path());
        std::fs::create_dir_all(pointer.parent().expect("current pointer has a parent"))?;
        std::fs::write(pointer, b"malformed current publication")?;
        let storage = FrankenStorage::open_strict_readonly(&database)?;
        let mut transaction = storage.raw().transaction()?;
        let error = load_retained_semantic_context(
            root.path(),
            &transaction,
            RetainedSemanticLoadPolicy {
                source_contract: contract,
                vectors: SemanticSelectionBudget::default(),
                ann: AnnAdmissionBudget::default(),
            },
            None,
            |_, _| panic!("a malformed publication must not initialize a model"),
        )
        .expect_err("a valid source certificate does not authorize malformed publication metadata");
        assert!(matches!(error, RetainedSemanticLoadError::Publication(_)));
        assert_eq!(
            error.reason_code(),
            "semantic_publication_metadata_rejected"
        );
        transaction.rollback()?;
        drop(transaction);
        storage.close_without_checkpoint()?;
        Ok(())
    }

    #[test]
    fn retained_loader_empty_publication_skips_factory_and_revalidates_source() -> anyhow::Result<()>
    {
        use crate::search::semantic_manifest::source_snapshot::{
            persist_semantic_source_certificate_in_transaction,
            semantic_source_snapshot_in_transaction,
        };
        use crate::search::semantic_manifest::{
            SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION, SemanticExpectedCurrentV1,
            SemanticGenerationStatus, SemanticGenerationTopology, SemanticProducerAttestation,
        };

        let root = tempdir()?;
        let storage = FrankenStorage::open(&root.path().join("empty-loader-source.db"))?;
        let contract = SemanticSourceContract::new(PassageProjection::RawPassagesV2);
        let mut transaction = storage.raw().transaction()?;
        let source =
            semantic_source_snapshot_in_transaction(&transaction, contract, Default::default())?;
        assert_eq!(source.identity().document_count, 0);
        persist_semantic_source_certificate_in_transaction(&transaction, &source)?;
        transaction.commit()?;
        drop(transaction);

        // Publication provenance is a test declaration, not native producer
        // proof. Source authority comes from the actual migrated empty archive.
        let manifest = SemanticGenerationManifestV1 {
            schema_version: SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION,
            purpose: SemanticGenerationPurpose::EmptyCorpus,
            generation_id: "empty-loader-generation".into(),
            build_id: "empty-loader-build".into(),
            request_id: None,
            previous_generation_id: None,
            created_at_ms: 1,
            completed_at_ms: 2,
            sealed_at_ms: 3,
            requested_topology: SemanticGenerationTopology::QualityOnly,
            realized_topology: SemanticGenerationTopology::QualityOnly,
            status: SemanticGenerationStatus::Sealed,
            corpus: source.identity().clone(),
            producer: SemanticProducerAttestation {
                cass_git_commit: "a".repeat(40),
                frankensearch_git_commit: "b".repeat(40),
                cargo_lock_sha256: "c".repeat(64),
                rust_toolchain: "loader-test-toolchain".into(),
                enabled_features: vec!["semantic".into()],
                build_profile: "test".into(),
                binary_elf_sha256: "d".repeat(64),
            },
            selected_document_count: 0,
            total_vector_slot_count_across_tiers: 0,
            total_live_vector_count_across_tiers: 0,
            total_tombstone_vector_count_across_tiers: 0,
            total_wal_entry_count_across_tiers: 0,
            artifacts: Vec::new(),
        };
        manifest.write_immutable(root.path())?;
        let pointer = SemanticCurrentPointerV1::for_manifest(&manifest, 4, 1)?;
        pointer.publish_atomic(root.path(), &SemanticExpectedCurrentV1::Absent)?;

        let mut transaction = storage.raw().transaction()?;
        let outcome = load_retained_semantic_context(
            root.path(),
            &transaction,
            RetainedSemanticLoadPolicy {
                source_contract: contract,
                vectors: SemanticSelectionBudget {
                    max_declared_vector_bytes: 0,
                },
                ann: AnnAdmissionBudget {
                    max_declared_graph_bytes: 0,
                },
            },
            None,
            |_, _| panic!("an explicit empty publication must not initialize a model"),
        )?
        .expect("the selected empty publication exists");
        let RetainedSemanticLoadOutcome::Empty(context) = &outcome else {
            panic!("empty source must not acquire vector or producer owners");
        };
        assert_eq!(context.pointer(), &pointer);
        assert_eq!(context.manifest(), &manifest);
        assert_eq!(context.source_snapshot().identity(), source.identity());
        assert!(context.manifest().artifacts.is_empty());
        outcome.verify_source_in_transaction(&transaction)?;
        transaction.rollback()?;
        drop(transaction);

        storage.raw().execute_batch(
            "INSERT INTO agents(id, slug, name, kind, created_at, updated_at)
                 VALUES(9201, 'empty-loader', 'empty-loader', 'cli', 1, 1);
             INSERT INTO conversations(id, agent_id, source_path)
                 VALUES(9202, 9201, '/empty-loader.jsonl');
             INSERT INTO messages(id, conversation_id, idx, role, content)
                 VALUES(9203, 9202, 0, 'user', 'new source after empty publication');",
        )?;
        let mut transaction = storage.raw().transaction()?;
        let current =
            semantic_source_snapshot_in_transaction(&transaction, contract, Default::default())?;
        persist_semantic_source_certificate_in_transaction(&transaction, &current)?;
        transaction.commit()?;
        drop(transaction);
        let mut transaction = storage.raw().transaction()?;
        assert!(matches!(
            outcome.verify_source_in_transaction(&transaction),
            Err(RetainedSemanticLoadError::SourceChanged)
        ));
        transaction.rollback()?;
        drop(transaction);
        storage.close()?;
        Ok(())
    }

    #[test]
    fn test_semantic_availability_ready() {
        let ready = SemanticAvailability::Ready {
            embedder_id: "test-123".into(),
        };
        assert!(ready.summary().contains("semantic ready"));
        assert!(ready.is_ready());
        assert!(!ready.has_update());
        assert!(ready.can_search());
        assert_eq!(ready.status_label(), "SEM");
    }

    #[test]
    fn semantic_sidecar_path_rejects_paths_outside_data_dir() {
        let tmp = tempdir().unwrap();
        let safe = semantic_sidecar_path(tmp.path(), "vector_index/shards/hash/shard-0.fsvi")
            .expect("safe relative shard path");
        assert_eq!(
            safe,
            tmp.path().join("vector_index/shards/hash/shard-0.fsvi")
        );

        for unsafe_path in [
            tmp.path()
                .join("outside.fsvi")
                .to_string_lossy()
                .to_string(),
            "../outside.fsvi".to_string(),
            "vector_index/../outside.fsvi".to_string(),
            "./vector_index/shards/hash/shard-0.fsvi".to_string(),
        ] {
            assert!(
                semantic_sidecar_path(tmp.path(), &unsafe_path).is_none(),
                "unsafe semantic sidecar path should be rejected: {unsafe_path}"
            );
        }
    }

    #[test]
    fn test_semantic_availability_update() {
        let update = SemanticAvailability::UpdateAvailable {
            embedder_id: "test".into(),
            current_revision: "v1".into(),
            latest_revision: "v2".into(),
        };
        assert!(update.summary().contains("update available"));
        assert!(!update.is_ready());
        assert!(update.has_update());
        assert_eq!(update.status_label(), "UPD");
    }

    #[test]
    fn cache_state_preserves_selected_embedder_identity() {
        let model_dir = Path::new("/tmp/cass/models/multilingual-minilm");
        let missing = semantic_availability_from_cache_state(
            "multilingual-minilm-384",
            model_dir,
            &ModelCacheState::NotAcquired {
                missing_files: vec!["model.safetensors".to_string()],
                needs_consent: false,
            },
            true,
        )
        .expect("missing cache state");
        assert!(matches!(
            missing,
            SemanticAvailability::ModelMissing { embedder_id, model_dir: actual_dir, .. }
                if embedder_id == "multilingual-minilm-384" && actual_dir == model_dir
        ));

        let update = semantic_availability_from_cache_state(
            "multilingual-minilm-384",
            model_dir,
            &ModelCacheState::IncompatibleVersion {
                current_revision: "old".to_string(),
                expected_revision: "new".to_string(),
            },
            true,
        )
        .expect("update cache state");
        assert!(matches!(
            update,
            SemanticAvailability::UpdateAvailable { embedder_id, .. }
                if embedder_id == "multilingual-minilm-384"
        ));
    }

    #[test]
    fn test_semantic_availability_index_building() {
        let building = SemanticAvailability::IndexBuilding {
            embedder_id: "test".into(),
            progress_pct: Some(45),
            items_indexed: 100,
            total_items: 200,
        };
        assert!(building.summary().contains("building index"));
        assert!(building.summary().contains("45%"));
        assert!(building.is_building());
        assert_eq!(building.status_label(), "IDX...");

        let (pct, done, total) = building.index_progress().unwrap();
        assert_eq!(pct, Some(45));
        assert_eq!(done, 100);
        assert_eq!(total, 200);
    }

    #[test]
    fn test_semantic_availability_downloading() {
        let downloading = SemanticAvailability::Downloading {
            progress_pct: 50,
            bytes_downloaded: 10_000_000,
            total_bytes: 20_000_000,
        };
        assert!(downloading.is_downloading());
        assert!(downloading.summary().contains("downloading"));
        assert!(downloading.summary().contains("50%"));
        assert_eq!(downloading.status_label(), "DL...");

        let (pct, bytes, total) = downloading.download_progress().unwrap();
        assert_eq!(pct, 50);
        assert_eq!(bytes, 10_000_000);
        assert_eq!(total, 20_000_000);
    }

    #[test]
    fn test_semantic_availability_tui_states() {
        let cases: &[AvailabilityTuiCase] = &[
            (
                SemanticAvailability::NotInstalled,
                "LEX",
                SemanticAvailability::is_not_installed,
            ),
            (
                SemanticAvailability::NeedsConsent,
                "LEX",
                SemanticAvailability::needs_consent,
            ),
            (SemanticAvailability::Verifying, "VFY...", |state| {
                state.summary().contains("verifying")
            }),
            (SemanticAvailability::HashFallback, "SEM*", |state| {
                state.is_hash_fallback() && state.can_search()
            }),
            (
                SemanticAvailability::Disabled {
                    reason: "offline mode".into(),
                },
                "OFF",
                |state| state.is_disabled() && state.summary().contains("offline"),
            ),
        ];

        for (state, expected_label, predicate) in cases {
            assert_eq!(state.status_label(), *expected_label, "{state:?}");
            assert!(predicate(state), "{state:?}");
        }
    }

    #[test]
    fn test_semantic_availability_error_states() {
        let load_failed = SemanticAvailability::LoadFailed {
            context: "test error".into(),
        };
        assert!(load_failed.is_error());
        assert_eq!(load_failed.status_label(), "ERR");

        let db_unavail = SemanticAvailability::DatabaseUnavailable {
            db_path: PathBuf::from("/test"),
            error: "locked".into(),
        };
        assert!(db_unavail.is_error());
        assert_eq!(db_unavail.status_label(), "NODB");
    }

    #[test]
    fn test_needs_index_rebuild_no_index() {
        let tmp = tempdir().unwrap();
        assert!(!needs_index_rebuild(tmp.path()));
    }

    #[test]
    fn rebuild_probe_preserves_legacy_bytes_and_directory_entries() -> anyhow::Result<()> {
        let tmp = tempdir()?;
        let path = vector_index_path(tmp.path(), FastEmbedder::embedder_id_static());
        std::fs::create_dir_all(path.parent().expect("vector parent"))?;
        let revision = expected_vector_space_revision(FastEmbedder::embedder_id_static())
            .ok_or_else(|| anyhow::anyhow!("MiniLM vector-space revision is missing"))?;
        VectorIndex::create_with_revision(
            &path,
            FastEmbedder::embedder_id_static(),
            revision,
            384,
            frankensearch::index::Quantization::F16,
        )?
        .finish()?;
        let before = std::fs::read(&path)?;
        let modified = std::fs::metadata(&path)?.modified()?;
        let entries = |parent: &Path| -> std::io::Result<Vec<std::ffi::OsString>> {
            let mut names = std::fs::read_dir(parent)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<std::io::Result<Vec<_>>>()?;
            names.sort();
            Ok(names)
        };
        let parent = path.parent().expect("vector parent");
        let before_entries = entries(parent)?;
        for _ in 0..2 {
            assert!(!needs_index_rebuild(tmp.path()));
        }
        assert_eq!(std::fs::read(&path)?, before);
        assert_eq!(std::fs::metadata(&path)?.modified()?, modified);
        assert_eq!(entries(parent)?, before_entries);

        // An invalid journal must not be truncated, replayed or replaced by a
        // compatibility probe, even when it makes the probe fail closed.
        let wal = frankensearch::index::wal_path_for(&path);
        std::fs::write(&wal, b"owned malformed WAL control")?;
        let wal_before = std::fs::read(&wal)?;
        let entries_with_wal = entries(parent)?;
        let _ = needs_index_rebuild(tmp.path());
        assert_eq!(std::fs::read(&wal)?, wal_before);
        assert_eq!(std::fs::read(&path)?, before);
        assert_eq!(std::fs::metadata(&path)?.modified()?, modified);
        assert_eq!(entries(parent)?, entries_with_wal);
        Ok(())
    }

    #[test]
    fn rebuild_probe_does_not_bypass_selected_generation() -> anyhow::Result<()> {
        let tmp = tempdir()?;
        let pointer = SemanticCurrentPointerV1::path(tmp.path());
        std::fs::create_dir_all(pointer.parent().expect("pointer parent"))?;
        std::fs::write(&pointer, b"owned malformed current pointer")?;
        assert!(
            needs_index_rebuild(tmp.path()),
            "selection must not look like an absent legacy index"
        );

        let legacy = vector_index_path(tmp.path(), FastEmbedder::embedder_id_static());
        std::fs::create_dir_all(legacy.parent().expect("legacy parent"))?;
        let revision = expected_vector_space_revision(FastEmbedder::embedder_id_static())
            .ok_or_else(|| anyhow::anyhow!("MiniLM vector-space revision is missing"))?;
        VectorIndex::create_with_revision(
            &legacy,
            FastEmbedder::embedder_id_static(),
            revision,
            384,
            frankensearch::index::Quantization::F16,
        )?
        .finish()?;
        let before = std::fs::read(&legacy)?;
        assert!(
            needs_index_rebuild(tmp.path()),
            "compatible legacy bytes must not override current selection"
        );
        assert_eq!(std::fs::read(&legacy)?, before);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let dangling = tempdir()?;
            let pointer = SemanticCurrentPointerV1::path(dangling.path());
            std::fs::create_dir_all(pointer.parent().expect("dangling pointer parent"))?;
            symlink(dangling.path().join("absent-target"), &pointer)?;
            assert!(
                needs_index_rebuild(dangling.path()),
                "dangling selection is not absence"
            );
        }
        Ok(())
    }

    #[test]
    fn test_delete_vector_index_no_file() {
        let tmp = tempdir().unwrap();
        let result = delete_vector_index_for_rebuild(tmp.path());
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    fn write_hash_vector_index(path: &Path, record_count: usize) {
        let embedder = HashEmbedder::default();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create vector index parent");
        }
        let mut writer = VectorIndex::create_with_revision(
            path,
            embedder.id(),
            crate::indexer::semantic::HASH_VECTOR_SPACE_REVISION,
            embedder.dimension(),
            frankensearch::index::Quantization::F16,
        )
        .expect("create hash vector index");
        let mut vector = vec![0.0_f32; embedder.dimension()];
        vector[0] = 1.0;
        for idx in 0..record_count {
            writer
                .write_record(&format!("doc-{idx}"), &vector)
                .expect("write hash vector record");
        }
        writer.finish().expect("finish hash vector index");
    }

    #[test]
    fn exact_artifact_contract_pairs_only_existing_consumer_owned_ann() {
        let tmp = tempdir().expect("hash context fixture");
        let db_path = tmp.path().join("cass.db");
        let storage = FrankenStorage::open(&db_path).expect("create cass db");
        drop(storage);

        let embedder = HashEmbedder::default();
        let fsvi_path = vector_index_path(tmp.path(), embedder.id());
        write_hash_vector_index(&fsvi_path, 1);

        let without_ann = load_hash_semantic_context(tmp.path(), &db_path)
            .context
            .expect("hash context without ANN");
        assert_eq!(without_ann.artifacts.len(), 1);
        assert_eq!(without_ann.artifacts[0].fsvi_path(), fsvi_path);
        assert_eq!(
            without_ann.artifacts[0].ann_path(),
            None,
            "a synthesized-but-missing ANN filename must not become a serving artifact"
        );

        let ann_path = hnsw_index_path(tmp.path(), embedder.id());
        std::fs::write(&ann_path, b"selected CASS ANN sidecar fixture")
            .expect("write selected ANN sidecar");
        let with_ann = load_hash_semantic_context(tmp.path(), &db_path)
            .context
            .expect("hash context with ANN");
        assert_eq!(
            with_ann.artifacts[0].ann_path(),
            Some(ann_path.as_path()),
            "an existing CASS-owned ANN path must stay paired with its exact FSVI"
        );
    }

    /// GH #404: a vector index that parses but whose manifest record was
    /// built against a different database fingerprint must not be served —
    /// `cass status` already reports `can_search:false` for it, and the
    /// query path must agree instead of returning k arbitrary neighbours.
    #[test]
    fn stale_manifest_tier_is_refused_even_when_the_artifact_loads() {
        use crate::search::semantic_manifest::{SemanticManifest, TierKind};

        let tmp = tempdir().expect("stale context fixture");
        let db_path = tmp.path().join("cass.db");
        let storage = FrankenStorage::open(&db_path).expect("create cass db");
        drop(storage);

        let embedder = HashEmbedder::default();
        let fsvi_path = vector_index_path(tmp.path(), embedder.id());
        write_hash_vector_index(&fsvi_path, 1);
        let ann_path = hnsw_index_path(tmp.path(), embedder.id());
        std::fs::write(&ann_path, b"selected CASS ANN sidecar fixture")
            .expect("write selected ANN sidecar");

        // Control: with no manifest at all, the legacy manifest-less artifact
        // still serves (no verdict contradicts it).
        assert!(
            load_hash_semantic_context(tmp.path(), &db_path)
                .context
                .is_some(),
            "manifest-less artifacts keep serving"
        );

        // Canonical identity rewrites (such as Pi -> OMP reclassification)
        // do not change the count/max-id fingerprint. The durable storage
        // marker must therefore override even the legacy manifest-less
        // serving allowance until a semantic republish acknowledges it.
        let storage = FrankenStorage::open(&db_path).expect("open identity marker fixture");
        storage
            .mark_semantic_identity_rebuild_required(SemanticIdentityTier::Fast)
            .expect("mark semantic identity stale");
        drop(storage);
        let identity_stale = load_hash_semantic_context(tmp.path(), &db_path);
        assert!(identity_stale.context.is_none());
        assert!(
            identity_stale.availability.is_index_stale(),
            "identity-invalidated semantic assets must fail closed: {:?}",
            identity_stale.availability
        );
        let storage = FrankenStorage::open(&db_path).expect("reopen identity marker fixture");
        storage
            .complete_semantic_identity_rebuild(SemanticIdentityTier::Fast)
            .expect("complete semantic identity rebuild");
        drop(storage);
        assert!(
            load_hash_semantic_context(tmp.path(), &db_path)
                .context
                .is_some(),
            "an acknowledged semantic republish should restore manifest-less serving"
        );

        // A manifest record whose fingerprint belongs to some other database.
        let mut manifest = SemanticManifest::load_or_default(tmp.path()).expect("manifest");
        assert!(manifest.adopt_legacy_artifact(
            TierKind::Fast,
            embedder.id(),
            "hash",
            embedder.dimension(),
            1,
            1,
            "content-v1:999999:999999:999999",
            "vector_index/index-fnv1a-384.fsvi",
            1,
        ));
        manifest.save(tmp.path()).expect("save stale manifest");

        let setup = load_hash_semantic_context(tmp.path(), &db_path);
        assert!(
            setup.context.is_none(),
            "stale tier must not produce a serving context"
        );
        assert!(
            setup.availability.is_index_stale(),
            "expected IndexStale, got {:?}",
            setup.availability
        );
        assert!(
            !setup.availability.can_search(),
            "stale assets must report can_search=false to the query path"
        );

        // Once the record matches the live database again, serving resumes.
        let current = crate::indexer::lexical_storage_fingerprint_for_db(&db_path)
            .expect("current fingerprint");
        let mut manifest = SemanticManifest::load_or_default(tmp.path()).expect("manifest");
        assert!(manifest.adopt_legacy_artifact(
            TierKind::Fast,
            embedder.id(),
            "hash",
            embedder.dimension(),
            1,
            1,
            &current,
            "vector_index/index-fnv1a-384.fsvi",
            1,
        ));
        manifest.save(tmp.path()).expect("save current manifest");
        assert!(
            load_hash_semantic_context(tmp.path(), &db_path)
                .context
                .is_some(),
            "a current tier record must serve again"
        );
    }

    #[cfg(unix)]
    #[test]
    fn readiness_and_serving_reject_the_same_final_component_fsvi_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = tempdir().expect("FSVI symlink fixture");
        let db_path = tmp.path().join("cass.db");
        let storage = FrankenStorage::open(&db_path).expect("create cass db");
        drop(storage);

        let embedder = HashEmbedder::default();
        let selected_path = vector_index_path(tmp.path(), embedder.id());
        let target_path = selected_path.with_file_name("unselected-target.fsvi");
        write_hash_vector_index(&target_path, 1);
        let target_bytes = std::fs::read(&target_path).expect("read target FSVI before probes");
        symlink(&target_path, &selected_path).expect("create final-component FSVI symlink");

        let serving_error = SemanticIndexArtifact::open(&selected_path, None)
            .expect_err("serving must reject a final-component FSVI symlink");
        assert!(serving_error.to_string().contains("symlink"));

        let readiness = probe_hash_semantic_availability(tmp.path());
        let readiness_context = match readiness {
            SemanticAvailability::LoadFailed { context } => context,
            other => panic!("readiness must reject the same FSVI symlink: {other:?}"),
        };
        let serving = load_hash_semantic_context(tmp.path(), &db_path);
        let serving_context = match serving.availability {
            SemanticAvailability::LoadFailed { context } => context,
            other => panic!("semantic serving must reject the same FSVI symlink: {other:?}"),
        };

        assert_eq!(
            readiness_context, serving_context,
            "readiness and serving must use one exact-artifact admission policy"
        );
        assert!(readiness_context.contains("symlink"));
        assert!(
            serving.context.is_none(),
            "a rejected exact artifact must never produce a serving context"
        );
        assert_eq!(
            std::fs::read_link(&selected_path).expect("read selected FSVI symlink"),
            target_path
        );
        assert_eq!(
            std::fs::read(&target_path).expect("read target FSVI after probes"),
            target_bytes,
            "readiness and serving probes must not mutate the symlink target"
        );

        let broken = tempdir().expect("broken FSVI symlink fixture");
        let broken_db_path = broken.path().join("cass.db");
        let storage = FrankenStorage::open(&broken_db_path).expect("create broken fixture db");
        drop(storage);
        let broken_path = vector_index_path(broken.path(), embedder.id());
        std::fs::create_dir_all(
            broken_path
                .parent()
                .expect("broken selected FSVI must have a parent"),
        )
        .expect("create broken selected FSVI parent");
        symlink(broken.path().join("absent-target.fsvi"), &broken_path)
            .expect("create broken final-component FSVI symlink");

        let broken_readiness = probe_hash_semantic_availability(broken.path());
        let broken_serving = load_hash_semantic_context(broken.path(), &broken_db_path);
        let broken_readiness_context = match broken_readiness {
            SemanticAvailability::LoadFailed { context } => context,
            other => panic!("broken FSVI symlink must be rejected by readiness: {other:?}"),
        };
        let broken_serving_context = match broken_serving.availability {
            SemanticAvailability::LoadFailed { context } => context,
            other => panic!("broken FSVI symlink must be rejected by serving: {other:?}"),
        };
        assert_eq!(broken_readiness_context, broken_serving_context);
        assert!(broken_readiness_context.contains("symlink"));
        assert!(broken_serving.context.is_none());

        let non_file = tempdir().expect("non-file FSVI fixture");
        let non_file_db_path = non_file.path().join("cass.db");
        let storage = FrankenStorage::open(&non_file_db_path).expect("create non-file fixture db");
        drop(storage);
        let non_file_path = vector_index_path(non_file.path(), embedder.id());
        std::fs::create_dir_all(
            non_file_path
                .parent()
                .expect("non-file selected FSVI must have a parent"),
        )
        .expect("create non-file selected FSVI parent");
        std::fs::create_dir(&non_file_path).expect("create directory at selected FSVI path");

        let non_file_readiness = probe_hash_semantic_availability(non_file.path());
        let non_file_serving = load_hash_semantic_context(non_file.path(), &non_file_db_path);
        let non_file_readiness_context = match non_file_readiness {
            SemanticAvailability::LoadFailed { context } => context,
            other => panic!("non-file FSVI must be rejected by readiness: {other:?}"),
        };
        let non_file_serving_context = match non_file_serving.availability {
            SemanticAvailability::LoadFailed { context } => context,
            other => panic!("non-file FSVI must be rejected by serving: {other:?}"),
        };
        assert_eq!(non_file_readiness_context, non_file_serving_context);
        assert!(
            non_file_readiness_context.contains("open semantic FSVI"),
            "non-file selection must reach the exact-artifact opener: {non_file_readiness_context}"
        );
        assert!(non_file_serving.context.is_none());
    }

    #[test]
    fn exact_artifact_contract_ann_selection_preserves_existing_invalid_artifacts() {
        let tmp = tempdir().expect("ANN selection fixture");
        let missing = tmp.path().join("missing.chsw");
        assert_eq!(select_existing_ann_candidate(missing), None);

        let regular = tmp.path().join("regular.chsw");
        std::fs::write(&regular, b"not necessarily valid ANN metadata")
            .expect("write ANN candidate");
        assert_eq!(
            select_existing_ann_candidate(regular.clone()),
            Some(regular)
        );

        let directory = tmp.path().join("directory.chsw");
        std::fs::create_dir(&directory).expect("create invalid ANN directory");
        assert_eq!(
            select_existing_ann_candidate(directory.clone()),
            Some(directory),
            "an existing invalid candidate must reach bounded rejection instead of being mislabeled missing"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let broken_symlink = tmp.path().join("broken-symlink.chsw");
            symlink(tmp.path().join("absent-target"), &broken_symlink)
                .expect("create broken ANN symlink");
            assert_eq!(
                select_existing_ann_candidate(broken_symlink.clone()),
                Some(broken_symlink),
                "a selected final-component symlink must reach explicit rejection"
            );
        }
    }

    #[test]
    fn exact_artifact_contract_pointer_selected_generation_requires_retained_owner() {
        let tmp = tempdir().expect("pointer-selected fixture");
        let db_path = tmp.path().join("cass.db");
        let storage = FrankenStorage::open(&db_path).expect("create cass db");
        drop(storage);

        let embedder = HashEmbedder::default();
        let legacy_path = vector_index_path(tmp.path(), embedder.id());
        write_hash_vector_index(&legacy_path, 1);
        let legacy_bytes = std::fs::read(&legacy_path).expect("read legacy FSVI");
        let legacy_mtime = std::fs::metadata(&legacy_path)
            .and_then(|metadata| metadata.modified())
            .expect("legacy FSVI mtime");

        let pointer = SemanticCurrentPointerV1 {
            schema_version: 1,
            generation_id: "gen-selected-owner-contract".to_string(),
            manifest_sha256: "a".repeat(64),
            selection_epoch: 1,
            selected_at_ms: 1_733_100_000_000,
        };
        let pointer_path = SemanticCurrentPointerV1::path(tmp.path());
        std::fs::create_dir_all(pointer_path.parent().expect("pointer parent"))
            .expect("create pointer parent");
        std::fs::write(
            &pointer_path,
            serde_json::to_vec(&pointer).expect("serialize selected pointer"),
        )
        .expect("write selected pointer");
        let mut entries_before = std::fs::read_dir(tmp.path())
            .expect("list fixture before load")
            .map(|entry| entry.expect("fixture entry").file_name())
            .collect::<Vec<_>>();
        entries_before.sort();

        for availability in [
            probe_hash_semantic_availability(tmp.path()),
            probe_semantic_availability(tmp.path()),
            load_hash_semantic_context(tmp.path(), &db_path).availability,
            load_semantic_context(tmp.path(), &db_path).availability,
        ] {
            assert!(
                matches!(
                    &availability,
                    SemanticAvailability::LoadFailed { context }
                        if context
                            == SemanticProgressiveUnavailableReason::OwnerBackedReaderRequired
                                .code()
                ),
                "pointer-selected generation must fail closed with the stable owner reason: {availability:?}"
            );
        }
        assert!(
            load_hash_semantic_context(tmp.path(), &db_path)
                .context
                .is_none(),
            "a valid legacy file must not bypass the selected-generation owner requirement"
        );
        assert_eq!(
            std::fs::read(&legacy_path).expect("read legacy FSVI after rejected load"),
            legacy_bytes
        );
        assert_eq!(
            std::fs::metadata(&legacy_path)
                .and_then(|metadata| metadata.modified())
                .expect("legacy FSVI mtime after rejected load"),
            legacy_mtime
        );
        let mut entries_after = std::fs::read_dir(tmp.path())
            .expect("list fixture after load")
            .map(|entry| entry.expect("fixture entry").file_name())
            .collect::<Vec<_>>();
        entries_after.sort();
        assert_eq!(
            entries_after, entries_before,
            "fail-closed pointer handling must not create serving aliases"
        );
    }

    #[test]
    fn same_id_and_dimension_with_legacy_revision_requires_rebuild() -> anyhow::Result<()> {
        let tmp = tempdir()?;
        let index_path = vector_index_path(tmp.path(), FastEmbedder::embedder_id_static());
        let Some(parent) = index_path.parent() else {
            anyhow::bail!("vector index path has no parent");
        };
        std::fs::create_dir_all(parent)?;

        for legacy in [
            "1.0",
            "native-minilm-v1:c9745ed1d9f207416be6d2e6f8de32d1f16199bf",
        ] {
            let writer = VectorIndex::create_with_revision(
                &index_path,
                FastEmbedder::embedder_id_static(),
                legacy,
                384,
                frankensearch::index::Quantization::F16,
            )?;
            writer.finish()?;
            assert!(needs_index_rebuild(tmp.path()));
        }

        let expected_revision = expected_vector_space_revision(FastEmbedder::embedder_id_static())
            .ok_or_else(|| anyhow::anyhow!("MiniLM vector-space revision is not registered"))?;
        let writer = VectorIndex::create_with_revision(
            &index_path,
            FastEmbedder::embedder_id_static(),
            expected_revision,
            384,
            frankensearch::index::Quantization::F16,
        )?;
        writer.finish()?;
        assert!(!needs_index_rebuild(tmp.path()));
        Ok(())
    }

    fn semantic_shard_record(
        tier: TierKind,
        embedder_id: &str,
        db_fingerprint: &str,
        shard_index: u32,
        shard_count: u32,
    ) -> SemanticShardRecord {
        SemanticShardRecord {
            tier,
            embedder_id: embedder_id.to_string(),
            model_revision: "test-revision".to_string(),
            schema_version: crate::search::policy::SEMANTIC_SCHEMA_VERSION,
            chunking_version: crate::search::policy::CHUNKING_STRATEGY_VERSION,
            dimension: 384,
            shard_index,
            shard_count,
            doc_count: 1,
            total_conversations: 1,
            db_fingerprint: db_fingerprint.to_string(),
            index_path: format!("vector_index/shards/{embedder_id}/shard-{shard_index}.fsvi"),
            quantization: "f16".to_string(),
            mmap_ready: true,
            ann_index_path: None,
            ann_size_bytes: 0,
            ann_ready: false,
            size_bytes: 128,
            started_at_ms: 1_733_100_000_000,
            completed_at_ms: 1_733_100_000_000 + i64::from(shard_index),
            ready: true,
        }
    }

    #[test]
    fn shard_candidate_probe_is_false_without_manifest() {
        let tmp = tempdir().unwrap();
        assert!(
            !complete_shard_generation_candidate_exists(tmp.path(), "fnv1a-384"),
            "missing shard manifest must not trigger a current-DB fingerprint"
        );
        assert!(
            load_complete_shard_artifacts_with_fingerprint(
                tmp.path(),
                "fnv1a-384",
                384,
                "test",
                || panic!("missing shards must not open the archive for fingerprinting"),
            )
            .is_none()
        );
    }

    #[test]
    fn shard_candidate_probe_is_false_for_unreadable_manifest() {
        let tmp = tempdir().unwrap();
        let path = SemanticShardManifest::path(tmp.path());
        std::fs::create_dir_all(path.parent().expect("manifest parent"))
            .expect("create shard manifest dir");
        std::fs::write(&path, b"not json").expect("write invalid shard manifest");

        assert!(
            !complete_shard_generation_candidate_exists(tmp.path(), "fnv1a-384"),
            "corrupt shard metadata must not trigger a query-time current-DB fingerprint"
        );
        assert!(
            load_complete_shard_artifacts_with_fingerprint(
                tmp.path(),
                "fnv1a-384",
                384,
                "test",
                || panic!("invalid shards must not open the archive for fingerprinting"),
            )
            .is_none()
        );
    }

    #[test]
    fn shard_candidate_probe_ignores_other_or_incomplete_generations() {
        let tmp = tempdir().unwrap();
        let mut manifest = SemanticShardManifest {
            shards: vec![
                semantic_shard_record(TierKind::Fast, "other-384", "fp-other", 0, 1),
                semantic_shard_record(TierKind::Fast, "fnv1a-384", "fp-partial", 0, 2),
            ],
            ..Default::default()
        };
        manifest.save(tmp.path()).expect("save shard manifest");

        assert!(
            !complete_shard_generation_candidate_exists(tmp.path(), "fnv1a-384"),
            "incomplete or unrelated shard generations must not trigger a current-DB fingerprint"
        );
        assert!(
            load_complete_shard_artifacts_with_fingerprint(
                tmp.path(),
                "fnv1a-384",
                384,
                "test",
                || panic!("incomplete or unrelated shards must not fingerprint the archive"),
            )
            .is_none()
        );
    }

    #[test]
    fn shard_candidate_probe_detects_complete_generation_for_embedder() {
        let tmp = tempdir().unwrap();
        let mut manifest = SemanticShardManifest {
            shards: vec![
                semantic_shard_record(TierKind::Fast, "fnv1a-384", "fp-current", 0, 2),
                semantic_shard_record(TierKind::Fast, "fnv1a-384", "fp-current", 1, 2),
            ],
            ..Default::default()
        };
        manifest.save(tmp.path()).expect("save shard manifest");

        assert!(
            complete_shard_generation_candidate_exists(tmp.path(), "fnv1a-384"),
            "complete candidate generations should allow the current-DB fingerprint check"
        );
        let fingerprint_calls = std::cell::Cell::new(0);
        assert!(
            load_complete_shard_artifacts_with_fingerprint(
                tmp.path(),
                "fnv1a-384",
                384,
                "test",
                || {
                    fingerprint_calls.set(fingerprint_calls.get() + 1);
                    Ok("fp-current".to_string())
                },
            )
            .is_none(),
            "metadata completeness alone must not admit missing shard artifacts"
        );
        assert_eq!(
            fingerprint_calls.get(),
            1,
            "a complete candidate must still fingerprint the current archive exactly once"
        );
    }

    #[test]
    fn load_hash_context_prefers_current_complete_shards_over_monolithic_file() {
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("cass.db");
        let storage = FrankenStorage::open(&db_path).expect("create cass db");
        drop(storage);
        let db_fingerprint = crate::indexer::lexical_storage_fingerprint_for_db(&db_path)
            .expect("fingerprint cass db");

        let embedder = HashEmbedder::default();
        write_hash_vector_index(&vector_index_path(tmp.path(), embedder.id()), 1);

        let mut records = Vec::new();
        for shard_index in 0..2_u32 {
            let relative_path = format!("vector_index/shards/hash/shard-{shard_index}.fsvi");
            let shard_path = tmp.path().join(&relative_path);
            write_hash_vector_index(&shard_path, 1);
            records.push(SemanticShardRecord {
                tier: TierKind::Fast,
                embedder_id: embedder.id().to_string(),
                model_revision: "hash".to_string(),
                schema_version: crate::search::policy::SEMANTIC_SCHEMA_VERSION,
                chunking_version: crate::search::policy::CHUNKING_STRATEGY_VERSION,
                dimension: embedder.dimension(),
                shard_index,
                shard_count: 2,
                doc_count: 1,
                total_conversations: 1,
                db_fingerprint: db_fingerprint.clone(),
                index_path: relative_path,
                quantization: "f16".to_string(),
                mmap_ready: true,
                ann_index_path: None,
                ann_size_bytes: 0,
                ann_ready: false,
                size_bytes: std::fs::metadata(&shard_path)
                    .expect("stat hash shard")
                    .len(),
                started_at_ms: 1_733_100_000_000,
                completed_at_ms: 1_733_100_000_000 + i64::from(shard_index),
                ready: true,
            });
        }
        let mut manifest = SemanticShardManifest {
            shards: records,
            ..Default::default()
        };
        manifest.save(tmp.path()).expect("save shard manifest");

        let setup = load_hash_semantic_context(tmp.path(), &db_path);
        assert!(
            matches!(setup.availability, SemanticAvailability::HashFallback),
            "hash semantic availability should remain ready: {:?}",
            setup.availability
        );
        let context = setup
            .context
            .expect("complete current shards should load a semantic context");
        assert_eq!(
            context.artifacts.len(),
            2,
            "complete current shards must not be shadowed by an older monolithic vector file"
        );
        let loaded_records = context
            .artifacts
            .iter()
            .map(|artifact| artifact.index().record_count())
            .sum::<usize>();
        assert_eq!(loaded_records, 2);
    }

    #[test]
    fn snapshot_fingerprint_matches_storage_fingerprint() -> anyhow::Result<()> {
        let tmp = tempdir()?;
        let db_path = tmp.path().join("snapshot-fingerprint.db");
        let storage = FrankenStorage::open(&db_path)?;
        storage.raw().execute_batch(
            "INSERT INTO agents(id, slug, name, kind, created_at, updated_at)
                 VALUES(8101, 'snapshot', 'snapshot', 'cli', 1, 1);
             INSERT INTO conversations(id, agent_id, source_path)
                 VALUES(8102, 8101, '/snapshot.jsonl');
             INSERT INTO messages(id, conversation_id, idx, role, content)
                 VALUES(8103, 8102, 0, 'user', 'first'),
                       (8104, 8102, 1, 'assistant', 'second');",
        )?;
        let snapshot = ConnectionReadSnapshot(storage.raw());
        // Semantic admission must fingerprint the same archive generation it
        // will hydrate from, so the snapshot path must agree byte-for-byte
        // with the storage adapter used by status and other callers.
        assert_eq!(
            lexical_fingerprint_from_snapshot(&snapshot)?,
            crate::indexer::lexical_storage_fingerprint_for_storage(&storage)?
        );
        Ok(())
    }
}
