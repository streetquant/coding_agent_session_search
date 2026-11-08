//! Bounded, measured canonical-archive message counting (GH #381).
//!
//! GH #381 reports the canonical-archive count wedging on the reported
//! 468,216-message / 10,252-conversation shape: stock SQLite answers
//! `SELECT COUNT(*) FROM messages` in about 0.11 s on that shape, while the
//! writable first contact of the archive hangs. The count is only a
//! *heuristic input* — it decides whether the live lexical index looks
//! sparse — so the missing behavior is not "a faster COUNT" but "a count
//! that is bounded, measurable, and honest about not knowing".
//!
//! This module provides that behavior:
//!
//! * [`count_messages_bounded`] counts canonical messages in bounded windows
//!   and stops when its wall-clock budget is exhausted, reporting
//!   [`ArchiveCountOutcome::Unknown`] together with the rows counted so far.
//!   It never reports a fabricated zero, and it only issues read statements,
//!   so it never mutates the archive.
//! * [`measure_archive_count`] wraps the count with the measurement the
//!   report asks for — total wall time, count-stage time, RSS and block-IO
//!   deltas — and serves the count from an on-disk memo
//!   ([`ARCHIVE_COUNT_CACHE_FILE`]) keyed by the archive's physical identity,
//!   so that repeated first access does not pay for the scan again.
//!
//! The memo reuses the identity the shipped fingerprint cache already uses
//! ([`super::lexical_storage_physical_identity`]): the size and mtime of the
//! database file and of its WAL sidecar. Any committed write changes at least
//! one of them, so a memoized count is only served while the archive is
//! unchanged. The identity is captured *before* the count, so a write that
//! lands during the count invalidates the memo instead of publishing a count
//! that belongs to a newer archive state.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::franken_sync::compat::{ConnectionExt, ParamValue, RowExt};
use crate::storage::sqlite::FrankenStorage;

/// Sidecar (inside the lexical index directory, next to the fingerprint
/// memo) that caches the exact canonical message count against the
/// archive's physical identity.
pub const ARCHIVE_COUNT_CACHE_FILE: &str = ".archive-count-cache.json";

/// Schema version of [`ARCHIVE_COUNT_CACHE_FILE`]. A mismatch invalidates
/// the memo instead of being guessed around.
const ARCHIVE_COUNT_CACHE_SCHEMA_VERSION: u32 = 1;

/// Number of message ids read per bounded count window.
pub const ARCHIVE_COUNT_WINDOW_ROWS: i64 = 10_000;

/// Default wall-clock budget for one bounded archive count.
pub const ARCHIVE_COUNT_DEFAULT_BUDGET_MS: u64 = 30_000;

/// Environment variable that overrides [`ARCHIVE_COUNT_DEFAULT_BUDGET_MS`].
pub const ARCHIVE_COUNT_BUDGET_ENV: &str = "CASS_ARCHIVE_COUNT_BUDGET_MS";

/// Physical identity of the canonical archive a count was taken from.
///
/// This is the identity the shipped fingerprint cache uses (database file and
/// WAL sidecar size and mtime), republished here so that the count memo can
/// be keyed the same way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveCountIdentity {
    /// Size of the canonical database file.
    pub db_len: u64,
    /// Modification time of the canonical database file (whole seconds).
    pub db_mtime_secs: u64,
    /// Modification time of the canonical database file (nanoseconds).
    pub db_mtime_nanos: u32,
    /// Size of the WAL sidecar (0 when there is none).
    pub wal_len: u64,
    /// Modification time of the WAL sidecar (whole seconds).
    pub wal_mtime_secs: u64,
    /// Modification time of the WAL sidecar (nanoseconds).
    pub wal_mtime_nanos: u32,
}

/// Why an exact archive count is not available.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArchiveCountUnknownReason {
    /// The wall-clock budget expired before the count completed. The rows
    /// counted so far are reported as a lower bound of the work done, never
    /// as the total.
    BudgetExhausted {
        /// Rows counted before the budget expired.
        rows_scanned: usize,
    },
    /// The archive's physical metadata could not be read, so no count can be
    /// attributed to a known archive state.
    ArchiveMetadataUnavailable,
}

/// The outcome of a bounded archive count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArchiveCountOutcome {
    /// The count completed and is exact.
    Exact {
        /// The exact number of canonical messages.
        count: usize,
        /// Wall-clock milliseconds spent producing this outcome.
        elapsed_ms: u64,
        /// Number of bounded windows that were read (0 for a memo hit).
        windows: u32,
        /// Whether the count was served from the on-disk memo.
        from_cache: bool,
    },
    /// The count is not known. This is never a fabricated zero.
    Unknown {
        /// Why the count is not known.
        reason: ArchiveCountUnknownReason,
        /// Wall-clock milliseconds spent producing this outcome.
        elapsed_ms: u64,
        /// Rows counted before the count stopped (0 when nothing was read).
        rows_scanned: usize,
    },
}

impl ArchiveCountOutcome {
    /// The exact count, when it is known.
    pub fn exact_count(&self) -> Option<usize> {
        match self {
            Self::Exact { count, .. } => Some(*count),
            Self::Unknown { .. } => None,
        }
    }

    /// Whether the count is unknown.
    pub fn is_unknown(&self) -> bool {
        self.exact_count().is_none()
    }

    /// Wall-clock milliseconds spent producing this outcome.
    pub fn elapsed_ms(&self) -> u64 {
        match self {
            Self::Exact { elapsed_ms, .. } | Self::Unknown { elapsed_ms, .. } => *elapsed_ms,
        }
    }
}

/// A bounded archive count together with the measurement the GH #381 report
/// asks for: total wall time, count-stage time, RSS and block-IO deltas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveCountMeasurement {
    /// The count outcome.
    pub outcome: ArchiveCountOutcome,
    /// Wall-clock milliseconds of the whole measurement (metadata, memo
    /// lookup and count stage).
    pub wall_ms: u64,
    /// Resident set size before the count, in bytes.
    pub rss_bytes_before: Option<u64>,
    /// Resident set size after the count, in bytes.
    pub rss_bytes_after: Option<u64>,
    /// Block-IO bytes before the count.
    pub io_bytes_before: Option<u64>,
    /// Block-IO bytes after the count.
    pub io_bytes_after: Option<u64>,
}

/// The wall-clock budget for one bounded archive count.
///
/// `CASS_ARCHIVE_COUNT_BUDGET_MS` overrides the default; a zero budget is
/// honored and makes the count stop before the first window, which is how
/// the tests exercise the "budget exhausted" path.
pub fn archive_count_budget() -> Duration {
    let millis = std::env::var(ARCHIVE_COUNT_BUDGET_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(ARCHIVE_COUNT_DEFAULT_BUDGET_MS);
    Duration::from_millis(millis)
}

/// The physical identity of the canonical archive at `db_path`.
///
/// `None` when the archive (or its metadata) cannot be read: that is the
/// "missing metadata" state, and it must never be reported as a zero count.
pub fn archive_count_identity(db_path: &Path) -> Option<ArchiveCountIdentity> {
    let raw = super::lexical_storage_physical_identity(db_path)?;
    let epoch: serde_json::Value = serde_json::from_str(&raw.source_epoch).ok()?;
    let (db_len, db_mtime_secs, db_mtime_nanos) = observation_parts(epoch.get("database"));
    let (wal_len, wal_mtime_secs, wal_mtime_nanos) = observation_parts(epoch.get("wal"));
    Some(ArchiveCountIdentity {
        db_len,
        db_mtime_secs,
        db_mtime_nanos,
        wal_len,
        wal_mtime_secs,
        wal_mtime_nanos,
    })
}

/// Split one archive-epoch file observation into `(len, mtime_secs, mtime_nanos)`.
///
/// The epoch stores `size` and `mtime_ns` (a string) for a present file and
/// `{"absent": true}` for a missing sidecar. A missing sidecar has length and
/// mtime zero, matching the identity the count memo has always used.
fn observation_parts(observation: Option<&serde_json::Value>) -> (u64, u64, u32) {
    let Some(observation) = observation else {
        return (0, 0, 0);
    };
    if observation
        .get("absent")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return (0, 0, 0);
    }
    let len = observation
        .get("size")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let mtime_ns = observation
        .get("mtime_ns")
        .and_then(|value| {
            value
                .as_u64()
                .map(u128::from)
                .or_else(|| value.as_str()?.parse::<u128>().ok())
        })
        .unwrap_or(0);
    let secs = mtime_ns / 1_000_000_000;
    let nanos = (mtime_ns % 1_000_000_000) as u32;
    (len, secs.min(u128::from(u64::MAX)) as u64, nanos)
}

/// Count the canonical messages of `storage` inside a wall-clock budget.
///
/// The count reads the canonical `messages` table in bounded windows of
/// [`ARCHIVE_COUNT_WINDOW_ROWS`] ids, advancing a cursor over the (rowid)
/// message ids, and checks the budget between windows. When the budget is
/// exhausted the count stops and reports [`ArchiveCountOutcome::Unknown`]
/// with the rows counted so far — it never reports a fabricated zero and
/// never issues a write, so the archive is left untouched.
///
/// The bound is cooperative: fsqlite 0.4.12 exposes no interrupt or progress
/// callback in its public API, so a single window statement cannot be
/// cancelled once it started. The count therefore stops at
/// `budget + one window` and reports the count as unknown when it does.
pub fn count_messages_bounded(
    storage: &FrankenStorage,
    budget: Duration,
) -> Result<ArchiveCountOutcome> {
    let started = Instant::now();
    let deadline = started.checked_add(budget);
    let window_sql = format!(
        "SELECT id FROM messages WHERE id > ?1 ORDER BY id ASC LIMIT {}",
        ARCHIVE_COUNT_WINDOW_ROWS
    );
    let mut cursor = i64::MIN;
    let mut count: u64 = 0;
    let mut windows: u32 = 0;
    loop {
        let expired = deadline.is_none_or(|deadline| Instant::now() >= deadline);
        if expired {
            return Ok(ArchiveCountOutcome::Unknown {
                reason: ArchiveCountUnknownReason::BudgetExhausted {
                    rows_scanned: count as usize,
                },
                elapsed_ms: started.elapsed().as_millis() as u64,
                rows_scanned: count as usize,
            });
        }
        let ids = storage
            .raw()
            .query_map_collect(&window_sql, &[ParamValue::from(cursor)], |row| {
                row.get_typed::<i64>(0)
            })
            .context("counting canonical messages in a bounded window")?;
        let Some(&last_id) = ids.last() else {
            break;
        };
        // The cursor advances to the last id of the window, so the window has
        // to be strictly ascending; a window that is not would silently skip
        // rows and would report a wrong (too low) exact count.
        if ids
            .iter()
            .zip(ids.iter().skip(1))
            .any(|(left, right)| left >= right)
        {
            anyhow::bail!(
                "canonical message ids are not strictly ascending inside a count window; \
                 refusing to report an exact count"
            );
        }
        count += ids.len() as u64;
        windows += 1;
        if ids.len() < ARCHIVE_COUNT_WINDOW_ROWS as usize {
            break;
        }
        cursor = last_id;
    }
    Ok(ArchiveCountOutcome::Exact {
        count: count as usize,
        elapsed_ms: started.elapsed().as_millis() as u64,
        windows,
        from_cache: false,
    })
}

/// Count the canonical messages of `storage` and measure the work.
///
/// The measurement separates the count stage (the time the count itself took,
/// [`ArchiveCountOutcome::elapsed_ms`]) from the total wall time of the call
/// ([`ArchiveCountMeasurement::wall_ms`]), and captures the resident-set and
/// block-IO deltas around the count.
///
/// The count is served from the on-disk memo in `cache_dir` when the memo
/// matches the archive's current physical identity; otherwise the bounded
/// count runs and, when it completes, refreshes the memo. A count that is
/// unknown is never memoized, so an unknown count can never be replayed as
/// an exact one.
pub fn measure_archive_count(
    storage: &FrankenStorage,
    db_path: &Path,
    cache_dir: &Path,
    budget: Duration,
) -> Result<ArchiveCountMeasurement> {
    let wall_started = Instant::now();
    let rss_before = super::responsiveness::process_resident_memory_bytes();
    let io_before = crate::process_block_io_bytes();

    let outcome = match archive_count_identity(db_path) {
        None => ArchiveCountOutcome::Unknown {
            reason: ArchiveCountUnknownReason::ArchiveMetadataUnavailable,
            elapsed_ms: 0,
            rows_scanned: 0,
        },
        Some(identity) => match cached_archive_count(cache_dir, &identity) {
            Some(count) => ArchiveCountOutcome::Exact {
                count,
                elapsed_ms: 0,
                windows: 0,
                from_cache: true,
            },
            None => {
                let outcome = count_messages_bounded(storage, budget)?;
                match &outcome {
                    ArchiveCountOutcome::Exact { count, .. } => {
                        // Best effort: a failed memo write only costs a later scan.
                        if let Err(error) = write_cached_archive_count(cache_dir, &identity, *count)
                        {
                            tracing::debug!(
                                error = %error,
                                "writing the archive count memo failed (continuing)"
                            );
                        }
                    }
                    ArchiveCountOutcome::Unknown { .. } => {}
                }
                outcome
            }
        },
    };

    Ok(ArchiveCountMeasurement {
        outcome,
        wall_ms: wall_started.elapsed().as_millis() as u64,
        rss_bytes_before: rss_before,
        rss_bytes_after: super::responsiveness::process_resident_memory_bytes(),
        io_bytes_before: io_before,
        io_bytes_after: crate::process_block_io_bytes(),
    })
}

/// The memoized count for `identity`, when the memo in `cache_dir` is valid.
pub fn cached_archive_count(cache_dir: &Path, identity: &ArchiveCountIdentity) -> Option<usize> {
    let text = fs::read_to_string(archive_count_cache_path(cache_dir)).ok()?;
    let cache: ArchiveCountCache = serde_json::from_str(&text).ok()?;
    if cache.schema_version != ARCHIVE_COUNT_CACHE_SCHEMA_VERSION {
        return None;
    }
    if &cache.identity != identity {
        return None;
    }
    usize::try_from(cache.count).ok()
}

/// The path of the count memo inside `cache_dir`.
pub fn archive_count_cache_path(cache_dir: &Path) -> PathBuf {
    // ubs:ignore[rust.security.path-traversal] -- The child is a fixed basename; the operator-selected cache directory is the intended parent.
    cache_dir.join(ARCHIVE_COUNT_CACHE_FILE)
}

#[derive(Serialize, Deserialize)]
struct ArchiveCountCache {
    schema_version: u32,
    identity: ArchiveCountIdentity,
    count: u64,
    computed_at_ms: i64,
}

fn write_cached_archive_count(
    cache_dir: &Path,
    identity: &ArchiveCountIdentity,
    count: usize,
) -> Result<()> {
    fs::create_dir_all(cache_dir)
        .with_context(|| format!("creating archive count cache dir {}", cache_dir.display()))?;
    let cache = ArchiveCountCache {
        schema_version: ARCHIVE_COUNT_CACHE_SCHEMA_VERSION,
        identity: identity.clone(),
        count: count as u64,
        computed_at_ms: now_millis(),
    };
    let text =
        serde_json::to_string_pretty(&cache).context("serializing the archive count cache")?;
    fs::write(archive_count_cache_path(cache_dir), text)
        .with_context(|| format!("writing the archive count cache in {}", cache_dir.display()))?;
    Ok(())
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}
