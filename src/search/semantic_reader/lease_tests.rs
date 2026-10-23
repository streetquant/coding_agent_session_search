//! Real FSVI v2 owners exercise lease identity, rows and immutability.
//! These are admission controls, not model or retrieval-quality certificates.
#![cfg(any(target_os = "linux", target_os = "android"))]

use std::fs;
use std::path::Path;

use frankensearch::core::generation::{ArtifactGenerationIdentityV1, QuantizationFormat};
use frankensearch::core::{
    EMBEDDING_INPUT_CONTRACT_SCHEMA_V1, EMBEDDING_PRODUCER_ATTESTATION_SCHEMA_V1,
    EMBEDDING_SPACE_IDENTITY_SCHEMA_V1, EmbeddingArtifactIdentityV1, EmbeddingIdentityBundleV1,
    EmbeddingInputContractV1, EmbeddingProducerAttestationV1, EmbeddingSpaceIdentityV1,
    EmbeddingSpaceKindV1, GoldenVectorCertificateV1, VECTOR_STORAGE_IDENTITY_SCHEMA_V1,
    VectorStorageIdentityV1,
};
use frankensearch::index::{
    FsviAdmissionError, FsviReindexReason, FsviSnapshotRejectionReason, FsviV2IdentityBinding,
    Quantization, ValidatedFsviBytes, VectorIndex, wal_path_for,
};
use sha2::{Digest, Sha256};

use crate::search::semantic_manifest::{
    SemanticAnnBaseBindingV1, SemanticArtifactRole, SemanticArtifactSealStateV1,
    SemanticArtifactValidationEvidence, SemanticGenerationArtifact, TierKind,
};
use crate::search::semantic_reader::{
    SemanticGenerationReader, SemanticOwnerRowSource, SemanticReaderError, SemanticShardExpectation,
};
use crate::search::vector_index::{ROLE_USER, SemanticDocId};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const DIMENSION: usize = 4;
const DISPLAY_MODEL: &str = "cass-fixture-model";
const DISPLAY_REVISION: &str = "cass-fixture-revision-v1";
const GENERATION_SEQUENCE: u64 = 7;
const GENERATION_NONCE: [u8; 16] = [7; 16];
const OWNED_ROWS: [(u64, [f32; DIMENSION]); 2] =
    [(1, [1.0, 0.0, 0.0, 0.0]), (2, [0.0, 1.0, 0.0, 0.0])];
const FOREIGN_ROWS: [(u64, [f32; DIMENSION]); 2] =
    [(1, [0.0, 0.0, 1.0, 0.0]), (2, [0.0, 0.0, 0.0, 1.0])];

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Variants retain identical display strings and dimensions while changing
/// the tokenizer, model artifacts, pooling and input contracts.
fn identity(display: &str, variant: &str) -> EmbeddingIdentityBundleV1 {
    let input = EmbeddingInputContractV1 {
        schema_version: EMBEDDING_INPUT_CONTRACT_SCHEMA_V1,
        canonicalization: "cass-lease-canonical-v1".into(),
        content_selection: "cass-lease-content-v1".into(),
        chunking: format!("cass-lease-chunks-{variant}-v1"),
        query_instruction: "query:".into(),
        document_instruction: "document:".into(),
        doc_id_semantics: "cass-lease-passage-id-v1".into(),
    };
    let space = EmbeddingSpaceIdentityV1 {
        schema_version: EMBEDDING_SPACE_IDENTITY_SCHEMA_V1,
        logical_model_id: display.into(),
        immutable_revision: DISPLAY_REVISION.into(),
        kind: EmbeddingSpaceKindV1::Semantic,
        artifact_manifest_fingerprint: digest(format!("lease-manifest-{variant}").as_bytes()),
        artifacts: vec![
            EmbeddingArtifactIdentityV1 {
                role: "tokenizer".into(),
                sha256: digest(format!("lease-tokenizer-{variant}").as_bytes()),
                size: 9,
            },
            EmbeddingArtifactIdentityV1 {
                role: "weights".into(),
                sha256: digest(format!("lease-weights-{variant}").as_bytes()),
                size: 7,
            },
        ],
        tokenizer_fingerprint: digest(format!("lease-tokenizer-contract-{variant}").as_bytes()),
        vocabulary_fingerprint: digest(b"lease-vocabulary"),
        model_config_fingerprint: digest(b"lease-config"),
        model_preprocessing: "unicode=nfc".into(),
        sequence_policy: "truncate=tail;max_tokens=512;padding=none".into(),
        query_instruction: "query:".into(),
        document_instruction: "document:".into(),
        pooling: format!("mean-mask-aware-{variant}"),
        output_normalization: "l2-f32-v1".into(),
        dimension: DIMENSION as u32,
        input_contract_fingerprint: input.fingerprint(),
        hash_control: None,
        projection: None,
    };
    let producer = EmbeddingProducerAttestationV1 {
        schema_version: EMBEDDING_PRODUCER_ATTESTATION_SCHEMA_V1,
        backend: "cass-lease-fixture".into(),
        implementation_revision: "fixture-v1".into(),
        protocol_revision: "in-process-v1".into(),
        numeric_profile: "test-f32-v1".into(),
        provenance_manifest_fingerprint: digest(b"lease-provenance"),
        space_fingerprint: space.fingerprint(),
        golden_vectors: GoldenVectorCertificateV1 {
            corpus_sha256: digest(b"lease-corpus"),
            vectors_sha256: digest(b"lease-vectors"),
            vector_count: 2,
            dimension: DIMENSION as u32,
        },
    };
    EmbeddingIdentityBundleV1 {
        space,
        producer,
        input,
        storage: VectorStorageIdentityV1 {
            schema_version: VECTOR_STORAGE_IDENTITY_SCHEMA_V1,
            format: "fsvi-v2".into(),
            quantization: QuantizationFormat::F16,
            endianness: "little-endian".into(),
            vector_normalization: "l2-f32-v1".into(),
            dimension: DIMENSION as u32,
        },
    }
}

fn binding(variant: &str) -> FsviV2IdentityBinding {
    FsviV2IdentityBinding::new(
        ArtifactGenerationIdentityV1::new(GENERATION_SEQUENCE, GENERATION_NONCE).unwrap(),
        identity(DISPLAY_MODEL, variant).freeze().unwrap(),
    )
    .unwrap()
}

fn document(id: u64) -> String {
    SemanticDocId {
        message_id: id,
        chunk_idx: 0,
        agent_id: 1,
        workspace_id: 1,
        source_id: 0,
        role: ROLE_USER,
        created_at_ms: 100,
        content_hash: Some([id as u8; 32]),
    }
    .to_doc_id_string()
}

fn write_vector(
    path: &Path,
    binding: &FsviV2IdentityBinding,
    rows: &[(u64, [f32; DIMENSION])],
) -> ValidatedFsviBytes {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut writer = VectorIndex::create_v2(path, binding.clone()).unwrap();
    for (id, vector) in rows {
        writer.write_record(&document(*id), vector).unwrap();
    }
    writer.finish().unwrap();
    ValidatedFsviBytes::open_published(path, binding).unwrap()
}

fn reader_for(
    path: &Path,
    binding: FsviV2IdentityBinding,
    witness: frankensearch::index::FsviV2Witness,
) -> Result<SemanticGenerationReader, SemanticReaderError> {
    SemanticGenerationReader::open(
        None,
        Some(&[SemanticShardExpectation {
            path: path.to_path_buf(),
            binding,
            witness,
        }]),
    )
}

fn row_index(source: &SemanticOwnerRowSource<'_>, id: u64) -> usize {
    let expected = document(id);
    (0..source.record_count())
        .find(|&index| source.row(index).unwrap().doc_id() == expected)
        .expect("the writer-produced document is retained")
}

fn json_keys(value: &serde_json::Value, keys: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                keys.push(key.clone());
                json_keys(child, keys);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                json_keys(item, keys);
            }
        }
        _ => {}
    }
}

#[test]
fn the_lease_publishes_the_complete_owner_backed_row_source() -> TestResult {
    let root = tempfile::tempdir()?;
    let path = root.path().join("lease.fsvi");
    let expected = binding("alpha");
    let owner = write_vector(&path, &expected, &OWNED_ROWS);
    let witness = owner.witness().clone();
    let reader = reader_for(&path, expected.clone(), witness.clone())?;
    let source = reader
        .row_source(TierKind::Quality, 0)
        .expect("the retained shard has a row source");
    assert_eq!(source.dimension(), DIMENSION);
    assert_eq!(source.quantization(), Quantization::F16);
    assert_eq!(source.record_count(), 2);
    assert_eq!(source.live_count(), 2);
    assert_eq!(source.tombstone_count(), 0);
    assert_eq!(source.generation(), expected.generation());
    assert_eq!(source.generation(), witness.generation);
    assert_eq!(
        source.identity_bundle_canonical_bytes(),
        expected.frozen_identity().canonical_bytes
    );
    assert_eq!(
        source.space_identity_canonical_bytes(),
        expected.frozen_identity().identity.space.canonical_bytes()
    );
    assert_eq!(
        source.storage_identity_canonical_bytes(),
        expected
            .frozen_identity()
            .identity
            .storage
            .canonical_bytes()
    );
    assert_eq!(source.artifact_digest(), &witness.whole_image_sha256);
    assert_eq!(source.content_digest(), &witness.vector_content_digest);
    assert_eq!(source.docset_digest(), &witness.ordered_live_docset_digest);
    assert_eq!(source.space_fingerprint(), &witness.space_fingerprint);
    assert_eq!(source.producer_fingerprint(), &witness.producer_fingerprint);
    assert_eq!(source.input_fingerprint(), &witness.input_fingerprint);
    assert_eq!(source.storage_fingerprint(), &witness.storage_fingerprint);
    assert_eq!(
        source.identity_bundle_fingerprint(),
        &witness.identity_bundle_fingerprint
    );
    assert_eq!(
        source.generation_fingerprint(),
        &witness.generation_fingerprint
    );

    // FSVI sorts physical rows by document hash rather than insertion order.
    let position = row_index(&source, 1);
    let row = source.row(position)?;
    assert_eq!(row.physical_index(), position);
    assert_eq!(row.doc_id(), document(1));
    assert_eq!(row.vector_bytes(), [0, 0x3c, 0, 0, 0, 0, 0, 0]);
    assert!(row.is_live());
    assert_eq!(row.flags().bits(), 0);
    assert!(!format!("{row:?}").contains(&document(1)));
    assert!(source.row(2).is_err());
    assert!(reader.row_source(TierKind::Fast, 0).is_none());
    assert!(reader.row_source(TierKind::Quality, 1).is_none());
    Ok(())
}

#[test]
fn tombstone_rows_keep_their_exact_flags_vectors_and_physical_counts() -> TestResult {
    let root = tempfile::tempdir()?;
    let path = root.path().join("tombstone.fsvi");
    let expected = binding("alpha");
    let mut writer = VectorIndex::create_v2(&path, expected.clone())?;
    for (id, vector) in OWNED_ROWS {
        writer.write_record(&document(id), &vector)?;
    }
    writer.write_tombstone_record(&document(3), &[0.0, 0.0, 1.0, 0.0])?;
    writer.finish()?;
    let owner = ValidatedFsviBytes::open_published(&path, &expected)?;
    let reader = reader_for(&path, expected, owner.witness().clone())?;
    let source = reader.row_source(TierKind::Quality, 0).unwrap();
    assert_eq!(source.record_count(), 3);
    assert_eq!(source.live_count(), 2);
    assert_eq!(source.tombstone_count(), 1);
    let position = row_index(&source, 3);
    let row = source.row(position)?;
    assert_eq!(row.physical_index(), position);
    assert_eq!(row.doc_id(), document(3));
    assert!(row.flags().is_tombstone());
    assert!(!row.is_live());
    assert_eq!(row.vector_bytes(), [0, 0, 0, 0, 0, 0x3c, 0, 0]);
    assert!(source.row(3).is_err());
    Ok(())
}

#[test]
fn the_lease_keeps_the_verified_bytes_after_the_pathname_is_replaced() -> TestResult {
    let root = tempfile::tempdir()?;
    let path = root.path().join("lease.fsvi");
    let expected = binding("alpha");
    let owner = write_vector(&path, &expected, &OWNED_ROWS);
    let witness = owner.witness().clone();
    let reader = reader_for(&path, expected.clone(), witness.clone())?;
    let admitted = reader.row_source(TierKind::Quality, 0).unwrap();
    let position = row_index(&admitted, 1);
    let admitted_bytes = admitted.row(position)?.vector_bytes().to_vec();
    let admitted_doc = admitted.row(position)?.doc_id().to_owned();
    let replacement = root.path().join("replacement.fsvi");
    write_vector(&replacement, &binding("beta"), &FOREIGN_ROWS);
    fs::rename(&replacement, &path)?;
    let retained = reader.row_source(TierKind::Quality, 0).unwrap();
    assert_eq!(retained.artifact_digest(), &witness.whole_image_sha256);
    assert_eq!(
        retained.row(position)?.vector_bytes(),
        admitted_bytes.as_slice()
    );
    assert_eq!(retained.row(position)?.doc_id(), admitted_doc);
    assert_eq!(retained.live_count(), 2);
    assert!(ValidatedFsviBytes::open_published(&path, &expected).is_err());
    Ok(())
}

#[test]
fn the_lease_keeps_the_verified_bytes_after_the_pathname_becomes_a_symlink() -> TestResult {
    let root = tempfile::tempdir()?;
    let path = root.path().join("lease.fsvi");
    let other = root.path().join("other.fsvi");
    let expected = binding("alpha");
    let owner = write_vector(&path, &expected, &OWNED_ROWS);
    let witness = owner.witness().clone();
    let reader = reader_for(&path, expected.clone(), witness.clone())?;
    write_vector(&other, &binding("beta"), &FOREIGN_ROWS);
    fs::rename(&path, root.path().join("before-symlink.fsvi"))?;
    std::os::unix::fs::symlink(&other, &path)?;
    let retained = reader.row_source(TierKind::Quality, 0).unwrap();
    assert_eq!(retained.artifact_digest(), &witness.whole_image_sha256);
    assert_eq!(
        retained.row(row_index(&retained, 1))?.vector_bytes(),
        [0, 0x3c, 0, 0, 0, 0, 0, 0]
    );
    match ValidatedFsviBytes::open_published(&path, &expected) {
        Err(FsviAdmissionError::SnapshotRejected(rejected)) => {
            assert_eq!(rejected.reason, FsviSnapshotRejectionReason::SymbolicLink);
        }
        other => panic!("expected a symlink refusal, got {other:?}"),
    }
    Ok(())
}

#[test]
fn the_lease_keeps_the_verified_bytes_after_an_alias_mutates_the_inode() -> TestResult {
    let root = tempfile::tempdir()?;
    let path = root.path().join("lease.fsvi");
    let expected = binding("alpha");
    let owner = write_vector(&path, &expected, &OWNED_ROWS);
    let witness = owner.witness().clone();
    let reader = reader_for(&path, expected.clone(), witness.clone())?;
    let alias = root.path().join("alias.fsvi");
    fs::hard_link(&path, &alias)?;
    fs::write(&alias, b"replacement-bytes")?;
    let retained = reader.row_source(TierKind::Quality, 0).unwrap();
    assert_eq!(retained.artifact_digest(), &witness.whole_image_sha256);
    assert_eq!(
        retained.row(row_index(&retained, 1))?.vector_bytes(),
        [0, 0x3c, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(retained.record_count(), 2);
    assert!(ValidatedFsviBytes::open_published(&path, &expected).is_err());
    Ok(())
}

#[test]
fn the_wrong_owner_cannot_serve_the_expected_generation() -> TestResult {
    let root = tempfile::tempdir()?;
    let expected_path = root.path().join("expected.fsvi");
    let foreign_path = root.path().join("foreign.fsvi");
    let expected = binding("alpha");
    let foreign = binding("beta");
    assert_eq!(expected.generation(), foreign.generation());
    assert_ne!(
        expected.frozen_identity().fingerprint,
        foreign.frozen_identity().fingerprint
    );
    let expected_owner = write_vector(&expected_path, &expected, &OWNED_ROWS);
    write_vector(&foreign_path, &foreign, &FOREIGN_ROWS);
    match ValidatedFsviBytes::open_published(&foreign_path, &expected) {
        Err(FsviAdmissionError::ReindexRequired(required)) => {
            assert_eq!(required.reason, FsviReindexReason::IdentityMismatch);
        }
        other => panic!("expected an identity-mismatch refusal, got {other:?}"),
    }
    match reader_for(
        &foreign_path,
        expected.clone(),
        expected_owner.witness().clone(),
    ) {
        Err(SemanticReaderError::Admission { source, .. }) => {
            assert!(matches!(
                source.as_ref(),
                FsviAdmissionError::ReindexRequired(required)
                    if required.reason == FsviReindexReason::IdentityMismatch
            ));
        }
        other => panic!("expected an admission refusal, got {other:?}"),
    }
    let forged = format!(
        "generation={} identity={} image={}",
        expected.generation().sequence,
        expected.frozen_identity().fingerprint,
        hex::encode(expected_owner.witness().whole_image_sha256)
    );
    fs::write(wal_path_for(&foreign_path), forged.as_bytes())?;
    assert!(ValidatedFsviBytes::open_published(&foreign_path, &expected).is_err());
    Ok(())
}

#[test]
fn a_published_generation_requires_wal_absence() -> TestResult {
    let root = tempfile::tempdir()?;
    let path = root.path().join("published.fsvi");
    let expected = binding("alpha");
    write_vector(&path, &expected, &OWNED_ROWS);
    let owner = ValidatedFsviBytes::open_published(&path, &expected)?;
    assert!(owner.published_wal_absent());
    let wal = wal_path_for(&path);
    for (label, bytes) in [
        ("empty", b"".as_slice()),
        ("nonempty", b"cass-wal-entries".as_slice()),
        ("stale", b"stale-generation-wal".as_slice()),
        ("partial", b"partial".as_slice()),
        ("corrupt", &[0xff_u8, 0x00, 0x7f][..]),
    ] {
        fs::write(&wal, bytes)?;
        match ValidatedFsviBytes::open_published(&path, &expected) {
            Err(FsviAdmissionError::SnapshotRejected(rejected)) => {
                assert_eq!(
                    rejected.reason,
                    FsviSnapshotRejectionReason::PublishedWalPresent,
                    "{label}"
                );
            }
            other => panic!("{label}: expected a published-WAL refusal, got {other:?}"),
        }
        fs::rename(&wal, root.path().join(format!("rejected-{label}.wal")))?;
    }
    assert!(
        ValidatedFsviBytes::open_published(&path, &expected)
            .expect("absence passes")
            .published_wal_absent()
    );
    Ok(())
}

#[test]
fn a_concurrent_publisher_never_mixes_owners_into_one_lease() -> TestResult {
    let root = tempfile::tempdir()?;
    let path = root.path().join("lease.fsvi");
    let expected = binding("alpha");
    let owner = write_vector(&path, &expected, &OWNED_ROWS);
    let witness = owner.witness().clone();
    let reader = reader_for(&path, expected.clone(), witness.clone())?;
    let admitted = reader.row_source(TierKind::Quality, 0).unwrap();
    let position = row_index(&admitted, 1);
    let admitted_bytes = admitted.row(position)?.vector_bytes().to_vec();
    let source = root.path().join("replacement.fsvi");
    write_vector(&source, &binding("beta"), &FOREIGN_ROWS);
    let published = path.clone();
    let (ready, started) = std::sync::mpsc::sync_channel(0);
    let publisher = std::thread::spawn(move || -> std::io::Result<usize> {
        for round in 0..64 {
            let staged = published.with_extension("staged");
            fs::copy(&source, &staged)?;
            fs::rename(&staged, &published)?;
            if round == 0 {
                ready.send(()).map_err(std::io::Error::other)?;
            }
        }
        Ok(64)
    });
    started.recv_timeout(std::time::Duration::from_secs(5))?;
    for _ in 0..64 {
        let retained = reader.row_source(TierKind::Quality, 0).unwrap();
        assert_eq!(retained.artifact_digest(), &witness.whole_image_sha256);
        assert_eq!(
            retained.row(position)?.vector_bytes(),
            admitted_bytes.as_slice()
        );
        assert_eq!(retained.row(position)?.doc_id(), document(1));
        assert_eq!(
            reader
                .witness(TierKind::Quality, 0)
                .map(|seen| seen.whole_image_sha256),
            Some(witness.whole_image_sha256)
        );
    }
    assert_eq!(
        publisher
            .join()
            .map_err(|_| "the publisher thread panicked")??,
        64,
        "every replacement must have succeeded"
    );
    assert!(ValidatedFsviBytes::open_published(&path, &expected).is_err());
    Ok(())
}

#[test]
fn inspection_never_changes_the_published_image() -> TestResult {
    let root = tempfile::tempdir()?;
    let path = root.path().join("lease.fsvi");
    let expected = binding("alpha");
    write_vector(&path, &expected, &OWNED_ROWS);
    let before_bytes = fs::read(&path)?;
    let before_metadata = fs::metadata(&path)?;
    let before_entries = directory_entries(root.path())?;
    let owner = ValidatedFsviBytes::open_published(&path, &expected)?;
    let reader = reader_for(&path, expected, owner.witness().clone())?;
    let source = reader.row_source(TierKind::Quality, 0).unwrap();
    for index in 0..source.record_count() {
        assert!(!source.row(index)?.doc_id().is_empty());
    }
    let after_metadata = fs::metadata(&path)?;
    let after_entries = directory_entries(root.path())?;
    assert_eq!(before_metadata.len(), after_metadata.len());
    assert_eq!(before_metadata.modified()?, after_metadata.modified()?);
    assert_eq!(before_metadata.accessed()?, after_metadata.accessed()?);
    assert_eq!(before_entries, after_entries);
    assert_eq!(before_bytes, fs::read(&path)?);
    Ok(())
}

fn directory_entries(directory: &Path) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory)? {
        entries.push(entry?.file_name().to_string_lossy().into_owned());
    }
    entries.sort();
    Ok(entries)
}

#[test]
fn an_ann_descriptor_carries_no_wal_state() -> TestResult {
    let ann = serde_json::to_value(SemanticArtifactSealStateV1::Ann {})?;
    let vector = serde_json::to_value(SemanticArtifactSealStateV1::Vector { wal_entry_count: 0 })?;
    assert!(ann.get("wal_entry_count").is_none());
    assert_eq!(
        vector
            .get("wal_entry_count")
            .and_then(serde_json::Value::as_u64),
        Some(0)
    );
    let base = binding("alpha");
    let artifact = SemanticGenerationArtifact {
        role: SemanticArtifactRole::FastAnn,
        shard: None,
        relative_path: "fast/ann.hnsw".into(),
        artifact_sha256: digest(b"ann-graph"),
        size_bytes: 11,
        artifact_format: "native-hnsw-v2".into(),
        fsvi_generation: None,
        artifact_parameters_sha256: digest(b"ann-parameters"),
        embedding_identity: base.frozen_identity().clone(),
        ann_base: Some(SemanticAnnBaseBindingV1 {
            base_role: SemanticArtifactRole::FastVector,
            base_artifact_sha256: digest(b"base-image"),
            base_storage_fingerprint: digest(b"base-storage"),
            base_artifact_parameters_sha256: digest(b"base-parameters"),
            algorithm: "native-hnsw".into(),
            metric: "cosine".into(),
            parameters_sha256: digest(b"ann-parameters"),
        }),
        selected_document_count: 2,
        covered_document_count: 2,
        vector_slot_count: 2,
        live_vector_count: 2,
        tombstone_vector_count: 0,
        seal: SemanticArtifactSealStateV1::Ann {},
        generation_corpus_sha256: digest(b"lease-corpus"),
        covered_live_docset_sha256: digest(b"lease-docset"),
        covered_content_sha256: digest(b"lease-content"),
        validation: SemanticArtifactValidationEvidence {
            validator_id: "cass-lease-fixture".into(),
            validated_at_ms: 25,
            artifact_sha256: digest(b"ann-graph"),
            size_bytes: 11,
            selected_document_count: 2,
            covered_document_count: 2,
            vector_slot_count: 2,
            live_vector_count: 2,
            tombstone_vector_count: 0,
            seal: SemanticArtifactSealStateV1::Ann {},
            generation_corpus_sha256: digest(b"lease-corpus"),
            covered_live_docset_sha256: digest(b"lease-docset"),
            covered_content_sha256: digest(b"lease-content"),
        },
    };
    let mut keys = Vec::new();
    json_keys(&serde_json::to_value(&artifact)?, &mut keys);
    assert!(
        !keys.iter().any(|key| key.contains("wal")),
        "an ANN descriptor must carry no WAL field: {keys:?}"
    );
    Ok(())
}
