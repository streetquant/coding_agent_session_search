//! Native Kilo CLI/IDE history. Never fall back to the OpenCode or Cline stores.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation,
    NormalizedMessage, ScanContext, ScanRoot, flatten_content, parse_timestamp,
};
use crate::franken_sync::compat::{ConnectionExt, OpenFlags, RowExt, open_with_flags};

#[derive(Default)]
pub struct KiloConnector;

fn json_field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or(&Value::Null)
}

fn append_part_content(content: &mut String, part: &Value) {
    if let Some(text) = json_field(part, "text").as_str() {
        if text.is_empty() {
            return;
        }
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(text);
        return;
    }

    if json_field(part, "type").as_str() == Some("tool") {
        if !content.is_empty() {
            content.push('\n');
        }
        let tool = json_field(part, "tool").as_str().unwrap_or("tool");
        let state = json_field(part, "state");
        let input = json_field(state, "input");
        let output = json_field(state, "output").as_str().unwrap_or("");
        // String's fmt::Write implementation is infallible; this reuses the
        // destination buffer instead of allocating a formatted String per part.
        let _ = write!(content, "{tool}\n{input}\n{output}");
    } else if json_field(part, "type").as_str() == Some("file") {
        if !content.is_empty() {
            content.push('\n');
        }
        let filename = json_field(part, "filename").as_str().unwrap_or("file");
        content.push_str("[attachment: ");
        content.push_str(filename);
        content.push(']');
    }
}

impl KiloConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn defaults() -> Vec<PathBuf> {
        if let Ok(root) = dotenvy::var("CASS_KILO_DATA_ROOT") {
            return vec![PathBuf::from(root)];
        }
        let Some(home) = dirs::home_dir() else {
            return Vec::new();
        };
        let data = dotenvy::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".local/share"));
        let mut roots = vec![data.join("kilo/kilo.db")];
        for editor in ["Code", "Code - Insiders", "VSCodium", "Cursor"] {
            for (base, suffix) in [
                (".config", "User/globalStorage/kilocode.kilo-code/tasks"),
                (
                    "Library/Application Support",
                    "User/globalStorage/kilocode.kilo-code/tasks",
                ),
                (
                    "AppData/Roaming",
                    "User/globalStorage/kilocode.kilo-code/tasks",
                ),
            ] {
                let mut root = home.join(base);
                root.push(editor);
                root.push(suffix);
                roots.push(root);
            }
        }
        roots
    }

    fn sources(ctx: &ScanContext) -> Result<Vec<(ScanRoot, PathBuf)>> {
        let roots = if ctx.use_default_detection() {
            Self::defaults().into_iter().map(ScanRoot::local).collect()
        } else {
            ctx.scan_roots.clone()
        };
        let mut sources = Vec::new();
        let mut seen = HashSet::new();
        for root in roots {
            let path = &root.path;
            let mut candidates = Vec::new();
            if path.file_name().is_some_and(|n| n == "kilo.db") {
                candidates.push(path.clone()); // ubs:ignore -- Candidate ownership is needed while the original scan root remains provenance.
            } else if path.is_dir() {
                candidates.push(path.join("kilo.db"));
                candidates.push(path.join("kilo/kilo.db"));
                // Only explicitly named Kilo stores are admitted. A broad shared
                // root must not cause another provider's task logs to be relabelled.
                let is_ide = path
                    .components()
                    .any(|c| c.as_os_str() == "kilocode.kilo-code");
                if is_ide {
                    let tasks = if path.join("tasks").is_dir() {
                        path.join("tasks")
                    } else {
                        path.clone() // ubs:ignore -- The candidate path is owned while the scan root remains provenance.
                    };
                    if tasks.join("api_conversation_history.json").is_file() {
                        candidates.push(tasks.join("api_conversation_history.json"));
                    } else if tasks.is_dir() {
                        for task in fs::read_dir(&tasks)? {
                            let task = task?;
                            if !task.file_type()?.is_dir() {
                                continue;
                            }
                            let api = task.path().join("api_conversation_history.json");
                            candidates.push(if api.is_file() {
                                api
                            } else {
                                task.path().join("ui_messages.json")
                            });
                        }
                    }
                }
            } else if path
                .components()
                .any(|c| c.as_os_str() == "kilocode.kilo-code")
                && path.file_name().is_some_and(|n| {
                    n == "api_conversation_history.json" || n == "ui_messages.json"
                })
            {
                candidates.push(path.clone()); // ubs:ignore -- Candidate ownership is needed while the original scan root remains provenance.
            }
            for path in candidates {
                if !path.is_file() {
                    continue;
                }
                let canonical = fs::canonicalize(&path)?;
                let canonical_key = canonical.clone(); // ubs:ignore -- Dedupe and output both retain the canonical path.
                if seen.insert(canonical_key) {
                    let source_root = root.clone(); // ubs:ignore -- Each source retains its originating scan-root provenance.
                    sources.push((source_root, canonical));
                }
            }
        }
        sources.sort_by(|a, b| a.1.cmp(&b.1));
        Ok(sources)
    }

    fn database(
        root: &ScanRoot,
        path: &Path,
        ctx: &ScanContext,
        emit: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        // Never recover/checkpoint or modify an agent-owned database.
        let db = open_with_flags(&path.to_string_lossy(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        db.execute("BEGIN")?;
        let sessions = db.query_map_collect(
            "SELECT id, directory, title, time_created, time_updated FROM session ORDER BY id",
            &[],
            |r| {
                Ok((
                    r.get_typed::<String>(0)?,
                    r.get_typed::<Option<String>>(1)?,
                    r.get_typed::<Option<String>>(2)?,
                    r.get_typed::<Option<i64>>(3)?,
                    r.get_typed::<Option<i64>>(4)?,
                ))
            },
        )?;
        // Read each native table once in the same snapshot. Per-message part
        // queries repeatedly traverse large session histories on SQLite
        // implementations that do not use the compound index for this shape.
        let message_rows = db.query_map_collect(
            "SELECT session_id, id, time_created, data FROM message ORDER BY session_id, time_created, id",
            &[],
            |r| Ok((r.get_typed::<String>(0)?, r.get_typed::<String>(1)?,
                r.get_typed::<Option<i64>>(2)?, r.get_typed::<String>(3)?)),
        )?;
        let mut messages_by_session: HashMap<String, Vec<_>> = HashMap::new();
        for (session, message, time, data) in message_rows {
            messages_by_session
                .entry(session)
                .or_default()
                .push((message, time, data));
        }
        let part_rows = db.query_map_collect(
            "SELECT session_id, message_id, data FROM part ORDER BY session_id, message_id, time_created, id",
            &[],
            |r| Ok((r.get_typed::<String>(0)?, r.get_typed::<String>(1)?, r.get_typed::<String>(2)?)),
        )?;
        let mut parts_by_session: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
        for (session, message, data) in part_rows {
            parts_by_session
                .entry(session)
                .or_default()
                .entry(message)
                .or_default()
                .push(data);
        }
        for (id, workspace, title, started_at, ended_at) in sessions {
            if ctx
                .since_ts
                .is_some_and(|since| ended_at.is_some_and(|end| end < since))
            {
                continue;
            }
            let rows = messages_by_session.remove(&id).unwrap_or_default();
            let mut session_parts = parts_by_session.remove(&id).unwrap_or_default();
            let mut messages = Vec::new();
            for (message_id, created_at, raw) in rows {
                let mut extra: Value = serde_json::from_str(&raw).context("Kilo message JSON")?;
                ensure!(extra.is_object(), "Kilo message must be a JSON object");
                let role = json_field(&extra, "role")
                    .as_str()
                    .context("Kilo message role missing")?
                    .to_string(); // ubs:ignore -- NormalizedMessage owns role while preserving the source JSON in extra.
                let parts = session_parts
                    .remove(&message_id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|s| serde_json::from_str::<Value>(&s))
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                let mut content = String::new();
                for part in &parts {
                    append_part_content(&mut content, part);
                }
                let extra_fields = extra
                    .as_object_mut()
                    .context("Kilo message must be a JSON object")?;
                extra_fields.insert("native_message_id".to_owned(), json!(message_id)); // ubs:ignore -- The JSON object owns this key to preserve each native message identity.
                extra_fields.insert("parts".to_owned(), json!(parts)); // ubs:ignore -- The JSON object owns this key to preserve the original provider parts.
                messages.push(NormalizedMessage {
                    idx: messages.len() as i64,
                    role,
                    author: None,
                    created_at,
                    content,
                    extra,
                    snippets: vec![],
                    invocations: vec![],
                });
            }
            if messages.is_empty() {
                continue;
            }
            let content_hash = hex::encode(Sha256::digest(serde_json::to_vec(&messages)?));
            emit(NormalizedConversation {
                agent_slug: "kilo".into(),
                external_id: Some(format!("cli:{id}")), // ubs:ignore -- This owned ID is required for native session identity and dedupe.
                title,
                workspace: workspace
                    .filter(|s| !s.is_empty())
                    .map(|s| PathBuf::from(root.rewrite_workspace(&s, Some("kilo")))),
                source_path: path.to_path_buf(),
                started_at,
                ended_at,
                metadata: json!({"source":"kilo", "surface":"cli", "native_session_id":id,
                    "content_hash":content_hash,"coverage":"native_store", "origin":root.origin}),
                messages,
            })?;
        }
        db.execute("ROLLBACK")?;
        Ok(())
    }

    fn ide(root: &ScanRoot, path: &Path) -> Result<NormalizedConversation> {
        let bytes = fs::read(path)?;
        let rows: Vec<Value> = serde_json::from_slice(&bytes).context("Kilo IDE history JSON")?;
        let task = path.parent().context("Kilo task parent")?;
        let id = task.file_name().context("Kilo task ID")?.to_string_lossy();
        let meta_path = task.join("task_metadata.json");
        let meta: Value = if meta_path.is_file() {
            serde_json::from_slice(&fs::read(meta_path)?)?
        } else {
            json!({})
        };
        let api = path
            .file_name()
            .is_some_and(|n| n == "api_conversation_history.json");
        let messages = rows
            .into_iter()
            .enumerate()
            .map(|(idx, extra)| {
                let role = json_field(&extra, "role")
                    .as_str()
                    .unwrap_or_else(|| {
                        if json_field(&extra, "say").as_str() == Some("user_feedback") {
                            "user"
                        } else {
                            "assistant"
                        }
                    })
                    .to_string(); // ubs:ignore -- NormalizedMessage owns role alongside the original IDE row.
                let content = if api {
                    flatten_content(json_field(&extra, "content"))
                } else {
                    json_field(&extra, "text")
                        .as_str()
                        .unwrap_or("")
                        .to_string() // ubs:ignore -- NormalizedMessage owns text alongside the original IDE row.
                };
                let created_at = extra
                    .get("ts")
                    .or_else(|| extra.get("timestamp"))
                    .and_then(parse_timestamp);
                NormalizedMessage {
                    idx: idx as i64,
                    role,
                    author: None,
                    created_at,
                    content,
                    extra,
                    snippets: vec![],
                    invocations: vec![],
                }
            })
            .collect::<Vec<_>>();
        let workspace = ["workspace", "cwd", "rootPath"]
            .iter()
            .find_map(|k| json_field(&meta, k).as_str())
            .map(|s| PathBuf::from(root.rewrite_workspace(s, Some("kilo"))));
        ensure!(!id.is_empty(), "empty Kilo IDE task ID");
        let workspace_observed = workspace.is_some();
        Ok(NormalizedConversation {
            agent_slug: "kilo".into(),
            external_id: Some(format!("ide:{id}")),
            title: json_field(&meta, "title").as_str().map(str::to_owned),
            workspace,
            source_path: path.to_path_buf(),
            started_at: messages.iter().filter_map(|m| m.created_at).min(),
            ended_at: messages.iter().filter_map(|m| m.created_at).max(),
            metadata: json!({"source":"kilo","surface":"ide","native_session_id":id,
                "content_hash":hex::encode(Sha256::digest(&bytes)),"coverage":if api {"native_api_history"} else {"ui_only"},
                "workspace_observed":workspace_observed,"origin":root.origin}),
            messages,
        })
    }
}

impl Connector for KiloConnector {
    fn detect(&self) -> DetectionResult {
        let roots = Self::defaults()
            .into_iter()
            .filter(|p| p.exists())
            .collect::<Vec<_>>();
        DetectionResult {
            detected: !roots.is_empty(),
            evidence: roots.iter().map(|p| p.display().to_string()).collect(),
            root_paths: roots,
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
            if path.file_name().is_some_and(|n| n == "kilo.db") {
                Self::database(&root, &path, ctx, emit)?;
            } else {
                emit(Self::ide(&root, &path)?)?;
            }
        }
        Ok(())
    }
    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let mut out = Vec::new();
        for (root, path) in Self::sources(ctx)? {
            let db = path.file_name().is_some_and(|n| n == "kilo.db");
            let sidecars = if db {
                vec![PathBuf::from(format!("{}-wal", path.display()))] // ubs:ignore -- Discovery owns one WAL sidecar path per database for filesystem metadata checks.
            } else {
                vec![
                    path.parent()
                        .context("Kilo source parent missing")?
                        .join("task_metadata.json"),
                ]
            };
            out.push(
                DiscoveredSourceFile::new(
                    "kilo",
                    &root,
                    path,
                    if db {
                        DiscoveredSourceRole::SqliteDatabase
                    } else {
                        DiscoveredSourceRole::PrimarySessionLog
                    },
                    true,
                )
                .with_fs_metadata(),
            );
            for sidecar in sidecars.into_iter().filter(|p| p.is_file()) {
                out.push(
                    DiscoveredSourceFile::new(
                        "kilo",
                        &root,
                        sidecar,
                        DiscoveredSourceRole::MetadataSidecar,
                        true,
                    )
                    .with_fs_metadata(),
                );
            }
        }
        Ok(out)
    }
}
