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
use walkdir::WalkDir;

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, NormalizedConversation, ScanContext, ScanRoot,
};

pub struct AntigravityConnector {
    inner: franken_agent_detection::AntigravityConnector,
}

struct SelectedContext {
    scan: ScanContext,
    original_roots: HashMap<PathBuf, PathBuf>,
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
        for root in roots {
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
        SelectedContext {
            scan: selected_ctx,
            original_roots,
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
        self.inner.scan(&selected.scan)
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
            .scan_with_callback(&selected.scan, on_conversation)
    }
}
