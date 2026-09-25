//! Preserve the pinned legacy parser and support Cline's current `tasks/<id>` layout.
//! Modern UI files are presentation events; the API history carries native roles.

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation,
    NormalizedMessage, ScanContext, ScanRoot, file_modified_since, flatten_content,
    parse_timestamp, reindex_messages,
};

const MAX_HISTORY_BYTES: u64 = 100 * 1024 * 1024;
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

fn messages(value: Value) -> Vec<NormalizedMessage> {
    let Value::Array(items) = value else {
        return Vec::new();
    };
    let mut result = Vec::with_capacity(items.len());
    for item in items {
        let content_value = item
            .get("content")
            .or_else(|| item.get("text"))
            .or_else(|| item.get("message"));
        let content = content_value.map(flatten_content).unwrap_or_default();
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
            let parsed = messages(value);
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
                if !scoped_legacy && let Some(home) = dirs::home_dir() {
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
        franken_agent_detection::ClineConnector::new().detect()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = franken_agent_detection::ClineConnector::new().scan(ctx)?;
        conversations
            .retain(|conversation| !conversation.source_path.parent().is_some_and(modern_task));
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
        Ok(conversations)
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let mut sources =
            franken_agent_detection::ClineConnector::new().discover_source_files(ctx)?;
        sources.retain(|source| !source.source_path.parent().is_some_and(modern_task));
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
        let mut seen = HashSet::new();
        sources.retain(|source| seen.insert(source.source_path.clone()));
        Ok(sources)
    }
}
