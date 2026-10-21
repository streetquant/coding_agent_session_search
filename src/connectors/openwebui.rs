use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation,
    NormalizedMessage, ScanContext, ScanRoot, parse_timestamp,
};

const CATALOG_SCHEMA: &str = "openwebui.capture-catalog.v1";
const SNAPSHOT_SCHEMA: &str = "openwebui.saved-conversation.v1";

#[derive(Default)]
pub struct OpenWebUiConnector;

#[derive(Deserialize)]
struct Catalog {
    schema_version: String,
    instance_id: String,
    workspace_root: String,
    sessions: Vec<CatalogEntry>,
    retained_missing: Vec<CatalogEntry>,
}

#[derive(Deserialize)]
struct CatalogEntry {
    session_key: String,
    conversation_id: String,
    user_id: Option<String>,
    content_sha256: String,
    projection: String,
}

struct CatalogSource {
    root: ScanRoot,
    path: PathBuf,
    session_key: Option<String>,
}

fn field<'value>(value: &'value Value, key: &str) -> &'value Value {
    value.get(key).unwrap_or(&Value::Null)
}

fn text<'value>(value: &'value Value, key: &str) -> Result<&'value str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .with_context(|| format!("OpenWebUI {key} missing"))
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|character| character.is_ascii_digit() || (b'a'..=b'f').contains(&character))
}

fn read_regular(path: &Path) -> Result<Vec<u8>> {
    ensure!(
        !path.components().any(|part| part == Component::ParentDir),
        "OpenWebUI source path traversal"
    );
    for ancestor in path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
    {
        ensure!(
            !fs::symlink_metadata(ancestor)?.file_type().is_symlink(),
            "OpenWebUI symlink source rejected"
        );
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "OpenWebUI source is not a regular file"
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn sorted_value(value: &Value) -> Value {
    match value {
        Value::Object(fields) => {
            let ordered: BTreeMap<_, _> = fields.iter().collect();
            Value::Object(
                ordered
                    .into_iter()
                    .map(|(key, value)| (key.to_owned(), sorted_value(value)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.iter().map(sorted_value).collect()),
        other => other.clone(),
    }
}

fn canonical_digest(value: &Value) -> Result<String> {
    let encoded = serde_json::to_string(&sorted_value(value))?;
    let mut ascii = String::with_capacity(encoded.len());
    for character in encoded.chars() {
        if character < '\u{7f}' {
            ascii.push(character);
        } else {
            let mut units = [0_u16; 2];
            for unit in character.encode_utf16(&mut units) {
                write!(ascii, "\\u{unit:04x}")?;
            }
        }
    }
    ascii.push('\n');
    Ok(hex::encode(Sha256::digest(ascii.as_bytes())))
}

fn same_public_session_key(left: &str, right: &str) -> bool {
    let matches = left == right; // ubs:ignore -- OpenWebUI session keys are public catalog selectors, not authentication material.
    matches
}

fn source_selection(path: &Path) -> Option<(PathBuf, Option<String>)> {
    if path
        .file_name()
        .is_some_and(|name| name == "catalog.json" || name == "instance.json")
    {
        return Some((path.parent()?.join("catalog.json"), None));
    }
    if path.file_name().is_some_and(|name| name == "snapshot.json") {
        let session = path.parent()?;
        let sessions = session.parent()?;
        if sessions.file_name().is_some_and(|name| name == "sessions") {
            return Some((
                sessions.parent()?.join("catalog.json"),
                Some(session.file_name()?.to_str()?.to_owned()),
            ));
        }
    }
    None
}

pub(crate) fn owns_source_path(path: &Path) -> bool {
    let Some((catalog, _)) = source_selection(path) else {
        return false;
    };
    let Some(parent) = catalog.parent() else {
        return false;
    };
    read_regular(&parent.join("instance.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .is_some_and(|instance| {
            field(&instance, "schema_version").as_str() == Some("openwebui.capture-instance.v1")
        })
}

impl OpenWebUiConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn default_root() -> PathBuf {
        dotenvy::var("CASS_OPENWEBUI_DATA_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/warp/cass/raw-mirror/openwebui/v1"))
    }

    fn sources(context: &ScanContext) -> Result<Vec<CatalogSource>> {
        let roots = if context.use_default_detection() {
            vec![ScanRoot::local(Self::default_root())]
        } else {
            context.scan_roots.clone()
        };
        let mut sources = Vec::new();
        let mut seen = HashSet::new();
        for root in roots {
            let mut candidates = Vec::new();
            if let Some(selected) = source_selection(&root.path) {
                candidates.push(selected);
            } else if root.path.join("catalog.json").is_file() {
                candidates.push((root.path.join("catalog.json"), None));
            } else if root.path.is_dir()
                && (root.path == Self::default_root() || root.path.ends_with("openwebui/v1"))
            {
                for child in fs::read_dir(&root.path)? {
                    let child = child?;
                    if child.file_type()?.is_dir() && child.path().join("catalog.json").is_file() {
                        candidates.push((child.path().join("catalog.json"), None));
                    }
                }
            }
            for (path, session_key) in candidates {
                let instance_path = path.parent().map(|parent| parent.join("instance.json"));
                let candidate = owns_source_path(&path)
                    || instance_path
                        .is_some_and(|instance| instance.exists() || instance.is_symlink());
                if candidate && seen.insert((path.clone(), session_key.clone())) {
                    sources.push(CatalogSource {
                        root: root.clone(),
                        path,
                        session_key,
                    });
                }
            }
        }
        sources.sort_by(|left, right| {
            (&left.path, &left.session_key).cmp(&(&right.path, &right.session_key))
        });
        Ok(sources)
    }

    fn catalog(source: &CatalogSource) -> Result<(Catalog, Value, Vec<u8>)> {
        let bytes = read_regular(&source.path)?;
        let catalog: Catalog = serde_json::from_slice(&bytes)?;
        ensure!(
            catalog.schema_version == CATALOG_SCHEMA,
            "unsupported OpenWebUI catalog schema"
        );
        ensure!(
            !catalog.instance_id.is_empty(),
            "OpenWebUI instance identity missing"
        );
        ensure!(
            Path::new(&catalog.workspace_root).is_absolute(),
            "OpenWebUI workspace must be absolute"
        );
        let directory = source
            .path
            .parent()
            .context("OpenWebUI catalog directory missing")?;
        let instance: Value =
            serde_json::from_slice(&read_regular(&directory.join("instance.json"))?)?;
        ensure!(
            text(&instance, "schema_version")? == "openwebui.capture-instance.v1"
                && text(&instance, "instance_id")? == catalog.instance_id
                && text(&instance, "workspace_root")? == catalog.workspace_root,
            "OpenWebUI instance identity mismatch"
        );
        let mut keys = HashSet::new();
        let mut identifiers = HashSet::new();
        for entry in catalog.sessions.iter().chain(&catalog.retained_missing) {
            ensure!(
                valid_hash(&entry.session_key) && valid_hash(&entry.content_sha256),
                "invalid OpenWebUI revision path hash"
            );
            ensure!(
                keys.insert(&entry.session_key) && identifiers.insert(&entry.conversation_id),
                "duplicate OpenWebUI conversation identity"
            );
            ensure!(
                canonical_digest(&json!(entry.conversation_id))? == entry.session_key, // ubs:ignore -- These are deterministic public catalog identifiers, not authentication secrets.
                "OpenWebUI session key mismatch"
            );
        }
        if let Some(selected) = &source.session_key {
            ensure!(
                keys.contains(selected),
                "OpenWebUI selected session is not committed in catalog"
            );
        }
        Ok((catalog, instance, bytes))
    }

    fn parse(
        source: &CatalogSource,
        catalog: &Catalog,
        instance: &Value,
        catalog_bytes: &[u8],
        entry: &CatalogEntry,
        source_present: bool,
    ) -> Result<NormalizedConversation> {
        ensure!(
            entry.projection != "quarantined",
            "OpenWebUI conversation is quarantined; raw evidence retained"
        );
        let owner = entry
            .user_id
            .as_deref()
            .filter(|owner| !owner.is_empty())
            .context("OpenWebUI owner missing")?;
        let directory = source
            .path
            .parent()
            .context("OpenWebUI catalog directory missing")?
            .join("sessions")
            .join(&entry.session_key);
        let snapshot_path = directory.join("snapshot.json");
        let revision_path = directory
            .join("revisions")
            .join(format!("{}.json", entry.content_sha256));
        let bytes = read_regular(&revision_path)?;
        ensure!(
            hex::encode(Sha256::digest(&bytes)) == entry.content_sha256,
            "OpenWebUI revision digest mismatch"
        );
        ensure!(
            read_regular(&snapshot_path)? == bytes,
            "OpenWebUI snapshot publication in progress; retry after catalog commit"
        );
        let snapshot: Value = serde_json::from_slice(&bytes)?;
        ensure!(
            text(&snapshot, "schema_version")? == SNAPSHOT_SCHEMA
                && text(&snapshot, "agent")? == "openwebui"
                && text(&snapshot, "instance_id")? == catalog.instance_id
                && text(&snapshot, "conversation_id")? == entry.conversation_id
                && text(&snapshot, "user_id")? == owner
                && text(&snapshot, "workspace_root")? == catalog.workspace_root
                && field(&snapshot, "source_database") == field(instance, "source_database"),
            "OpenWebUI snapshot identity mismatch"
        );
        let raw = field(&snapshot, "source");
        ensure!(
            canonical_digest(raw)? == text(&snapshot, "source_rows_sha256")?,
            "OpenWebUI source rows digest mismatch"
        );
        let chat = field(raw, "chat");
        ensure!(
            text(chat, "id")? == entry.conversation_id && text(chat, "user_id")? == owner,
            "OpenWebUI source chat ownership mismatch"
        );
        for table in ["chat_message", "chat_file"] {
            let rows = field(raw, table)
                .as_array()
                .context("OpenWebUI source rows missing")?;
            for row in rows {
                ensure!(
                    text(row, "user_id")? == owner
                        && text(row, "chat_id")? == entry.conversation_id,
                    "OpenWebUI source row ownership mismatch"
                );
            }
        }
        let coverage = field(&snapshot, "coverage");
        ensure!(
            text(coverage, "capture")? == "saved_database_snapshot"
                && ["ready", "partial"].contains(&entry.projection.as_str())
                && text(coverage, "projection")? == entry.projection
                && field(coverage, "conversation_complete").is_null(),
            "OpenWebUI coverage claim is unsupported"
        );
        let projection = field(&snapshot, "projection");
        let projected = field(projection, "messages")
            .as_array()
            .context("OpenWebUI projected messages missing")?;
        let mut seen = HashSet::new();
        let mut messages = Vec::new();
        for projected_message in projected {
            let identifier = text(projected_message, "id")?;
            ensure!(
                seen.insert(identifier),
                "duplicate OpenWebUI message identity"
            );
            let message = field(projected_message, "message");
            ensure!(
                text(message, "id")? == identifier,
                "OpenWebUI projected message identity mismatch"
            );
            let role = text(message, "role")?;
            ensure!(
                ["user", "assistant", "tool", "function", "system"].contains(&role),
                "unsupported OpenWebUI message role"
            );
            let mut content = match field(message, "content") {
                Value::String(content) => content.clone(),
                Value::Null => String::new(),
                content => serde_json::to_string(content)?,
            };
            for name in ["output", "tool_calls", "error"] {
                if let Some(value) = message.get(name).filter(|value| !value.is_null()) {
                    write!(content, "\n{name}: {}", serde_json::to_string(value)?)?;
                }
            }
            messages.push(NormalizedMessage {
                idx: i64::try_from(messages.len())?,
                role: role.to_owned(),
                author: if role == "user" { Some(owner.to_owned()) } else { message.get("model").and_then(Value::as_str).map(str::to_owned) },
                created_at: message.get("timestamp").and_then(parse_timestamp),
                content,
                extra: json!({"openwebui": projected_message, "user_id": owner, "instance_id": catalog.instance_id, "conversation_id": entry.conversation_id}),
                snippets: Vec::new(),
                invocations: Vec::new(),
            });
        }
        ensure!(
            read_regular(&source.path)? == catalog_bytes,
            "OpenWebUI catalog changed during read; retry"
        );
        ensure!(
            read_regular(&snapshot_path)? == bytes,
            "OpenWebUI snapshot changed during read; retry"
        );
        Ok(NormalizedConversation {
            agent_slug: "openwebui".to_owned(),
            external_id: Some(serde_json::to_string(&[
                &catalog.instance_id,
                &entry.conversation_id,
            ])?),
            title: chat.get("title").and_then(Value::as_str).map(str::to_owned),
            workspace: Some(PathBuf::from(
                source
                    .root
                    .rewrite_workspace(&catalog.workspace_root, Some("openwebui")),
            )),
            source_path: snapshot_path,
            started_at: chat.get("created_at").and_then(parse_timestamp),
            ended_at: None,
            metadata: json!({
                "schema_version": SNAPSHOT_SCHEMA, "source": "openwebui",
                "instance_id": catalog.instance_id, "conversation_id": entry.conversation_id,
                "user_id": owner, "workspace_root": catalog.workspace_root,
                "source_database": field(&snapshot, "source_database"),
                "source_rows_sha256": field(&snapshot, "source_rows_sha256"),
                "content_hash": entry.content_sha256, "revision_path": revision_path,
                "catalog_path": source.path, "catalog_sha256": hex::encode(Sha256::digest(catalog_bytes)),
                "source_present": source_present, "saved_state_coverage": coverage,
                "coverage": "partial", "coverage_source": "saved_database_snapshot",
                "authority": "historical_evidence_only", "all_transcript_complete": false,
                "current_message_id": field(projection, "current_message_id"),
                "issues": field(projection, "issues"),
                "retained_legacy_only_ids": field(projection, "retained_legacy_only_ids"),
                "origin": source.root.origin,
            }),
            messages,
        })
    }
}

impl Connector for OpenWebUiConnector {
    fn detect(&self) -> DetectionResult {
        let root = Self::default_root();
        let detected = root.is_dir();
        DetectionResult {
            detected,
            evidence: if detected {
                vec![root.display().to_string()]
            } else {
                Vec::new()
            },
            root_paths: if detected { vec![root] } else { Vec::new() },
        }
    }

    fn scan(&self, context: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = Vec::new();
        self.scan_with_callback(context, &mut |conversation| {
            conversations.push(conversation);
            Ok(())
        })?;
        Ok(conversations)
    }

    fn supports_streaming_scan(&self) -> bool {
        true
    }

    fn scan_with_callback(
        &self,
        context: &ScanContext,
        emit: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        let mut seen = HashSet::new();
        for source in Self::sources(context)? {
            let (catalog, instance, bytes) = Self::catalog(&source)?;
            for (entries, present) in [
                (&catalog.sessions, true),
                (&catalog.retained_missing, false),
            ] {
                for entry in entries {
                    if source
                        .session_key
                        .as_ref()
                        .is_some_and(|key| !same_public_session_key(key, &entry.session_key))
                    {
                        continue;
                    }
                    if seen.insert((
                        source.root.origin.source_id.clone(),
                        catalog.instance_id.clone(),
                        entry.conversation_id.clone(),
                    )) {
                        emit(Self::parse(
                            &source, &catalog, &instance, &bytes, entry, present,
                        )?)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn discover_source_files(&self, context: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let mut files = Vec::new();
        let mut seen = HashSet::new();
        for source in Self::sources(context)? {
            let (catalog, _, _) = Self::catalog(&source)?;
            let directory = source
                .path
                .parent()
                .context("OpenWebUI catalog directory missing")?;
            let mut paths = vec![
                (source.path.clone(), DiscoveredSourceRole::MetadataSidecar),
                (
                    directory.join("instance.json"),
                    DiscoveredSourceRole::MetadataSidecar,
                ),
            ];
            for entry in catalog.sessions.iter().chain(&catalog.retained_missing) {
                if source
                    .session_key
                    .as_ref()
                    .is_some_and(|key| !same_public_session_key(key, &entry.session_key))
                {
                    continue;
                }
                let session = directory.join("sessions").join(&entry.session_key);
                paths.push((
                    session.join("snapshot.json"),
                    DiscoveredSourceRole::PrimarySessionLog,
                ));
                paths.push((
                    session
                        .join("revisions")
                        .join(format!("{}.json", entry.content_sha256)),
                    DiscoveredSourceRole::MetadataSidecar,
                ));
            }
            for (path, role) in paths {
                if seen.insert(path.clone()) {
                    files.push(
                        DiscoveredSourceFile::new("openwebui", &source.root, path, role, true)
                            .with_fs_metadata(),
                    );
                }
            }
        }
        Ok(files)
    }
}
