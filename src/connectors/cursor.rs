//! CASS compatibility adapter for Cursor session logs.
//!
//! The published `franken-agent-detection` 0.2.3 parser reconstructs Cursor
//! Agent workspaces from a lossy hyphenated project slug. Until the upstream
//! `.workspace-trusted` parser ships in a registry release, this adapter
//! backports that exact authority rule and source-sidecar accounting. It does
//! not modify Cursor IDE database conversations.

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation,
    ScanContext, ScanRoot, file_modified_since,
};
use walkdir::WalkDir;

const MAX_WORKSPACE_METADATA_BYTES: u64 = 1024 * 1024;
const AGENT_TRANSCRIPTS_DIR: &str = "agent-transcripts";
const WORKSPACE_TRUSTED_FILE: &str = ".workspace-trusted";
const MAX_AGENT_SCAN_DEPTH: usize = 8;

/// Preserve the published FAD unit-struct construction surface.
pub struct CursorConnector;

impl Default for CursorConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorConnector {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Preserve the public helper exposed by the registry connector.
    #[must_use]
    pub fn app_support_dir() -> Option<PathBuf> {
        franken_agent_detection::CursorConnector::app_support_dir()
    }

    fn workspace_metadata_path(transcript: &Path) -> Option<PathBuf> {
        Some(
            transcript
                .parent()?
                .parent()?
                .parent()?
                .join(WORKSPACE_TRUSTED_FILE),
        )
    }

    fn workspace_from_metadata(transcript: &Path) -> Option<PathBuf> {
        let path = Self::workspace_metadata_path(transcript)?;
        let mut file = File::open(path).ok()?;
        let mut bytes = Vec::new();
        file.by_ref()
            .take(MAX_WORKSPACE_METADATA_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > MAX_WORKSPACE_METADATA_BYTES {
            return None;
        }
        let metadata: Value = serde_json::from_slice(&bytes).ok()?;
        let workspace = metadata.get("workspacePath")?.as_str()?;
        let raw = workspace.as_bytes();
        let drive_absolute = raw.len() >= 3
            && raw[0].is_ascii_alphabetic()
            && raw[1] == b':'
            && matches!(raw[2], b'/' | b'\\');
        let unc_absolute = workspace.strip_prefix("\\\\").is_some_and(|rest| {
            let mut components = rest.split('\\');
            components.next().is_some_and(|part| !part.is_empty())
                && components.next().is_some_and(|part| !part.is_empty())
        });
        if workspace.chars().any(char::is_control)
            || !(workspace.starts_with('/') || drive_absolute || unc_absolute)
        {
            return None;
        }
        Some(PathBuf::from(workspace))
    }

    fn workspace_for_scan(transcript: &Path, ctx: &ScanContext, workspace: PathBuf) -> PathBuf {
        let Some(root) = ctx
            .scan_roots
            .iter()
            .filter(|root| transcript.starts_with(&root.path))
            .max_by_key(|root| root.path.components().count())
        else {
            return workspace;
        };
        PathBuf::from(root.rewrite_workspace(&workspace.to_string_lossy(), Some("cursor")))
    }

    fn apply_workspace_authority(conversation: &mut NormalizedConversation, ctx: &ScanContext) {
        if conversation.agent_slug != "cursor"
            || conversation.metadata["cursor_format"].as_str() != Some("agent")
        {
            return;
        }

        let workspace = Self::workspace_from_metadata(&conversation.source_path)
            .map(|workspace| Self::workspace_for_scan(&conversation.source_path, ctx, workspace));
        conversation.workspace.clone_from(&workspace);
        let attribution = if workspace.is_some() {
            "workspace_trusted"
        } else {
            "unresolved"
        };
        if !conversation.metadata.is_object() {
            conversation.metadata = Value::Object(serde_json::Map::new());
        }
        conversation.metadata["cursor_workspace_attribution"] = Value::String(attribution.into());
    }

    fn sidecar_source(source: &DiscoveredSourceFile) -> Option<DiscoveredSourceFile> {
        if source.provider_slug != "cursor"
            || source.role != DiscoveredSourceRole::PrimarySessionLog
        {
            return None;
        }
        let path = Self::workspace_metadata_path(&source.source_path)?;
        if !path.is_file() {
            return None;
        }
        let mut sidecar = source.clone();
        sidecar.source_path = path;
        sidecar.role = DiscoveredSourceRole::MetadataSidecar;
        sidecar.required_for_reconstruction = true;
        sidecar.size_bytes = None;
        sidecar.modified_at_ms = None;
        Some(sidecar.with_fs_metadata())
    }

    fn nonempty_env_path(key: &str) -> Option<PathBuf> {
        dotenvy::var(key)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }

    fn looks_like_cursor_base(path: &Path) -> bool {
        path.join("globalStorage").exists()
            || path.join("workspaceStorage").exists()
            || path
                .file_name()
                .is_some_and(|name| name == "globalStorage" || name == "workspaceStorage")
            || (path.is_file() && path.file_name().is_some_and(|name| name == "state.vscdb"))
    }

    fn agent_scan_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
        let mut roots = if ctx.scan_roots.is_empty() {
            if Self::looks_like_cursor_base(&ctx.data_dir)
                && Self::nonempty_env_path("CASS_CURSOR_PROJECTS_ROOT").is_none()
            {
                vec![ScanRoot::local(ctx.data_dir.clone())]
            } else {
                Self::nonempty_env_path("CASS_CURSOR_PROJECTS_ROOT")
                    .or_else(|| dirs::home_dir().map(|home| home.join(".cursor/projects")))
                    .into_iter()
                    .map(ScanRoot::local)
                    .collect()
            }
        } else {
            ctx.scan_roots.clone()
        };
        roots.sort_by(|left, right| left.path.cmp(&right.path));
        roots.dedup_by(|left, right| left.path == right.path);
        roots
    }

    fn is_primary_agent_transcript(path: &Path) -> bool {
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl")
            || path
                .components()
                .any(|component| component.as_os_str().to_str() == Some("subagents"))
        {
            return false;
        }
        let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
            return false;
        };
        let Some(parent) = path.parent() else {
            return false;
        };
        parent.file_name().and_then(|name| name.to_str()) == Some(stem)
            && parent
                .parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
                == Some(AGENT_TRANSCRIPTS_DIR)
    }

    /// Find old transcripts whose reconstruction sidecar changed after the
    /// incremental cutoff. The FAD registry parser filters only transcript
    /// mtimes, so those transcripts need one targeted uncapped parse each.
    fn sidecar_changed_transcripts(ctx: &ScanContext) -> Vec<(ScanRoot, PathBuf)> {
        let Some(since_ts) = ctx.since_ts else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        let mut changed = Vec::new();

        for root in Self::agent_scan_roots(ctx) {
            if !root.path.exists() {
                continue;
            }
            for entry in WalkDir::new(&root.path)
                .max_depth(MAX_AGENT_SCAN_DEPTH)
                .follow_links(false)
                .into_iter()
                .flatten()
            {
                let transcript = entry.path();
                if !entry.file_type().is_file() || !Self::is_primary_agent_transcript(transcript) {
                    continue;
                }
                let Some(sidecar) = Self::workspace_metadata_path(transcript) else {
                    continue;
                };
                if !sidecar.is_file() || !file_modified_since(&sidecar, Some(since_ts)) {
                    continue;
                }
                if seen.insert(transcript.to_path_buf()) {
                    changed.push((root.clone(), transcript.to_path_buf()));
                }
            }
        }

        changed.sort_by(|left, right| left.1.cmp(&right.1));
        changed
    }

    fn targeted_transcript_context(
        ctx: &ScanContext,
        root: &ScanRoot,
        transcript: &Path,
    ) -> ScanContext {
        let mut targeted = ctx.clone();
        let session_root = transcript.parent().unwrap_or(transcript).to_path_buf();
        targeted.scan_roots = vec![root.with_path(session_root)];
        targeted.since_ts = None;
        targeted
    }
}

impl Connector for CursorConnector {
    fn detect(&self) -> DetectionResult {
        franken_agent_detection::CursorConnector::new().detect()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let inner = franken_agent_detection::CursorConnector::new();
        let mut conversations = inner.scan(ctx)?;
        let mut seen_transcripts = conversations
            .iter()
            .map(|conversation| conversation.source_path.clone())
            .collect::<HashSet<_>>();

        for (root, transcript) in Self::sidecar_changed_transcripts(ctx) {
            if seen_transcripts.contains(&transcript) {
                continue;
            }
            let targeted_ctx = Self::targeted_transcript_context(ctx, &root, &transcript);
            for conversation in inner.scan(&targeted_ctx)? {
                if seen_transcripts.insert(conversation.source_path.clone()) {
                    conversations.push(conversation);
                }
            }
        }

        for conversation in &mut conversations {
            Self::apply_workspace_authority(conversation, ctx);
        }
        Ok(conversations)
    }

    fn supports_streaming_scan(&self) -> bool {
        false
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let inner = franken_agent_detection::CursorConnector::new();
        let mut sources = inner.discover_source_files(ctx)?;
        let mut seen = sources
            .iter()
            .map(|source| source.source_path.clone())
            .collect::<HashSet<_>>();

        for (root, transcript) in Self::sidecar_changed_transcripts(ctx) {
            if seen.insert(transcript.clone()) {
                sources.push(
                    DiscoveredSourceFile::new(
                        "cursor",
                        &root,
                        transcript,
                        DiscoveredSourceRole::PrimarySessionLog,
                        true,
                    )
                    .with_fs_metadata(),
                );
            }
        }

        let sidecars = sources
            .iter()
            .filter_map(Self::sidecar_source)
            .filter(|source| seen.insert(source.source_path.clone()))
            .collect::<Vec<_>>();
        sources.extend(sidecars);
        Ok(sources)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::TempDir;

    fn transcript(root: &Path) -> PathBuf {
        let path = root.join("project-parent-my-app/agent-transcripts/s/s.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}\n").unwrap();
        path
    }

    fn agent_transcript(
        projects_root: &Path,
        project: &str,
        session: &str,
        message: &str,
    ) -> (PathBuf, PathBuf) {
        let project_root = projects_root.join(project);
        let transcript = project_root
            .join(AGENT_TRANSCRIPTS_DIR)
            .join(session)
            .join(format!("{session}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let record = serde_json::json!({
            "role": "user",
            "message": {"content": [{"type": "text", "text": message}]}
        });
        std::fs::write(&transcript, format!("{record}\n")).unwrap();
        (transcript, project_root.join(WORKSPACE_TRUSTED_FILE))
    }

    fn set_mtime(path: &Path, seconds: u64) {
        std::fs::File::open(path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)),
            )
            .unwrap();
    }

    #[test]
    fn sidecar_mtime_reincludes_old_transcript_without_scanning_unchanged_projects() {
        let tmp = TempDir::new().unwrap();
        let projects = tmp.path().join("projects");
        let (changed, changed_sidecar) =
            agent_transcript(&projects, "changed-project", "changed-session", "changed");
        let (unchanged, unchanged_sidecar) = agent_transcript(
            &projects,
            "unchanged-project",
            "unchanged-session",
            "unchanged",
        );

        set_mtime(&changed, 100);
        set_mtime(&unchanged, 100);
        std::fs::write(
            &changed_sidecar,
            serde_json::json!({"workspacePath": "/workspace/changed"}).to_string(),
        )
        .unwrap();
        std::fs::write(
            &unchanged_sidecar,
            serde_json::json!({"workspacePath": "/workspace/unchanged"}).to_string(),
        )
        .unwrap();
        set_mtime(&changed_sidecar, 300);
        set_mtime(&unchanged_sidecar, 100);

        let root = ScanRoot::local(projects.clone());
        let ctx = ScanContext::with_roots(projects, vec![root], Some(200_000));
        let connector = CursorConnector::new();
        let conversations = connector.scan(&ctx).unwrap();

        assert_eq!(
            conversations.len(),
            1,
            "only the project whose sidecar changed should be parsed"
        );
        assert_eq!(conversations[0].source_path, changed);
        assert_eq!(
            conversations[0].workspace.as_deref(),
            Some(Path::new("/workspace/changed"))
        );
        assert_eq!(
            conversations[0].metadata["cursor_workspace_attribution"],
            "workspace_trusted"
        );

        let sources = connector.discover_source_files(&ctx).unwrap();
        assert!(sources.iter().any(|source| {
            source.source_path == changed && source.role == DiscoveredSourceRole::PrimarySessionLog
        }));
        assert!(sources.iter().any(|source| {
            source.source_path == changed_sidecar
                && source.role == DiscoveredSourceRole::MetadataSidecar
                && source.required_for_reconstruction
        }));
        assert!(
            !sources.iter().any(|source| source.source_path == unchanged),
            "unchanged project should stay outside the incremental source set"
        );
    }

    #[test]
    fn deleting_workspace_sidecar_clears_workspace_authority_on_rescan() {
        let tmp = TempDir::new().unwrap();
        let projects = tmp.path().join("projects");
        let (transcript, sidecar) =
            agent_transcript(&projects, "delete-project", "delete-session", "delete");
        std::fs::write(
            &sidecar,
            serde_json::json!({"workspacePath": "/workspace/delete"}).to_string(),
        )
        .unwrap();

        let ctx = ScanContext::with_roots(projects.clone(), vec![ScanRoot::local(projects)], None);
        let connector = CursorConnector::new();
        let trusted = connector.scan(&ctx).unwrap();
        assert_eq!(trusted.len(), 1);
        assert_eq!(
            trusted[0].workspace.as_deref(),
            Some(Path::new("/workspace/delete"))
        );

        std::fs::remove_file(sidecar).unwrap();
        let unresolved = connector.scan(&ctx).unwrap();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].source_path, transcript);
        assert_eq!(unresolved[0].workspace, None);
        assert_eq!(
            unresolved[0].metadata["cursor_workspace_attribution"],
            "unresolved"
        );
    }

    #[test]
    fn public_cursor_connector_surface_supports_registry_construction_and_helper() {
        let _unit_constructed = CursorConnector;
        let _constructed = CursorConnector::new();
        let _app_support = CursorConnector::app_support_dir();
    }

    #[test]
    fn workspace_metadata_requires_a_bounded_absolute_path() {
        let tmp = TempDir::new().unwrap();
        let transcript = transcript(tmp.path());
        let sidecar = CursorConnector::workspace_metadata_path(&transcript).unwrap();

        for workspace in [
            "/project-parent/my-app",
            r"C:\Users\example\my-app",
            r"\\server\share\my-app",
        ] {
            std::fs::write(
                &sidecar,
                serde_json::json!({"workspacePath": workspace}).to_string(),
            )
            .unwrap();
            assert_eq!(
                CursorConnector::workspace_from_metadata(&transcript).as_deref(),
                Some(Path::new(workspace))
            );
        }

        for payload in [
            "{malformed",
            r#"{"workspacePath":"relative/path"}"#,
            "{\"workspacePath\":\"/bad\\u0000path\"}",
        ] {
            std::fs::write(&sidecar, payload).unwrap();
            assert_eq!(CursorConnector::workspace_from_metadata(&transcript), None);
        }
        std::fs::write(
            &sidecar,
            vec![b'x'; MAX_WORKSPACE_METADATA_BYTES as usize + 1],
        )
        .unwrap();
        assert_eq!(CursorConnector::workspace_from_metadata(&transcript), None);
    }

    #[test]
    fn trusted_workspace_uses_matching_scan_root_rewrite() {
        let tmp = TempDir::new().unwrap();
        let transcript = transcript(tmp.path());
        let sidecar = CursorConnector::workspace_metadata_path(&transcript).unwrap();
        std::fs::write(
            &sidecar,
            serde_json::json!({"workspacePath": "/remote/projects/my-app"}).to_string(),
        )
        .unwrap();

        let matching = super::super::ScanRoot::local(tmp.path().to_path_buf())
            .with_rewrite("/remote/projects", "/warp/projects");
        let unrelated = super::super::ScanRoot::local(tmp.path().join("other"))
            .with_rewrite("/remote/projects", "/wrong");
        let ctx =
            ScanContext::with_roots(tmp.path().to_path_buf(), vec![unrelated, matching], None);
        let workspace = CursorConnector::workspace_from_metadata(&transcript)
            .map(|workspace| CursorConnector::workspace_for_scan(&transcript, &ctx, workspace));

        assert_eq!(
            workspace.as_deref(),
            Some(Path::new("/warp/projects/my-app"))
        );
    }
}
