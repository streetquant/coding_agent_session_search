//! Independently fingerprint canonical passages in a caller-owned read snapshot.
//!
//! This code never opens a database, starts or ends a transaction, installs a
//! model, or changes archive rows. A selected manifest is deliberately not an
//! input. Publication and serving must compare the result with their manifest.
//! Only document identities and passage hashes survive each bounded body read.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::fmt;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::franken_sync::compat::RowExt;
use crate::franken_sync::{Row, SqliteValue, compat::Transaction};
use crate::search::canonicalize::{
    CODE_HEAD_LINES, CODE_TAIL_LINES, EMBEDDING_PASSAGE_CHARS, MAX_EMBED_CHARS,
    MAX_EMBEDDING_PASSAGES, canonicalize_for_embedding, content_hash, embedding_passages,
};
use crate::search::policy::{CHUNKING_STRATEGY_VERSION, SEMANTIC_SCHEMA_VERSION};
use crate::search::semantic_reader::publication::text::hydration::{
    PassageProjection, SemanticReadSnapshot,
};
use crate::search::tantivy::normalized_index_source_id;
use crate::search::vector_index::{ROLE_USER, SemanticDocId, role_code_from_str};

use super::SemanticCorpusSnapshotIdentity;

/// Additive storage authority. Read paths refuse an archive without this row;
/// initialization and canonical change-clock maintenance belong to storage.
pub(crate) const SOURCE_IDENTITY_TABLE: &str = "semantic_source_identity_v1";
const PAGE_ROWS: usize = 64;
const MAX_SCHEMA_ROWS: usize = 256;
const MAX_SCHEMA_BYTES: usize = 1024 * 1024;
const MAX_CERTIFICATE_BYTES: usize = 16 * 1024;

/// The explicit source projection used by both publication and query admission.
/// Budget changes never change this contract or select a partial corpus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticSourceContract {
    projection: PassageProjection,
}

impl SemanticSourceContract {
    #[must_use]
    pub const fn new(projection: PassageProjection) -> Self {
        Self { projection }
    }

    #[must_use]
    pub const fn projection(self) -> PassageProjection {
        self.projection
    }

    /// Exact code/dependency/selection contract; never a model display name.
    #[must_use]
    pub fn fingerprint(self) -> String {
        let hashes = self.hashes();
        hex::encode(hash_fields(
            b"cass.semantic-source.contract.v1",
            &[
                &hashes.selection,
                &hashes.canonicalization,
                &hashes.chunking,
                &hashes.document_id,
                &hashes.config,
            ],
        ))
    }

    fn hashes(self) -> ContractHashes {
        let canonicalization = hash_fields(
            b"cass.semantic-source.canonicalization.v1",
            &[
                b"DefaultCanonicalizer:NFC-markdown-code-whitespace-low-signal-v1",
                &(MAX_EMBED_CHARS as u64).to_be_bytes(),
                &(CODE_HEAD_LINES as u64).to_be_bytes(),
                &(CODE_TAIL_LINES as u64).to_be_bytes(),
                include_bytes!("../canonicalize.rs"),
            ],
        );
        let projection: &[u8] = match self.projection {
            PassageProjection::CanonicalPrefixV1 => b"canonical-prefix-v1;chunk=0",
            PassageProjection::RawPassagesV2 => b"raw-passages-v2;canonicalize-each-window",
        };
        ContractHashes {
            selection: hash_fields(
                b"cass.semantic-source.selection.v1",
                &[b"all-canonical-message-roles;nonempty-canonical-passages", include_bytes!("../../model/conversation_packet.rs")],
            ),
            canonicalization,
            chunking: hash_fields(
                b"cass.semantic-source.chunking.v1",
                &[
                    projection,
                    &CHUNKING_STRATEGY_VERSION.to_be_bytes(),
                    &(EMBEDDING_PASSAGE_CHARS as u64).to_be_bytes(),
                    &(MAX_EMBEDDING_PASSAGES as u64).to_be_bytes(),
                ],
            ),
            document_id: hash_fields(
                b"cass.semantic-source.document-id.v1",
                &[b"SemanticDocId:message,chunk,agent,workspace,crc32-source,role,time,sha256-canonical;null-time=0;saturating-u32", include_bytes!("../vector_index.rs")],
            ),
            config: hash_fields(
                b"cass.semantic-source.config.v1",
                &[&SEMANTIC_SCHEMA_VERSION.to_be_bytes(), projection,
                    include_bytes!("source_snapshot.rs"),
                    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock"))],
            ),
        }
    }
}

/// Cooperative bounds on a complete source scan. They bound transferred bodies
/// and retained identity payloads, not SQLite scratch, allocator overhead, or a
/// native SQL/canonicalization call's execution time. Exhaustion returns no proof.
#[derive(Debug, Clone, Copy)]
pub struct SemanticSourceSnapshotBudget {
    pub max_documents: usize,
    pub max_identity_bytes: usize,
    pub max_message_bytes: usize,
    pub max_total_source_bytes: u64,
    pub timeout: Duration,
}

impl Default for SemanticSourceSnapshotBudget {
    fn default() -> Self {
        Self {
            max_documents: 1_000_000,
            max_identity_bytes: 256 * 1024 * 1024,
            max_message_bytes: 8 * 1024 * 1024,
            max_total_source_bytes: 16 * 1024 * 1024 * 1024,
            timeout: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SemanticSourceSnapshotError {
    #[error("semantic source identity is absent; initialize it during explicit index maintenance")]
    Uninitialized,
    #[error("semantic source identity or change sequence is invalid")]
    InvalidIdentity,
    #[error("canonical source schema is incomplete or exceeds its inspection budget")]
    InvalidSchema,
    #[error("canonical source contains an invalid or missing message identity")]
    InvalidSource,
    #[error(
        "semantic source has no certificate for this explicit projection; index it before serving"
    )]
    MissingCertificate,
    #[error("semantic source changed after its certificate; rebuild its semantic generation")]
    StaleCertificate,
    #[error("semantic source certificate is incomplete or inconsistent with its source authority")]
    InvalidCertificate,
    #[error("canonical source contains a duplicate semantic document identity")]
    DuplicateDocument,
    #[error("semantic source snapshot exceeded its document or byte budget")]
    BudgetExceeded,
    #[error("semantic source snapshot exceeded its cooperative deadline")]
    DeadlineExceeded,
    #[error("canonical source read failed while inspecting its pinned snapshot")]
    Database,
}

impl SemanticSourceSnapshotError {
    #[must_use]
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::Uninitialized => "semantic_source_uninitialized",
            Self::InvalidIdentity => "semantic_source_identity_invalid",
            Self::InvalidSchema => "semantic_source_schema_invalid",
            Self::InvalidSource => "semantic_source_row_invalid",
            Self::MissingCertificate => "semantic_source_certificate_missing",
            Self::StaleCertificate => "semantic_source_certificate_stale",
            Self::InvalidCertificate => "semantic_source_certificate_invalid",
            Self::DuplicateDocument => "semantic_source_duplicate_document",
            Self::BudgetExceeded => "semantic_source_snapshot_budget_exceeded",
            Self::DeadlineExceeded => "semantic_source_snapshot_deadline_exceeded",
            Self::Database => "semantic_source_snapshot_database",
        }
    }
}

/// A complete independently read source identity and its explicit projection.
/// Construction is restricted to a successful scan of an existing transaction.
#[derive(Debug, Clone)]
pub struct SemanticSourceSnapshot {
    identity: SemanticCorpusSnapshotIdentity,
    contract: SemanticSourceContract,
}

impl SemanticSourceSnapshot {
    #[must_use]
    pub fn identity(&self) -> &SemanticCorpusSnapshotIdentity {
        &self.identity
    }

    #[must_use]
    pub const fn contract(&self) -> SemanticSourceContract {
        self.contract
    }
}

/// A borrowed passage admitted from the caller's actual source transaction.
/// There is no constructor from a caller-provided ID, text or digest. The raw
/// passage is the producer input; canonical text independently determines its
/// document identity. Neither borrow can escape the synchronous callback.
pub struct SemanticSourcePassage<'source> {
    document: SemanticDocId,
    raw: &'source str,
    canonical: &'source str,
}

impl fmt::Debug for SemanticSourcePassage<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SemanticSourcePassage")
            .field("message_id", &self.document.message_id)
            .field("chunk_idx", &self.document.chunk_idx)
            .field("raw_bytes", &self.raw.len())
            .field("canonical_bytes", &self.canonical.len())
            .finish_non_exhaustive()
    }
}

impl SemanticSourcePassage<'_> {
    #[must_use]
    pub const fn document(&self) -> SemanticDocId {
        self.document
    }

    #[must_use]
    pub const fn raw(&self) -> &str {
        self.raw
    }

    #[must_use]
    pub const fn canonical(&self) -> &str {
        self.canonical
    }
}

/// A failed traversal returns no source certificate, including when a consumer
/// has already prepared an unpublished prefix. The original consumer error is
/// retained for deliberate handling; default displays contain no source text.
#[derive(Debug)]
pub enum SemanticSourceVisitError<E> {
    Source(SemanticSourceSnapshotError),
    Consumer(E),
}

impl<E> From<SemanticSourceSnapshotError> for SemanticSourceVisitError<E> {
    fn from(error: SemanticSourceSnapshotError) -> Self {
        Self::Source(error)
    }
}

impl<E> fmt::Display for SemanticSourceVisitError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => fmt::Display::fmt(error, formatter),
            Self::Consumer(_) => {
                formatter.write_str("semantic source passage consumer refused the snapshot")
            }
        }
    }
}

impl<E: fmt::Debug> std::error::Error for SemanticSourceVisitError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Consumer(_) => None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceCertificate {
    schema_version: u32,
    contract_sha256: String,
    identity: SemanticCorpusSnapshotIdentity,
}

/// Read a source certificate written by explicit maintenance. This performs
/// bounded source-authority/schema reads, never a corpus scan or a model load.
/// A changed clock is an error even when document counts are unchanged.
pub fn semantic_source_certificate_in_transaction<R: SemanticReadSnapshot>(
    transaction: &R,
    contract: SemanticSourceContract,
) -> Result<SemanticSourceSnapshot, SemanticSourceSnapshotError> {
    require_clock_contract(transaction)?;
    let schema = read_schema(transaction)?;
    let authority = read_identity(transaction)?;
    let key = contract.fingerprint();
    let rows = query(
        transaction,
        "SELECT length(CAST(snapshot_json AS BLOB)), length(CAST(snapshot_sha256 AS BLOB)) FROM semantic_source_certificates_v1 WHERE contract_sha256 = ?1 LIMIT 2",
        &[SqliteValue::Text(key.clone().into())],
    )?;
    if rows.is_empty() {
        return Err(SemanticSourceSnapshotError::MissingCertificate);
    }
    if rows.len() != 1 {
        return Err(SemanticSourceSnapshotError::InvalidCertificate);
    }
    let row = rows
        .first()
        .ok_or(SemanticSourceSnapshotError::InvalidCertificate)?;
    let json_bytes: i64 = row
        .get_typed(0)
        .map_err(|_| SemanticSourceSnapshotError::InvalidCertificate)?;
    let digest_bytes: i64 = row
        .get_typed(1)
        .map_err(|_| SemanticSourceSnapshotError::InvalidCertificate)?;
    if !(1..=MAX_CERTIFICATE_BYTES as i64).contains(&json_bytes) || digest_bytes != 64 {
        return Err(SemanticSourceSnapshotError::InvalidCertificate);
    }
    let rows = query(
        transaction,
        "SELECT snapshot_json, snapshot_sha256 FROM semantic_source_certificates_v1 WHERE contract_sha256 = ?1 LIMIT 2",
        &[SqliteValue::Text(key.clone().into())],
    )?;
    if rows.len() != 1 {
        return Err(SemanticSourceSnapshotError::InvalidCertificate);
    }
    let row = rows
        .first()
        .ok_or(SemanticSourceSnapshotError::InvalidCertificate)?;
    let json: String = row
        .get_typed(0)
        .map_err(|_| SemanticSourceSnapshotError::InvalidCertificate)?;
    let expected: String = row
        .get_typed(1)
        .map_err(|_| SemanticSourceSnapshotError::InvalidCertificate)?;
    if json.len() as i64 != json_bytes || hex::encode(Sha256::digest(json.as_bytes())) != expected {
        return Err(SemanticSourceSnapshotError::InvalidCertificate);
    }
    let certificate: SourceCertificate =
        serde_json::from_str(&json).map_err(|_| SemanticSourceSnapshotError::InvalidCertificate)?;
    if certificate.schema_version != 1 || certificate.contract_sha256 != key {
        return Err(SemanticSourceSnapshotError::InvalidCertificate);
    }
    super::validate_corpus_identity(&certificate.identity)
        .map_err(|_| SemanticSourceSnapshotError::InvalidCertificate)?;
    let hashes = contract.hashes();
    if certificate.identity.content_selection_sha256 != hex::encode(hashes.selection)
        || certificate.identity.canonicalization_sha256 != hex::encode(hashes.canonicalization)
        || certificate.identity.chunking_sha256 != hex::encode(hashes.chunking)
        || certificate.identity.document_id_contract_sha256 != hex::encode(hashes.document_id)
        || certificate.identity.config_sha256 != hex::encode(hashes.config)
    {
        return Err(SemanticSourceSnapshotError::InvalidCertificate);
    }
    require_current(&certificate.identity, &authority, schema)?;
    let docset: [u8; 32] = hex::decode(&certificate.identity.ordered_live_docset_sha256)
        .map_err(|_| SemanticSourceSnapshotError::InvalidCertificate)?
        .try_into()
        .map_err(|_| SemanticSourceSnapshotError::InvalidCertificate)?;
    let content: [u8; 32] = hex::decode(&certificate.identity.content_sha256)
        .map_err(|_| SemanticSourceSnapshotError::InvalidCertificate)?
        .try_into()
        .map_err(|_| SemanticSourceSnapshotError::InvalidCertificate)?;
    if certificate.identity.corpus_sha256
        != hex::encode(corpus_digest(
            &authority,
            schema,
            &hashes,
            docset,
            content,
            certificate.identity.document_count,
        ))
    {
        return Err(SemanticSourceSnapshotError::InvalidCertificate);
    }
    Ok(SemanticSourceSnapshot {
        identity: certificate.identity,
        contract,
    })
}

/// Save only a snapshot produced by the complete authoritative scan above.
/// The caller owns this writable transaction and its commit/rollback. Recheck
/// source authority and schema before saving, so an earlier scan cannot certify
/// a newer archive. Certificate writes do not advance the canonical clock.
pub fn persist_semantic_source_certificate_in_transaction(
    transaction: &Transaction<'_>,
    snapshot: &SemanticSourceSnapshot,
) -> Result<(), SemanticSourceSnapshotError> {
    require_clock_contract(transaction)?;
    let schema = read_schema(transaction)?;
    let authority = read_identity(transaction)?;
    require_current(snapshot.identity(), &authority, schema)?;
    let certificate = SourceCertificate {
        schema_version: 1,
        contract_sha256: snapshot.contract.fingerprint(),
        identity: snapshot.identity.clone(),
    };
    let json = serde_json::to_string(&certificate)
        .map_err(|_| SemanticSourceSnapshotError::InvalidCertificate)?;
    if json.len() > MAX_CERTIFICATE_BYTES {
        return Err(SemanticSourceSnapshotError::InvalidCertificate);
    }
    let digest = hex::encode(Sha256::digest(json.as_bytes()));
    transaction.execute_with_params(
        "INSERT OR REPLACE INTO semantic_source_certificates_v1(contract_sha256, snapshot_json, snapshot_sha256) VALUES(?1, ?2, ?3)",
        &[SqliteValue::Text(certificate.contract_sha256.into()), SqliteValue::Text(json.into()), SqliteValue::Text(digest.into())],
    ).map_err(|_| SemanticSourceSnapshotError::Database)?;
    Ok(())
}

fn require_current(
    identity: &SemanticCorpusSnapshotIdentity,
    authority: &SourceIdentity,
    schema: [u8; 32],
) -> Result<(), SemanticSourceSnapshotError> {
    if identity.database_uuid != authority.database_uuid
        || identity.source_change_sequence != authority.change_sequence
        || identity.schema_contract_sha256 != hex::encode(schema)
    {
        return Err(SemanticSourceSnapshotError::StaleCertificate);
    }
    Ok(())
}

fn require_clock_contract<R: SemanticReadSnapshot>(
    transaction: &R,
) -> Result<(), SemanticSourceSnapshotError> {
    let mut expected: BTreeMap<_, _> =
        crate::storage::sqlite::semantic_source_clock_trigger_contracts()
            .into_iter()
            .map(|(name, sql)| (name, normalized_clock_sql(&sql)))
            .collect();
    let rows = query(
        transaction,
        "SELECT CASE WHEN length(CAST(name AS BLOB)) <= 128 THEN name ELSE NULL END, CASE WHEN length(CAST(sql AS BLOB)) <= 16384 THEN sql ELSE NULL END FROM sqlite_master WHERE type = 'trigger' AND name LIKE 'cass_semantic_source_%' ORDER BY name LIMIT 19",
        &[],
    )?;
    if rows.len() != expected.len() {
        return Err(SemanticSourceSnapshotError::InvalidSchema);
    }
    for row in rows {
        let name: String = row
            .get_typed(0)
            .map_err(|_| SemanticSourceSnapshotError::InvalidSchema)?;
        let sql: String = row
            .get_typed(1)
            .map_err(|_| SemanticSourceSnapshotError::InvalidSchema)?;
        if expected.remove(&name).as_deref() != Some(normalized_clock_sql(&sql).as_str()) {
            return Err(SemanticSourceSnapshotError::InvalidSchema);
        }
    }
    if !expected.is_empty() {
        return Err(SemanticSourceSnapshotError::InvalidSchema);
    }
    Ok(())
}

fn normalized_clock_sql(sql: &str) -> String {
    // Catalogs may omit IF NOT EXISTS and the final delimiter. No other SQL
    // tokens are removed or interpreted; the compiled statement must match.
    sql.trim()
        .trim_end_matches(';')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
        .replacen("CREATE TRIGGER IF NOT EXISTS ", "CREATE TRIGGER ", 1)
}

/// Inspect the caller's actual read transaction without adopting its lifecycle.
/// All schema, authority, body and identity reads use that same snapshot.
pub fn semantic_source_snapshot_in_transaction<R: SemanticReadSnapshot>(
    transaction: &R,
    contract: SemanticSourceContract,
    budget: SemanticSourceSnapshotBudget,
) -> Result<SemanticSourceSnapshot, SemanticSourceSnapshotError> {
    match visit_semantic_source_passages_in_transaction(transaction, contract, budget, |_| {
        Ok::<(), Infallible>(())
    }) {
        Ok(snapshot) => Ok(snapshot),
        Err(SemanticSourceVisitError::Source(error)) => Err(error),
        Err(SemanticSourceVisitError::Consumer(impossible)) => match impossible {},
    }
}

struct ContractHashes {
    selection: [u8; 32],
    canonicalization: [u8; 32],
    chunking: [u8; 32],
    document_id: [u8; 32],
    config: [u8; 32],
}

#[derive(PartialEq, Eq)]
struct SourceIdentity {
    database_uuid: String,
    change_sequence: u64,
}

struct SourceDocument {
    key: u64,
    id: String,
    content_sha256: [u8; 32],
}

struct Work {
    budget: SemanticSourceSnapshotBudget,
    started: Instant,
    source_bytes: u64,
    identity_bytes: usize,
    documents: Vec<SourceDocument>,
}

impl Work {
    fn check_time(&self) -> Result<(), SemanticSourceSnapshotError> {
        if self.started.elapsed() >= self.budget.timeout {
            return Err(SemanticSourceSnapshotError::DeadlineExceeded);
        }
        Ok(())
    }

    fn add(
        &mut self,
        document: SemanticDocId,
        canonical: &str,
    ) -> Result<(), SemanticSourceSnapshotError> {
        self.check_time()?;
        let id = document.to_doc_id_string();
        self.identity_bytes = self
            .identity_bytes
            .checked_add(id.capacity())
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<SourceDocument>()))
            .ok_or(SemanticSourceSnapshotError::BudgetExceeded)?;
        if self.documents.len() >= self.budget.max_documents
            || self.identity_bytes > self.budget.max_identity_bytes
        {
            return Err(SemanticSourceSnapshotError::BudgetExceeded);
        }
        let key = id
            .as_bytes()
            .iter()
            .fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x00000100000001b3)
            });
        self.documents
            .try_reserve(1)
            .map_err(|_| SemanticSourceSnapshotError::BudgetExceeded)?;
        self.documents.push(SourceDocument {
            key,
            id,
            content_sha256: content_hash(canonical),
        });
        Ok(())
    }
}

/// Stream source-admitted raw passages without retaining an archive-sized
/// text/vector collection. A successful final snapshot certifies the complete
/// traversal, never a consumer's prefix. No source writes, transaction lifecycle
/// or artifact publication occur here. A consumer must keep prepared artifacts
/// unpublished until this complete result and its final source check succeed.
pub fn visit_semantic_source_passages_in_transaction<R, E, V>(
    transaction: &R,
    contract: SemanticSourceContract,
    budget: SemanticSourceSnapshotBudget,
    mut visitor: V,
) -> Result<SemanticSourceSnapshot, SemanticSourceVisitError<E>>
where
    R: SemanticReadSnapshot,
    V: for<'source> FnMut(SemanticSourcePassage<'source>) -> Result<(), E>,
{
    let mut work = Work {
        budget,
        started: Instant::now(),
        source_bytes: 0,
        identity_bytes: 0,
        documents: Vec::new(),
    };
    work.check_time()?;
    let schema = read_schema(transaction)?;
    let authority = read_identity(transaction)?;
    if !query(
        transaction,
        "SELECT id FROM messages WHERE id < 0 LIMIT 1",
        &[],
    )?
    .is_empty()
    {
        return Err(SemanticSourceSnapshotError::InvalidSource.into());
    }
    let mut after_id = -1_i64;
    loop {
        work.check_time()?;
        let rows = query(
            transaction,
            "SELECT id, length(CAST(content AS BLOB)) FROM messages WHERE id > ?1 ORDER BY id LIMIT ?2",
            &[
                SqliteValue::Integer(after_id),
                SqliteValue::Integer(PAGE_ROWS as i64),
            ],
        )?;
        if rows.len() > PAGE_ROWS {
            return Err(SemanticSourceSnapshotError::InvalidSource.into());
        }
        let exhausted = rows.len() < PAGE_ROWS;
        for row in rows {
            work.check_time()?;
            let id: i64 = row
                .get_typed(0)
                .map_err(|_| SemanticSourceSnapshotError::InvalidSource)?;
            let bytes: i64 = row
                .get_typed(1)
                .map_err(|_| SemanticSourceSnapshotError::InvalidSource)?;
            if id <= after_id || id < 0 {
                return Err(SemanticSourceSnapshotError::InvalidSource.into());
            }
            let bytes =
                u64::try_from(bytes).map_err(|_| SemanticSourceSnapshotError::InvalidSource)?;
            work.source_bytes = work
                .source_bytes
                .checked_add(bytes)
                .ok_or(SemanticSourceSnapshotError::BudgetExceeded)?;
            if bytes > budget.max_message_bytes as u64
                || work.source_bytes > budget.max_total_source_bytes
            {
                return Err(SemanticSourceSnapshotError::BudgetExceeded.into());
            }
            read_document(transaction, id, bytes, contract, &mut work, &mut visitor)?;
            after_id = id;
        }
        if exhausted {
            break;
        }
    }
    work.check_time()?;
    if read_identity(transaction)? != authority {
        return Err(SemanticSourceSnapshotError::InvalidIdentity.into());
    }
    if read_schema(transaction)? != schema {
        return Err(SemanticSourceSnapshotError::InvalidSchema.into());
    }
    finish(authority, schema, contract, work).map_err(Into::into)
}

fn query<R: SemanticReadSnapshot>(
    transaction: &R,
    sql: &str,
    params: &[SqliteValue],
) -> Result<Vec<Row>, SemanticSourceSnapshotError> {
    transaction
        .query_with_params(sql, params)
        .map_err(|_| SemanticSourceSnapshotError::Database)
}

fn read_identity<R: SemanticReadSnapshot>(
    transaction: &R,
) -> Result<SourceIdentity, SemanticSourceSnapshotError> {
    let rows = query(
        transaction,
        "SELECT CASE WHEN length(CAST(database_uuid AS BLOB)) = 32 THEN database_uuid ELSE NULL END, change_sequence FROM semantic_source_identity_v1 WHERE singleton = 1 LIMIT 2",
        &[],
    )?;
    if rows.is_empty() {
        return Err(SemanticSourceSnapshotError::Uninitialized);
    }
    if rows.len() != 1 {
        return Err(SemanticSourceSnapshotError::InvalidIdentity);
    }
    let row = rows
        .first()
        .ok_or(SemanticSourceSnapshotError::InvalidIdentity)?;
    let database_uuid: String = row
        .get_typed(0)
        .map_err(|_| SemanticSourceSnapshotError::InvalidIdentity)?;
    let change_sequence: i64 = row
        .get_typed(1)
        .map_err(|_| SemanticSourceSnapshotError::InvalidIdentity)?;
    if database_uuid.len() != 32
        || !database_uuid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(SemanticSourceSnapshotError::InvalidIdentity);
    }
    Ok(SourceIdentity {
        database_uuid,
        change_sequence: u64::try_from(change_sequence)
            .map_err(|_| SemanticSourceSnapshotError::InvalidIdentity)?,
    })
}

fn read_schema<R: SemanticReadSnapshot>(
    transaction: &R,
) -> Result<[u8; 32], SemanticSourceSnapshotError> {
    let rows = query(
        transaction,
        "SELECT type, CASE WHEN length(CAST(name AS BLOB)) <= 4096 THEN name ELSE NULL END, tbl_name, length(CAST(COALESCE(sql, '') AS BLOB)) FROM sqlite_master WHERE tbl_name IN ('agents', 'workspaces', 'sources', 'conversations', 'messages', 'semantic_source_identity_v1') ORDER BY type, name LIMIT 257",
        &[],
    )?;
    if rows.len() > MAX_SCHEMA_ROWS {
        return Err(SemanticSourceSnapshotError::InvalidSchema);
    }
    let mut tables = BTreeSet::new();
    let mut bytes = 0_usize;
    let mut digest = begin_digest(b"cass.semantic-source.schema.v1");
    for row in rows {
        let kind: String = row
            .get_typed(0)
            .map_err(|_| SemanticSourceSnapshotError::InvalidSchema)?;
        let name: String = row
            .get_typed(1)
            .map_err(|_| SemanticSourceSnapshotError::InvalidSchema)?;
        let table: String = row
            .get_typed(2)
            .map_err(|_| SemanticSourceSnapshotError::InvalidSchema)?;
        let sql_bytes: i64 = row
            .get_typed(3)
            .map_err(|_| SemanticSourceSnapshotError::InvalidSchema)?;
        let sql_bytes =
            usize::try_from(sql_bytes).map_err(|_| SemanticSourceSnapshotError::InvalidSchema)?;
        bytes = bytes
            .checked_add(kind.len())
            .and_then(|n| n.checked_add(name.len()))
            .and_then(|n| n.checked_add(table.len()))
            .and_then(|n| n.checked_add(sql_bytes))
            .ok_or(SemanticSourceSnapshotError::InvalidSchema)?;
        if bytes > MAX_SCHEMA_BYTES {
            return Err(SemanticSourceSnapshotError::InvalidSchema);
        }
        let payload = query(
            transaction,
            "SELECT sql FROM sqlite_master WHERE type = ?1 AND name = ?2 LIMIT 2",
            &[
                // ubs:ignore[rust.collections.clone-in-loop] -- Bound schema lookup parameters own their text while the original kind still feeds the snapshot digest.
                SqliteValue::Text(kind.clone().into()),
                // ubs:ignore[rust.collections.clone-in-loop] -- Keep the schema name for canonical hashing and the required-table set after this owned parameter query.
                SqliteValue::Text(name.clone().into()),
            ],
        )?;
        if payload.len() != 1 {
            return Err(SemanticSourceSnapshotError::InvalidSchema);
        }
        let sql: Option<String> = payload
            .first()
            .ok_or(SemanticSourceSnapshotError::InvalidSchema)?
            .get_typed(0)
            .map_err(|_| SemanticSourceSnapshotError::InvalidSchema)?;
        if sql.as_ref().map_or(0, String::len) != sql_bytes {
            return Err(SemanticSourceSnapshotError::InvalidSchema);
        }
        for value in [&kind, &name, &table] {
            digest_field(&mut digest, value.as_bytes());
        }
        digest_field(&mut digest, sql.as_deref().unwrap_or_default().as_bytes());
        if kind == "table" {
            tables.insert(name);
        }
    }
    if !tables.contains(SOURCE_IDENTITY_TABLE) {
        return Err(SemanticSourceSnapshotError::Uninitialized);
    }
    for table in [
        "agents",
        "workspaces",
        "sources",
        "conversations",
        "messages",
    ] {
        if !tables.contains(table) {
            return Err(SemanticSourceSnapshotError::InvalidSchema);
        }
    }
    Ok(digest.finalize().into())
}

fn read_document<R, E, V>(
    transaction: &R,
    id: i64,
    expected_bytes: u64,
    contract: SemanticSourceContract,
    work: &mut Work,
    visitor: &mut V,
) -> Result<(), SemanticSourceVisitError<E>>
where
    R: SemanticReadSnapshot,
    V: for<'source> FnMut(SemanticSourcePassage<'source>) -> Result<(), E>,
{
    // Admit metadata before transferring it, just as message bodies are sized
    // before loading. NULL source metadata is legitimate; oversize is an error,
    // never a reason to substitute a local/default source identity.
    // ubs:ignore[rust.domain.sql-concat] -- SQL adds byte lengths inside a fixed statement; the sole runtime message ID is bound as ?1.
    const METADATA_SIZE_SQL: &str = "SELECT length(CAST(COALESCE(m.role, '') AS BLOB)) + length(CAST(COALESCE(c.source_id, '') AS BLOB)) + length(CAST(COALESCE(c.origin_host, '') AS BLOB)) FROM messages m JOIN conversations c ON c.id = m.conversation_id WHERE m.id = ?1 LIMIT 2";
    let sizes = query(transaction, METADATA_SIZE_SQL, &[SqliteValue::Integer(id)])?;
    if sizes.len() != 1 {
        return Err(SemanticSourceSnapshotError::InvalidSource.into());
    }
    let metadata_bytes: i64 = sizes
        .first()
        .ok_or(SemanticSourceSnapshotError::InvalidSource)?
        .get_typed(0)
        .map_err(|_| SemanticSourceSnapshotError::InvalidSource)?;
    if !(0..=48 * 1024).contains(&metadata_bytes) {
        return Err(SemanticSourceSnapshotError::InvalidSource.into());
    }
    work.check_time()?;
    let rows = query(
        transaction,
        "SELECT m.content, m.role, m.created_at, c.agent_id, c.workspace_id, c.source_id, c.origin_host FROM messages m JOIN conversations c ON c.id = m.conversation_id WHERE m.id = ?1 LIMIT 2",
        &[SqliteValue::Integer(id)],
    )?;
    if rows.len() != 1 {
        return Err(SemanticSourceSnapshotError::InvalidSource.into());
    }
    let row = rows
        .first()
        .ok_or(SemanticSourceSnapshotError::InvalidSource)?;
    let content: String = row
        .get_typed(0)
        .map_err(|_| SemanticSourceSnapshotError::InvalidSource)?;
    let role: String = row
        .get_typed(1)
        .map_err(|_| SemanticSourceSnapshotError::InvalidSource)?;
    let created_at: Option<i64> = row
        .get_typed(2)
        .map_err(|_| SemanticSourceSnapshotError::InvalidSource)?;
    let agent_id: Option<i64> = row
        .get_typed(3)
        .map_err(|_| SemanticSourceSnapshotError::InvalidSource)?;
    let workspace_id: Option<i64> = row
        .get_typed(4)
        .map_err(|_| SemanticSourceSnapshotError::InvalidSource)?;
    let source_id: Option<String> = row
        .get_typed(5)
        .map_err(|_| SemanticSourceSnapshotError::InvalidSource)?;
    let origin_host: Option<String> = row
        .get_typed(6)
        .map_err(|_| SemanticSourceSnapshotError::InvalidSource)?;
    if content.len() as u64 != expected_bytes
        || role.len() > 16 * 1024
        || source_id.as_ref().is_some_and(|id| id.len() > 16 * 1024)
        || origin_host
            .as_ref()
            .is_some_and(|host| host.len() > 16 * 1024)
    {
        return Err(SemanticSourceSnapshotError::InvalidSource.into());
    }
    let source = normalized_index_source_id(source_id.as_deref(), None, origin_host.as_deref());
    let role = role.trim().to_ascii_lowercase();
    let passages = match contract.projection {
        PassageProjection::CanonicalPrefixV1 => vec![content.as_str()],
        PassageProjection::RawPassagesV2 => embedding_passages(&content),
    };
    for (ordinal, passage) in passages.into_iter().enumerate() {
        work.check_time()?;
        let canonical = canonicalize_for_embedding(passage);
        if canonical.is_empty() {
            continue;
        }
        let document = SemanticDocId {
            message_id: u64::try_from(id)
                .map_err(|_| SemanticSourceSnapshotError::InvalidSource)?,
            chunk_idx: u8::try_from(ordinal)
                .map_err(|_| SemanticSourceSnapshotError::InvalidSource)?,
            agent_id: saturating_id(agent_id.unwrap_or(0)),
            workspace_id: saturating_id(workspace_id.unwrap_or(0)),
            source_id: crc32fast::hash(source.as_bytes()),
            role: role_code_from_str(&role).unwrap_or(ROLE_USER),
            created_at_ms: created_at.unwrap_or(0),
            content_hash: Some(content_hash(&canonical)),
        };
        work.add(document, &canonical)?;
        visitor(SemanticSourcePassage {
            document,
            raw: passage,
            canonical: &canonical,
        })
        .map_err(SemanticSourceVisitError::Consumer)?;
    }
    Ok(())
}

fn saturating_id(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(if value.is_negative() { 0 } else { u32::MAX })
}

fn finish(
    authority: SourceIdentity,
    schema: [u8; 32],
    contract: SemanticSourceContract,
    mut work: Work,
) -> Result<SemanticSourceSnapshot, SemanticSourceSnapshotError> {
    work.documents
        .sort_unstable_by(|left, right| (left.key, &left.id).cmp(&(right.key, &right.id)));
    let count = u64::try_from(work.documents.len())
        .map_err(|_| SemanticSourceSnapshotError::BudgetExceeded)?;
    let mut docset = begin_digest(b"frankensearch.fsvi-v2.ordered-live-docset.v1");
    docset.update(count.to_be_bytes());
    let mut previous: Option<&str> = None;
    for document in &work.documents {
        work.check_time()?;
        if previous == Some(document.id.as_str()) {
            return Err(SemanticSourceSnapshotError::DuplicateDocument);
        }
        previous = Some(&document.id);
        digest_field(&mut docset, document.id.as_bytes());
    }
    let docset: [u8; 32] = docset.finalize().into();
    let content = ordered_source_content_digest(
        work.documents
            .iter()
            .map(|document| (document.id.as_str(), document.content_sha256)),
    );
    work.check_time()?;
    let hashes = contract.hashes();
    let corpus = corpus_digest(&authority, schema, &hashes, docset, content, count);
    Ok(SemanticSourceSnapshot {
        identity: SemanticCorpusSnapshotIdentity {
            database_uuid: authority.database_uuid,
            source_change_sequence: authority.change_sequence,
            schema_contract_sha256: hex::encode(schema),
            content_selection_sha256: hex::encode(hashes.selection),
            canonicalization_sha256: hex::encode(hashes.canonicalization),
            chunking_sha256: hex::encode(hashes.chunking),
            document_id_contract_sha256: hex::encode(hashes.document_id),
            config_sha256: hex::encode(hashes.config),
            corpus_sha256: hex::encode(corpus),
            ordered_live_docset_sha256: hex::encode(docset),
            content_sha256: hex::encode(content),
            document_count: count,
        },
        contract,
    })
}

fn begin_digest(domain: &[u8]) -> Sha256 {
    let mut digest = Sha256::new();
    digest_field(&mut digest, domain);
    digest
}

/// The native ordered-docset transcript and CASS content transcript for an
/// actual zero-document source. No model or artifact identity is fabricated.
pub(crate) fn empty_source_digests() -> (String, String) {
    let mut docset = begin_digest(b"frankensearch.fsvi-v2.ordered-live-docset.v1");
    docset.update(0_u64.to_be_bytes());
    let mut content = begin_digest(b"cass.semantic-source.content.v1");
    content.update(0_u64.to_be_bytes());
    (
        hex::encode(docset.finalize()),
        hex::encode(content.finalize()),
    )
}

/// Hash an already ordered set of source-admitted IDs and canonical passage
/// digests. This is a transcript utility, not authority for arbitrary rows.
pub(crate) fn ordered_source_content_digest<'source>(
    documents: impl ExactSizeIterator<Item = (&'source str, [u8; 32])>,
) -> [u8; 32] {
    let mut content = begin_digest(b"cass.semantic-source.content.v1");
    content.update((documents.len() as u64).to_be_bytes());
    for (id, canonical_sha256) in documents {
        digest_field(&mut content, id.as_bytes());
        digest_field(&mut content, &canonical_sha256);
    }
    content.finalize().into()
}

fn corpus_digest(
    authority: &SourceIdentity,
    schema: [u8; 32],
    hashes: &ContractHashes,
    docset: [u8; 32],
    content: [u8; 32],
    count: u64,
) -> [u8; 32] {
    hash_fields(
        b"cass.semantic-source.corpus.v1",
        &[
            authority.database_uuid.as_bytes(),
            &authority.change_sequence.to_be_bytes(),
            &schema,
            &hashes.selection,
            &hashes.canonicalization,
            &hashes.chunking,
            &hashes.document_id,
            &hashes.config,
            &docset,
            &content,
            &count.to_be_bytes(),
        ],
    )
}

fn digest_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn hash_fields(domain: &[u8], fields: &[&[u8]]) -> [u8; 32] {
    let mut digest = begin_digest(domain);
    for field in fields {
        digest_field(&mut digest, field);
    }
    digest.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::franken_sync::{Connection, compat::TransactionExt};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    // Real FrankenSQLite reads; this fixture does not qualify clock triggers.
    fn fixture() -> Result<Connection, Box<dyn std::error::Error>> {
        let connection = Connection::open(":memory:")?;
        connection.execute_batch(
            "CREATE TABLE agents(id INTEGER PRIMARY KEY, slug TEXT);
             CREATE TABLE workspaces(id INTEGER PRIMARY KEY, path TEXT);
             CREATE TABLE sources(id TEXT PRIMARY KEY);
             CREATE TABLE conversations(id INTEGER PRIMARY KEY, agent_id INTEGER, workspace_id INTEGER, source_id TEXT, origin_host TEXT);
             CREATE TABLE messages(id INTEGER PRIMARY KEY, conversation_id INTEGER, content TEXT NOT NULL, role TEXT NOT NULL, created_at INTEGER);
             CREATE TABLE semantic_source_identity_v1(singleton INTEGER PRIMARY KEY CHECK(singleton = 1), database_uuid TEXT NOT NULL, change_sequence INTEGER NOT NULL CHECK(change_sequence >= 0));
             CREATE TABLE semantic_source_certificates_v1(contract_sha256 TEXT PRIMARY KEY, snapshot_json TEXT NOT NULL, snapshot_sha256 TEXT NOT NULL);
             INSERT INTO semantic_source_identity_v1 VALUES(1, '0123456789abcdef0123456789abcdef', 7);
             INSERT INTO agents VALUES(1, 'codex');
             INSERT INTO workspaces VALUES(2, '/fixture/workspace');
             INSERT INTO sources VALUES('local');
             INSERT INTO conversations VALUES(1, 1, 2, 'local', NULL);
             INSERT INTO messages VALUES(10, 1, 'first canonical source passage', 'user', 100);
             INSERT INTO messages VALUES(11, 1, 'second canonical source passage', 'assistant', 101);"
        )?;
        Ok(connection)
    }

    fn capture(
        connection: &Connection,
        projection: PassageProjection,
    ) -> Result<SemanticSourceSnapshot, Box<dyn std::error::Error>> {
        let mut transaction = connection.transaction()?;
        let snapshot = semantic_source_snapshot_in_transaction(
            &transaction,
            SemanticSourceContract::new(projection),
            SemanticSourceSnapshotBudget::default(),
        )?;
        transaction.rollback()?;
        Ok(snapshot)
    }

    #[test]
    fn source_empty_certificate_survives_both_canonical_transitions() -> TestResult {
        let connection = fixture()?;
        for (_, sql) in crate::storage::sqlite::semantic_source_clock_trigger_contracts() {
            connection.execute(&sql)?;
        }
        connection.execute("DELETE FROM messages")?;
        let contract = SemanticSourceContract::new(PassageProjection::RawPassagesV2);
        let mut transaction = connection.transaction()?;
        let mut calls = 0;
        let empty = visit_semantic_source_passages_in_transaction(
            &transaction,
            contract,
            SemanticSourceSnapshotBudget {
                max_documents: 0,
                max_identity_bytes: 0,
                ..Default::default()
            },
            |_| {
                calls += 1;
                Ok::<(), Infallible>(())
            },
        )?;
        assert_eq!(calls, 0, "empty source must not request any producer work");
        assert_eq!(empty.identity().document_count, 0);
        persist_semantic_source_certificate_in_transaction(&transaction, &empty)?;
        transaction.commit()?;
        let mut transaction = connection.transaction()?;
        assert_eq!(
            semantic_source_certificate_in_transaction(&transaction, contract)?.identity(),
            empty.identity()
        );
        transaction.rollback()?;
        connection.execute(
            "INSERT INTO messages VALUES(20, 1, 'source became populated', 'user', 102)",
        )?;
        let mut transaction = connection.transaction()?;
        assert_eq!(
            semantic_source_certificate_in_transaction(&transaction, contract).unwrap_err(),
            SemanticSourceSnapshotError::StaleCertificate
        );
        let populated =
            semantic_source_snapshot_in_transaction(&transaction, contract, Default::default())?;
        assert_eq!(populated.identity().document_count, 1);
        persist_semantic_source_certificate_in_transaction(&transaction, &populated)?;
        transaction.commit()?;
        connection.execute("DELETE FROM messages")?;
        let mut transaction = connection.transaction()?;
        assert_eq!(
            semantic_source_certificate_in_transaction(&transaction, contract).unwrap_err(),
            SemanticSourceSnapshotError::StaleCertificate
        );
        let empty_again =
            semantic_source_snapshot_in_transaction(&transaction, contract, Default::default())?;
        assert_eq!(empty_again.identity().document_count, 0);
        assert_eq!(
            empty_again.identity().ordered_live_docset_sha256,
            empty.identity().ordered_live_docset_sha256
        );
        assert_eq!(
            empty_again.identity().content_sha256,
            empty.identity().content_sha256
        );
        assert_ne!(
            empty_again.identity().corpus_sha256,
            empty.identity().corpus_sha256
        );
        assert!(
            empty_again.identity().source_change_sequence
                > populated.identity().source_change_sequence
        );
        persist_semantic_source_certificate_in_transaction(&transaction, &empty_again)?;
        transaction.rollback()?;
        Ok(())
    }

    #[test]
    fn source_passage_visitor_binds_raw_text_and_complete_snapshot() -> TestResult {
        let connection = fixture()?;
        connection.execute("UPDATE messages SET content = '**actual raw** passage with [markup](https://example.invalid)' WHERE id = 10")?;
        let contract = SemanticSourceContract::new(PassageProjection::RawPassagesV2);
        let mut transaction = connection.transaction()?;
        let mut documents = Vec::new();
        let scanned = visit_semantic_source_passages_in_transaction(
            &transaction,
            contract,
            Default::default(),
            |passage| {
                assert_eq!(
                    passage.canonical(),
                    canonicalize_for_embedding(passage.raw())
                );
                assert_eq!(
                    passage.document().content_hash,
                    Some(content_hash(passage.canonical()))
                );
                if passage.document().message_id == 10 {
                    assert!(passage.raw().contains("**actual raw**"));
                    assert!(!passage.canonical().contains("**"));
                    assert!(!format!("{passage:?}").contains("actual raw"));
                }
                documents.push(passage.document());
                Ok::<(), Infallible>(())
            },
        )?;
        assert_eq!(documents.len(), 2);
        assert_eq!(
            scanned.identity(),
            semantic_source_snapshot_in_transaction(&transaction, contract, Default::default())?
                .identity()
        );
        transaction.rollback()?;
        Ok(())
    }

    #[test]
    fn source_passage_consumer_failure_never_returns_a_prefix_certificate() -> TestResult {
        let connection = fixture()?;
        let mut transaction = connection.transaction()?;
        let mut calls = 0;
        let result = visit_semantic_source_passages_in_transaction(
            &transaction,
            SemanticSourceContract::new(PassageProjection::RawPassagesV2),
            Default::default(),
            |_| {
                calls += 1;
                Err::<(), _>("deliberate unpublished consumer refusal")
            },
        );
        assert!(matches!(result, Err(SemanticSourceVisitError::Consumer(_))));
        assert_eq!(calls, 1);
        assert_eq!(transaction.query("SELECT id FROM messages")?.len(), 2);
        transaction.rollback()?;
        Ok(())
    }

    #[test]
    fn source_passage_visitor_rejects_clock_and_schema_drift() -> TestResult {
        for change_schema in [false, true] {
            let connection = fixture()?;
            for (_, sql) in crate::storage::sqlite::semantic_source_clock_trigger_contracts() {
                connection.execute(&sql)?;
            }
            let mut transaction = connection.transaction()?;
            let mut calls = 0;
            let result = visit_semantic_source_passages_in_transaction(
                &transaction,
                SemanticSourceContract::new(PassageProjection::RawPassagesV2),
                Default::default(),
                |_| {
                    if calls == 0 {
                        let sql = if change_schema {
                            "CREATE INDEX changed_during_visit ON messages(role)"
                        } else {
                            // Preserve the preflight body length so this exercises
                            // the final source clock check rather than size drift.
                            "UPDATE messages SET content = 'mutate canonical source passage' WHERE id = 11"
                        };
                        transaction.execute(sql)?;
                    }
                    calls += 1;
                    Ok::<(), crate::franken_sync::FrankenError>(())
                },
            );
            assert_eq!(calls, 2);
            assert!(matches!(
                result,
                Err(SemanticSourceVisitError::Source(error))
                    if error == if change_schema {
                        SemanticSourceSnapshotError::InvalidSchema
                    } else {
                        SemanticSourceSnapshotError::InvalidIdentity
                    }
            ));
            transaction.rollback()?;
        }
        Ok(())
    }

    #[test]
    fn source_passage_visitor_rejects_body_length_drift_before_certifying() -> TestResult {
        let connection = fixture()?;
        let mut transaction = connection.transaction()?;
        let mut calls = 0;
        let result = visit_semantic_source_passages_in_transaction(
            &transaction,
            SemanticSourceContract::new(PassageProjection::RawPassagesV2),
            Default::default(),
            |_| {
                transaction.execute(
                    "UPDATE messages SET content = 'changed during visit' WHERE id = 11",
                )?;
                calls += 1;
                Ok::<(), crate::franken_sync::FrankenError>(())
            },
        );
        assert_eq!(calls, 1);
        assert!(matches!(
            result,
            Err(SemanticSourceVisitError::Source(
                SemanticSourceSnapshotError::InvalidSource
            ))
        ));
        transaction.rollback()?;
        Ok(())
    }

    #[test]
    fn semantic_source_certificate_uses_actual_migration_and_rejects_replacement() -> TestResult {
        use crate::storage::sqlite::FrankenStorage;

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("actual-source-certificate.db");
        let storage = FrankenStorage::open(&path)?;
        storage.raw().execute_batch(
            "INSERT INTO agents(id, slug, name, kind, created_at, updated_at) VALUES(9001, 'certificate', 'certificate', 'cli', 1, 1);
             INSERT INTO conversations(id, agent_id, source_path) VALUES(9002, 9001, '/certificate.jsonl');
             INSERT INTO messages(id, conversation_id, idx, role, content) VALUES(9003, 9002, 0, 'user', 'actual migrated source certificate');"
        )?;
        let contract = SemanticSourceContract::new(PassageProjection::RawPassagesV2);
        let mut transaction = storage.raw().transaction()?;
        let snapshot =
            semantic_source_snapshot_in_transaction(&transaction, contract, Default::default())?;
        // This admits the actual migration's catalog definitions, rather than
        // a fixture installed from the reader's expected-contract helper.
        persist_semantic_source_certificate_in_transaction(&transaction, &snapshot)?;
        transaction.commit()?;
        drop(transaction);
        assert!(storage.raw().execute_with_params(
            "INSERT OR REPLACE INTO semantic_source_identity_v1(singleton, database_uuid, change_sequence) VALUES(1, ?1, 0)",
            &[SqliteValue::Text(snapshot.identity().database_uuid.clone().into())],
        ).is_err());
        storage.close()?;
        let readonly = FrankenStorage::open_strict_readonly(&path)?;
        let mut transaction = readonly.raw().transaction()?;
        assert_eq!(
            semantic_source_certificate_in_transaction(&transaction, contract)?.identity(),
            snapshot.identity()
        );
        transaction.rollback()?;
        drop(transaction);
        readonly.close_without_checkpoint()?;
        let storage = FrankenStorage::open(&path)?;
        storage.raw().execute(
            "UPDATE messages SET content = 'changed actual migrated source' WHERE id = 9003",
        )?;
        let mut transaction = storage.raw().transaction()?;
        assert_eq!(
            semantic_source_certificate_in_transaction(&transaction, contract).unwrap_err(),
            SemanticSourceSnapshotError::StaleCertificate
        );
        transaction.rollback()?;
        Ok(())
    }

    #[test]
    fn source_certificate_obeys_caller_commit_and_refuses_changed_source() -> TestResult {
        let connection = fixture()?;
        for (_, sql) in crate::storage::sqlite::semantic_source_clock_trigger_contracts() {
            connection.execute(&sql)?;
        }
        let contract = SemanticSourceContract::new(PassageProjection::RawPassagesV2);
        let mut transaction = connection.transaction()?;
        assert_eq!(
            semantic_source_certificate_in_transaction(&transaction, contract).unwrap_err(),
            SemanticSourceSnapshotError::MissingCertificate
        );
        let snapshot =
            semantic_source_snapshot_in_transaction(&transaction, contract, Default::default())?;
        persist_semantic_source_certificate_in_transaction(&transaction, &snapshot)?;
        assert_eq!(
            semantic_source_certificate_in_transaction(&transaction, contract)?.identity(),
            snapshot.identity()
        );
        transaction.rollback()?;
        let mut transaction = connection.transaction()?;
        assert_eq!(
            semantic_source_certificate_in_transaction(&transaction, contract).unwrap_err(),
            SemanticSourceSnapshotError::MissingCertificate
        );
        persist_semantic_source_certificate_in_transaction(&transaction, &snapshot)?;
        transaction.commit()?;
        // ubs:ignore[rust.domain.sql-concat] -- This immutable negative fixture edits a fixed row and increments the SQL source clock; no Rust concatenation or runtime value enters it.
        const STALE_CERTIFICATE_SQL: &str = "UPDATE messages SET content = 'same-count edited source' WHERE id = 10; UPDATE semantic_source_identity_v1 SET change_sequence = change_sequence + 1 WHERE singleton = 1;";
        connection.execute_batch(STALE_CERTIFICATE_SQL)?;
        let mut transaction = connection.transaction()?;
        assert_eq!(
            semantic_source_certificate_in_transaction(&transaction, contract).unwrap_err(),
            SemanticSourceSnapshotError::StaleCertificate
        );
        assert_eq!(
            persist_semantic_source_certificate_in_transaction(&transaction, &snapshot)
                .unwrap_err(),
            SemanticSourceSnapshotError::StaleCertificate
        );
        assert_eq!(transaction.query("SELECT id FROM messages")?.len(), 2);
        transaction.rollback()?;
        Ok(())
    }

    #[test]
    fn source_certificate_rejects_a_named_but_nonadvancing_clock_trigger() -> TestResult {
        let connection = fixture()?;
        for (_, sql) in crate::storage::sqlite::semantic_source_clock_trigger_contracts() {
            connection.execute(&sql)?;
        }
        let contract = SemanticSourceContract::new(PassageProjection::RawPassagesV2);
        let mut transaction = connection.transaction()?;
        let snapshot =
            semantic_source_snapshot_in_transaction(&transaction, contract, Default::default())?;
        persist_semantic_source_certificate_in_transaction(&transaction, &snapshot)?;
        transaction.commit()?;
        connection.execute_batch(
            "DROP TRIGGER cass_semantic_source_messages_update;
             CREATE TRIGGER cass_semantic_source_messages_update AFTER UPDATE ON messages BEGIN SELECT 1; END;"
        )?;
        let mut transaction = connection.transaction()?;
        assert_eq!(
            semantic_source_certificate_in_transaction(&transaction, contract).unwrap_err(),
            SemanticSourceSnapshotError::InvalidSchema
        );
        assert_eq!(
            persist_semantic_source_certificate_in_transaction(&transaction, &snapshot)
                .unwrap_err(),
            SemanticSourceSnapshotError::InvalidSchema
        );
        transaction.rollback()?;
        Ok(())
    }

    #[test]
    fn source_certificate_rejects_corruption_and_wrong_explicit_projection() -> TestResult {
        let connection = fixture()?;
        for (_, sql) in crate::storage::sqlite::semantic_source_clock_trigger_contracts() {
            connection.execute(&sql)?;
        }
        let contract = SemanticSourceContract::new(PassageProjection::RawPassagesV2);
        let mut transaction = connection.transaction()?;
        let snapshot =
            semantic_source_snapshot_in_transaction(&transaction, contract, Default::default())?;
        persist_semantic_source_certificate_in_transaction(&transaction, &snapshot)?;
        assert_eq!(
            semantic_source_certificate_in_transaction(
                &transaction,
                SemanticSourceContract::new(PassageProjection::CanonicalPrefixV1)
            )
            .unwrap_err(),
            SemanticSourceSnapshotError::MissingCertificate
        );
        transaction.commit()?;
        connection
            .execute("UPDATE semantic_source_certificates_v1 SET snapshot_json = '{corrupt' ")?;
        let mut transaction = connection.transaction()?;
        assert_eq!(
            semantic_source_certificate_in_transaction(&transaction, contract).unwrap_err(),
            SemanticSourceSnapshotError::InvalidCertificate
        );
        transaction.rollback()?;
        Ok(())
    }

    #[test]
    fn source_digest_observes_same_count_content_and_filter_identity_changes() -> TestResult {
        let connection = fixture()?;
        let before = capture(&connection, PassageProjection::RawPassagesV2)?;
        connection.execute(
            "UPDATE messages SET content = 'changed canonical source passage' WHERE id = 10",
        )?;
        let edited = capture(&connection, PassageProjection::RawPassagesV2)?;
        assert_eq!(
            before.identity().document_count,
            edited.identity().document_count
        );
        assert_ne!(
            before.identity().content_sha256,
            edited.identity().content_sha256
        );
        assert_ne!(
            before.identity().ordered_live_docset_sha256,
            edited.identity().ordered_live_docset_sha256
        );
        assert_ne!(
            before.identity().corpus_sha256,
            edited.identity().corpus_sha256
        );
        connection.execute("UPDATE conversations SET workspace_id = 3 WHERE id = 1")?;
        let moved = capture(&connection, PassageProjection::RawPassagesV2)?;
        assert_eq!(
            edited.identity().document_count,
            moved.identity().document_count
        );
        assert_ne!(
            edited.identity().ordered_live_docset_sha256,
            moved.identity().ordered_live_docset_sha256
        );
        connection.execute("DELETE FROM messages WHERE id = 11")?;
        let removed = capture(&connection, PassageProjection::RawPassagesV2)?;
        assert_eq!(removed.identity().document_count, 1);
        assert_ne!(
            moved.identity().corpus_sha256,
            removed.identity().corpus_sha256
        );
        Ok(())
    }

    #[test]
    fn source_projection_is_explicit_and_keeps_caller_transaction_active() -> TestResult {
        let connection = fixture()?;
        let text = format!(
            "{} significant tail evidence",
            "long source material ".repeat(200)
        );
        connection.execute_with_params(
            "UPDATE messages SET content = ?1 WHERE id = 10",
            &[SqliteValue::Text(text.into())],
        )?;
        let mut transaction = connection.transaction()?;
        let prefix = semantic_source_snapshot_in_transaction(
            &transaction,
            SemanticSourceContract::new(PassageProjection::CanonicalPrefixV1),
            SemanticSourceSnapshotBudget::default(),
        )?;
        let passages = semantic_source_snapshot_in_transaction(
            &transaction,
            SemanticSourceContract::new(PassageProjection::RawPassagesV2),
            SemanticSourceSnapshotBudget::default(),
        )?;
        assert_eq!(prefix.identity().document_count, 2);
        assert!(passages.identity().document_count > prefix.identity().document_count);
        assert_ne!(
            prefix.identity().chunking_sha256,
            passages.identity().chunking_sha256
        );
        assert_eq!(
            passages.contract().projection(),
            PassageProjection::RawPassagesV2
        );
        assert_eq!(transaction.query("SELECT id FROM messages")?.len(), 2);
        transaction.rollback()?;
        Ok(())
    }

    #[test]
    fn source_scan_budget_or_missing_authority_returns_no_snapshot() -> TestResult {
        let connection = fixture()?;
        let mut transaction = connection.transaction()?;
        let contract = SemanticSourceContract::new(PassageProjection::RawPassagesV2);
        for budget in [
            SemanticSourceSnapshotBudget {
                max_documents: 1,
                ..Default::default()
            },
            SemanticSourceSnapshotBudget {
                max_identity_bytes: 1,
                ..Default::default()
            },
            SemanticSourceSnapshotBudget {
                max_message_bytes: 1,
                ..Default::default()
            },
            SemanticSourceSnapshotBudget {
                max_total_source_bytes: 1,
                ..Default::default()
            },
        ] {
            assert_eq!(
                semantic_source_snapshot_in_transaction(&transaction, contract, budget)
                    .unwrap_err(),
                SemanticSourceSnapshotError::BudgetExceeded
            );
        }
        assert_eq!(
            semantic_source_snapshot_in_transaction(
                &transaction,
                contract,
                SemanticSourceSnapshotBudget {
                    timeout: Duration::ZERO,
                    ..Default::default()
                }
            )
            .unwrap_err(),
            SemanticSourceSnapshotError::DeadlineExceeded
        );
        assert_eq!(transaction.query("SELECT id FROM messages")?.len(), 2);
        transaction.rollback()?;
        connection.execute("DELETE FROM semantic_source_identity_v1")?;
        let mut transaction = connection.transaction()?;
        assert_eq!(
            semantic_source_snapshot_in_transaction(&transaction, contract, Default::default())
                .unwrap_err(),
            SemanticSourceSnapshotError::Uninitialized
        );
        transaction.rollback()?;
        Ok(())
    }

    #[test]
    fn source_authority_changes_are_independent_of_corpus_counts() -> TestResult {
        let connection = fixture()?;
        let before = capture(&connection, PassageProjection::RawPassagesV2)?;
        connection.execute(
            "UPDATE semantic_source_identity_v1 SET change_sequence = 8 WHERE singleton = 1",
        )?;
        let advanced = capture(&connection, PassageProjection::RawPassagesV2)?;
        assert_eq!(
            before.identity().content_sha256,
            advanced.identity().content_sha256
        );
        assert_eq!(
            before.identity().ordered_live_docset_sha256,
            advanced.identity().ordered_live_docset_sha256
        );
        assert_ne!(
            before.identity().corpus_sha256,
            advanced.identity().corpus_sha256
        );
        connection.execute("UPDATE semantic_source_identity_v1 SET database_uuid = 'fedcba9876543210fedcba9876543210' WHERE singleton = 1")?;
        let foreign = capture(&connection, PassageProjection::RawPassagesV2)?;
        assert_ne!(
            advanced.identity().corpus_sha256,
            foreign.identity().corpus_sha256
        );
        connection.execute(
            "UPDATE semantic_source_identity_v1 SET database_uuid = 'invalid' WHERE singleton = 1",
        )?;
        let mut transaction = connection.transaction()?;
        assert_eq!(
            semantic_source_snapshot_in_transaction(
                &transaction,
                SemanticSourceContract::new(PassageProjection::RawPassagesV2),
                Default::default()
            )
            .unwrap_err(),
            SemanticSourceSnapshotError::InvalidIdentity
        );
        transaction.rollback()?;
        Ok(())
    }

    #[test]
    fn independent_source_docset_matches_public_retained_fsvi_hash_control_owner() -> TestResult {
        use crate::search::{embedder::Embedder, hash_embedder::HashEmbedder};
        use frankensearch::core::generation::{ArtifactGenerationIdentityV1, QuantizationFormat};
        use frankensearch::index::{FsviV2IdentityBinding, ValidatedFsviBytes, VectorIndex};

        let connection = fixture()?;
        let snapshot = capture(&connection, PassageProjection::RawPassagesV2)?;
        let model = HashEmbedder::new(16);
        let mut identity = model.identity()?.clone();
        identity.storage.format = "fsvi-v2".into();
        identity.storage.quantization = QuantizationFormat::F16;
        identity.storage.endianness = "little-endian".into();
        let binding = FsviV2IdentityBinding::new(
            ArtifactGenerationIdentityV1::new(1, [19; 16])?,
            identity.freeze()?,
        )?;
        let root = tempfile::tempdir()?;
        let path = root.path().join("source-hash-control.fsvi");
        let mut writer = VectorIndex::create_v2(&path, binding.clone())?;
        for (message_id, text, role, created_at_ms) in [
            (10, "first canonical source passage", ROLE_USER, 100),
            (
                11,
                "second canonical source passage",
                role_code_from_str("assistant").ok_or("missing assistant role")?,
                101,
            ),
        ] {
            let canonical = canonicalize_for_embedding(text);
            let bound = model.embed_bound_sync(&canonical)?;
            let document = SemanticDocId {
                message_id,
                chunk_idx: 0,
                agent_id: 1,
                workspace_id: 2,
                source_id: crc32fast::hash(b"local"),
                role,
                created_at_ms,
                content_hash: Some(content_hash(&canonical)),
            };
            writer.write_record(&document.to_doc_id_string(), &bound.values)?;
        }
        writer.finish()?;
        let owner = ValidatedFsviBytes::open_published(&path, &binding)?;
        assert_eq!(
            snapshot.identity().ordered_live_docset_sha256,
            hex::encode(owner.witness().ordered_live_docset_digest)
        );
        assert_eq!(
            snapshot.identity().document_count,
            owner.witness().live_count
        );
        // This proves the native doc-order contract using a real hash control,
        // not semantic quality, production publication or clock admission.
        Ok(())
    }
}
