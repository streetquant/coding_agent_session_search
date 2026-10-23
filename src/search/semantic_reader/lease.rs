//! The generation lease's sealed row source, backed by its retained byte owner.
//!
//! Rows, vectors, counts and complete identities come from one admitted FSVI v2
//! image. Replacing or mutating its old pathname cannot change this source.
//! This module uses the public validated-owner API without parsing wire bytes.

use frankensearch::core::generation::ArtifactGenerationIdentityV1;
use frankensearch::index::{
    FsviRecordFlags, FsviV2IdentityMetadata, Quantization, ValidatedFsviBytes, ValidatedFsviRow,
    ValidatedFsviRowSource,
};

use super::{SemanticReaderError, SemanticReaderResult};

/// One immutable FSVI v2 image borrowed from a retained generation lease.
#[derive(Clone, Copy)]
pub struct SemanticOwnerRowSource<'owner> {
    source: ValidatedFsviRowSource<'owner>,
    identity: &'owner FsviV2IdentityMetadata,
}

impl std::fmt::Debug for SemanticOwnerRowSource<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Canonical identities can be large. Keep Debug bounded to the public
        // witness; callers explicitly request the complete bytes below.
        self.source.fmt(formatter)
    }
}

impl<'owner> SemanticOwnerRowSource<'owner> {
    pub(super) fn new(owner: &'owner ValidatedFsviBytes) -> Self {
        Self {
            source: owner.row_source(),
            identity: owner.identity_v2(),
        }
    }

    /// Persisted vector dimension of the owned image.
    #[must_use]
    pub const fn dimension(&self) -> usize {
        self.source.dimension()
    }

    /// Persisted vector storage kind of the owned image.
    #[must_use]
    pub const fn quantization(&self) -> Quantization {
        self.source.quantization()
    }

    /// Physical rows in the owned image, including tombstones.
    #[must_use]
    pub const fn record_count(&self) -> usize {
        self.source.record_count()
    }

    /// Searchable live rows in the owned image.
    #[must_use]
    pub fn live_count(&self) -> usize {
        self.source.live_count()
    }

    /// Retained tombstone rows in the owned image.
    #[must_use]
    pub fn tombstone_count(&self) -> usize {
        self.source.tombstone_count()
    }

    /// Full-width immutable generation of the owned image.
    #[must_use]
    pub const fn generation(&self) -> ArtifactGenerationIdentityV1 {
        self.source.generation()
    }

    /// Complete canonical bundle, including space, producer, input and storage.
    #[must_use]
    pub fn identity_bundle_canonical_bytes(&self) -> &[u8] {
        &self.identity.identity_bundle_canonical_bytes
    }

    /// Complete canonical mathematical embedding-space identity.
    #[must_use]
    pub fn space_identity_canonical_bytes(&self) -> &[u8] {
        &self.identity.space_identity_canonical_bytes
    }

    /// Complete canonical vector-storage identity.
    #[must_use]
    pub fn storage_identity_canonical_bytes(&self) -> &[u8] {
        &self.identity.storage_identity_canonical_bytes
    }

    /// Mathematical embedding-space fingerprint of the frozen identity.
    #[must_use]
    pub const fn space_fingerprint(&self) -> &[u8; 32] {
        self.source.space_fingerprint()
    }

    /// Embedding producer-attestation fingerprint of the frozen identity.
    #[must_use]
    pub const fn producer_fingerprint(&self) -> &[u8; 32] {
        self.source.producer_fingerprint()
    }

    /// Outer embedding-input-contract fingerprint of the frozen identity.
    #[must_use]
    pub const fn input_fingerprint(&self) -> &[u8; 32] {
        self.source.input_fingerprint()
    }

    /// Physical vector-storage fingerprint of the frozen identity.
    #[must_use]
    pub const fn storage_fingerprint(&self) -> &[u8; 32] {
        self.source.storage_fingerprint()
    }

    /// Complete frozen embedding-bundle fingerprint.
    #[must_use]
    pub const fn identity_bundle_fingerprint(&self) -> &[u8; 32] {
        self.source.identity_bundle_fingerprint()
    }

    /// Full-width generation fingerprint.
    #[must_use]
    pub const fn generation_fingerprint(&self) -> &[u8; 32] {
        self.source.generation_fingerprint()
    }

    /// Exact digest of every byte in the owned image.
    #[must_use]
    pub const fn artifact_digest(&self) -> &[u8; 32] {
        &self.source.witness().whole_image_sha256
    }

    /// Exact digest of the owned image's vector slab.
    #[must_use]
    pub const fn content_digest(&self) -> &[u8; 32] {
        self.source.vector_content_digest()
    }

    /// Ordered digest of the owned image's live document set.
    #[must_use]
    pub const fn docset_digest(&self) -> &[u8; 32] {
        self.source.ordered_live_docset_digest()
    }

    /// Borrow one validated physical row of the owned image.
    ///
    /// # Errors
    ///
    /// Returns [`SemanticReaderError::Search`] for an out-of-range row.
    pub fn row(&self, index: usize) -> SemanticReaderResult<SemanticOwnerRow<'_>> {
        self.source
            .row(index)
            .map(|row| SemanticOwnerRow { row })
            .map_err(SemanticReaderError::Search)
    }
}

/// One immutable physical FSVI row borrowed from a retained owner.
#[derive(Debug)]
pub struct SemanticOwnerRow<'owner> {
    row: ValidatedFsviRow<'owner>,
}

impl SemanticOwnerRow<'_> {
    /// Physical row position inside the owned image.
    #[must_use]
    pub const fn physical_index(&self) -> usize {
        self.row.physical_index()
    }

    /// Validated document identifier of this row.
    #[must_use]
    pub const fn doc_id(&self) -> &str {
        self.row.doc_id()
    }

    /// Exact persisted vector bytes of this row.
    #[must_use]
    pub const fn vector_bytes(&self) -> &[u8] {
        self.row.vector_bytes()
    }

    /// Validated live/tombstone state of this row.
    #[must_use]
    pub const fn flags(&self) -> FsviRecordFlags {
        self.row.flags()
    }

    /// Whether this row participates in search and the live document-set digest.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        self.row.flags().is_live()
    }
}
