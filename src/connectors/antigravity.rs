//! CASS adapter for the pinned Antigravity (agy) connector.
//!
//! The upstream parser is authoritative. Its bounded walk still descends into
//! conversation-local `.git` and `skills` trees, though transcripts can only
//! live below `.system_generated/logs`. Preselect conversation directories
//! while pruning those unrelated trees, then let the upstream connector parse
//! and discover sources with its normal identity and provenance rules.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;
use walkdir::WalkDir;

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation,
    ScanContext, ScanRoot, file_modified_since,
};
use crate::franken_sync::compat::{ConnectionExt, OpenFlags, RowExt, open_with_flags};

pub struct AntigravityConnector {
    inner: franken_agent_detection::AntigravityConnector,
}

struct SelectedContext {
    scan: ScanContext,
    original_roots: HashMap<PathBuf, PathBuf>,
    workspace_metadata: HashMap<PathBuf, Option<HashMap<String, String>>>,
}

impl SelectedContext {
    fn bind_workspace(&self, conversation: &mut NormalizedConversation) {
        let Some(dir) =
            AntigravityConnector::conversation_dir_for_transcript(&conversation.source_path)
        else {
            return;
        };
        let Some(database) = AntigravityConnector::summary_database(dir) else {
            return;
        };
        let native_id = dir.file_name().and_then(|name| name.to_str());
        let rows = self
            .workspace_metadata
            .get(&database)
            .and_then(Option::as_ref);
        let raw = native_id.and_then(|id| rows.and_then(|rows| rows.get(id)));
        let mut state = if rows.is_some() {
            "missing"
        } else {
            "unavailable"
        };
        if let Some(raw) = raw {
            match serde_json::from_str::<Vec<String>>(raw) {
                Ok(uris) => {
                    let paths: Option<HashSet<PathBuf>> = uris
                        .iter()
                        .map(|uri| {
                            let url = url::Url::parse(uri).ok()?;
                            if url.query().is_some() || url.fragment().is_some() {
                                return None;
                            }
                            url.to_file_path().ok().filter(|path| path.is_absolute())
                        })
                        .collect();
                    match paths {
                        Some(paths) if paths.len() == 1 => {
                            conversation.workspace = paths.into_iter().next();
                            state = "bound";
                        }
                        Some(paths) if paths.is_empty() => state = "missing",
                        Some(_) => state = "ambiguous",
                        None => state = "invalid",
                    }
                }
                Err(_) => state = "invalid",
            }
        }
        conversation.metadata["workspace_binding"] = json!({
            "state": state,
            "source": "native_conversation_summaries",
            "source_path": database,
            "native_conversation_id": native_id,
        });
    }
}

impl Default for AntigravityConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl AntigravityConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: franken_agent_detection::AntigravityConnector::new(),
        }
    }

    fn default_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
        if let Some(path) = dotenvy::var("CASS_ANTIGRAVITY_DATA_ROOT")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            return vec![ScanRoot::local(PathBuf::from(path))];
        }
        let data_dir = &ctx.data_dir;
        if data_dir.join("brain").is_dir()
            || data_dir.file_name().is_some_and(|name| name == "brain")
        {
            return vec![ScanRoot::local(data_dir.clone())];
        }
        let gemini = dirs::home_dir().unwrap_or_default().join(".gemini");
        vec![
            ScanRoot::local(gemini.join("antigravity")),
            ScanRoot::local(gemini.join("antigravity-cli")),
        ]
    }

    fn transcript_for_conversation(dir: &Path) -> PathBuf {
        dir.join(".system_generated")
            .join("logs")
            .join("transcript.jsonl")
    }

    fn conversation_dir_for_transcript(path: &Path) -> Option<&Path> {
        path.parent()
            .filter(|logs| logs.file_name().is_some_and(|name| name == "logs"))
            .and_then(Path::parent)
            .filter(|generated| {
                generated
                    .file_name()
                    .is_some_and(|name| name == ".system_generated")
            })
            .and_then(Path::parent)
    }

    fn summary_database(conversation_dir: &Path) -> Option<PathBuf> {
        conversation_dir
            .parent()
            .filter(|parent| parent.file_name().is_some_and(|name| name == "brain"))
            .and_then(Path::parent)
            .map(|root| root.join("conversation_summaries.db"))
    }

    fn read_workspaces(path: &Path) -> Result<HashMap<String, String>> {
        // This is agent-owned state. Never create, migrate, checkpoint, or write it.
        let db = open_with_flags(&path.to_string_lossy(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let rows = db.query_map_collect(
            "SELECT conversation_id, workspace_uris FROM conversation_summaries",
            &[],
            |row| Ok((row.get_typed::<String>(0)?, row.get_typed::<String>(1)?)),
        )?;
        Ok(rows.into_iter().collect())
    }

    fn selected_context(&self, ctx: &ScanContext) -> SelectedContext {
        let mut roots = if ctx.use_default_detection() {
            Self::default_roots(ctx)
        } else {
            ctx.scan_roots.clone()
        };
        roots.sort_by(|a, b| a.path.cmp(&b.path));
        roots.dedup_by(|a, b| a.path == b.path);
        let mut selected = Vec::new();
        let mut seen = HashSet::new();
        let mut original_roots = HashMap::new();
        for mut root in roots {
            if root.path.is_file()
                && root
                    .path
                    .file_name()
                    .is_some_and(|name| name == "transcript.jsonl")
                && let Some(conversation_dir) = Self::conversation_dir_for_transcript(&root.path)
            {
                let path = conversation_dir.to_path_buf();
                original_roots.insert(path.clone(), root.path.clone());
                root = root.with_path(path);
            }
            if Self::transcript_for_conversation(&root.path).is_file() {
                if seen.insert(root.path.clone()) {
                    selected.push(root);
                }
                continue;
            }
            for entry in WalkDir::new(&root.path)
                .min_depth(1)
                .max_depth(8)
                .into_iter()
                .filter_entry(|entry| {
                    !entry.file_type().is_dir()
                        || (entry.file_name() != ".git" && entry.file_name() != "skills")
                })
                .flatten()
            {
                if !entry.file_type().is_file() || entry.file_name() != "transcript.jsonl" {
                    continue;
                }
                if let Some(conversation_dir) = Self::conversation_dir_for_transcript(entry.path())
                {
                    let path = conversation_dir.to_path_buf();
                    if seen.insert(path.clone()) {
                        original_roots.insert(path.clone(), root.path.clone());
                        selected.push(root.with_path(path));
                    }
                }
            }
        }
        let mut selected_ctx = ctx.clone();
        selected_ctx.scan_roots = selected;
        let mut workspace_metadata = HashMap::new();
        for root in &selected_ctx.scan_roots {
            if let Some(database) = Self::summary_database(&root.path) {
                if ctx.since_ts.is_some()
                    && (file_modified_since(&database, ctx.since_ts)
                        || file_modified_since(&database.with_extension("db-wal"), ctx.since_ts))
                {
                    // A metadata change can bind an unchanged transcript to its workspace.
                    selected_ctx.since_ts = None;
                }
                workspace_metadata
                    .entry(database)
                    .or_insert_with_key(|path| Self::read_workspaces(path).ok());
            }
        }
        SelectedContext {
            scan: selected_ctx,
            original_roots,
            workspace_metadata,
        }
    }
}

impl Connector for AntigravityConnector {
    fn detect(&self) -> DetectionResult {
        self.inner.detect()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let selected = self.selected_context(ctx);
        if selected.scan.scan_roots.is_empty() {
            return Ok(Vec::new());
        }
        let mut conversations = self.inner.scan(&selected.scan)?;
        for conversation in &mut conversations {
            selected.bind_workspace(conversation);
        }
        Ok(conversations)
    }

    fn supports_streaming_scan(&self) -> bool {
        self.inner.supports_streaming_scan()
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let selected = self.selected_context(ctx);
        if selected.scan.scan_roots.is_empty() {
            return Ok(Vec::new());
        }
        let mut discovered = self.inner.discover_source_files(&selected.scan)?;
        for source in &mut discovered {
            if let Some(original_root) = selected.original_roots.get(&source.scan_root) {
                source.scan_root = original_root.clone();
            }
        }
        let mut seen: HashSet<PathBuf> = discovered
            .iter()
            .map(|source| source.source_path.clone())
            .collect();
        for root in &selected.scan.scan_roots {
            if let Some(database) = Self::summary_database(&root.path)
                && database.is_file()
                && seen.insert(database.clone())
            {
                let original = selected
                    .original_roots
                    .get(&root.path)
                    .unwrap_or(&root.path);
                let source_root = root.with_path(original.clone());
                discovered.push(
                    DiscoveredSourceFile::new(
                        "antigravity",
                        &source_root,
                        database,
                        DiscoveredSourceRole::MetadataSidecar,
                        true,
                    )
                    .with_fs_metadata(),
                );
                let wal =
                    Self::summary_database(&root.path).map(|path| path.with_extension("db-wal"));
                if let Some(wal) = wal
                    && wal.is_file()
                {
                    discovered.push(
                        DiscoveredSourceFile::new(
                            "antigravity",
                            &source_root,
                            wal,
                            DiscoveredSourceRole::MetadataSidecar,
                            false,
                        )
                        .with_fs_metadata(),
                    );
                }
            }
        }
        Ok(discovered)
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        let selected = self.selected_context(ctx);
        if selected.scan.scan_roots.is_empty() {
            return Ok(());
        }
        self.inner
            .scan_with_callback(&selected.scan, &mut |mut conversation| {
                selected.bind_workspace(&mut conversation);
                on_conversation(conversation)
            })
    }
}
