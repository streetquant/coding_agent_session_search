//! Verified CloudMCP transcript projections; partial host coverage stays partial.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation,
    NormalizedMessage, ScanContext, ScanRoot, parse_timestamp,
};

#[derive(Default)]
pub struct CloudMcpConnector;

fn json_field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or(&Value::Null)
}

/// A bounded header probe distinguishes exported CloudMCP logs from genuine
/// Codex rollouts without relying on their deliberately compatible filename.
pub(crate) fn rollout_header(path: &Path) -> Option<Value> {
    if !path.file_name()?.to_str()?.starts_with("rollout-") {
        return None;
    }
    let file = fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file.take(64 * 1024))
        .read_line(&mut line)
        .ok()?;
    let row: Value = serde_json::from_str(&line).ok()?;
    (json_field(&row, "type").as_str() == Some("session_meta")
        && json_field(json_field(&row, "payload"), "originator").as_str() == Some("cloudmcp")
        && json_field(json_field(&row, "payload"), "source").as_str() == Some("cloudmcp-contextos"))
    .then(|| json_field(&row, "payload").clone())
}

fn rollout_manifest(header: &Value, projection_root: &Path) -> Result<PathBuf> {
    let workspace = json_field(header, "workspace_root")
        .as_str()
        .context("CloudMCP rollout workspace missing")?;
    let hash = json_field(header, "conversation_id_sha256")
        .as_str()
        .context("CloudMCP rollout identity missing")?;
    ensure!(
        hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid CloudMCP rollout identity hash"
    );
    let workspace_hash = hex::encode(Sha256::digest(workspace.as_bytes()));
    let workspace_prefix = workspace_hash
        .get(..32)
        .context("invalid CloudMCP workspace hash")?;
    let identity_prefix = hash.get(..32).context("invalid CloudMCP identity hash")?;
    Ok(projection_root
        .join(workspace_prefix)
        .join(identity_prefix)
        .join("manifest.json"))
}

impl CloudMcpConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn default_root() -> PathBuf {
        dotenvy::var("CASS_CLOUDMCP_DATA_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/warp/cloudmcp/contextos/transcripts/v1"))
    }

    fn sources(ctx: &ScanContext) -> Result<Vec<(ScanRoot, PathBuf)>> {
        let roots = if ctx.use_default_detection() {
            vec![ScanRoot::local(Self::default_root())]
        } else {
            ctx.scan_roots.clone()
        };
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for root in roots {
            let mut candidates = Vec::new();
            if let Some(header) = rollout_header(&root.path) {
                let manifest = rollout_manifest(&header, &Self::default_root())?;
                ensure!(
                    manifest.is_file(),
                    "CloudMCP export has no verified transcript manifest: {}",
                    manifest.display()
                );
                candidates.push(manifest);
            } else if root.path.is_file()
                && root
                    .path
                    .file_name()
                    .is_some_and(|n| n == "manifest.json" || n == "transcript.jsonl")
            {
                candidates.push(
                    root.path
                        .parent()
                        .context("CloudMCP transcript parent")?
                        .join("manifest.json"),
                );
            } else if root.path.is_dir()
                && (root.path.join("manifest.json").is_file()
                    || root.path.ends_with("contextos/transcripts/v1")
                    || root.path == Self::default_root())
            {
                // Canonical layout is root/workspace/conversation/{manifest,transcript}.
                // Never traverse arbitrary project or home directory trees.
                for entry in WalkDir::new(&root.path).max_depth(3).follow_links(false) {
                    let entry = entry?;
                    if entry.file_type().is_file() && entry.file_name() == "manifest.json" {
                        candidates.push(entry.into_path());
                    }
                }
            }
            for candidate in candidates {
                if !candidate.is_file() {
                    continue;
                }
                let path = fs::canonicalize(candidate)?;
                let canonical_key = path.clone(); // ubs:ignore -- Dedupe and output both retain the canonical path.
                if !seen.insert(canonical_key) {
                    continue;
                }
                let manifest: Value = serde_json::from_slice(&fs::read(&path)?)?;
                if json_field(&manifest, "schema_version").as_str()
                    == Some("cloudmcp.conversation-manifest.v1")
                {
                    let source_root = root.clone(); // ubs:ignore -- Each source retains its originating scan-root provenance.
                    out.push((source_root, path));
                }
            }
        }
        out.sort_by(|a, b| a.1.cmp(&b.1));
        Ok(out)
    }

    fn parse(root: &ScanRoot, path: &Path) -> Result<NormalizedConversation> {
        let manifest_bytes = fs::read(path)?;
        let manifest: Value = serde_json::from_slice(&manifest_bytes)?;
        ensure!(
            manifest.is_object(),
            "CloudMCP manifest must be a JSON object"
        );
        let id = json_field(&manifest, "conversation_id")
            .as_str()
            .filter(|s| !s.is_empty())
            .context("CloudMCP conversation ID missing")?;
        let workspace = json_field(&manifest, "workspace_root")
            .as_str()
            .filter(|s| !s.is_empty())
            .context("CloudMCP workspace missing")?;
        ensure!(
            json_field(&manifest, "transcript_file").as_str() == Some("transcript.jsonl"),
            "CloudMCP transcript must be an adjacent canonical file"
        );
        let transcript = path
            .parent()
            .context("CloudMCP manifest parent")?
            .join("transcript.jsonl");
        ensure!(
            !fs::symlink_metadata(&transcript)?.file_type().is_symlink(),
            "CloudMCP transcript cannot be a symlink"
        );
        let bytes = fs::read(&transcript)?;
        let digest = hex::encode(Sha256::digest(&bytes));
        ensure!(
            json_field(&manifest, "transcript_sha256").as_str() == Some(&digest),
            "CloudMCP transcript digest mismatch (possibly active publication; retry after commit)"
        );
        let text = std::str::from_utf8(&bytes)?;
        let mut messages = Vec::new();
        let mut seen = HashMap::new();
        let mut previous = None;
        let mut rows = 0u64;
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let event: Value = serde_json::from_str(line)?;
            rows += 1;
            ensure!(
                json_field(&event, "schema_version").as_str()
                    == Some("cloudmcp.conversation-event.v1"),
                "unsupported CloudMCP event schema"
            );
            ensure!(
                json_field(&event, "conversation_id").as_str() == Some(id)
                    && json_field(&event, "workspace_root").as_str() == Some(workspace),
                "CloudMCP event identity mismatch"
            );
            let content = json_field(&event, "content")
                .as_str()
                .context("CloudMCP event content missing")?;
            let hash = hex::encode(Sha256::digest(content.as_bytes()));
            ensure!(
                json_field(&event, "content_sha256").as_str() == Some(&hash),
                "CloudMCP content digest mismatch"
            );
            let sequence = json_field(&event, "sequence")
                .as_u64()
                .context("CloudMCP sequence missing")?;
            ensure!(
                previous.is_none_or(|n| sequence > n),
                "CloudMCP sequence must increase"
            );
            previous = Some(sequence);
            let event_id = json_field(&event, "source_event_id")
                .as_str()
                .context("CloudMCP source event ID missing")?;
            let source_hash = json_field(&event, "source_event_sha256")
                .as_str()
                .context("CloudMCP source event digest missing")?;
            if let Some((old_source_hash, old_hash)) = seen.get(event_id) {
                ensure!(
                    old_source_hash == source_hash && old_hash == &hash,
                    "conflicting duplicate CloudMCP source event"
                );
                continue;
            }
            seen.insert(event_id.to_owned(), (source_hash.to_owned(), hash)); // ubs:ignore -- Owned keys and digests are required for cross-row duplicate conflict checks.
            let role = json_field(&event, "role")
                .as_str()
                .context("CloudMCP role missing")?;
            ensure!(
                ["user", "assistant", "tool", "system"].contains(&role),
                "unknown CloudMCP role"
            );
            messages.push(NormalizedMessage {
                idx: messages.len() as i64,
                role: role.to_string(), // ubs:ignore -- The normalized role is owned while the source event stays in extra.
                author: json_field(&event, "agent_id").as_str().map(str::to_owned), // ubs:ignore -- The normalized author is owned while the source event stays in extra.
                created_at: event.get("created_at").and_then(parse_timestamp),
                content: content.to_string(), // ubs:ignore -- Normalized content is owned while the full event stays in extra.
                extra: event,
                snippets: vec![],
                invocations: vec![],
            });
        }
        ensure!(
            json_field(&manifest, "event_count").as_u64() == Some(rows),
            "CloudMCP event count mismatch"
        );
        ensure!(
            fs::read(path)? == manifest_bytes,
            "CloudMCP manifest changed during read"
        );
        let mut metadata = manifest.clone();
        let all_transcript_complete =
            json_field(&manifest, "all_transcript_complete").as_bool() == Some(true);
        {
            let metadata_fields = metadata
                .as_object_mut()
                .context("CloudMCP manifest must be a JSON object")?;
            metadata_fields.insert("source".to_owned(), json!("cloudmcp"));
            metadata_fields.insert("content_hash".to_owned(), json!(digest));
            metadata_fields.insert("origin".to_owned(), json!(root.origin));
            metadata_fields.insert("authority".to_owned(), json!("historical_evidence_only"));
            // Never infer complete host coverage from engine health or tool events.
            metadata_fields.insert(
                "all_transcript_complete".to_owned(),
                json!(all_transcript_complete),
            );
        }
        Ok(NormalizedConversation {
            agent_slug: "cloudmcp".into(),
            external_id: Some(id.to_string()),
            title: json_field(&manifest, "title").as_str().map(str::to_owned),
            workspace: Some(PathBuf::from(
                root.rewrite_workspace(workspace, Some("cloudmcp")),
            )),
            source_path: transcript,
            started_at: manifest.get("started_at").and_then(parse_timestamp),
            ended_at: manifest.get("ended_at").and_then(parse_timestamp),
            metadata,
            messages,
        })
    }
}

impl Connector for CloudMcpConnector {
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
        let mut out = Vec::new();
        self.scan_with_callback(ctx, &mut |c| {
            out.push(c);
            Ok(())
        })?;
        Ok(out)
    }
    fn supports_streaming_scan(&self) -> bool {
        true
    }
    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        emit: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        for (root, path) in Self::sources(ctx)? {
            emit(Self::parse(&root, &path)?)?;
        }
        Ok(())
    }
    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let mut out = Vec::new();
        for (root, path) in Self::sources(ctx)? {
            let transcript = path
                .parent()
                .context("CloudMCP manifest parent")?
                .join("transcript.jsonl");
            out.push(
                DiscoveredSourceFile::new(
                    "cloudmcp",
                    &root,
                    transcript,
                    DiscoveredSourceRole::PrimarySessionLog,
                    true,
                )
                .with_fs_metadata(),
            );
            out.push(
                DiscoveredSourceFile::new(
                    "cloudmcp",
                    &root,
                    path,
                    DiscoveredSourceRole::MetadataSidecar,
                    true,
                )
                .with_fs_metadata(),
            );
        }
        Ok(out)
    }
}
