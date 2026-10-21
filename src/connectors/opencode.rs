//! Native OpenCode history, including the OpenCode2 session_v2 store.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation,
    NormalizedMessage, ScanContext, ScanRoot,
};
use crate::franken_sync::compat::{ConnectionExt, OpenFlags, RowExt, open_with_flags};
use crate::franken_sync::params;

#[derive(Default)]
pub struct OpenCodeConnector;

impl OpenCodeConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn default_v2_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for name in ["CASS_OPENCODE2_DB", "OPENCODE_DB", "OPENCODE_SQLITE_DB"] {
            if let Ok(value) = dotenvy::var(name)
                && !value.trim().is_empty()
            {
                paths.push(PathBuf::from(value));
            }
        }
        if let Some(home) = dirs::home_dir() {
            let data = dotenvy::var("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| home.join(".local/share"));
            paths.push(data.join("opencode/opencode2.db"));
        }
        paths
    }

    fn v2_sources(ctx: &ScanContext) -> Result<Vec<(ScanRoot, PathBuf)>> {
        let roots = if ctx.use_default_detection() {
            Self::default_v2_paths()
                .into_iter()
                .map(ScanRoot::local)
                .collect()
        } else {
            ctx.scan_roots.clone()
        };
        let mut sources = Vec::new();
        let mut seen = HashSet::new();
        for root in roots {
            let candidates = if root.path.is_file() {
                vec![root.path.clone()] // ubs:ignore -- Candidate owns its path; the original root is retained for provenance.
            } else {
                ["opencode2.db", "opencode/opencode2.db", "opencode.db"]
                    .into_iter()
                    .map(|suffix| root.path.join(suffix))
                    .collect()
            };
            for candidate in candidates {
                if candidate.extension().is_none_or(|suffix| suffix != "db") || !candidate.is_file()
                {
                    continue;
                }
                let path = fs::canonicalize(candidate)?;
                let identity = (root.origin.source_id.clone(), path.clone()); // ubs:ignore -- Owned dedup keys outlive candidate iteration; one key per configured source database.
                if seen.insert(identity) {
                    sources.push((root.clone(), path)); // ubs:ignore -- Each returned database retains its independent source and path-rewrite provenance.
                }
            }
        }
        Ok(sources)
    }

    fn scan_v2_database(
        root: &ScanRoot,
        path: &Path,
        ctx: &ScanContext,
        emit: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        // Reading native history must never checkpoint or migrate its database.
        let db = open_with_flags(&path.to_string_lossy(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let tables = db.query_map_collect(
            "SELECT name FROM sqlite_master WHERE type='table' AND name IN ('session_v2','session_message')",
            &[],
            |row| row.get_typed::<String>(0),
        )?;
        if tables.len() != 2 {
            return Ok(());
        }
        db.execute("BEGIN")?;
        let cutoff = ctx.since_ts.map_or(i64::MIN, |ts| ts.saturating_sub(1_000));
        let sessions = db.query_map_collect(
            "SELECT s.id,s.directory,s.title,s.time_created,s.time_updated FROM session_v2 AS s
             WHERE s.time_updated >= ?1 OR EXISTS
               (SELECT 1 FROM session_message AS m WHERE m.session_id=s.id AND m.time_updated >= ?1)
             ORDER BY s.id",
            params![cutoff],
            |row| {
                Ok((
                    row.get_typed::<String>(0)?,
                    row.get_typed::<String>(1)?,
                    row.get_typed::<Option<String>>(2)?,
                    row.get_typed::<i64>(3)?,
                    row.get_typed::<i64>(4)?,
                ))
            },
        )?;
        for (id, directory, title, started_at, mut ended_at) in sessions {
            if let Some(tick) = &ctx.progress_tick {
                tick();
            }
            let rows = db.query_map_collect(
                "SELECT id,type,seq,time_created,time_updated,data FROM session_message
                 WHERE session_id=?1 ORDER BY seq,id",
                params![id.as_str()],
                |row| {
                    Ok((
                        row.get_typed::<String>(0)?,
                        row.get_typed::<String>(1)?,
                        row.get_typed::<i64>(2)?,
                        row.get_typed::<i64>(3)?,
                        row.get_typed::<i64>(4)?,
                        row.get_typed::<String>(5)?,
                    ))
                },
            )?;
            let mut messages = Vec::with_capacity(rows.len());
            let mut message_ids = HashSet::new();
            let mut sequences = HashSet::new();
            for (message_id, role, sequence, created_at, updated_at, raw) in rows {
                ensure!(
                    message_ids.insert(message_id.clone()), // ubs:ignore -- Duplicate detection retains the ID while the original moves into the persisted message payload.
                    "duplicate OpenCode2 message ID"
                );
                ensure!(
                    sequences.insert(sequence),
                    "duplicate OpenCode2 message sequence"
                );
                let mut extra: Value =
                    serde_json::from_str(&raw).context("OpenCode2 message JSON")?;
                ensure!(extra.is_object(), "OpenCode2 message must be a JSON object");
                let content = native_message_content(&extra)?;
                let fields = extra
                    .as_object_mut()
                    .context("OpenCode2 message object missing")?;
                fields.insert("native_message_id".into(), json!(message_id));
                fields.insert("native_message_type".into(), json!(role));
                fields.insert("native_sequence".into(), json!(sequence));
                fields.insert("native_updated_at".into(), json!(updated_at));
                messages.push(NormalizedMessage {
                    idx: i64::try_from(messages.len())
                        .context("OpenCode2 message count overflow")?,
                    role,
                    author: None,
                    created_at: Some(created_at),
                    content,
                    extra,
                    snippets: Vec::new(),
                    invocations: Vec::new(),
                });
                ended_at = ended_at.max(updated_at);
                if let Some(tick) = &ctx.progress_tick {
                    tick();
                }
            }
            // A native session can exist before its first turn. Preserve its
            // original identity and metadata without inventing a message; the
            // canonical archive supports zero-message conversations.
            let content_hash = hex::encode(Sha256::digest(serde_json::to_vec(&messages)?));
            emit(NormalizedConversation {
                agent_slug: "opencode".into(),
                external_id: Some(id.clone()), // ubs:ignore -- One native session ID is required in both the external identity and provenance metadata.
                title,
                workspace: (!directory.is_empty())
                    .then(|| PathBuf::from(root.rewrite_workspace(&directory, Some("opencode")))),
                source_path: path.to_path_buf(),
                started_at: Some(started_at),
                ended_at: Some(ended_at),
                metadata: json!({"source":"opencode", "harness":"opencode2", "native_session_id":id,
                    "native_format":"session_v2/session_message", "content_hash":content_hash,
                    "coverage":"native_store", "origin":root.origin}),
                messages,
            })?;
        }
        db.execute("ROLLBACK")?;
        Ok(())
    }
}

fn native_message_content(message: &Value) -> Result<String> {
    let mut text = String::new();
    if let Some(value) = message.get("text").and_then(Value::as_str) {
        text.push_str(value);
    }
    if let Some(parts) = message.get("content").and_then(Value::as_array) {
        for part in parts {
            if !text.is_empty() {
                text.push('\n');
            }
            if part.get("type").and_then(Value::as_str) == Some("text")
                && let Some(value) = part.get("text").and_then(Value::as_str)
            {
                text.push_str(value);
            } else {
                // Retain tool inputs/results, reasoning and unknown native parts.
                text.push_str(&serde_json::to_string(part)?);
            }
        }
    }
    if text.is_empty() {
        text = serde_json::to_string(message)?;
    }
    Ok(text)
}

impl Connector for OpenCodeConnector {
    fn detect(&self) -> DetectionResult {
        let mut result = franken_agent_detection::OpenCodeConnector::new().detect();
        for path in Self::default_v2_paths()
            .into_iter()
            .filter(|path| path.is_file())
        {
            if !result.root_paths.contains(&path) {
                result.evidence.push(path.display().to_string()); // ubs:ignore -- One owned diagnostic string per discovered database, outside message processing.
                result.root_paths.push(path); // ubs:ignore -- Vec insertion reports a read root; this is not PathBuf joining or a write destination.
            }
            result.detected = true;
        }
        result
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut result = Vec::new();
        self.scan_with_callback(ctx, &mut |conversation| {
            result.push(conversation);
            Ok(())
        })?;
        Ok(result)
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        emit: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        let mut seen = HashSet::new();
        let mut unique = |conversation: NormalizedConversation| {
            let source = conversation
                .metadata
                .get("origin")
                .and_then(|origin| origin.get("source_id"))
                .and_then(Value::as_str)
                .unwrap_or("local");
            if let Some(id) = &conversation.external_id
                && !seen.insert((source.to_owned(), id.clone()))
            // ubs:ignore -- Dedup keys must survive the conversation moving into the caller's callback.
            {
                return Ok(());
            }
            emit(conversation)
        };
        for (root, path) in Self::v2_sources(ctx)? {
            Self::scan_v2_database(&root, &path, ctx, &mut unique)?;
        }
        franken_agent_detection::OpenCodeConnector::new().scan_with_callback(ctx, &mut unique)
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let mut result =
            franken_agent_detection::OpenCodeConnector::new().discover_source_files(ctx)?;
        for (root, path) in Self::v2_sources(ctx)? {
            let mut wal_name = path
                .file_name()
                .context("database filename missing")?
                .to_os_string();
            wal_name.push("-wal");
            let wal = path.with_file_name(wal_name);
            result.push(
                DiscoveredSourceFile::new(
                    "opencode",
                    &root,
                    path,
                    DiscoveredSourceRole::SqliteDatabase,
                    true,
                )
                .with_fs_metadata(),
            );
            if wal.is_file() {
                result.push(
                    DiscoveredSourceFile::new(
                        "opencode",
                        &root,
                        wal,
                        DiscoveredSourceRole::MetadataSidecar,
                        true,
                    )
                    .with_fs_metadata(),
                );
            }
        }
        // Legacy discovery also recognizes explicitly supplied .db files.
        // One physical source per origin must have only one discovery record.
        let mut seen = HashSet::new();
        result.retain(|source| {
            let canonical = fs::canonicalize(&source.source_path)
                .unwrap_or_else(|_| source.source_path.clone());
            seen.insert((source.origin.source_id.clone(), canonical))
        });
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use anyhow::Context;
    use asupersync::runtime::{Runtime, RuntimeHandle};
    use franken_agent_detection::connectors::sqlite_sync::{Connection, ConnectionExt};

    use crate::franken_sync::compat::RowExt;

    fn connection_with_observed_runtime() -> anyhow::Result<(Connection, RuntimeHandle)> {
        let connection = Connection::open(":memory:")?;
        connection.execute_batch(
            "CREATE TABLE messages (body TEXT); INSERT INTO messages VALUES ('complete');",
        )?;
        // The row mapper runs inside FAD's actual bridge, so this is its
        // runtime, not a separate runtime constructed by the test.
        let (body, runtime) =
            connection.query_row_map("SELECT body FROM messages", &[], |row| {
                Ok((row.get_typed::<String>(0)?, Runtime::current_handle()))
            })?;
        assert_eq!(body, "complete");
        let runtime = runtime.context("FAD query must install its bridge runtime")?;
        Ok((connection, runtime))
    }

    fn weak_worker_runtime_handle(runtime: RuntimeHandle) -> anyhow::Result<RuntimeHandle> {
        let (sender, receiver) = mpsc::channel();
        // A handle captured inside block_on is strong. An Asupersync native
        // worker's ambient handle is weak, so it can witness runtime death
        // without itself keeping the runtime alive. Spawn after the DB call
        // has returned, when this task can only run on the background worker.
        let task = runtime.try_spawn(async move {
            let _sent = sender.send((std::thread::current().id(), Runtime::current_handle()));
        })?;
        drop(runtime);
        let (worker_id, runtime) = receiver.recv_timeout(Duration::from_secs(10))?;
        drop(task);
        assert_ne!(
            worker_id,
            std::thread::current().id(),
            "the lifecycle witness must come from the real scheduler worker"
        );
        runtime.context("FAD scheduler worker must install its runtime handle")
    }

    fn assert_fad_runtime_drained(runtime: &RuntimeHandle) {
        assert!(
            runtime.trace_snapshot().is_some(),
            "the FAD runtime must be alive before CASS cleanup"
        );
        assert!(crate::shutdown_thread_local_bridge_runtimes());
        assert!(
            runtime.trace_snapshot().is_none(),
            "CASS cleanup must destroy FAD's cached runtime before thread exit"
        );
        assert!(
            crate::shutdown_thread_local_bridge_runtimes(),
            "cleanup must be idempotent when the caches are empty"
        );
    }

    #[test]
    fn sqlite_export_cleanup_destroys_fad_runtime_and_allows_recreation() -> anyhow::Result<()> {
        let (connection, runtime) = connection_with_observed_runtime()?;
        drop(connection);
        let first_runtime = weak_worker_runtime_handle(runtime)?;
        assert_fad_runtime_drained(&first_runtime);

        // This real query must work after cleanup, with a fresh live runtime;
        // the previously retired runtime must remain unavailable.
        let (connection, runtime) = connection_with_observed_runtime()?;
        drop(connection);
        let second_runtime = weak_worker_runtime_handle(runtime)?;
        assert!(first_runtime.trace_snapshot().is_none());
        assert_fad_runtime_drained(&second_runtime);
        Ok(())
    }

    #[test]
    fn sqlite_export_cleanup_destroys_fad_runtime_after_failed_read() -> anyhow::Result<()> {
        let (connection, runtime) = connection_with_observed_runtime()?;
        let read = connection.query_row_map("SELECT body FROM missing_messages", &[], |row| {
            row.get_typed::<String>(0)
        });
        assert!(read.is_err(), "the database read must actually fail");
        drop(connection);
        let runtime = weak_worker_runtime_handle(runtime)?;
        assert_fad_runtime_drained(&runtime);
        Ok(())
    }
}
