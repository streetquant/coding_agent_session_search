//! Preserve the pinned legacy parser and support Cline's current `tasks/<id>` layout.
//! Modern UI files are presentation events; the API history carries native roles.

use std::collections::HashSet;
use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation,
    NormalizedMessage, ScanContext, ScanRoot, file_modified_since, flatten_content,
    parse_timestamp, reindex_messages,
};

const MAX_HISTORY_BYTES: u64 = 100 * 1024 * 1024;
const CLI_MESSAGES_SUFFIX: &str = ".messages.json";
const LOG_NAMES: [&str; 2] = ["api_conversation_history.json", "ui_messages.json"];
const EXTENSIONS: [&str; 2] = ["saoudrizwan.claude-dev", "rooveterinaryinc.roo-cline"];
const STORAGE_SUFFIXES: [&str; 20] = [
    "",
    "globalStorage",
    "User/globalStorage",
    "Code/User/globalStorage",
    "Code - Insiders/User/globalStorage",
    "VSCodium/User/globalStorage",
    "Cursor/User/globalStorage",
    ".config/Code/User/globalStorage",
    ".config/Code - Insiders/User/globalStorage",
    ".config/VSCodium/User/globalStorage",
    ".config/Cursor/User/globalStorage",
    "Library/Application Support/Code/User/globalStorage",
    "Library/Application Support/Code - Insiders/User/globalStorage",
    "Library/Application Support/VSCodium/User/globalStorage",
    "Library/Application Support/Cursor/User/globalStorage",
    "AppData/Roaming/Code/User/globalStorage",
    "AppData/Roaming/Code - Insiders/User/globalStorage",
    "AppData/Roaming/VSCodium/User/globalStorage",
    "AppData/Roaming/Cursor/User/globalStorage",
    ".cline",
];

pub struct ClineConnector;

impl Default for ClineConnector {
    fn default() -> Self {
        Self::new()
    }
}

struct Task {
    path: PathBuf,
    root: ScanRoot,
}

struct CliSession {
    messages_path: PathBuf,
    metadata_path: PathBuf,
    root: ScanRoot,
}

impl CliSession {
    fn changed(&self, since: Option<i64>) -> bool {
        file_modified_since(&self.messages_path, since)
            || file_modified_since(&self.metadata_path, since)
    }

    fn conversation(&self) -> Result<NormalizedConversation> {
        let envelope = read_cli_json(&self.messages_path)?;
        let object = envelope
            .as_object()
            .context("Cline CLI message source must be a JSON object")?;
        let native_messages = object
            .get("messages")
            .and_then(Value::as_array)
            .context("Cline CLI message source is missing its messages array")?;

        let path_session_id = self
            .messages_path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(CLI_MESSAGES_SUFFIX))
            .context("Cline CLI message filename has no native session identifier")?;
        let directory_session_id = self
            .messages_path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .context("Cline CLI message source is not inside a session directory")?;
        let session_id = object
            .get("sessionId")
            .or_else(|| object.get("session_id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .context("Cline CLI message source is missing its native session identifier")?;
        ensure!(
            session_id == path_session_id && session_id == directory_session_id, // ubs:ignore -- Public native session identifiers bind the transcript to its filename, not credential authentication.
            "Cline CLI session identity does not match its source path"
        );

        let session_metadata = if self.metadata_path.is_file() {
            Some(read_cli_json(&self.metadata_path)?)
        } else {
            None
        };
        if let Some(metadata_id) = session_metadata
            .as_ref()
            .and_then(|value| value.get("session_id"))
        {
            let metadata_id = metadata_id
                .as_str()
                .context("Cline CLI sidecar session_id must be a string")?;
            ensure!(
                metadata_id == session_id, // ubs:ignore -- Public sidecar/transcript provenance identifiers, not bearer secrets.
                "Cline CLI sidecar identity does not match its message source"
            );
        }
        let session_id = session_id.to_owned();

        let mut messages = Vec::with_capacity(native_messages.len());
        for (index, item) in native_messages.iter().enumerate() {
            ensure!(
                item.is_object(),
                "Cline CLI message at position {index} is not a JSON object"
            );
            let content_value = item
                .get("content")
                .or_else(|| item.get("text"))
                .or_else(|| item.get("message"));
            let content = match content_value
                .map(flatten_cli_content)
                .filter(|content| !content.trim().is_empty())
            {
                Some(content) => content,
                None => serde_json::to_string(item)
                    .context("failed to preserve a Cline CLI message without searchable text")?,
            };
            let idx = i64::try_from(index)
                .context("Cline CLI message order exceeds the supported index range")?;
            messages.push(NormalizedMessage {
                idx,
                role: item
                    .get("role")
                    .or_else(|| item.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("agent")
                    .to_owned(), // ubs:ignore -- One owned normalized role per retained native message, bounded by the source byte limit.
                author: None,
                created_at: item
                    .get("timestamp")
                    .or_else(|| item.get("created_at"))
                    .or_else(|| item.get("ts"))
                    .and_then(parse_timestamp),
                content,
                extra: item.clone(), // ubs:ignore -- Preserve the complete native JSON separately from searchable rendering; the source is byte bounded.
                invocations: content_value.map_or_else(Vec::new, |value| {
                    franken_agent_detection::extract_invocations_from_content_blocks(value)
                }),
                snippets: Vec::new(),
            });
        }
        reindex_messages(&mut messages);

        let title = session_metadata
            .as_ref()
            .and_then(|metadata| metadata.get("title"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                messages
                    .first()
                    .and_then(|message| message.content.lines().next())
                    .map(|line| line.chars().take(100).collect())
            });
        let workspace = session_metadata
            .as_ref()
            .and_then(|metadata| {
                [
                    "workspace_root",
                    "workspaceHome",
                    "rootPath",
                    "cwd",
                    "workspace",
                    "cwdOnTaskInitialization",
                ]
                .iter()
                .find_map(|key| metadata.get(*key).and_then(Value::as_str))
            })
            .or_else(|| {
                [
                    "workspace_root",
                    "workspaceHome",
                    "rootPath",
                    "cwd",
                    "workspace",
                ]
                .iter()
                .find_map(|key| object.get(*key).and_then(Value::as_str))
            })
            .filter(|path| !path.trim().is_empty())
            .map(PathBuf::from);
        let started_at = messages
            .iter()
            .filter_map(|message| message.created_at)
            .min()
            .or_else(|| {
                session_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("started_at"))
                    .and_then(parse_timestamp)
            });
        let ended_at = messages
            .iter()
            .filter_map(|message| message.created_at)
            .max()
            .or_else(|| object.get("updated_at").and_then(parse_timestamp));

        let mut native_header = envelope;
        if let Some(header) = native_header.as_object_mut() {
            header.remove("messages");
        }
        Ok(NormalizedConversation {
            agent_slug: "cline".to_owned(),
            external_id: Some(session_id),
            title,
            workspace,
            started_at,
            ended_at,
            metadata: json!({
                "source": "cline",
                "layout": "standalone_cli",
                "coverage": "native_cli_message_snapshot",
                "source_read_complete": true,
                "transcript_finality": "unknown",
                "message_count": messages.len(),
                "native_header": native_header,
                "native_session_metadata": session_metadata,
            }),
            source_path: self.messages_path.clone(),
            messages,
        })
    }
}

fn is_cli_messages_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(CLI_MESSAGES_SUFFIX))
}

fn configured_cli_root() -> Option<PathBuf> {
    dotenvy::var("CASS_CLINE_DATA_ROOT")
        .ok()
        .filter(|root| !root.trim().is_empty())
        .map(PathBuf::from)
}

fn host_cli_root() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".cline/data/sessions"))
}

fn same_file_object(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        left.dev() == right.dev() && left.ino() == right.ino()
    }
    #[cfg(windows)]
    {
        left.volume_serial_number().is_some()
            && left.volume_serial_number() == right.volume_serial_number()
            && left.file_index().is_some()
            && left.file_index() == right.file_index()
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (left, right);
        false
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    left == right
        || left
            .canonicalize()
            .ok()
            .zip(right.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

fn legacy_source_layout(path: &Path) -> bool {
    let base = if path
        .file_name()
        .is_some_and(|name| name == "settings" || name == "settings.json")
    {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    let is_extension_root = base
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains("claude-dev") || name.contains("roo-cline"));
    let has_legacy_task = base.is_dir()
        && fs::read_dir(base).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                entry.file_type().is_ok_and(|kind| kind.is_dir())
                    && LOG_NAMES.iter().any(|log| entry.path().join(log).is_file())
            })
        });
    if is_extension_root || has_legacy_task {
        return true;
    }

    let base_name = base.file_name().and_then(|name| name.to_str());
    let suffixes: &[&str] = match base_name {
        Some(".config") => &[
            "Code/User/globalStorage",
            "Code - Insiders/User/globalStorage",
            "VSCodium/User/globalStorage",
        ],
        Some("Application Support") => &[
            "Code/User/globalStorage",
            "Code - Insiders/User/globalStorage",
            "VSCodium/User/globalStorage",
        ],
        Some("Roaming")
            if base
                .parent()
                .is_some_and(|parent| parent.file_name().is_some_and(|name| name == "AppData")) =>
        {
            &[
                "Code/User/globalStorage",
                "Code - Insiders/User/globalStorage",
                "VSCodium/User/globalStorage",
            ]
        }
        Some("Code" | "Code - Insiders" | "VSCodium") => &["User/globalStorage"],
        Some("User") => &["globalStorage"],
        Some("globalStorage") => &[""],
        _ => &STORAGE_SUFFIXES,
    };
    suffixes.iter().any(|suffix| {
        EXTENSIONS
            .iter()
            .any(|extension| base.join(suffix).join(extension).is_dir())
    })
}

fn native_cli_root_for(path: &Path, default_cli_root: Option<&Path>) -> Option<PathBuf> {
    let name = path.file_name()?;
    let parent_name = path.parent()?.file_name();
    let grandparent_name = path.parent()?.parent()?.file_name();
    if name == "sessions"
        && parent_name.is_some_and(|parent| parent == "data")
        && grandparent_name.is_some_and(|parent| parent == ".cline")
    {
        return Some(path.to_path_buf());
    }
    if name == "data" && parent_name.is_some_and(|parent| parent == ".cline") {
        return Some(path.join("sessions"));
    }
    if name == ".cline" {
        return Some(path.join("data/sessions"));
    }

    default_cli_root.and_then(|root| {
        let home = root.parent()?.parent()?.parent()?;
        same_path(path, home).then(|| root.to_path_buf())
    })
}

fn cli_scan_roots_with(
    ctx: &ScanContext,
    configured_root: Option<PathBuf>,
    default_cli_root: Option<PathBuf>,
) -> Result<Vec<ScanRoot>> {
    if !ctx.use_default_detection() {
        return Ok(ctx.scan_roots.clone());
    }

    // A source-root override remains isolated; `data_dir` is normally CASS's
    // archive and must not suppress default agent-source discovery.
    if let Some(root) = configured_root {
        return Ok(vec![ScanRoot::local(root)]);
    }

    if let Some(root) = native_cli_root_for(&ctx.data_dir, default_cli_root.as_deref()) {
        return Ok(vec![ScanRoot::local(root)]);
    }

    let mut modern_tasks = Vec::new();
    let mut seen_tasks = HashSet::new();
    ClineConnector::collect_tasks(
        &ScanRoot::local(ctx.data_dir.clone()),
        &mut modern_tasks,
        &mut seen_tasks,
    );
    let task_container = ctx.data_dir.file_name().is_some_and(|name| name == "tasks");
    if !modern_tasks.is_empty() || task_container || legacy_source_layout(&ctx.data_dir) {
        return Ok(vec![ScanRoot::local(ctx.data_dir.clone())]);
    }

    Ok(default_cli_root.into_iter().map(ScanRoot::local).collect())
}

fn cli_scan_roots(ctx: &ScanContext) -> Result<Vec<ScanRoot>> {
    cli_scan_roots_with(ctx, configured_cli_root(), host_cli_root())
}

fn push_cli_session(
    path: PathBuf,
    root: &ScanRoot,
    sessions: &mut Vec<CliSession>,
    seen: &mut HashSet<PathBuf>,
) {
    if !is_cli_messages_path(&path) || !path.is_file() {
        return;
    }
    let key = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    if !seen.insert(key) {
        return;
    }
    let metadata_path = path.with_file_name(
        path.file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(CLI_MESSAGES_SUFFIX))
            .map_or_else(|| "session.json".to_owned(), |id| format!("{id}.json")),
    );
    sessions.push(CliSession {
        messages_path: path,
        metadata_path,
        root: root.clone(),
    });
}

fn collect_cli_sessions(
    root: &ScanRoot,
    sessions: &mut Vec<CliSession>,
    seen: &mut HashSet<PathBuf>,
) -> Result<()> {
    let path = &root.path;
    if !path.exists() {
        return Ok(());
    }
    if path.is_file() {
        push_cli_session(path.clone(), root, sessions, seen);
        return Ok(());
    }

    let directory_name = path.file_name().and_then(|name| name.to_str());
    let scoped_path = match directory_name {
        Some(".cline") => Some(path.join("data/sessions")),
        Some("data") => Some(path.join("sessions")),
        _ => None,
    };
    if let Some(scoped_path) = scoped_path {
        if scoped_path.exists() {
            collect_cli_sessions(&root.with_path(scoped_path), sessions, seen)?;
        }
        return Ok(());
    }

    let expected_file = directory_name.map(|id| path.join(format!("{id}{CLI_MESSAGES_SUFFIX}")));
    if let Some(expected_file) = expected_file.filter(|candidate| candidate.is_file()) {
        push_cli_session(expected_file, root, sessions, seen);
        return Ok(());
    }

    let entries = fs::read_dir(path)
        .with_context(|| format!("could not enumerate Cline CLI root {}", path.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| {
            format!( /* ubs:ignore -- This diagnostic allocates only if reading a directory entry fails. */
                "could not enumerate an entry in Cline CLI root {}",
                path.display()
            )
        })?;
        let entry_path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_file() && is_cli_messages_path(&entry_path) {
            push_cli_session(entry_path, root, sessions, seen);
        } else if file_type.is_dir() {
            let Some(id) = entry_path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let mut candidate_name = std::ffi::OsString::from(id);
            candidate_name.push(CLI_MESSAGES_SUFFIX);
            let candidate = entry_path.join(candidate_name); // ubs:ignore -- Name comes from a single read_dir basename plus a constant suffix; this is read discovery, not an extraction/write path.
            if candidate.is_file() {
                push_cli_session(candidate, root, sessions, seen);
            }
        }
    }
    Ok(())
}

fn cli_sessions(ctx: &ScanContext) -> Result<Vec<CliSession>> {
    let mut sessions = Vec::new();
    let mut seen = HashSet::new();
    for root in cli_scan_roots(ctx)? {
        collect_cli_sessions(&root, &mut sessions, &mut seen)?;
    }
    sessions.sort_by(|a, b| a.messages_path.cmp(&b.messages_path));
    Ok(sessions)
}

fn read_cli_json(path: &Path) -> Result<Value> {
    for _attempt in 0..2 {
        let mut file = fs::File::open(path)
            .with_context(|| format!("could not open Cline CLI source {}", path.display()))?; // ubs:ignore -- Error-only diagnostic in a two-attempt stable-read loop.
        let before = file.metadata()?;
        ensure!(
            before.len() <= MAX_HISTORY_BYTES,
            "Cline CLI source {} exceeds the 100 MiB safety limit",
            path.display()
        );
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_HISTORY_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("could not read Cline CLI source {}", path.display()))?; // ubs:ignore -- Error-only diagnostic in a two-attempt stable-read loop.
        ensure!(
            u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= MAX_HISTORY_BYTES,
            "Cline CLI source {} exceeded the 100 MiB safety limit while being read",
            path.display()
        );
        let after = file.metadata()?;
        let current = fs::metadata(path)?;
        if !same_file_object(&before, &after)
            || !same_file_object(&after, &current)
            || before.len() != after.len()
            || before.modified().ok() != after.modified().ok()
            || after.len() != current.len()
            || after.modified().ok() != current.modified().ok()
            || u64::try_from(bytes.len()).unwrap_or(u64::MAX) != after.len()
        {
            continue;
        }
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!("Cline CLI source {} is not valid UTF-8", path.display()))? // ubs:ignore -- Error-only diagnostic, never allocated on a successful decode.
            .trim_start_matches('\u{feff}');
        return serde_json::from_str(text).with_context(|| {
            format!( /* ubs:ignore -- Error-only diagnostic, never allocated on successful JSON parsing. */
                "Cline CLI source {} contains malformed or truncated JSON",
                path.display()
            )
        });
    }
    anyhow::bail!(
        "Cline CLI source {} changed while being read; retry after the writer settles",
        path.display()
    )
}

fn flatten_cli_content(value: &Value) -> String {
    let Some(blocks) = value.as_array() else {
        return flatten_content(value);
    };
    let mut parts = Vec::with_capacity(blocks.len());
    for block in blocks {
        let rendered = if block.get("type").and_then(Value::as_str) == Some("tool_result") {
            let mut result_text = Vec::new();
            if let Some(content) = block.get("content") {
                collect_result_text(content, &mut result_text);
            }
            if result_text.is_empty() {
                block.to_string() // ubs:ignore -- Retains an otherwise unrenderable native tool-result block within the source byte limit.
            } else {
                result_text.join("\n")
            }
        } else {
            let rendered = flatten_content(&Value::Array(vec![block.clone()])); // ubs:ignore -- The shared renderer accepts an array; one bounded block is copied while the raw native payload stays intact.
            if rendered.trim().is_empty() {
                block.to_string() // ubs:ignore -- Retains an otherwise unrenderable native block within the source byte limit.
            } else {
                rendered
            }
        };
        if !rendered.trim().is_empty() {
            parts.push(rendered);
        }
    }
    parts.join("\n")
}

fn collect_result_text(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::String(text) if !text.trim().is_empty() => output.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                collect_result_text(item, output);
            }
        }
        Value::Object(fields) => {
            for key in ["text", "content", "output", "result"] {
                if let Some(value) = fields.get(key) {
                    collect_result_text(value, output);
                }
            }
        }
        _ => {}
    }
}

fn modern_task(path: &Path) -> bool {
    path.parent()
        .is_some_and(|parent| parent.file_name().is_some_and(|n| n == "tasks"))
        && log_paths(path).iter().any(|log| log.is_file())
}

fn log_paths(task: &Path) -> [PathBuf; 2] {
    [
        task.join("api_conversation_history.json"),
        task.join("ui_messages.json"),
    ]
}

fn read_json(path: &Path) -> Option<Value> {
    let file = fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_HISTORY_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_HISTORY_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if u64::try_from(bytes.len()).ok()? > MAX_HISTORY_BYTES {
        return None;
    }
    let text = std::str::from_utf8(&bytes)
        .ok()?
        .trim_start_matches('\u{feff}');
    serde_json::from_str(text).ok()
}

fn messages(value: Value, preserve_native_content: bool) -> Vec<NormalizedMessage> {
    let Value::Array(items) = value else {
        return Vec::new();
    };
    let mut result = Vec::with_capacity(items.len());
    for item in items {
        let content_value = item
            .get("content")
            .or_else(|| item.get("text"))
            .or_else(|| item.get("message"));
        let content = content_value
            .map(|value| {
                if preserve_native_content {
                    flatten_cli_content(value)
                } else {
                    flatten_content(value)
                }
            })
            .unwrap_or_default();
        let content = if content.trim().is_empty()
            && preserve_native_content
            && content_value.is_some_and(|value| {
                !value.is_null() && !value.as_array().is_some_and(Vec::is_empty)
            }) {
            item.to_string() // ubs:ignore -- Retains the whole native record when ordinary text rendering is empty; source size is bounded.
        } else {
            content
        };
        if content.trim().is_empty() {
            continue;
        }
        let invocations = content_value.map_or_else(
            Vec::new,
            franken_agent_detection::extract_invocations_from_content_blocks,
        );
        result.push(NormalizedMessage {
            idx: i64::try_from(result.len()).unwrap_or(i64::MAX),
            role: item
                .get("role")
                .or_else(|| item.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("agent")
                .to_owned(), // ubs:ignore -- Normalized role owns its string; raw native JSON is retained independently below.
            author: None,
            created_at: item
                .get("timestamp")
                .or_else(|| item.get("created_at"))
                .or_else(|| item.get("ts"))
                .and_then(parse_timestamp),
            content,
            extra: item,
            invocations,
            snippets: Vec::new(),
        });
    }
    // API ordering is native conversation ordering; do not move tool responses past their calls.
    reindex_messages(&mut result);
    result
}

impl Task {
    fn history_path(&self) -> Option<PathBuf> {
        let parent = self.path.parent()?.parent()?;
        let modern = parent.join("state/taskHistory.json");
        if modern.is_file() {
            Some(modern)
        } else {
            let legacy = parent.join("taskHistory.json");
            legacy.is_file().then_some(legacy)
        }
    }

    fn history_entry(&self) -> Option<Value> {
        let value = read_json(&self.history_path()?)?;
        let id = self.path.file_name()?.to_str()?;
        let mut matching = value
            .as_array()?
            .iter()
            .filter(|item| item.get("id").and_then(Value::as_str) == Some(id));
        let first = matching.next()?.clone();
        matching.next().is_none().then_some(first)
    }

    fn selected_log(&self) -> Option<(PathBuf, Vec<NormalizedMessage>)> {
        for path in log_paths(&self.path) {
            let Some(value) = read_json(&path) else {
                continue;
            };
            if path
                .file_name()
                .is_some_and(|name| name == "api_conversation_history.json")
                && !value.as_array().is_some_and(|items| {
                    items
                        .iter()
                        .any(|item| item.get("role").and_then(Value::as_str).is_some())
                })
            {
                continue;
            }
            let preserve_native_content = path
                .file_name()
                .is_some_and(|name| name == "api_conversation_history.json");
            let parsed = messages(value, preserve_native_content);
            if !parsed.is_empty() {
                return Some((path, parsed));
            }
        }
        None
    }

    fn changed(&self, since: Option<i64>) -> bool {
        log_paths(&self.path)
            .into_iter()
            .chain([self.path.join("task_metadata.json")])
            .chain(self.history_path())
            .any(|path| path.is_file() && file_modified_since(&path, since))
    }

    fn conversation(&self) -> Option<NormalizedConversation> {
        let (source_path, messages) = self.selected_log()?;
        let metadata = read_json(&self.path.join("task_metadata.json"));
        let history = self.history_entry();
        let workspace = metadata
            .as_ref()
            .and_then(|value| {
                value
                    .get("rootPath")
                    .or_else(|| value.get("cwd"))
                    .or_else(|| value.get("workspace"))
            })
            .or_else(|| {
                history
                    .as_ref()
                    .and_then(|value| value.get("cwdOnTaskInitialization"))
            })
            .and_then(Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .map(PathBuf::from);
        let title = metadata
            .as_ref()
            .and_then(|value| value.get("title"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                messages
                    .first()
                    .and_then(|message| message.content.lines().next())
                    .map(|line| line.chars().take(100).collect())
            });
        Some(NormalizedConversation {
            agent_slug: "cline".to_owned(),
            external_id: self
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned),
            title,
            workspace,
            started_at: messages
                .iter()
                .filter_map(|message| message.created_at)
                .min(),
            ended_at: messages
                .iter()
                .filter_map(|message| message.created_at)
                .max(),
            metadata: json!({
                "source": "cline", "layout": "tasks",
                "history_log": source_path.file_name(),
                "coverage": if source_path.file_name().is_some_and(|name| name == "api_conversation_history.json") { "native_api_history" } else { "ui_events_only" },
                "task_history_path": self.history_path()
            }),
            source_path,
            messages,
        })
    }
}

impl ClineConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn collect_tasks(root: &ScanRoot, tasks: &mut Vec<Task>, seen: &mut HashSet<PathBuf>) {
        if root.path.is_file()
            && !root.path.file_name().is_some_and(|name| {
                LOG_NAMES.iter().any(|log| name == *log) || name == "task_metadata.json"
            })
        {
            return;
        }
        let base = if root.path.is_file() {
            root.path.parent().unwrap_or(&root.path)
        } else {
            &root.path
        };
        if modern_task(base) {
            let key = fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
            if seen.insert(key) {
                tasks.push(Task {
                    path: base.to_path_buf(),
                    root: root.clone(),
                });
            }
            return;
        }
        let mut containers = Vec::new();
        if base.file_name().is_some_and(|name| name == "tasks") {
            containers.push(base.to_path_buf());
        }
        for suffix in STORAGE_SUFFIXES {
            let storage = base.join(suffix);
            containers.push(storage.join("tasks"));
            containers.extend(EXTENSIONS.map(|name| storage.join(name).join("tasks")));
        }
        for container in containers {
            let Ok(entries) = fs::read_dir(&container) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !modern_task(&path) {
                    continue;
                }
                let key = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                if seen.insert(key) {
                    tasks.push(Task {
                        path,
                        root: root.clone(), // ubs:ignore -- Each discovered task retains the exact originating scan-root provenance.
                    });
                }
            }
        }
    }

    fn tasks(ctx: &ScanContext) -> Vec<Task> {
        let mut tasks = Vec::new();
        let mut seen = HashSet::new();
        if ctx.use_default_detection() {
            Self::collect_tasks(
                &ScanRoot::local(ctx.data_dir.clone()),
                &mut tasks,
                &mut seen,
            );
            if tasks.is_empty() {
                // Match the pinned parser's data-dir override: a legacy fixture or
                // mirrored store must not gain unrelated histories from this host.
                let mut discovery = ctx.clone();
                discovery.since_ts = None;
                let scoped_legacy = franken_agent_detection::ClineConnector::new()
                    .discover_source_files(&discovery)
                    .unwrap_or_default()
                    .iter()
                    .any(|source| source.source_path.starts_with(&ctx.data_dir));
                let scoped_cli =
                    configured_cli_root().is_some_and(|root| root.starts_with(&ctx.data_dir));
                if !scoped_legacy
                    && !scoped_cli
                    && let Some(home) = dirs::home_dir()
                {
                    Self::collect_tasks(&ScanRoot::local(home), &mut tasks, &mut seen);
                }
            }
        } else {
            for root in &ctx.scan_roots {
                Self::collect_tasks(root, &mut tasks, &mut seen);
            }
        }
        tasks.sort_by(|a, b| a.path.cmp(&b.path));
        tasks
    }
}

impl Connector for ClineConnector {
    fn detect(&self) -> DetectionResult {
        let mut result = franken_agent_detection::ClineConnector::new().detect();
        if let Some(root) = configured_cli_root().or_else(host_cli_root) {
            let context =
                ScanContext::with_roots(PathBuf::new(), vec![ScanRoot::local(root.clone())], None);
            if let Ok(sessions) = cli_sessions(&context)
                && !sessions.is_empty()
            {
                if !result.root_paths.contains(&root) {
                    result.root_paths.push(root.clone());
                }
                result.evidence.push(format!(
                    "{} standalone Cline CLI session source(s) under {}",
                    sessions.len(),
                    root.display()
                ));
                result.detected = true;
            }
        }
        result
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = franken_agent_detection::ClineConnector::new().scan(ctx)?;
        conversations.retain(|conversation| {
            !conversation.source_path.parent().is_some_and(modern_task)
                && !is_cli_messages_path(&conversation.source_path)
        });
        for task in Self::tasks(ctx) {
            if let Some(tick) = &ctx.progress_tick {
                tick();
            }
            if task.changed(ctx.since_ts)
                && let Some(conversation) = task.conversation()
            {
                conversations.push(conversation);
            }
        }
        for session in cli_sessions(ctx)? {
            if let Some(tick) = &ctx.progress_tick {
                tick();
            }
            if session.changed(ctx.since_ts) {
                conversations.push(session.conversation()?);
            }
        }
        Ok(conversations)
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let mut sources =
            franken_agent_detection::ClineConnector::new().discover_source_files(ctx)?;
        sources.retain(|source| {
            !source.source_path.parent().is_some_and(modern_task)
                && !is_cli_messages_path(&source.source_path)
        });
        for task in Self::tasks(ctx) {
            if !task.changed(ctx.since_ts) {
                continue;
            }
            let Some((primary, _)) = task.selected_log() else {
                continue;
            };
            sources.push(
                DiscoveredSourceFile::new(
                    "cline",
                    &task.root,
                    primary.clone(), // ubs:ignore -- The receipt owns its path; sidecar selection below also needs the primary path.
                    DiscoveredSourceRole::PrimarySessionLog,
                    true,
                )
                .with_fs_metadata(),
            );
            for path in log_paths(&task.path)
                .into_iter()
                .chain([task.path.join("task_metadata.json")])
                .chain(task.history_path())
            {
                if path != primary && path.is_file() {
                    let required = path.file_name().is_some_and(|name| {
                        name == "task_metadata.json" || name == "taskHistory.json"
                    });
                    sources.push(
                        DiscoveredSourceFile::new(
                            "cline",
                            &task.root,
                            path,
                            DiscoveredSourceRole::MetadataSidecar,
                            required,
                        )
                        .with_fs_metadata(),
                    );
                }
            }
        }
        for session in cli_sessions(ctx)? {
            if !session.changed(ctx.since_ts) {
                continue;
            }
            let CliSession {
                messages_path,
                metadata_path,
                root,
            } = session;
            sources.push(
                DiscoveredSourceFile::new(
                    "cline",
                    &root,
                    messages_path,
                    DiscoveredSourceRole::PrimarySessionLog,
                    true,
                )
                .with_fs_metadata(),
            );
            if metadata_path.is_file() {
                sources.push(
                    DiscoveredSourceFile::new(
                        "cline",
                        &root,
                        metadata_path,
                        DiscoveredSourceRole::MetadataSidecar,
                        false,
                    )
                    .with_fs_metadata(),
                );
            }
        }
        let mut seen = HashSet::new();
        sources.retain(|source| {
            let key = fs::canonicalize(&source.source_path)
                .unwrap_or_else(|_| source.source_path.clone());
            seen.insert(key)
        });
        Ok(sources)
    }
}

#[cfg(test)]
mod cli_root_tests {
    use super::{
        ClineConnector, Connector, ScanContext, ScanRoot, cli_scan_roots_with, collect_cli_sessions,
    };
    use anyhow::{Context, ensure};
    use serde_json::json;
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn write_native_session(root: &Path, session_id: &str) -> anyhow::Result<()> {
        let session = root.join(session_id);
        fs::create_dir_all(&session)?;
        fs::write(
            session.join(format!("{session_id}.messages.json")),
            serde_json::to_vec(&json!({
                "sessionId": session_id,
                "messages": [{"role": "user", "content": "fixture"}]
            }))?,
        )?;
        fs::write(
            session.join(format!("{session_id}.json")),
            serde_json::to_vec(&json!({"session_id": session_id}))?,
        )?;
        Ok(())
    }

    fn sessions_under(root: &ScanRoot) -> anyhow::Result<Vec<String>> {
        let mut sessions = Vec::new();
        let mut seen = HashSet::new();
        collect_cli_sessions(root, &mut sessions, &mut seen)?;
        sessions
            .into_iter()
            .map(|session| {
                session
                    .conversation()
                    .map(|conversation| conversation.external_id.unwrap_or_default())
            })
            .collect()
    }

    #[test]
    fn ordinary_custom_archive_uses_default_host_cli_root() -> anyhow::Result<()> {
        let home = TempDir::new()?;
        let archive = TempDir::new()?;
        let host_cli_root = home.path().join(".cline/data/sessions");
        write_native_session(&host_cli_root, "host-session")?;
        write_native_session(&archive.path().join("old-export"), "archive-session")?;

        let context = ScanContext::local_default(archive.path().to_path_buf(), None);
        let roots = cli_scan_roots_with(&context, None, Some(host_cli_root.clone()))?;
        ensure!(roots.len() == 1);
        let root = roots.first().context("missing selected Cline root")?;
        ensure!(root.path == host_cli_root);
        ensure!(sessions_under(root)? == ["host-session"]); // ubs:ignore -- Public fixture session ID checks discovery scope, not credentials.
        Ok(())
    }

    #[test]
    fn recognized_legacy_source_stays_scoped_with_default_host_cli_present() -> anyhow::Result<()> {
        let home = TempDir::new()?;
        let legacy = TempDir::new()?;
        let host_cli_root = home.path().join(".cline/data/sessions");
        write_native_session(&host_cli_root, "host-session")?;

        let legacy_root = legacy.path().join("saoudrizwan.claude-dev");
        let task = legacy_root.join("task-legacy");
        fs::create_dir_all(&task)?;
        fs::write(
            task.join("api_conversation_history.json"),
            serde_json::to_vec(&json!([
                {"role": "user", "content": "legacy fixture"}
            ]))?,
        )?;

        let context = ScanContext::local_default(legacy_root.clone(), None);
        let roots = cli_scan_roots_with(&context, None, Some(host_cli_root))?;
        ensure!(roots.len() == 1);
        let root = roots.first().context("missing selected Cline root")?;
        ensure!(root.path == legacy_root);
        ensure!(sessions_under(root)?.is_empty());

        let conversations = ClineConnector::new().scan(&context)?;
        ensure!(conversations.len() == 1);
        ensure!(
            conversations
                .first()
                .and_then(|conversation| conversation.messages.first())
                .is_some_and(|message| message.content.contains("legacy fixture"))
        );
        Ok(())
    }
}
