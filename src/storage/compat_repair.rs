//! GH #450: the pre-index compatibility repair as an explicit, admitted phase.
//!
//! Frankensqlite stamps `<db>.fsqlite-migration-state` when its one-time
//! first-open repair pass has completed. An archive that lacks the marker (a
//! pre-v9 engine archive, or a copy of one) makes the next writable open run a
//! whole-archive `integrity_check` and, when it finds repairable damage, a
//! whole-archive backup plus repair. On the GH #450 bundle (2,842,697,904
//! bytes) that took 1,491 s while the index progress still read
//! `preparing 0/0` and the process read 1.16 GB / wrote 2.86 GB.
//!
//! The engine callback is opaque, so this module owns everything cass can
//! honestly observe around it:
//!
//! * **Admission before the open.** Free space for the backup, available
//!   memory, an optional wall-clock budget, and host I/O pressure are checked
//!   from metadata and `/proc`, before any archive page is touched.
//! * **A named phase with bytes.** The caller publishes
//!   `INDEX_PHASE_COMPATIBILITY_REPAIR` with `unit = "bytes"`; the numerator
//!   is logical bytes read (`/proc/self/io.rchar`) or backup bytes written,
//!   whichever is further along, capped by the archive bundle size.
//! * **Durable progress.** `<db>.cass-compat-repair.json` is rewritten
//!   atomically while the pass runs (archive bytes, pages, bytes read/written,
//!   elapsed, RSS, PSI IO, last activity). A receipt left in `running` by a
//!   killed process is reported as `interrupted` and the pass resumes by
//!   re-running; completion is only ever claimed from the engine marker.
//! * **Explicit deferral.** A refused admission leaves the engine marker
//!   absent and writes a `deferred` receipt with the exact reason, so the next
//!   run retries instead of silently skipping required correctness work.
//!
//! The engine pass is idempotent and marker-last (see its `migration.rs`):
//! an interruption re-runs the pass on the next open, so a byte checkpoint
//! exists for observability, not for skipping engine work.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::storage::sqlite::index_engine_migration_is_complete;

/// Sidecar carrying the durable repair receipt. Deliberately distinct from the
/// engine's `.fsqlite-migration-state` marker: the marker means "the engine
/// finished", this receipt means "cass observed this attempt".
pub(crate) const RECEIPT_SUFFIX: &str = ".cass-compat-repair.json";

const MIB: u64 = 1024 * 1024;

/// Default free-space headroom beyond the archive copy the engine writes.
const DEFAULT_FREE_SPACE_MARGIN_BYTES: u64 = 64 * MIB;
/// Default available-memory floor. The engine's integrity walk and repair are
/// pager-backed, but an open on a memory-pressured host still compounds it.
const DEFAULT_MIN_AVAILABLE_MEMORY_BYTES: u64 = 256 * MIB;
/// Below this archive size an I/O-pressure refusal would block cheap repairs;
/// small archives finish in seconds even on a saturated host.
const DEFAULT_IO_ADMISSION_MIN_ARCHIVE_BYTES: u64 = 256 * MIB;
/// Observed GH #450 throughput was ~2.7 MiB/s (4 GB touched in 1,491 s). The
/// budget estimator stays deliberately pessimistic; it is only consulted when
/// an operator sets an explicit `CASS_COMPAT_REPAIR_MAX_SECS`.
const DEFAULT_ESTIMATE_THROUGHPUT_BYTES_PER_SEC: f64 = 4.0 * (MIB as f64);
/// Host `io.pressure some avg10` at or above this is severe enough that
/// starting a whole-archive rewrite is worth deferring... unless the archive is
/// small (`DEFAULT_IO_ADMISSION_MIN_ARCHIVE_BYTES`) or the operator overrides.
const DEFAULT_MAX_PSI_IO_SOME_AVG10: f64 = 90.0;
const DEFAULT_PROGRESS_INTERVAL_MS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompatRepairState {
    /// A repair is required and admissible but has not started.
    Pending,
    /// An attempt is in flight right now.
    Running,
    /// A previous attempt's process exited without recording completion.
    Interrupted,
    /// The engine marker records the pass as complete.
    Completed,
    /// Admission refused the expensive work; the marker stays absent.
    Deferred,
    /// The writable open failed, or returned without recording the marker.
    Failed,
}

impl CompatRepairState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Interrupted => "interrupted",
            Self::Completed => "completed",
            Self::Deferred => "deferred",
            Self::Failed => "failed",
        }
    }
}

/// Everything the receipt and the live progress payload carry. One shape for
/// both, so the durable record and the operator-visible counters cannot drift.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CompatRepairReceipt {
    pub state: CompatRepairState,
    pub archive_path: String,
    pub archive_bytes: u64,
    pub pages_total: u64,
    pub page_size: u64,
    #[serde(default)]
    pub backup_bytes: u64,
    #[serde(default)]
    pub pages_copied: u64,
    /// Logical bytes read through the process (`/proc/self/io.rchar` delta).
    #[serde(default)]
    pub bytes_read: u64,
    /// Logical bytes written (`/proc/self/io.wchar` delta).
    #[serde(default)]
    pub bytes_written: u64,
    /// Storage-layer reads/writes (`read_bytes`/`write_bytes` deltas).
    #[serde(default)]
    pub storage_read_bytes: u64,
    #[serde(default)]
    pub storage_write_bytes: u64,
    pub progress_current: u64,
    pub progress_total: u64,
    pub started_at_unix: u64,
    pub updated_at_unix: u64,
    #[serde(default)]
    pub last_activity_at_unix: u64,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub interrupted_attempts: u32,
    #[serde(default)]
    pub rss_bytes: u64,
    #[serde(default)]
    pub psi_io_some_avg10: Option<f64>,
    /// CLI error code this phase produces: 0 completed, 7 deferred, 9 failed.
    /// Not the process's final exit status (indexing continues after success).
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub admission: Option<CompatRepairAdmission>,
}

impl CompatRepairReceipt {
    /// Live progress JSON for the indexing snapshot. Adds the age of the last
    /// proof of work so an operator viewing a snapshot can tell "slow but
    /// active" (`last_activity_age_ms` small) from "no work observed".
    pub(crate) fn to_progress_json(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        let now = now_unix_ms();
        let last_activity_ms = self.last_activity_at_unix.saturating_mul(1_000);
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "last_activity_age_ms".to_string(),
                serde_json::Value::from(now.saturating_sub(last_activity_ms)),
            );
            object.insert(
                "state".to_string(),
                serde_json::Value::from(self.state.as_str()),
            );
        }
        value
    }
}

/// The resource facts admission decided on, kept in the durable receipt so a
/// deferral is explainable after the process has exited.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CompatRepairAdmission {
    pub free_bytes: Option<u64>,
    pub required_free_bytes: u64,
    pub available_memory_bytes: Option<u64>,
    pub required_memory_bytes: u64,
    pub psi_io_some_avg10: Option<f64>,
    pub max_psi_io_some_avg10: Option<f64>,
    pub estimated_secs: f64,
    pub max_secs: Option<f64>,
}

#[derive(Debug, Clone)]
pub(crate) struct CompatRepairLimits {
    pub required_free_bytes: u64,
    pub required_memory_bytes: u64,
    pub max_secs: Option<f64>,
    pub max_psi_io_some_avg10: Option<f64>,
    pub io_admission_min_archive_bytes: u64,
    pub estimate_throughput_bytes_per_sec: f64,
    pub progress_interval: Duration,
}

impl CompatRepairLimits {
    /// Read the operator knobs. `bundle_bytes` sizes the default backup
    /// requirement; a set `CASS_COMPAT_REPAIR_MIN_FREE_BYTES` overrides it.
    pub(crate) fn from_env(bundle_bytes: u64) -> Self {
        Self::from_lookup(bundle_bytes, |key| dotenvy::var(key).ok())
    }

    fn from_lookup(bundle_bytes: u64, lookup: impl Fn(&str) -> Option<String>) -> Self {
        let parse_u64 = |key: &str| lookup(key).and_then(|raw| raw.trim().parse::<u64>().ok());
        let parse_f64 = |key: &str| {
            lookup(key)
                .and_then(|raw| raw.trim().parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value >= 0.0)
        };
        let default_free = bundle_bytes
            .saturating_add(DEFAULT_FREE_SPACE_MARGIN_BYTES)
            .max(DEFAULT_FREE_SPACE_MARGIN_BYTES);
        let progress_interval_ms = parse_u64("CASS_COMPAT_REPAIR_PROGRESS_INTERVAL_MS")
            .unwrap_or(DEFAULT_PROGRESS_INTERVAL_MS)
            .clamp(100, 60_000);
        Self {
            required_free_bytes: parse_u64("CASS_COMPAT_REPAIR_MIN_FREE_BYTES")
                .unwrap_or(default_free),
            required_memory_bytes: parse_u64("CASS_COMPAT_REPAIR_MIN_MEMORY_BYTES")
                .unwrap_or(DEFAULT_MIN_AVAILABLE_MEMORY_BYTES),
            max_secs: parse_f64("CASS_COMPAT_REPAIR_MAX_SECS").filter(|value| *value > 0.0),
            max_psi_io_some_avg10: parse_f64("CASS_COMPAT_REPAIR_MAX_PSI_IO_SOME_AVG10")
                .or(Some(DEFAULT_MAX_PSI_IO_SOME_AVG10)),
            io_admission_min_archive_bytes: parse_u64(
                "CASS_COMPAT_REPAIR_IO_ADMISSION_MIN_ARCHIVE_BYTES",
            )
            .unwrap_or(DEFAULT_IO_ADMISSION_MIN_ARCHIVE_BYTES),
            estimate_throughput_bytes_per_sec: parse_f64(
                "CASS_COMPAT_REPAIR_ESTIMATE_THROUGHPUT_BYTES_PER_SEC",
            )
            .filter(|value| *value > 0.0)
            .unwrap_or(DEFAULT_ESTIMATE_THROUGHPUT_BYTES_PER_SEC),
            progress_interval: Duration::from_millis(progress_interval_ms),
        }
    }

    /// Estimated wall clock for the worst case: the archive is read once and
    /// copied once. The engine only writes the backup when it repairs, so this
    /// is an upper bound and admission uses it only against an explicit
    /// operator budget.
    pub(crate) fn estimated_secs(&self, bundle_bytes: u64) -> f64 {
        bundle_bytes.saturating_mul(2) as f64 / self.estimate_throughput_bytes_per_sec
    }

    /// Pure admission decision. Every refusal names the limiting resource and
    /// its measured value; none silently skipped correctness work, and none is
    /// reached when no repair is pending.
    pub(crate) fn admit(
        &self,
        bundle_bytes: u64,
        resources: &CompatRepairResources,
    ) -> CompatRepairAdmission {
        let estimated_secs = self.estimated_secs(bundle_bytes);
        CompatRepairAdmission {
            free_bytes: resources.free_bytes,
            required_free_bytes: self.required_free_bytes,
            available_memory_bytes: resources.available_memory_bytes,
            required_memory_bytes: self.required_memory_bytes,
            psi_io_some_avg10: resources.psi_io_some_avg10,
            max_psi_io_some_avg10: self.max_psi_io_some_avg10,
            estimated_secs,
            max_secs: self.max_secs,
        }
    }

    fn refusal(
        &self,
        bundle_bytes: u64,
        resources: &CompatRepairResources,
    ) -> (CompatRepairAdmission, Option<String>) {
        let admission = self.admit(bundle_bytes, resources);
        if let Some(free) = resources.free_bytes
            && free < self.required_free_bytes
        {
            return (
                admission,
                Some(format!(
                    "it needs {} bytes of free space for the archive copy the engine writes as \
                     .pre-migration-bak and only {free} bytes are free",
                    self.required_free_bytes
                )),
            );
        }
        if let Some(available) = resources.available_memory_bytes
            && available < self.required_memory_bytes
        {
            return (
                admission,
                Some(format!(
                    "it needs at least {} bytes of available memory and only {available} bytes are \
                     available",
                    self.required_memory_bytes
                )),
            );
        }
        if let (Some(psi), Some(max)) = (resources.psi_io_some_avg10, self.max_psi_io_some_avg10)
            && bundle_bytes >= self.io_admission_min_archive_bytes
            && psi >= max
        {
            return (
                admission,
                Some(format!(
                    "host IO pressure is io.pressure some avg10={psi:.2} (limit {max:.2}) for a \
                     {bundle_bytes}-byte archive"
                )),
            );
        }
        if let Some(max_secs) = self.max_secs
            && admission.estimated_secs > max_secs
        {
            let estimated_secs = admission.estimated_secs;
            return (
                admission,
                Some(format!(
                    "its pessimistic estimate is {estimated_secs:.0}s ({} bytes at {:.1} bytes/s) and the budget \
                     is {max_secs:.0}s",
                    bundle_bytes.saturating_mul(2),
                    self.estimate_throughput_bytes_per_sec
                )),
            );
        }
        (admission, None)
    }
}

/// Measured host facts admission consults. Every field is optional: an
/// unreadable probe skips only its own check rather than inventing a value.
#[derive(Debug, Clone, Default)]
pub(crate) struct CompatRepairResources {
    pub free_bytes: Option<u64>,
    pub available_memory_bytes: Option<u64>,
    pub psi_io_some_avg10: Option<f64>,
}

/// Typed deferral so the CLI can report `migration-repair-pending` (exit 7)
/// rather than a generic index failure. The archive is untouched and readable.
#[derive(Debug)]
pub(crate) struct CompatRepairDeferredError {
    pub(crate) detail: String,
    pub(crate) receipt_path: PathBuf,
}

impl std::fmt::Display for CompatRepairDeferredError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "pre-index compatibility repair deferred: {}. The archive was not modified and stays \
             readable; rerun in the foreground when the resource is available (receipt: {})",
            self.detail,
            self.receipt_path.display()
        )
    }
}

impl std::error::Error for CompatRepairDeferredError {}

pub(crate) fn receipt_path(db_path: &Path) -> PathBuf {
    path_with_suffix(db_path, RECEIPT_SUFFIX)
}

fn path_with_suffix(db_path: &Path, suffix: &str) -> PathBuf {
    let Some(file_name) = db_path.file_name() else {
        return PathBuf::from(format!("{}{}", db_path.display(), suffix));
    };
    let mut name = file_name.to_os_string();
    name.push(suffix);
    db_path.with_file_name(name)
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// The archive bundle whose copy is the expensive part: main file plus WAL and
/// shared-memory sidecars. Symlinked/missing sidecars count zero.
pub(crate) fn archive_bundle_bytes(db_path: &Path) -> u64 {
    let mut total = file_len(db_path);
    for suffix in ["-wal", "-shm"] {
        total = total.saturating_add(file_len(&path_with_suffix(db_path, suffix)));
    }
    total
}

/// The bytes of the engine's pre-migration backup, including its WAL/SHM
/// companions when the engine copied them. This is the repair's own write
/// volume, not a sidecar left by an unrelated integrity failure.
fn backup_bundle_bytes(db_path: &Path) -> u64 {
    let mut total = file_len(&path_with_suffix(db_path, ".pre-migration-bak"));
    for suffix in [".pre-migration-bak-wal", ".pre-migration-bak-shm"] {
        total = total.saturating_add(file_len(&path_with_suffix(db_path, suffix)));
    }
    total
}

/// Whether the next writable open must run the engine's compatibility repair.
/// A missing, truncated or stale marker means yes; the engine owns that
/// contract (`last_upgrade_version >= CURRENT_MIGRATION_VERSION`).
pub(crate) fn repair_pending(db_path: &Path) -> bool {
    db_path.is_file() && !index_engine_migration_is_complete(db_path)
}

/// Bytes to report when a repair is pending, `None` otherwise. The size guard
/// is the same policy boundary the full-rebuild integrity preflight uses.
pub(crate) fn pending_bundle_bytes_with_max(db_path: &Path, max_bytes: u64) -> Option<u64> {
    if !repair_pending(db_path) {
        return None;
    }
    let bundle_bytes = archive_bundle_bytes(db_path);
    // Same policy as `should_run_engine_backed_archive_integrity_preflight`:
    // zero disables the size guard, and an archive at or below the cap may
    // repair in the background. Only a larger archive defers.
    if max_bytes == 0 || bundle_bytes <= max_bytes {
        None
    } else {
        Some(bundle_bytes)
    }
}

/// Read the durable receipt, ignoring an unreadable or older/invalid file.
pub(crate) fn read_receipt(db_path: &Path) -> Option<CompatRepairReceipt> {
    let file = fs::File::open(receipt_path(db_path)).ok()?;
    // Bound the parse; the schema is small and a hostile file must not
    // allocate unboundedly.
    serde_json::from_reader(std::io::Read::take(file, 64 * 1024)).ok()
}

/// Rewrite the receipt atomically. Best effort: the receipt is observability,
/// and failing the repair because its journal could not be written would be
/// worse. Callers that need the refusal to be durable log the failure.
pub(crate) fn write_receipt(db_path: &Path, receipt: &CompatRepairReceipt) -> Result<()> {
    let final_path = receipt_path(db_path);
    let tmp_path = path_with_suffix(db_path, &format!("{RECEIPT_SUFFIX}.tmp"));
    let json = serde_json::to_vec_pretty(receipt).context("serializing compat repair receipt")?;
    fs::write(&tmp_path, json)
        .with_context(|| format!("writing compat repair receipt {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("publishing compat repair receipt {}", final_path.display()))
}

/// Blame-free best-effort receipt update for a background deferral, which runs
/// before the indexer and has no live progress object.
pub(crate) fn record_background_deferral(db_path: &Path, bundle_bytes: u64, reason: &str) {
    let prior = read_receipt(db_path);
    let now = now_unix_secs();
    let receipt = CompatRepairReceipt {
        state: CompatRepairState::Deferred,
        archive_path: db_path.display().to_string(),
        archive_bytes: bundle_bytes,
        pages_total: 0,
        page_size: 0,
        backup_bytes: 0,
        pages_copied: 0,
        bytes_read: 0,
        bytes_written: 0,
        storage_read_bytes: 0,
        storage_write_bytes: 0,
        progress_current: 0,
        progress_total: bundle_bytes,
        started_at_unix: prior.as_ref().map_or(now, |prior| prior.started_at_unix),
        updated_at_unix: now,
        last_activity_at_unix: prior
            .as_ref()
            .map_or(now, |prior| prior.last_activity_at_unix),
        elapsed_ms: prior.as_ref().map_or(0, |prior| prior.elapsed_ms),
        attempts: prior.as_ref().map_or(0, |prior| prior.attempts),
        interrupted_attempts: prior.as_ref().map_or(0, |prior| prior.interrupted_attempts),
        rss_bytes: 0,
        psi_io_some_avg10: None,
        exit_code: Some(7),
        detail: Some(reason.to_string()),
        admission: prior.and_then(|prior| prior.admission),
    };
    if let Err(err) = write_receipt(db_path, &receipt) {
        tracing::debug!(error = %err, db_path = %db_path.display(), "could not write deferred compat repair receipt");
    }
}

/// SQLite header geometry, so progress reports pages and page size without
/// opening the engine. Returns `(page_size, pages_total)`; zeros for a file
/// that is not a SQLite database yet.
pub(crate) fn sqlite_page_metrics(db_path: &Path) -> (u64, u64) {
    let mut header = [0_u8; 64];
    let Ok(mut file) = fs::File::open(db_path) else {
        return (0, 0);
    };
    let Ok(read) = file.read(&mut header) else {
        return (0, 0);
    };
    if read < 32 || header.get(..16) != Some(b"SQLite format 3\0".as_slice()) {
        return (0, 0);
    }
    let raw_page_size = u64::from(u16::from_be_bytes([
        header.get(16).copied().unwrap_or(0),
        header.get(17).copied().unwrap_or(0),
    ]));
    let page_size = if raw_page_size == 1 {
        65_536
    } else if raw_page_size.is_power_of_two() && (512..=65_536).contains(&raw_page_size) {
        raw_page_size
    } else {
        return (0, 0);
    };
    let header_pages = u64::from(u32::from_be_bytes([
        header.get(28).copied().unwrap_or(0),
        header.get(29).copied().unwrap_or(0),
        header.get(30).copied().unwrap_or(0),
        header.get(31).copied().unwrap_or(0),
    ]));
    let file_pages = file_len(db_path).div_ceil(page_size);
    let wal_pages = file_len(&path_with_suffix(db_path, "-wal")).div_ceil(page_size);
    (
        page_size,
        header_pages.max(file_pages).saturating_add(wal_pages),
    )
}

/// One `/proc/self/io` sample. All fields are cumulative process counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ProcessIo {
    rchar: u64,
    wchar: u64,
    read_bytes: u64,
    write_bytes: u64,
}

fn process_io() -> ProcessIo {
    let Ok(text) = fs::read_to_string("/proc/self/io") else {
        return ProcessIo::default();
    };
    let mut io = ProcessIo::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Ok(value) = value.trim().parse::<u64>() else {
            continue;
        };
        match key {
            "rchar" => io.rchar = value,
            "wchar" => io.wchar = value,
            "read_bytes" => io.read_bytes = value,
            "write_bytes" => io.write_bytes = value,
            _ => {}
        }
    }
    io
}

/// Host `io.pressure` "some avg10" in percent, the same signal the kernel
/// exports under `/proc/pressure/io`. `None` where PSI is unavailable.
pub(crate) fn psi_io_some_avg10() -> Option<f64> {
    let text = fs::read_to_string("/proc/pressure/io").ok()?;
    parse_psi_some_avg10(&text)
}

fn parse_psi_some_avg10(text: &str) -> Option<f64> {
    let line = text.lines().find(|line| line.starts_with("some"))?;
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

fn available_free_bytes(db_path: &Path) -> Option<u64> {
    let parent = db_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    fs2::available_space(parent.unwrap_or_else(|| Path::new("."))).ok()
}

fn process_rss_bytes() -> u64 {
    crate::indexer::responsiveness::process_resident_memory_bytes().unwrap_or(0)
}

fn available_memory_bytes() -> Option<u64> {
    crate::indexer::responsiveness::available_memory_bytes()
}

fn resources_for(db_path: &Path) -> CompatRepairResources {
    CompatRepairResources {
        free_bytes: available_free_bytes(db_path),
        available_memory_bytes: available_memory_bytes(),
        psi_io_some_avg10: psi_io_some_avg10(),
    }
}

/// Start (or decline) the repair phase before the writable open.
///
/// Returns `None` when the engine marker already records completion. Returns
/// an error with a durable `deferred` receipt when admission refuses; the
/// archive was not touched, so the caller must surface exit 7 and leave the
/// marker absent. On admission, returns a monitor whose receipt is already
/// `running` and which the open's liveness heartbeat samples.
pub(crate) fn prepare(db_path: &Path) -> Result<Option<CompatRepairMonitor>> {
    if !repair_pending(db_path) {
        return Ok(None);
    }
    let bundle_bytes = archive_bundle_bytes(db_path);
    let limits = CompatRepairLimits::from_env(bundle_bytes);
    let resources = resources_for(db_path);
    let (page_size, pages_total) = sqlite_page_metrics(db_path);
    let prior = read_receipt(db_path);
    let now = now_unix_secs();

    let mut attempts = prior.as_ref().map_or(0, |prior| prior.attempts);
    let mut interrupted_attempts = prior.as_ref().map_or(0, |prior| prior.interrupted_attempts);
    let mut resume_detail: Option<String> = None;
    // The engine writes its marker last, so a `running` receipt whose process
    // is gone means the pass was interrupted between atomic commits. It is
    // safe to re-run (idempotent), and the old attempt's numbers stay in the
    // durable record as evidence.
    if prior
        .as_ref()
        .is_some_and(|prior| prior.state == CompatRepairState::Running)
    {
        interrupted_attempts = interrupted_attempts.saturating_add(1);
        let mut interrupted = prior
            .clone()
            .unwrap_or_else(|| pending_receipt(db_path, bundle_bytes, page_size, pages_total, now));
        interrupted.state = CompatRepairState::Interrupted;
        interrupted.updated_at_unix = now;
        interrupted.exit_code = None;
        interrupted.detail = Some(format!(
            "previous attempt did not record completion (last update {}s ago, {} bytes read, \
             {} backup bytes written); the engine pass is idempotent and will rerun",
            now.saturating_sub(prior.as_ref().map_or(now, |prior| prior.updated_at_unix)),
            prior.as_ref().map_or(0, |prior| prior.bytes_read),
            prior.as_ref().map_or(0, |prior| prior.backup_bytes),
        ));
        resume_detail = interrupted.detail.clone();
        if let Err(err) = write_receipt(db_path, &interrupted) {
            tracing::warn!(error = %err, db_path = %db_path.display(), "could not record interrupted compat repair attempt");
        } else {
            tracing::warn!(
                db_path = %db_path.display(),
                prior_elapsed_ms = interrupted.elapsed_ms,
                prior_bytes_read = interrupted.bytes_read,
                prior_backup_bytes = interrupted.backup_bytes,
                "pre-index compatibility repair resuming after an interrupted attempt"
            );
        }
    }
    attempts = attempts.saturating_add(1);

    let (admission, refusal) = limits.refusal(bundle_bytes, &resources);
    if let Some(detail) = refusal {
        let receipt = CompatRepairReceipt {
            state: CompatRepairState::Deferred,
            exit_code: Some(7),
            detail: Some(detail.clone()),
            attempts,
            interrupted_attempts,
            archive_bytes: bundle_bytes,
            pages_total,
            page_size,
            updated_at_unix: now,
            started_at_unix: now,
            last_activity_at_unix: now,
            progress_total: bundle_bytes,
            psi_io_some_avg10: resources.psi_io_some_avg10,
            admission: Some(admission),
            ..pending_receipt(db_path, bundle_bytes, page_size, pages_total, now)
        };
        let receipt_path = receipt_path(db_path);
        if let Err(err) = write_receipt(db_path, &receipt) {
            tracing::warn!(error = %err, db_path = %db_path.display(), "could not write deferred compat repair receipt");
        }
        return Err(CompatRepairDeferredError {
            detail,
            receipt_path,
        }
        .into());
    }

    let snapshot = CompatRepairReceipt {
        state: CompatRepairState::Running,
        exit_code: None,
        detail: resume_detail,
        started_at_unix: now,
        updated_at_unix: now,
        last_activity_at_unix: now,
        attempts,
        interrupted_attempts,
        admission: Some(admission),
        ..pending_receipt(db_path, bundle_bytes, page_size, pages_total, now)
    };
    let monitor = CompatRepairMonitor {
        db_path: db_path.to_path_buf(),
        snapshot,
        baseline_io: process_io(),
        last_receipt_write: Instant::now(),
        progress_interval: limits.progress_interval,
        started: Instant::now(),
    };
    if let Err(err) = write_receipt(db_path, &monitor.snapshot) {
        tracing::warn!(error = %err, db_path = %db_path.display(), "could not write running compat repair receipt");
    }
    Ok(Some(monitor))
}

fn pending_receipt(
    db_path: &Path,
    bundle_bytes: u64,
    page_size: u64,
    pages_total: u64,
    now: u64,
) -> CompatRepairReceipt {
    CompatRepairReceipt {
        state: CompatRepairState::Pending,
        archive_path: db_path.display().to_string(),
        archive_bytes: bundle_bytes,
        pages_total,
        page_size,
        backup_bytes: 0,
        pages_copied: 0,
        bytes_read: 0,
        bytes_written: 0,
        storage_read_bytes: 0,
        storage_write_bytes: 0,
        progress_current: 0,
        progress_total: bundle_bytes,
        started_at_unix: now,
        updated_at_unix: now,
        last_activity_at_unix: now,
        elapsed_ms: 0,
        attempts: 0,
        interrupted_attempts: 0,
        rss_bytes: 0,
        psi_io_some_avg10: None,
        exit_code: None,
        detail: None,
        admission: None,
    }
}

/// Live view of one in-flight repair. `observe` runs on the open's liveness
/// heartbeat (only in intervals where the engine actually did work), so a
/// growing counter here is an honest forward-progress signal and a frozen one
/// is a genuine lack of observed work, not an unlucky sample.
#[derive(Debug)]
pub(crate) struct CompatRepairMonitor {
    db_path: PathBuf,
    snapshot: CompatRepairReceipt,
    baseline_io: ProcessIo,
    last_receipt_write: Instant,
    progress_interval: Duration,
    started: Instant,
}

impl CompatRepairMonitor {
    /// Sample the open's footprint and rewrite the durable receipt at most
    /// once per interval. Returns the live snapshot for the caller's progress
    /// payload.
    pub(crate) fn observe(&mut self) -> CompatRepairReceipt {
        let io = process_io();
        let now = now_unix_secs();
        let backup_bytes = backup_bundle_bytes(&self.db_path);
        let bytes_read = io.rchar.saturating_sub(self.baseline_io.rchar);
        let bytes_written = io.wchar.saturating_sub(self.baseline_io.wchar);
        let storage_read_bytes = io.read_bytes.saturating_sub(self.baseline_io.read_bytes);
        let storage_write_bytes = io.write_bytes.saturating_sub(self.baseline_io.write_bytes);
        // Reads and the backup copy are both real work toward the same archive.
        // Whichever is further along is the honest numerator; neither alone
        // covers both the integrity walk (reads) and a repairing copy (writes).
        let current = bytes_read
            .max(backup_bytes)
            .min(self.snapshot.progress_total.max(1));
        self.snapshot.bytes_read = bytes_read;
        self.snapshot.bytes_written = bytes_written;
        self.snapshot.storage_read_bytes = storage_read_bytes;
        self.snapshot.storage_write_bytes = storage_write_bytes;
        self.snapshot.backup_bytes = backup_bytes;
        self.snapshot.pages_copied = backup_bytes
            .checked_div(self.snapshot.page_size)
            .unwrap_or(0);
        self.snapshot.progress_current = current;
        self.snapshot.elapsed_ms =
            u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.snapshot.updated_at_unix = now;
        self.snapshot.last_activity_at_unix = now;
        self.snapshot.rss_bytes = process_rss_bytes();
        self.snapshot.psi_io_some_avg10 = psi_io_some_avg10();
        // The engine may have changed the bundle size (copy, WAL growth). The
        // denominator stays the admission-time archive size so progress never
        // regresses; a bigger observed bundle is recorded separately.
        if self.snapshot.bytes_read > self.snapshot.progress_total
            && self.snapshot.progress_total < self.snapshot.archive_bytes
        {
            self.snapshot.progress_total = self.snapshot.archive_bytes;
        }
        if self.last_receipt_write.elapsed() >= self.progress_interval {
            self.last_receipt_write = Instant::now();
            if let Err(err) = write_receipt(&self.db_path, &self.snapshot) {
                tracing::debug!(error = %err, db_path = %self.db_path.display(), "compat repair progress receipt write failed");
            }
        }
        self.snapshot.clone()
    }

    pub(crate) fn snapshot(&self) -> CompatRepairReceipt {
        self.snapshot.clone()
    }

    /// Finish the phase. Completion is claimed ONLY from the engine marker; an
    /// open that returned without writing it is a required repair still
    /// pending, and must surface instead of being silently skipped.
    pub(crate) fn finish(mut self) -> Result<CompatRepairReceipt> {
        let complete = index_engine_migration_is_complete(&self.db_path);
        let now = now_unix_secs();
        let mut snapshot = self.observe();
        snapshot.updated_at_unix = now;
        snapshot.elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if complete {
            snapshot.state = CompatRepairState::Completed;
            snapshot.exit_code = Some(0);
            snapshot.progress_current = snapshot.progress_total;
            snapshot.detail = None;
            if let Err(err) = write_receipt(&self.db_path, &snapshot) {
                tracing::warn!(error = %err, db_path = %self.db_path.display(), "could not write completed compat repair receipt");
            }
            tracing::info!(
                db_path = %self.db_path.display(),
                archive_bytes = snapshot.archive_bytes,
                pages = snapshot.pages_total,
                elapsed_ms = snapshot.elapsed_ms,
                bytes_read = snapshot.bytes_read,
                bytes_written = snapshot.bytes_written,
                backup_bytes = snapshot.backup_bytes,
                rss_bytes = snapshot.rss_bytes,
                psi_io_some_avg10 = snapshot.psi_io_some_avg10,
                "pre-index compatibility repair completed"
            );
            return Ok(snapshot);
        }
        let detail = format!(
            "the writable open returned but the engine migration marker is still absent; the \
             required compatibility repair did not complete (archive {} bytes, {} pages read, \
             {} backup bytes written)",
            snapshot.archive_bytes, snapshot.pages_total, snapshot.backup_bytes
        );
        snapshot.state = CompatRepairState::Deferred;
        snapshot.exit_code = Some(7);
        snapshot.detail = Some(detail.clone());
        let path = receipt_path(&self.db_path);
        if let Err(err) = write_receipt(&self.db_path, &snapshot) {
            tracing::warn!(error = %err, db_path = %self.db_path.display(), "could not write pending-repair compat receipt");
        }
        Err(CompatRepairDeferredError {
            detail,
            receipt_path: path,
        }
        .into())
    }

    /// Record the writable open's failure with the progress observed so far.
    /// The engine marker remains absent, so the next run retries.
    pub(crate) fn failed(mut self, error: &anyhow::Error) -> CompatRepairReceipt {
        let now = now_unix_secs();
        let mut snapshot = self.observe();
        snapshot.state = CompatRepairState::Failed;
        snapshot.exit_code = Some(9);
        snapshot.updated_at_unix = now;
        snapshot.elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        snapshot.detail = Some(format!("writable open failed: {error:#}"));
        if let Err(err) = write_receipt(&self.db_path, &snapshot) {
            tracing::warn!(error = %err, db_path = %self.db_path.display(), "could not write failed compat repair receipt");
        }
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(name);
        (tmp, db)
    }

    fn bundle() -> CompatRepairResources {
        CompatRepairResources {
            free_bytes: Some(4 * 1024 * MIB),
            available_memory_bytes: Some(2 * 1024 * MIB),
            psi_io_some_avg10: Some(10.0),
        }
    }

    #[test]
    fn admission_names_the_limiting_resource_and_never_refuses_when_all_fit() {
        let limits = CompatRepairLimits::from_lookup(10 * MIB, |_| None);
        assert_eq!(limits.required_free_bytes, 74 * MIB);
        let (admission, refusal) = limits.refusal(10 * MIB, &bundle());
        assert_eq!(refusal, None);
        assert_eq!(
            admission.estimated_secs,
            (20 * MIB) as f64 / (4.0 * MIB as f64)
        );

        let tight_disk = CompatRepairResources {
            free_bytes: Some(74 * MIB - 1),
            ..bundle()
        };
        let (admission, refusal) = limits.refusal(10 * MIB, &tight_disk);
        // ubs:ignore[rust.ownership.guarded-later-unwrap] -- This negative fixture must fail if disk admission unexpectedly succeeds; the prior None belongs to the independent healthy case.
        let detail = refusal.expect("a short disk must defer");
        assert!(detail.contains("free space"), "{detail}");
        assert!(detail.contains(".pre-migration-bak"), "{detail}");
        assert_eq!(admission.required_free_bytes, 74 * MIB);

        let tight_memory = CompatRepairResources {
            available_memory_bytes: Some(256 * MIB - 1),
            ..bundle()
        };
        let (_, refusal) = limits.refusal(10 * MIB, &tight_memory);
        assert!(refusal.expect("low memory must defer").contains("memory"));

        let budgeted = CompatRepairLimits {
            max_secs: Some(1.0),
            ..limits.clone()
        };
        let (admission, refusal) = budgeted.refusal(10 * MIB, &bundle());
        assert!(
            refusal
                .expect("an explicit budget below the estimate must defer")
                .contains("budget")
        );
        assert!(admission.estimated_secs > 1.0);
    }

    #[test]
    fn io_pressure_admission_only_applies_to_large_archives() {
        let limits = CompatRepairLimits::from_lookup(1024 * MIB, |_| None);
        let pressured = CompatRepairResources {
            psi_io_some_avg10: Some(95.0),
            ..bundle()
        };
        let (_, refusal) = limits.refusal(1024 * MIB, &pressured);
        let detail = refusal.expect("a 1 GiB repair on a saturated host must defer");
        assert!(detail.contains("io.pressure"), "{detail}");

        let (_, small_refusal) = limits.refusal(16 * MIB, &pressured);
        assert!(
            small_refusal.is_none(),
            "a small archive must not be blocked at the same pressure"
        );
    }

    #[test]
    fn admission_skips_checks_whose_probe_is_unavailable() {
        let limits = CompatRepairLimits::from_lookup(1024 * MIB, |_| None);
        let unknown = CompatRepairResources::default();
        let (_, refusal) = limits.refusal(1024 * MIB, &unknown);
        assert_eq!(refusal, None, "unmeasured resources fail open, not closed");
    }

    #[test]
    fn limits_parse_operator_knobs_and_clamp_the_progress_interval() {
        let limits = CompatRepairLimits::from_lookup(100 * MIB, |key| match key {
            "CASS_COMPAT_REPAIR_MIN_FREE_BYTES" => Some("123".to_string()),
            "CASS_COMPAT_REPAIR_MIN_MEMORY_BYTES" => Some("456".to_string()),
            "CASS_COMPAT_REPAIR_MAX_SECS" => Some("30".to_string()),
            "CASS_COMPAT_REPAIR_MAX_PSI_IO_SOME_AVG10" => Some("12.5".to_string()),
            "CASS_COMPAT_REPAIR_PROGRESS_INTERVAL_MS" => Some("1".to_string()),
            _ => None,
        });
        assert_eq!(limits.required_free_bytes, 123);
        assert_eq!(limits.required_memory_bytes, 456);
        assert_eq!(limits.max_secs, Some(30.0));
        assert_eq!(limits.max_psi_io_some_avg10, Some(12.5));
        assert_eq!(limits.progress_interval, Duration::from_millis(100));
    }

    #[test]
    fn receipt_roundtrips_atomically_and_reports_activity_age() {
        let (tmp, db) = temp_db("agent_search.db");
        fs::write(&db, b"SQLite format 3\0").unwrap();
        let now = now_unix_secs();
        let receipt = CompatRepairReceipt {
            state: CompatRepairState::Running,
            last_activity_at_unix: now,
            ..pending_receipt(&db, 4096, 4096, 1, now)
        };
        write_receipt(&db, &receipt).unwrap();
        let read = read_receipt(&db).expect("receipt must round-trip");
        assert_eq!(read.state, CompatRepairState::Running);
        assert_eq!(read.archive_bytes, 4096);
        assert!(
            !path_with_suffix(&db, &format!("{RECEIPT_SUFFIX}.tmp")).exists(),
            "the atomic publish must leave no temp file"
        );
        let json = read.to_progress_json();
        assert_eq!(json["state"], "running");
        assert!(json["last_activity_age_ms"].as_u64().is_some());
        assert_eq!(
            tmp.path().join("agent_search.db").metadata().unwrap().len(),
            16
        );
    }

    #[test]
    fn sqlite_page_metrics_read_the_header_without_an_engine_open() {
        let (tmp, db) = temp_db("agent_search.db");
        // Minimal valid header: magic, 4096-byte pages, 3 pages.
        let mut header = vec![0_u8; 4096];
        header[..16].copy_from_slice(b"SQLite format 3\0");
        header[16..18].copy_from_slice(&4096_u16.to_be_bytes());
        header[28..32].copy_from_slice(&3_u32.to_be_bytes());
        fs::write(&db, &header).unwrap();
        assert_eq!(sqlite_page_metrics(&db), (4096, 3));

        fs::write(&db, b"not sqlite").unwrap();
        assert_eq!(sqlite_page_metrics(&db), (0, 0));
        assert_eq!(sqlite_page_metrics(&tmp.path().join("missing.db")), (0, 0));
    }

    #[test]
    fn psi_some_avg10_parser_reads_the_kernel_shape_and_rejects_junk() {
        let kernel = "some avg10=73.32 avg60=55.10 avg300=20.00 total=123\nfull avg10=1.00 avg60=0.50 avg300=0.20 total=1\n";
        assert_eq!(parse_psi_some_avg10(kernel), Some(73.32));
        assert_eq!(parse_psi_some_avg10("full avg10=1.0\n"), None);
        assert_eq!(parse_psi_some_avg10("some avg10=NaN\n"), None);
        assert_eq!(parse_psi_some_avg10(""), None);
    }

    #[test]
    fn interrupted_receipt_is_reported_before_the_retry_runs() {
        let (_tmp, db) = temp_db("agent_search.db");
        fs::write(&db, b"archive pending repair").unwrap();
        let now = now_unix_secs();
        let prior = CompatRepairReceipt {
            state: CompatRepairState::Running,
            archive_bytes: 8192,
            pages_total: 2,
            page_size: 4096,
            bytes_read: 4096,
            backup_bytes: 4096,
            progress_current: 4096,
            progress_total: 8192,
            started_at_unix: now.saturating_sub(600),
            updated_at_unix: now.saturating_sub(5),
            last_activity_at_unix: now.saturating_sub(5),
            elapsed_ms: 595_000,
            attempts: 2,
            ..pending_receipt(&db, 8192, 4096, 2, now)
        };
        write_receipt(&db, &prior).unwrap();
        // A small archive keeps the default admission passing on a busy host.
        let monitor = prepare(&db).unwrap().expect("a missing marker is pending");
        let read = read_receipt(&db).unwrap();
        assert_eq!(monitor.snapshot().state, CompatRepairState::Running);
        assert_eq!(read.attempts, 3);
        assert_eq!(read.interrupted_attempts, 1);
        assert!(
            read.detail
                .as_deref()
                .is_some_and(|detail| detail.contains("idempotent"))
        );
        // Finish without a marker: required work remains, and says so.
        let error = monitor.finish().unwrap_err();
        assert!(error.downcast_ref::<CompatRepairDeferredError>().is_some());
        let final_receipt = read_receipt(&db).unwrap();
        assert_eq!(final_receipt.state, CompatRepairState::Deferred);
        assert_eq!(final_receipt.exit_code, Some(7));
    }

    #[test]
    fn completed_marker_makes_prepare_a_no_op() {
        let (_tmp, db) = temp_db("agent_search.db");
        fs::write(&db, b"archive").unwrap();
        let marker = path_with_suffix(&db, ".fsqlite-migration-state");
        fs::write(
            &marker,
            br#"{"last_upgrade_version":1,"last_run_at":0,"repairs_applied":[]}"#,
        )
        .unwrap();
        assert!(!repair_pending(&db));
        assert!(prepare(&db).unwrap().is_none());
        assert!(
            !receipt_path(&db).exists(),
            "a completed archive must not gain a repair receipt"
        );
        assert_eq!(fs::read(&db).unwrap(), b"archive");
    }

    #[test]
    fn truncated_marker_is_still_pending() {
        let (_tmp, db) = temp_db("agent_search.db");
        fs::write(&db, b"archive").unwrap();
        fs::write(
            path_with_suffix(&db, ".fsqlite-migration-state"),
            br#"{"last_upgrade_version":1}"#,
        )
        .unwrap();
        assert!(repair_pending(&db));
    }

    #[test]
    fn pending_bundle_bytes_respects_the_policy_boundary() {
        let (_tmp, db) = temp_db("agent_search.db");
        assert_eq!(pending_bundle_bytes_with_max(&db, 4096), None);
        fs::write(&db, vec![0_u8; 8192]).unwrap();
        fs::write(path_with_suffix(&db, "-wal"), vec![0_u8; 1024]).unwrap();
        assert_eq!(pending_bundle_bytes_with_max(&db, 0), None);
        assert_eq!(pending_bundle_bytes_with_max(&db, 9216), None);
        assert_eq!(pending_bundle_bytes_with_max(&db, 9215), Some(9216));
        fs::write(
            path_with_suffix(&db, ".fsqlite-migration-state"),
            br#"{"last_upgrade_version":1,"last_run_at":0,"repairs_applied":[]}"#,
        )
        .unwrap();
        assert_eq!(pending_bundle_bytes_with_max(&db, 0), None);
    }

    /// GH #450 reader-contention clause: a reader that was already open keeps
    /// its snapshot across the writable open that runs the repair. The repair
    /// must not require exclusive access to a readable archive.
    #[test]
    fn a_readonly_reader_survives_the_engine_repair_open() {
        use crate::storage::sqlite::FrankenStorage;

        let (_tmp, db) = temp_db("agent_search.db");
        {
            let storage = FrankenStorage::open(&db).expect("create archive");
            storage
                .raw()
                .execute("CREATE TABLE repair_probe (value TEXT)")
                .expect("create probe table");
            storage
                .raw()
                .execute("INSERT INTO repair_probe (value) VALUES ('kept')")
                .expect("seed probe row");
        }
        fs::remove_file(path_with_suffix(&db, ".fsqlite-migration-state"))
            .expect("remove the engine marker");
        let reader = FrankenStorage::open_readonly(&db).expect("open reader before the repair");
        let before = reader
            .raw()
            .query("SELECT value FROM repair_probe")
            .expect("reader query before the repair");
        let monitor = prepare(&db).expect("admission").expect("a pending repair");
        {
            // This is the production writable open: it runs the engine's
            // first-open pass when the marker is missing, then reopens in the
            // bounded schema-only lane.
            let writer = crate::storage::sqlite::open_current_schema_storage_with_timeout(
                &db,
                std::time::Duration::from_secs(30),
            )
            .expect("repairing open")
            .expect("the archive is at the current cass schema");
            drop(writer);
        }
        monitor.finish().expect("the marker must record completion");
        let after = reader
            .raw()
            .query("SELECT value FROM repair_probe")
            .expect("the same reader must still query after the repair");
        assert_eq!(before.len(), 1);
        assert_eq!(after.len(), 1);
        assert_eq!(
            read_receipt(&db).unwrap().state,
            CompatRepairState::Completed
        );
    }
}
