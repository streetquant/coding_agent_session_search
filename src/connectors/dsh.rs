//! Native DeepSeek Harness event journals. Final messages are authoritative;
//! streamed chunks and request snapshots must not duplicate their content.

use std::fs;
use std::io::{BufRead, BufReader, Cursor, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use franken_agent_detection::connectors::{SourceCompletion, SourceScanHooks};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation,
    NormalizedMessage, ScanContext, ScanRoot, parse_timestamp,
};

const MAX_COMPRESSED_BYTES: u64 = 100 * 1024 * 1024;
const MAX_DECODED_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Default)]
pub struct DshConnector;

impl DshConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn default_root() -> PathBuf {
        dirs::home_dir().unwrap_or_default().join(".dsh/sessions")
    }

    fn is_journal(path: &Path) -> bool {
        path.file_name()
            .is_some_and(|name| name == "session.jsonl.zstd" || name == "session.jsonl")
            && path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("session-"))
    }

    fn source(root: &ScanRoot, path: PathBuf) -> DiscoveredSourceFile {
        let mut capture_root = root.clone();
        if capture_root.path.is_file() {
            capture_root.path = path.parent().unwrap_or(&root.path).to_path_buf();
        }
        DiscoveredSourceFile::new(
            "dsh",
            &capture_root,
            path,
            DiscoveredSourceRole::PrimarySessionLog,
            true,
        )
        .with_fs_metadata()
    }

    fn sources(ctx: &ScanContext) -> Result<Vec<(ScanRoot, PathBuf)>> {
        let roots = if ctx.use_default_detection() {
            vec![ScanRoot::local(Self::default_root())]
        } else {
            ctx.scan_roots.clone()
        };
        let mut sources = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for root in roots {
            if root.path.is_file() {
                // ubs:ignore -- Dedup owns a path key separately from the returned source metadata.
                if Self::is_journal(&root.path) && seen.insert(root.path.clone()) {
                    // ubs:ignore -- Each source owns its scan origin and path; no journal content is cloned.
                    sources.push((root.clone(), root.path.clone()));
                }
                continue;
            }
            if !root.path.is_dir() {
                continue;
            }
            for entry in WalkDir::new(&root.path).max_depth(3).follow_links(false) {
                let entry = entry?;
                if entry.file_type().is_file() && Self::is_journal(entry.path()) {
                    let path = entry.into_path();
                    // ubs:ignore -- Dedup retains an independent path key after the source takes ownership.
                    if seen.insert(path.clone()) {
                        // ubs:ignore -- Each discovered source must retain its originating scan-root metadata.
                        sources.push((root.clone(), path));
                    }
                }
            }
        }
        sources.sort_by(|a, b| a.1.cmp(&b.1));
        Ok(sources)
    }

    fn content(value: &Value) -> String {
        match value {
            Value::String(text) => text.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .map(Self::content)
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            Value::Object(block) => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    text.to_owned()
                } else if block.get("type").and_then(Value::as_str) == Some("tool-call") {
                    format!(
                        "{} {}",
                        block.get("name").and_then(Value::as_str).unwrap_or("tool"),
                        block.get("arguments").map_or_else(String::new, |args| {
                            args.as_str()
                                .map_or_else(|| args.to_string(), str::to_owned)
                        })
                    )
                } else {
                    block.get("content").map(Self::content).unwrap_or_default()
                }
            }
            _ => String::new(),
        }
    }

    fn parse(root: &ScanRoot, path: &Path) -> Result<NormalizedConversation> {
        let before = fs::metadata(path)?;
        ensure!(
            before.len() <= MAX_COMPRESSED_BYTES,
            "DSH journal exceeds compressed byte limit"
        );
        let bytes = fs::read(path)?;
        let digest = hex::encode(Sha256::digest(&bytes));
        let input: Box<dyn Read> = if path.extension().is_some_and(|ext| ext == "zstd") {
            Box::new(zstd::stream::read::Decoder::new(Cursor::new(bytes))?)
        } else {
            Box::new(Cursor::new(bytes))
        };
        let mut reader = BufReader::new(input.take(MAX_DECODED_BYTES + 1));
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let header: Value = serde_json::from_str(&line).context("DSH session header")?;
        ensure!(
            header.get("type").and_then(Value::as_str) == Some("session")
                && header.get("version").and_then(Value::as_u64) == Some(0),
            "unsupported DSH session header"
        );
        let id = header
            .get("id")
            .and_then(Value::as_str)
            .context("DSH native session ID missing")?;
        ensure!(
            path.parent()
                .and_then(Path::file_name)
                .and_then(|s| s.to_str())
                == Some(id),
            "DSH directory/session identity mismatch"
        );
        let cwd = header
            .get("cwd")
            .and_then(Value::as_str)
            .context("DSH native workspace missing")?;
        ensure!(
            Path::new(cwd).is_absolute(),
            "DSH workspace must be absolute"
        );
        let mut decoded_bytes = line.len() as u64;
        let mut messages = Vec::new();
        let mut title = None;
        let mut ended_at = header.get("createdAt").and_then(parse_timestamp);
        loop {
            line.clear();
            let count = reader.read_line(&mut line)?;
            if count == 0 {
                break;
            }
            decoded_bytes += count as u64;
            ensure!(
                decoded_bytes <= MAX_DECODED_BYTES,
                "DSH journal exceeds decoded byte limit"
            );
            let event: Value =
                serde_json::from_str(&line).context("invalid or unfinished DSH event")?;
            if let Some(timestamp) = event.get("time").and_then(parse_timestamp) {
                ended_at = Some(ended_at.map_or(timestamp, |old| old.max(timestamp)));
            }
            let kind = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if kind == "session/title" {
                title = event
                    .get("data")
                    .and_then(|data| data.get("title"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            if !matches!(kind, "user/message" | "assistant/message" | "tool/result") {
                continue;
            }
            let data = event
                .get("data")
                .context("DSH final message data missing")?;
            let (role, message) = match kind {
                "user/message" => ("user", data),
                "assistant/message" => (
                    "assistant",
                    data.get("message")
                        .context("DSH assistant message missing")?,
                ),
                "tool/result" => (
                    "tool",
                    data.get("message").context("DSH tool result missing")?,
                ),
                _ => continue,
            };
            let content = Self::content(
                message
                    .get("content")
                    .context("DSH final message content missing")?,
            );
            if content.is_empty() {
                continue;
            }
            messages.push(NormalizedMessage {
                idx: messages.len() as i64,
                // ubs:ignore -- NormalizedMessage owns its role string independently of the parsed event.
                role: role.to_owned(),
                author: None,
                created_at: event.get("time").and_then(parse_timestamp),
                content,
                extra: event,
                snippets: vec![],
                invocations: vec![],
            });
        }
        let after = fs::metadata(path)?;
        ensure!(
            before.len() == after.len() && before.modified()? == after.modified()?,
            "DSH journal changed during read; retry next scan"
        );
        Ok(NormalizedConversation {
            agent_slug: "dsh".to_owned(),
            external_id: Some(id.to_owned()),
            title,
            workspace: Some(PathBuf::from(root.rewrite_workspace(cwd, Some("dsh")))),
            source_path: path.to_path_buf(),
            started_at: header.get("createdAt").and_then(parse_timestamp),
            ended_at,
            metadata: json!({"native_header": header, "content_hash": digest, "origin": root.origin}),
            messages,
        })
    }
}

impl Connector for DshConnector {
    fn detect(&self) -> DetectionResult {
        let root = Self::default_root();
        let detected = root.is_dir();
        DetectionResult {
            detected,
            evidence: if detected {
                vec![root.display().to_string()]
            } else {
                vec![]
            },
            root_paths: if detected { vec![root] } else { vec![] },
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = Vec::new();
        self.scan_with_callback(ctx, &mut |conversation| {
            conversations.push(conversation);
            Ok(())
        })?;
        Ok(conversations)
    }

    fn supports_streaming_scan(&self) -> bool {
        true
    }

    fn supports_source_boundaries(&self) -> bool {
        true
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        emit: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        self.scan_with_source_boundaries(ctx, &mut SourceScanHooks::default(), emit)
    }

    fn scan_with_source_boundaries(
        &self,
        ctx: &ScanContext,
        hooks: &mut SourceScanHooks<'_>,
        emit: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        for (root, path) in Self::sources(ctx)? {
            if let Some(tick) = &ctx.progress_tick {
                tick();
            }
            // ubs:ignore -- Completion metadata owns its path while parsing still borrows the original.
            let source = Self::source(&root, path.clone());
            if !hooks.should_scan(&source) {
                continue;
            }
            // ubs:ignore -- The diagnostic path is allocated only on a parse error, not on successful scans.
            let conversation =
                Self::parse(&root, &path).with_context(|| path.display().to_string())?;
            ensure!(
                !source.fs_metadata_changed(),
                "DSH journal changed after discovery"
            );
            emit(conversation)?;
            ensure!(
                !source.fs_metadata_changed(),
                "DSH journal changed during delivery"
            );
            hooks.complete(&SourceCompletion {
                source,
                required_sidecars: vec![],
                conversations_emitted: 1,
            })?;
        }
        Ok(())
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        Ok(Self::sources(ctx)?
            .into_iter()
            .map(|(root, path)| Self::source(&root, path))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(temp: &tempfile::TempDir, body: &str, compressed: bool) -> PathBuf {
        let directory = temp.path().join("session-native-id");
        fs::create_dir_all(&directory).expect("session directory");
        let path = directory.join(if compressed {
            "session.jsonl.zstd"
        } else {
            "session.jsonl"
        });
        let text = format!(
            "{{\"type\":\"session\",\"version\":0,\"id\":\"session-native-id\",\"cwd\":\"/workspace/original\",\"createdAt\":1700000000000}}\n{body}"
        );
        let bytes = if compressed {
            zstd::stream::encode_all(text.as_bytes(), 1).expect("compress")
        } else {
            text.into_bytes()
        };
        fs::write(&path, bytes).expect("journal");
        path
    }

    #[test]
    fn compressed_native_identity_and_final_messages() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = journal(
            &temp,
            concat!(
                "{\"type\":\"user/message\",\"seq\":0,\"time\":1700000000010,\"data\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"original question\"}]}}\n",
                "{\"type\":\"assistant/chunk\",\"data\":{\"text\":\"duplicate stream\"}}\n",
                "{\"type\":\"assistant/message\",\"time\":1700000000020,\"data\":{\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"final answer\"}]}}}\n",
                "{\"type\":\"tool/result\",\"time\":1700000000030,\"data\":{\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool-result\",\"content\":[{\"type\":\"text\",\"text\":\"tool output\"}]}]}}}\n"
            ),
            true,
        );
        let root = ScanRoot::local(temp.path().to_path_buf());
        let result = DshConnector::parse(&root, &path).expect("parse native zstd");
        assert_eq!(result.agent_slug, "dsh");
        assert_eq!(result.external_id.as_deref(), Some("session-native-id"));
        assert_eq!(
            result.workspace.as_deref(),
            Some(Path::new("/workspace/original"))
        );
        assert_eq!(result.source_path, path);
        assert_eq!(
            result
                .messages
                .iter()
                .map(|m| (m.role.as_str(), m.content.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("user", "original question"),
                ("assistant", "final answer"),
                ("tool", "tool output")
            ]
        );
        assert_eq!(
            result.metadata["content_hash"]
                .as_str()
                .expect("hash")
                .len(),
            64
        );
    }

    #[test]
    fn rejects_incomplete_or_misattributed_history() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = journal(&temp, "{\"type\":\"user/message\"", false);
        let root = ScanRoot::local(temp.path().to_path_buf());
        assert!(DshConnector::parse(&root, &path).is_err());
        for body in [
            "{\"type\":\"user/message\"}\n",
            "{\"type\":\"assistant/message\",\"data\":{}}\n",
            "{\"type\":\"tool/result\",\"data\":{\"message\":{}}}\n",
        ] {
            let path = journal(&temp, body, false);
            assert!(DshConnector::parse(&root, &path).is_err());
        }
        let bytes = fs::read_to_string(&path).expect("read");
        fs::write(&path, bytes.replace("/workspace/original", "relative"))
            .expect("invalid workspace");
        assert!(DshConnector::parse(&root, &path).is_err());
        fs::write(
            &path,
            "{\"type\":\"session\",\"version\":1,\"id\":\"other\",\"cwd\":\"/workspace\"}\n",
        )
        .expect("unsupported header");
        assert!(DshConnector::parse(&root, &path).is_err());
    }

    #[test]
    fn source_completion_requires_admission_and_successful_delivery() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = journal(
            &temp,
            "{\"type\":\"user/message\",\"time\":1700000000010,\"data\":{\"content\":[{\"type\":\"text\",\"text\":\"question\"}]}}\n",
            true,
        );
        let ctx =
            ScanContext::with_roots(temp.path().to_path_buf(), vec![ScanRoot::local(path)], None);
        let connector = DshConnector::new();
        let discovered = connector.discover_source_files(&ctx).expect("discover");
        assert_eq!(discovered.len(), 1);
        let mut completions = 0;
        let mut complete = |value: &SourceCompletion| {
            assert_eq!(value.source, discovered[0]);
            assert!(value.required_sidecars.is_empty());
            completions += 1;
            Ok(())
        };
        let mut hooks = SourceScanHooks {
            should_scan_source: None,
            on_source_complete: Some(&mut complete),
        };
        assert!(
            connector
                .scan_with_source_boundaries(&ctx, &mut hooks, &mut |_| anyhow::bail!(
                    "delivery rejected"
                ))
                .is_err()
        );
        connector
            .scan_with_source_boundaries(&ctx, &mut hooks, &mut |_| Ok(()))
            .expect("deliver");
        assert_eq!(completions, 1);
        let mut skip = |_: &DiscoveredSourceFile| false;
        connector
            .scan_with_source_boundaries(
                &ctx,
                &mut SourceScanHooks {
                    should_scan_source: Some(&mut skip),
                    on_source_complete: None,
                },
                &mut |_| anyhow::bail!("skipped source must not emit"),
            )
            .expect("skip");
    }

    #[test]
    #[ignore = "read-only native DSH qualification; set CASS_DSH_NATIVE_ROOT and CASS_DSH_ACCEPTANCE_DIR"]
    fn native_store_roundtrip() -> Result<()> {
        use crate::indexer::{IndexOptions, run_index_with_local_connector_roots};
        let root = PathBuf::from(dotenvy::var("CASS_DSH_NATIVE_ROOT")?);
        let data_dir = PathBuf::from(dotenvy::var("CASS_DSH_ACCEPTANCE_DIR")?);
        ensure!(
            root.is_absolute() && data_dir.is_absolute(),
            "absolute native and acceptance paths required"
        );
        fs::create_dir(&data_dir).context("acceptance directory must be new")?;
        let ctx =
            ScanContext::with_roots(data_dir.clone(), vec![ScanRoot::local(root.clone())], None);
        let connector = DshConnector::new();
        let sources = connector.discover_source_files(&ctx)?;
        let conversations = connector.scan(&ctx)?;
        ensure!(
            !sources.is_empty() && conversations.len() == sources.len(),
            "native capture coverage gap"
        );
        let messages: usize = conversations.iter().map(|c| c.messages.len()).sum();
        ensure!(messages > 0, "native source must have real messages");
        let options = IndexOptions {
            full: false,
            force_rebuild: false,
            watch: false,
            watch_once_paths: None,
            db_path: data_dir.join("agent_search.db"),
            data_dir: data_dir.clone(),
            semantic: false,
            build_hnsw: false,
            embedder: "hash".into(),
            progress: None,
            watch_interval_secs: 30,
        };
        let roots = std::collections::HashMap::from([("dsh".to_owned(), vec![root])]);
        run_index_with_local_connector_roots(options.clone(), roots.clone(), None)?;
        run_index_with_local_connector_roots(options, roots, None)?;
        // Read canonical logical identities with the same production storage backend.
        let db = crate::storage::sqlite::FrankenStorage::open_readonly(
            &data_dir.join("agent_search.db"),
        )?;
        let rows = db.list_conversations(i64::MAX, 0)?;
        ensure!(
            rows.len() == conversations.len(),
            "replay duplicated native sessions"
        );
        for conversation in &conversations {
            ensure!(
                rows.iter().any(|row| row.agent_slug == "dsh"
                    && row.external_id == conversation.external_id
                    && row.workspace == conversation.workspace
                    && row.source_path == conversation.source_path),
                "native identity or workspace lost"
            );
        }
        println!(
            "DSH_NATIVE_ACCEPTANCE {}",
            json!({"sources": sources.len(), "conversations": rows.len(), "messages": messages, "archive": data_dir})
        );
        Ok(())
    }
}
