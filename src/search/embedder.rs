//! Embedder trait and types for semantic search.
//!
//! This module re-exports the canonical [`Embedder`] trait from frankensearch's
//! [`SyncEmbed`](frankensearch::SyncEmbed) trait. All embedding implementations
//! must satisfy `Embedder`, which provides a synchronous embedding interface
//! suitable for cass's sync call sites.
//!
//! The [`SyncEmbedderAdapter`](frankensearch::SyncEmbedderAdapter) can wrap any
//! `Embedder` implementor into frankensearch's async `Embedder` trait when needed
//! for the frankensearch search pipeline.
//!
//! # Implementations
//!
//! - **Hash embedder**: FNV-1a feature hashing (always available, ~256 dimensions)
//! - **ML embedder**: FastEmbed with the MiniLM model (requires model download, 384 dimensions)

use std::fmt;
use std::sync::Arc;

use frankensearch::core::generation::GoldenVectorCertificateV1;
use frankensearch::core::generation::QuantizationFormat;
use frankensearch::core::{
    EmbeddingIdentityBundleV1, EmbeddingSpaceKindV1, IdentityBoundEmbedding,
};
use sha2::{Digest, Sha256};

use super::canonicalize::canonicalize_for_embedding;
use super::semantic_manifest::source_snapshot::SemanticSourceContract;

pub use frankensearch::SearchError as EmbedderError;
pub use frankensearch::SearchResult as EmbedderResult;
pub use frankensearch::SyncEmbed as Embedder;

/// Public, non-customer inputs used to measure the composed producer itself.
/// These check exact implementation output, not semantic relevance.
pub const CASS_PASSAGE_CONFORMANCE_TEXTS_V1: [&str; 6] = [
    "**Authentication** middleware validates bearer tokens.",
    "# Résumé\n\nCafe\u{301} 東京 Unicode normalization.",
    "```rust\nfn retry() { reconnect(); }\n```\nExplain the retry implementation.",
    "Open [the guide](https://example.invalid/guide) for configuration.",
    "  agent\tworkspace\nsource    projection  ",
    "> Preserve quoted diagnostic context.\n\nThen inspect the transaction.",
];

/// An actual CASS input transformation followed by an identity-aware producer.
///
/// Both document writers and query callers supply one **raw** passage. This
/// owner canonicalizes it exactly once, checks the inner producer before and
/// after its bound operation, and returns its own measured identity. It does
/// not select source rows or split messages; those operations belong to the
/// explicitly named source contract. Passing an already canonicalized passage
/// would describe a different composition and is not supported.
pub struct CassPassageProducer {
    inner: Arc<dyn Embedder>,
    inner_identity: EmbeddingIdentityBundleV1,
    identity: EmbeddingIdentityBundleV1,
    source_contract: SemanticSourceContract,
    id: String,
}

impl CassPassageProducer {
    /// Freeze the actual inner identity and measure a new composition
    /// certificate. This performs real inference on the public corpus; callers
    /// must admit that work after source/certificate validation and model policy.
    /// No model acquisition, selected-manifest identity, or vector substitution
    /// participates in construction.
    pub fn new(
        inner: Arc<dyn Embedder>,
        source_contract: SemanticSourceContract,
    ) -> EmbedderResult<Self> {
        let inner_identity = inner.identity()?.clone();
        inner_identity.validate()?;
        if inner.dimension() != inner_identity.space.dimension as usize
            || inner.is_semantic() != (inner_identity.space.kind == EmbeddingSpaceKindV1::Semantic)
            || inner_identity.storage.quantization != QuantizationFormat::F32
            || !inner_identity.storage.format.starts_with("in-memory-")
        {
            return Err(passage_producer_error(
                "inner producer metadata disagrees with its complete identity",
            ));
        }
        let vectors = CASS_PASSAGE_CONFORMANCE_TEXTS_V1
            .iter()
            .map(|raw| Self::embed_inner(inner.as_ref(), &inner_identity, raw))
            .collect::<EmbedderResult<Vec<_>>>()?;
        let certificate = GoldenVectorCertificateV1::from_exact_f32(
            &CASS_PASSAGE_CONFORMANCE_TEXTS_V1,
            &vectors,
        )?;
        let mut identity = inner_identity.clone();
        let source_fingerprint = source_contract.fingerprint();
        let provenance = passage_producer_provenance(&inner_identity, &source_fingerprint);
        identity.input.canonicalization = format!(
            "cass-canonicalize-one-raw-passage-once-v1;source={source_fingerprint};then-inner={}",
            inner_identity.input.canonicalization
        );
        identity.input.content_selection = format!(
            "one-raw-query-or-source-selected-passage-v1;source={source_fingerprint};inner={}",
            inner_identity.input.content_selection
        );
        identity.input.chunking = format!(
            "caller-selects-raw-passage-under-explicit-source-contract={source_fingerprint};no-additional-wrapper-chunking;inner={}",
            inner_identity.input.chunking
        );
        identity.input.doc_id_semantics = format!(
            "source-bound-SemanticDocId-and-canonical-passage-sha256-v1;source={source_fingerprint};inner={}",
            inner_identity.input.doc_id_semantics
        );
        identity.space.input_contract_fingerprint = identity.input.fingerprint();
        // A hash-control profile is its complete algorithm manifest. Its new
        // profile must include the actual outer transform and full ancestry;
        // copying the old profile/provenance after composition would be false.
        if let Some(profile) = identity.space.hash_control.as_mut() {
            profile.algorithm_revision = format!("cass-passage-v1:provenance={provenance}");
            profile.tokenization_rules = format!(
                "canonicalize one raw passage once;source={source_fingerprint};then {}",
                profile.tokenization_rules
            );
            identity.space.artifact_manifest_fingerprint = profile.fingerprint();
            identity.producer.provenance_manifest_fingerprint = profile.fingerprint();
        } else {
            // Immutable model artifacts and model-internal preprocessing remain
            // the actual inner model's contracts. The new producer provenance
            // additionally binds its complete identity and the outer code.
            identity.producer.provenance_manifest_fingerprint = provenance.clone();
        }
        identity.producer.backend = format!("cass-passage-v1/{}", inner_identity.producer.backend);
        identity.producer.implementation_revision = format!("cass-passage-v1:{provenance}");
        identity.producer.protocol_revision = format!(
            "raw-passage-to-bound-inner-single-v1/{}",
            inner_identity.producer.protocol_revision
        );
        identity.producer.space_fingerprint = identity.space.fingerprint();
        identity.producer.golden_vectors = certificate;
        identity.validate()?;
        Self::check_inner(inner.as_ref(), &inner_identity)?;
        let id = format!("cass-passage-v1/{}", inner.id());
        Ok(Self {
            inner,
            inner_identity,
            identity,
            source_contract,
            id,
        })
    }

    #[must_use]
    pub const fn source_contract(&self) -> SemanticSourceContract {
        self.source_contract
    }

    /// Complete, unmodified ancestry, including the inner producer certificate.
    #[must_use]
    pub const fn inner_identity(&self) -> &EmbeddingIdentityBundleV1 {
        &self.inner_identity
    }

    fn check_inner(
        inner: &dyn Embedder,
        expected: &EmbeddingIdentityBundleV1,
    ) -> EmbedderResult<()> {
        if inner.identity()? != expected
            || inner.dimension() != expected.space.dimension as usize
            || inner.is_semantic() != (expected.space.kind == EmbeddingSpaceKindV1::Semantic)
        {
            return Err(passage_producer_error("inner producer identity changed"));
        }
        Ok(())
    }

    fn embed_inner(
        inner: &dyn Embedder,
        expected: &EmbeddingIdentityBundleV1,
        raw: &str,
    ) -> EmbedderResult<Vec<f32>> {
        Self::check_inner(inner, expected)?;
        let canonical = canonicalize_for_embedding(raw);
        if canonical.is_empty() {
            return Err(passage_producer_error(
                "raw passage has no canonical embedding content",
            ));
        }
        let response = inner.embed_bound_sync(&canonical)?;
        Self::check_inner(inner, expected)?;
        response.validate()?;
        if response.identity != *expected {
            return Err(passage_producer_error(
                "bound inner response disagrees with the frozen producer",
            ));
        }
        Ok(response.values)
    }
}

fn passage_producer_error(reason: &str) -> EmbedderError {
    EmbedderError::InvalidConfig {
        field: "cass_passage_producer".into(),
        value: "refused".into(),
        reason: reason.into(),
    }
}

fn passage_producer_provenance(
    inner: &EmbeddingIdentityBundleV1,
    source_fingerprint: &str,
) -> String {
    let mut hash = Sha256::new();
    let space = inner.space.canonical_bytes();
    let producer = inner.producer.canonical_bytes();
    let input = inner.input.canonical_bytes();
    let storage = inner.storage.canonical_bytes();
    for field in [
        b"cass.passage-producer.provenance.v1".as_slice(),
        space.as_slice(),
        producer.as_slice(),
        input.as_slice(),
        storage.as_slice(),
        source_fingerprint.as_bytes(),
        include_bytes!("embedder.rs").as_slice(),
        include_bytes!("canonicalize.rs").as_slice(),
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock")).as_slice(),
    ] {
        hash.update((field.len() as u64).to_be_bytes());
        hash.update(field);
    }
    hex::encode(hash.finalize())
}

impl Embedder for CassPassageProducer {
    fn embed_sync(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        Ok(self.embed_bound_sync(text)?.values)
    }

    fn embed_batch_sync(&self, texts: &[&str]) -> EmbedderResult<Vec<Vec<f32>>> {
        self.embed_batch_bound_sync(texts)
            .map(|batch| batch.into_iter().map(|response| response.values).collect())
    }

    fn embed_bound_sync(&self, text: &str) -> EmbedderResult<IdentityBoundEmbedding> {
        let values = Self::embed_inner(self.inner.as_ref(), &self.inner_identity, text)?;
        Ok(IdentityBoundEmbedding {
            values,
            identity: self.identity.clone(),
        })
    }

    fn embed_batch_bound_sync(
        &self,
        texts: &[&str],
    ) -> EmbedderResult<Vec<IdentityBoundEmbedding>> {
        Self::check_inner(self.inner.as_ref(), &self.inner_identity)?;
        // SyncEmbed does not advertise whether its bound batch preserves a
        // custom bound-single operation. Dispatch the actual authenticated
        // operation used to measure this composition, including for providers
        // whose raw batch has different behavior. No partial batch escapes.
        texts
            .iter()
            .map(|text| self.embed_bound_sync(text))
            .collect()
    }

    fn identity(&self) -> EmbedderResult<&EmbeddingIdentityBundleV1> {
        Self::check_inner(self.inner.as_ref(), &self.inner_identity)?;
        Ok(&self.identity)
    }

    fn dimension(&self) -> usize {
        self.identity.space.dimension as usize
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }

    fn is_semantic(&self) -> bool {
        self.identity.space.kind == EmbeddingSpaceKindV1::Semantic
    }

    fn category(&self) -> frankensearch::ModelCategory {
        self.inner.category()
    }

    fn tier(&self) -> frankensearch::ModelTier {
        self.inner.tier()
    }

    fn supports_mrl(&self) -> bool {
        self.inner.supports_mrl()
    }
}

/// Metadata about an embedder for display and logging.
#[derive(Debug, Clone)]
pub struct EmbedderInfo {
    /// The embedder's unique identifier.
    pub id: String,
    /// The output dimension.
    pub dimension: usize,
    /// Whether it's a semantic (ML) embedder.
    pub is_semantic: bool,
}

impl EmbedderInfo {
    /// Create info from an embedder instance.
    pub fn from_embedder(embedder: &dyn Embedder) -> Self {
        Self {
            id: embedder.id().to_string(),
            dimension: embedder.dimension(),
            is_semantic: embedder.is_semantic(),
        }
    }
}

impl fmt::Display for EmbedderInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = if self.is_semantic {
            "semantic"
        } else {
            "lexical"
        };
        write!(f, "{} ({}, {} dims)", self.id, kind, self.dimension)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::fastembed_embedder::FastEmbedder;
    use crate::search::hash_embedder::HashEmbedder;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use crate::search::semantic_reader::publication::text::hydration::PassageProjection;

    fn passage_contract() -> SemanticSourceContract {
        SemanticSourceContract::new(PassageProjection::RawPassagesV2)
    }

    #[test]
    fn cass_passage_producer_measures_actual_once_canonicalized_control() {
        let inner = Arc::new(HashEmbedder::new(32));
        let producer = CassPassageProducer::new(inner.clone(), passage_contract()).unwrap();
        let identity = producer.identity().unwrap();
        identity.validate().unwrap();
        assert_eq!(identity.space.kind, EmbeddingSpaceKindV1::HashControl);
        assert!(!producer.is_semantic());
        assert_eq!(producer.inner_identity(), inner.identity().unwrap());
        assert_ne!(identity, inner.identity().unwrap());
        assert_eq!(producer.source_contract(), passage_contract());
        let expected = CASS_PASSAGE_CONFORMANCE_TEXTS_V1
            .iter()
            .map(|raw| {
                inner
                    .embed_bound_sync(&canonicalize_for_embedding(raw))
                    .unwrap()
                    .values
            })
            .collect::<Vec<_>>();
        assert_eq!(
            identity.producer.golden_vectors,
            GoldenVectorCertificateV1::from_exact_f32(
                &CASS_PASSAGE_CONFORMANCE_TEXTS_V1,
                &expected
            )
            .unwrap()
        );
        for (raw, values) in CASS_PASSAGE_CONFORMANCE_TEXTS_V1.iter().zip(&expected) {
            let actual = producer.embed_bound_sync(raw).unwrap();
            actual.validate().unwrap();
            assert_eq!(&actual.identity, identity);
            assert_eq!(&actual.values, values);
        }
        assert_ne!(
            identity.producer.golden_vectors,
            inner.identity().unwrap().producer.golden_vectors
        );
        assert!(producer.embed_bound_sync("").is_err());
        assert!(producer.embed_bound_sync("thanks").is_err());
        assert!(
            producer
                .embed_batch_bound_sync(&["valid source passage", "thanks"])
                .is_err()
        );
        assert!(producer.embed_batch_bound_sync(&[]).unwrap().is_empty());
    }

    #[test]
    fn cass_passage_producer_identity_binds_explicit_projection_and_restart() {
        let first =
            CassPassageProducer::new(Arc::new(HashEmbedder::new(32)), passage_contract()).unwrap();
        let restarted =
            CassPassageProducer::new(Arc::new(HashEmbedder::new(32)), passage_contract()).unwrap();
        assert_eq!(first.identity().unwrap(), restarted.identity().unwrap());
        let prefix = CassPassageProducer::new(
            Arc::new(HashEmbedder::new(32)),
            SemanticSourceContract::new(PassageProjection::CanonicalPrefixV1),
        )
        .unwrap();
        assert_eq!(first.id(), prefix.id());
        assert_ne!(first.identity().unwrap(), prefix.identity().unwrap());
        // Equal numerical outputs for these inputs do not erase source-policy
        // differences in a corpus certificate or persisted identity.
        assert_eq!(
            first.embed_sync("matching words").unwrap(),
            prefix.embed_sync("matching words").unwrap()
        );
        assert_ne!(
            first.identity().unwrap().input.fingerprint(),
            prefix.identity().unwrap().input.fingerprint()
        );
    }

    struct BoundOnlyProducer {
        actual: HashEmbedder,
        foreign: HashEmbedder,
        drifted: AtomicBool,
        drift_during_embed: AtomicBool,
        foreign_response: AtomicBool,
        calls: AtomicUsize,
    }

    impl BoundOnlyProducer {
        fn new() -> Self {
            Self {
                actual: HashEmbedder::new(16),
                foreign: HashEmbedder::new(32),
                drifted: AtomicBool::new(false),
                drift_during_embed: AtomicBool::new(false),
                foreign_response: AtomicBool::new(false),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl Embedder for BoundOnlyProducer {
        fn embed_sync(&self, _: &str) -> EmbedderResult<Vec<f32>> {
            panic!("composition must dispatch the actual bound operation")
        }

        fn embed_batch_sync(&self, _: &[&str]) -> EmbedderResult<Vec<Vec<f32>>> {
            panic!("composition must not bypass bound-single overrides through raw batch")
        }

        fn embed_bound_sync(&self, text: &str) -> EmbedderResult<IdentityBoundEmbedding> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let result = if self.foreign_response.load(Ordering::SeqCst) {
                self.foreign.embed_bound_sync(text)
            } else {
                self.actual.embed_bound_sync(text)
            };
            if self.drift_during_embed.load(Ordering::SeqCst) {
                self.drifted.store(true, Ordering::SeqCst);
            }
            result
        }

        fn identity(&self) -> EmbedderResult<&EmbeddingIdentityBundleV1> {
            if self.drifted.load(Ordering::SeqCst) {
                self.foreign.identity()
            } else {
                self.actual.identity()
            }
        }

        fn dimension(&self) -> usize {
            self.actual.dimension()
        }

        fn id(&self) -> &str {
            "test-bound-dispatch"
        }

        fn is_semantic(&self) -> bool {
            false
        }

        fn category(&self) -> frankensearch::ModelCategory {
            frankensearch::ModelCategory::HashEmbedder
        }
    }

    #[test]
    fn cass_passage_producer_preserves_custom_bound_dispatch_and_batch_atomicity() {
        let inner = Arc::new(BoundOnlyProducer::new());
        let producer = CassPassageProducer::new(inner.clone(), passage_contract()).unwrap();
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            CASS_PASSAGE_CONFORMANCE_TEXTS_V1.len()
        );
        let batch = producer
            .embed_batch_bound_sync(&["first bound passage", "second bound passage"])
            .unwrap();
        assert_eq!(batch.len(), 2);
        for response in batch {
            response.validate().unwrap();
            assert_eq!(&response.identity, producer.identity().unwrap());
        }
        inner.foreign_response.store(true, Ordering::SeqCst);
        assert!(
            producer
                .embed_bound_sync("refuse foreign response")
                .is_err()
        );
        assert!(
            producer
                .embed_batch_bound_sync(&["refuse foreign batch"])
                .is_err()
        );
    }

    #[test]
    fn cass_passage_producer_rejects_drift_before_and_during_inference() {
        let inner = Arc::new(BoundOnlyProducer::new());
        let producer = CassPassageProducer::new(inner.clone(), passage_contract()).unwrap();
        let before = inner.calls.load(Ordering::SeqCst);
        inner.drifted.store(true, Ordering::SeqCst);
        assert!(producer.identity().is_err());
        assert!(producer.embed_bound_sync("pre-inference refusal").is_err());
        assert!(producer.embed_batch_bound_sync(&[]).is_err());
        assert_eq!(inner.calls.load(Ordering::SeqCst), before);
        inner.drifted.store(false, Ordering::SeqCst);
        inner.drift_during_embed.store(true, Ordering::SeqCst);
        assert!(producer.embed_bound_sync("post-inference refusal").is_err());
        assert_eq!(inner.calls.load(Ordering::SeqCst), before + 1);
        assert!(producer.identity().is_err());
    }

    fn fastembed_fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/models/xenova-paraphrase-minilm-l3-v2-int8")
    }

    // cass #308: the pure-Rust native backend loads f32 `model.safetensors` of the
    // all-MiniLM-L6-v2 topology; it cannot load the small committed int8 ONNX
    // fixture. Tests using this helper are `#[ignore]`d by default and run against
    // a real model supplied via `FRANKENSEARCH_MODEL_DIR` (`cargo test -- --ignored`).
    fn load_fastembed_fixture() -> FastEmbedder {
        let dir = crate::search::fastembed_embedder::model_dir_override()
            .unwrap_or_else(fastembed_fixture_dir);
        FastEmbedder::load_from_dir(&dir).expect("fastembed fixture should load")
    }

    #[test]
    fn test_embedder_trait_basic() {
        let embedder = HashEmbedder::new(256);
        let embedding = embedder.embed_sync("hello world").unwrap();
        assert_eq!(embedding.len(), 256);
        assert_eq!(embedder.id(), "fnv1a-256");
        assert!(!embedder.is_semantic());
    }

    #[test]
    #[ignore = "needs a real safetensors MiniLM model via FRANKENSEARCH_MODEL_DIR; the int8 ONNX fixture is incompatible with the native backend — cass #308"]
    fn test_embedder_trait_semantic() {
        let embedder = load_fastembed_fixture();
        assert_eq!(embedder.dimension(), 384);
        assert_eq!(embedder.id(), FastEmbedder::embedder_id_static());
        assert!(embedder.is_semantic());
    }

    #[test]
    #[ignore = "needs a real safetensors MiniLM model via FRANKENSEARCH_MODEL_DIR; the int8 ONNX fixture is incompatible with the native backend — cass #308"]
    fn test_embedder_batch() {
        let embedder = load_fastembed_fixture();
        let texts = &["hello", "world", "test"];
        let embeddings = embedder.embed_batch_sync(texts).unwrap();

        assert_eq!(embeddings.len(), 3);
        for embedding in &embeddings {
            assert_eq!(embedding.len(), 384);
        }
    }

    #[test]
    #[ignore = "needs a real safetensors MiniLM model via FRANKENSEARCH_MODEL_DIR; the int8 ONNX fixture is incompatible with the native backend — cass #308"]
    fn test_embedder_empty_input_error() {
        let embedder = load_fastembed_fixture();
        let result = embedder.embed_sync("");
        assert!(result.is_err());
    }

    #[test]
    #[ignore = "needs a real safetensors MiniLM model via FRANKENSEARCH_MODEL_DIR; the int8 ONNX fixture is incompatible with the native backend — cass #308"]
    fn test_embedder_info() {
        let embedder = load_fastembed_fixture();
        let info = EmbedderInfo::from_embedder(&embedder);
        assert_eq!(info.id, FastEmbedder::embedder_id_static());
        assert_eq!(info.dimension, 384);
        assert!(info.is_semantic);

        let display = format!("{info}");
        for expected in [FastEmbedder::embedder_id_static(), "semantic", "384"] {
            assert!(
                display.contains(expected),
                "display {display:?} should contain {expected:?}"
            );
        }
    }

    #[test]
    fn test_embedder_error_display() {
        let err = EmbedderError::EmbedderUnavailable {
            model: "test".to_string(),
            reason: "model not downloaded".to_string(),
        };
        assert!(err.to_string().contains("model not downloaded"));

        let err = EmbedderError::EmbeddingFailed {
            model: "test".to_string(),
            source: Box::new(std::io::Error::other("inference error")),
        };
        assert!(err.to_string().contains("inference error"));
    }
}
