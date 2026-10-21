//! Native connector -> canonical archive -> exact CLI, including append replay.
//! All sources and the archive are disposable; no host discovery is performed.

use anyhow::{Context, Result, ensure};
use assert_cmd::Command;
use coding_agent_search::franken_sync::Connection;
use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
use coding_agent_search::franken_sync::params;
use coding_agent_search::model::types::{Conversation, Message, MessageRole};
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::{Value, json};
use std::fs::{self, File, FileTimes};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

const CLINE_ID: &str = "native-cline-acceptance";
const OPENCODE_ID: &str = "ses_native_acceptance";

struct Archive {
    temporary: TempDir,
    data: PathBuf,
    workspace: PathBuf,
    cline: PathBuf,
    opencode: PathBuf,
    timestamp: i64,
}

impl Archive {
    fn new() -> Result<Self> {
        let temporary = TempDir::new()?;
        let data = temporary.path().join("cass");
        let workspace = temporary.path().join("project");
        let cline = temporary.path().join("cline");
        let opencode = temporary.path().join("opencode2.db");
        for directory in [&data, &workspace, &cline] {
            fs::create_dir(directory)?;
        }
        // Fixture timestamps stay ahead of incremental scan watermarks so each
        // replay reparses the same sources instead of merely skipping discovery.
        let timestamp = chrono::Utc::now().timestamp_millis() + 60_000;
        Ok(Self {
            temporary,
            data,
            workspace,
            cline,
            opencode,
            timestamp,
        })
    }

    fn database(&self) -> PathBuf {
        self.data.join("agent_search.db")
    }

    fn cline_path(&self) -> PathBuf {
        self.cline
            .join(CLINE_ID)
            .join(format!("{CLINE_ID}.messages.json"))
    }

    fn write_cline(&self, messages: &[Value]) -> Result<()> {
        let directory = self.cline.join(CLINE_ID);
        fs::create_dir_all(&directory)?;
        fs::write(
            directory.join(format!("{CLINE_ID}.json")),
            serde_json::to_vec(&json!({"session_id":CLINE_ID,
                "workspace_root":self.workspace,"created_at":self.timestamp - 1000}))?,
        )?;
        fs::write(
            self.cline_path(),
            serde_json::to_vec(&json!({"sessionId":CLINE_ID,"version":1,
                "agent":"cline-cli","messages":messages,"updated_at":self.timestamp}))?,
        )?;
        let modified =
            SystemTime::UNIX_EPOCH + Duration::from_millis(u64::try_from(self.timestamp)?);
        File::open(self.cline_path())?.set_times(FileTimes::new().set_modified(modified))?;
        Ok(())
    }

    fn create_opencode(&self) -> Result<()> {
        let db = Connection::open(self.opencode.to_string_lossy().as_ref())?;
        db.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY,directory TEXT NOT NULL,title TEXT,
                 time_created INTEGER NOT NULL,time_updated INTEGER NOT NULL);
             CREATE TABLE session_message (id TEXT PRIMARY KEY,session_id TEXT NOT NULL,
                 type TEXT NOT NULL,seq INTEGER NOT NULL,time_created INTEGER NOT NULL,
                 time_updated INTEGER NOT NULL,data TEXT NOT NULL);",
        )?;
        db.execute_compat(
            "INSERT INTO session_v2 VALUES (?1,?2,'Native acceptance',?3,?3)",
            params![
                OPENCODE_ID,
                self.workspace.to_string_lossy().as_ref(),
                self.timestamp
            ],
        )?;
        db.close()?;
        Ok(())
    }

    fn append_opencode(&self, sequence: i64, role: &str, payload: &Value) -> Result<()> {
        let db = Connection::open(self.opencode.to_string_lossy().as_ref())?;
        db.execute_compat(
            "INSERT INTO session_message VALUES (?1,?2,?3,?4,?5,?5,?6)",
            params![
                format!("msg_{sequence}"),
                OPENCODE_ID,
                role,
                sequence,
                self.timestamp + sequence,
                payload.to_string()
            ],
        )?;
        db.close()?;
        Ok(())
    }

    fn command(&self) -> Command {
        // Upstream OpenCode intentionally discovers local HOME/XDG databases
        // even for explicit local roots. Isolate every actual CLI invocation;
        // do not mutate the parent libtest environment or fake a remote origin.
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass")); // ubs:ignore -- Cargo selects this test executable; fixture input is argv and cannot select a program.
        self.environment(&mut command);
        command.arg("--db").arg(self.database());
        command
    }

    fn environment(&self, command: &mut Command) {
        command
            .current_dir(self.temporary.path())
            .env_clear()
            .env("HOME", self.temporary.path())
            .env("XDG_DATA_HOME", self.temporary.path().join("xdg-data"))
            .env("XDG_CONFIG_HOME", self.temporary.path().join("xdg-config"))
            .env("XDG_CACHE_HOME", self.temporary.path().join("xdg-cache"))
            .env("TMPDIR", self.temporary.path())
            .env("RUST_MIN_STACK", "134217728")
            .env("CASS_DATA_DIR", &self.data)
            .env("CASS_CLINE_DATA_ROOT", &self.cline)
            .env("CASS_OPENCODE2_DB", &self.opencode)
            .env(
                "CASS_CLOUDMCP_DATA_ROOT",
                self.temporary.path().join("empty-cloudmcp"),
            )
            .env(
                "CASS_OPENWEBUI_DATA_ROOT",
                self.temporary.path().join("empty-openwebui"),
            )
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_AUTO_REFRESH", "0")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("NO_COLOR", "1");
    }

    fn ee(&self, binary: &Path, caller: &str, args: &[&str]) -> Result<Value> {
        let mut command = Command::new(binary); // ubs:ignore -- Explicit opt-in acceptance candidate from the test operator; no native conversation can choose this executable.
        self.environment(&mut command);
        let output = command
            .env("EE_CASS_BINARY", assert_cmd::cargo::cargo_bin!("cass"))
            .env("EE_EMBED_DOWNLOAD", "off")
            .env("EE_AGENT_NAME", caller)
            .arg("--workspace")
            .arg(&self.workspace)
            .arg("--json")
            .args(args)
            .output()?;
        ensure!(
            output.status.success(),
            "native EE CLI failed for {args:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let body: Value = serde_json::from_slice(&output.stdout)?;
        ensure!(body.get("success").and_then(Value::as_bool) == Some(true));
        body.get("data")
            .cloned()
            .context("EE response data missing")
    }

    fn ee_import(&self, binary: &Path, agent: &str, native_id: &str) -> Result<Value> {
        self.ee(
            binary,
            "native-acceptance-importer",
            &[
                "import",
                "cass",
                "--exact-source-id",
                "local",
                "--exact-agent",
                agent,
                "--exact-external-id",
                native_id,
            ],
        )
    }

    fn index(&self) -> Result<()> {
        // Use the shipped CLI entrypoint, including its real subprocess WAL
        // finalization protocol; a libtest ELF cannot act as that worker.
        let output = self
            .command()
            .args(["index", "--json", "--no-progress-events", "--data-dir"])
            .arg(&self.data)
            .output()?;
        ensure!(
            output.status.success(),
            "native index CLI failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    fn archived(&self, agent: &str, native_id: &str) -> Result<(Conversation, Vec<Message>)> {
        let storage = FrankenStorage::open_readonly(&self.database())?;
        let conversations = storage.list_conversations(10, 0)?;
        ensure!(
            conversations.len() == 2,
            "expected two fixture conversations, found {} (agents: {:?})",
            conversations.len(),
            conversations
                .iter()
                .map(|item| &item.agent_slug)
                .collect::<Vec<_>>()
        );
        let mut selected = conversations.into_iter().filter(|item| {
            item.agent_slug == agent && item.external_id.as_deref() == Some(native_id)
        });
        let conversation = selected
            .next()
            .context("native identity missing from archive")?;
        ensure!(selected.next().is_none(), "duplicate native identity");
        ensure!(conversation.source_id == "local");
        ensure!(conversation.workspace.as_deref() == Some(self.workspace.as_path()));
        let messages = storage.fetch_messages(conversation.id.context("archive ID missing")?)?;
        Ok((conversation, messages))
    }

    fn exact_cli(&self, agent: &str, native_id: &str, expected: usize) -> Result<()> {
        let output = self
            .command()
            .args([
                "sessions",
                "--json",
                "--exact-source-id",
                "local",
                "--exact-agent",
                agent,
                "--exact-external-id",
                native_id,
            ])
            .output()?;
        ensure!(
            output.status.success(),
            "exact CLI failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let body: Value = serde_json::from_slice(&output.stdout)?;
        let sessions = body
            .get("sessions")
            .and_then(Value::as_array)
            .context("sessions missing")?;
        ensure!(
            sessions.len() == 1,
            "exact selector returned duplicate sessions"
        );
        let session = sessions.first().context("exact session missing")?;
        ensure!(session.get("external_id").and_then(Value::as_str) == Some(native_id));
        ensure!(
            session.get("message_count").and_then(Value::as_u64) == Some(u64::try_from(expected)?)
        );
        Ok(())
    }
}

fn verify_payloads(archive: &Archive, cline: &[Value], opencode: &[Value]) -> Result<Vec<i64>> {
    let (cline_session, cline_messages) = archive.archived("cline", CLINE_ID)?;
    ensure!(cline_session.source_path == archive.cline_path());
    ensure!(cline_messages.len() == cline.len());
    for (message, original) in cline_messages.iter().zip(cline) {
        ensure!(
            message.extra_json == *original,
            "native Cline payload was lost"
        );
    }
    let tool_result = cline_messages
        .get(1)
        .context("tool-result-only message missing")?;
    ensure!(
        tool_result.role == MessageRole::User,
        "original tool-result role changed"
    );
    ensure!(tool_result.content.contains("native tool result"));

    let (opencode_session, opencode_messages) = archive.archived("opencode", OPENCODE_ID)?;
    ensure!(opencode_session.source_path == archive.opencode);
    ensure!(opencode_messages.len() == opencode.len());
    for (message, original) in opencode_messages.iter().zip(opencode) {
        for (key, value) in original
            .as_object()
            .context("fixture payload must be an object")?
        {
            ensure!(
                message.extra_json.get(key) == Some(value),
                "native OpenCode2 field was lost"
            );
        }
    }
    archive.exact_cli("cline", CLINE_ID, cline.len())?;
    archive.exact_cli("opencode", OPENCODE_ID, opencode.len())?;
    Ok(vec![
        cline_session.id.context("Cline ID")?,
        opencode_session.id.context("OpenCode ID")?,
    ])
}

fn source_bytes(archive: &Archive) -> Result<(Vec<u8>, Vec<u8>)> {
    Ok((
        fs::read(archive.cline_path())?,
        fs::read(&archive.opencode)?,
    ))
}

#[test]
fn native_index_exact_cli_replay_and_append_preserve_payloads_without_duplicates() -> Result<()> {
    let archive = Archive::new()?;
    let mut cline = vec![
        json!({"id":"user_0","role":"user","ts":archive.timestamp,"content":"native user turn"}),
        json!({"id":"tool_1","role":"user","ts":archive.timestamp + 1,
            "content":[{"type":"tool_result","tool_use_id":"call_1",
                "content":[{"type":"text","text":"native tool result"}]}]}),
    ];
    let mut opencode = vec![
        json!({"text":"native OpenCode2 user","unknown_native_field":{"preserved":true}}),
        json!({"content":[{"type":"tool","name":"read_file","state":{"output":"native result"}}]}),
    ];
    archive.write_cline(&cline)?;
    archive.create_opencode()?;
    for (sequence, payload) in opencode.iter().enumerate() {
        archive.append_opencode(i64::try_from(sequence)?, "user", payload)?;
    }
    let before = source_bytes(&archive)?;
    archive.index()?;
    let ids = verify_payloads(&archive, &cline, &opencode)?;
    let initial_opencode = archive.archived("opencode", OPENCODE_ID)?.0;
    archive.index()?;
    ensure!(verify_payloads(&archive, &cline, &opencode)? == ids);
    ensure!(
        source_bytes(&archive)? == before,
        "indexing changed a native source"
    );

    let large = "完整 future native output\n".repeat(4_000);
    cline.push(json!({"id":"assistant_2","role":"assistant",
        "ts":archive.timestamp + 2,"content":large}));
    opencode.push(json!({"text":large,"append_provenance":"native"}));
    archive.write_cline(&cline)?;
    archive.append_opencode(2, "assistant", opencode.last().context("appended payload")?)?;
    let appended = source_bytes(&archive)?;
    archive.index()?;
    ensure!(
        verify_payloads(&archive, &cline, &opencode)? == ids,
        "append changed session identity"
    );
    let grown_opencode = archive.archived("opencode", OPENCODE_ID)?.0;
    ensure!(
        grown_opencode.metadata_json.get("content_hash")
            != initial_opencode.metadata_json.get("content_hash"),
        "append retained an older native snapshot hash"
    );
    archive.index()?;
    ensure!(
        verify_payloads(&archive, &cline, &opencode)? == ids,
        "append replay changed session identity"
    );
    ensure!(
        source_bytes(&archive)? == appended,
        "append indexing changed native sources"
    );
    Ok(())
}

#[test]
fn empty_native_session_survives_exact_cli_replay_and_first_turn() -> Result<()> {
    let archive = Archive::new()?;
    archive.write_cline(&[json!({"id":"supporting_turn","role":"user",
        "ts":archive.timestamp,"content":"Supporting native fixture"})])?;
    archive.create_opencode()?;
    let before = source_bytes(&archive)?;
    archive.index()?;
    let (empty, messages) = archive.archived("opencode", OPENCODE_ID)?;
    let identity = empty.id.context("empty archive identity missing")?;
    ensure!(empty.source_path == archive.opencode);
    ensure!(messages.is_empty(), "archive invented a native turn");
    let empty_hash = empty
        .metadata_json
        .get("content_hash")
        .context("empty native content hash missing")?;
    ensure!(empty_hash == "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945");
    archive.exact_cli("opencode", OPENCODE_ID, 0)?;
    archive.index()?;
    let (replayed, messages) = archive.archived("opencode", OPENCODE_ID)?;
    ensure!(replayed.id == Some(identity) && messages.is_empty());
    ensure!(
        source_bytes(&archive)? == before,
        "empty capture changed native sources"
    );

    let original = json!({"text":"First original turn","native_unknown_field":true});
    archive.append_opencode(0, "user", &original)?;
    let appended = source_bytes(&archive)?;
    archive.index()?;
    let (grown, messages) = archive.archived("opencode", OPENCODE_ID)?;
    ensure!(
        grown.id == Some(identity),
        "first turn replaced native identity"
    );
    ensure!(messages.len() == 1);
    let first = messages.first().context("first original turn missing")?;
    ensure!(first.role == MessageRole::User && first.idx == 0);
    ensure!(first.extra_json.get("native_message_id") == Some(&json!("msg_0")));
    ensure!(first.extra_json.get("native_unknown_field") == original.get("native_unknown_field"));
    ensure!(grown.metadata_json.get("content_hash") != Some(empty_hash));
    archive.exact_cli("opencode", OPENCODE_ID, 1)?;
    archive.index()?;
    let (replayed, messages) = archive.archived("opencode", OPENCODE_ID)?;
    ensure!(replayed.id == Some(identity) && messages.len() == 1);
    ensure!(replayed.metadata_json == grown.metadata_json);
    ensure!(
        source_bytes(&archive)? == appended,
        "growth replay changed native sources"
    );

    // Replaying an older native snapshot must preserve the already captured
    // first turn and its provenance hash. Only this disposable source changes.
    let db = Connection::open(archive.opencode.to_string_lossy().as_ref())?;
    db.execute_compat(
        "DELETE FROM session_message WHERE session_id = ?1",
        params![OPENCODE_ID],
    )?;
    db.close()?;
    let stale = source_bytes(&archive)?;
    archive.index()?;
    let (retained, messages) = archive.archived("opencode", OPENCODE_ID)?;
    ensure!(retained.id == Some(identity) && messages.len() == 1);
    ensure!(retained.metadata_json == grown.metadata_json);
    archive.exact_cli("opencode", OPENCODE_ID, 1)?;
    ensure!(
        source_bytes(&archive)? == stale,
        "stale replay changed native sources"
    );
    Ok(())
}

#[test]
#[ignore = "requires an explicitly built EE candidate via CASS_ACCEPTANCE_EE_BINARY"]
fn native_ee_import_replay_append_and_reviewed_cross_agent_reuse() -> Result<()> {
    let ee_binary = PathBuf::from(
        std::env::var_os("CASS_ACCEPTANCE_EE_BINARY")
            .context("set CASS_ACCEPTANCE_EE_BINARY to the qualified candidate")?,
    )
    .canonicalize()?;
    let archive = Archive::new()?;
    let lesson = "Always run cargo fmt --check before cutting a release tag.";
    let mut cline = vec![json!({"id":"rule_1","role":"assistant",
        "ts":archive.timestamp,"content":lesson})];
    archive.write_cline(&cline)?;
    archive.create_opencode()?;
    archive.append_opencode(0, "assistant", &json!({"text":lesson}))?;
    // Two native sessions genuinely share one OpenCode2 database path. Their
    // EE identities must remain separate rather than keying only by filename.
    let second_id = "ses_native_acceptance_second";
    let db = Connection::open(archive.opencode.to_string_lossy().as_ref())?;
    db.execute_compat(
        "INSERT INTO session_v2 VALUES (?1,?2,'Second native session',?3,?3)",
        params![
            second_id,
            archive.workspace.to_string_lossy().as_ref(),
            archive.timestamp
        ],
    )?;
    db.execute_compat(
        "INSERT INTO session_message VALUES ('second_msg',?1,'assistant',0,?2,?2,?3)",
        params![
            second_id,
            archive.timestamp,
            json!({"text":lesson}).to_string()
        ],
    )?;
    db.close()?;
    let before = source_bytes(&archive)?;
    archive.index()?;
    archive.ee(&ee_binary, "cline", &["init", "--skip-boilerplate"])?;

    let selections = [
        ("cline", CLINE_ID),
        ("opencode", OPENCODE_ID),
        ("opencode", second_id),
    ];
    let mut session_ids = Vec::new();
    for (agent, native_id) in selections {
        let first = archive.ee_import(&ee_binary, agent, native_id)?;
        ensure!(first.get("sessionsImported").and_then(Value::as_u64) == Some(1));
        ensure!(first.get("spansImported").and_then(Value::as_u64) == Some(1));
        let sessions = first
            .get("sessions")
            .and_then(Value::as_array)
            .context("import sessions")?;
        let id = sessions
            .first()
            .and_then(|v| v.get("sessionId"))
            .and_then(Value::as_str)
            .context("import session ID")?
            .to_owned(); // ubs:ignore -- Retain three native IDs after their individual JSON responses drop; this bounded fixture is not a hot allocation loop.
        ensure!(
            !session_ids.contains(&id),
            "distinct native sessions were merged"
        );
        session_ids.push(id);
        let replay = archive.ee_import(&ee_binary, agent, native_id)?;
        ensure!(replay.get("sessionsImported").and_then(Value::as_u64) == Some(0));
        ensure!(replay.get("sessionsSkipped").and_then(Value::as_u64) == Some(1));
    }
    ensure!(
        source_bytes(&archive)? == before,
        "CASS/EE changed native history"
    );

    let original_id = session_ids.first().context("Cline EE session ID")?;
    let review = archive.ee(
        &ee_binary,
        "cline",
        &[
            "review",
            "session",
            original_id,
            "--propose",
            "--min-confidence",
            "0",
        ],
    )?;
    let candidates = review
        .get("candidates")
        .and_then(Value::as_array)
        .context("review candidates")?;
    let candidate = candidates
        .iter()
        .find(|candidate| {
            candidate
                .get("proposedContent")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains(lesson))
        })
        .context("native evidence did not produce the expected review candidate")?;
    let candidate_id = candidate
        .get("candidateId")
        .and_then(Value::as_str)
        .context("candidate ID")?;
    archive.ee(
        &ee_binary,
        "native-acceptance-reviewer",
        &[
            "curate",
            "validate",
            candidate_id,
            "--actor",
            "native-acceptance-reviewer",
        ],
    )?;
    archive.ee(
        &ee_binary,
        "native-acceptance-reviewer",
        &[
            "curate",
            "accept",
            candidate_id,
            "--actor",
            "native-acceptance-reviewer",
            "--reason",
            "Reviewed the disposable native source and confirmed the exact release-check rule.",
        ],
    )?;
    archive.ee(
        &ee_binary,
        "native-acceptance-reviewer",
        &[
            "curate",
            "apply",
            candidate_id,
            "--actor",
            "native-acceptance-reviewer",
        ],
    )?;

    cline.push(
        json!({"id":"append_2","role":"user","ts":archive.timestamp + 1,
        "content":"完整 native append\n".repeat(5_000)}),
    );
    archive.write_cline(&cline)?;
    let appended = source_bytes(&archive)?;
    archive.index()?;
    let refreshed = archive.ee_import(&ee_binary, "cline", CLINE_ID)?;
    ensure!(refreshed.get("sessionsImported").and_then(Value::as_u64) == Some(1));
    let refreshed_id = refreshed
        .get("sessions")
        .and_then(Value::as_array)
        .and_then(|sessions| sessions.first())
        .and_then(|session| session.get("sessionId"))
        .and_then(Value::as_str)
        .context("refreshed session ID")?;
    ensure!(
        refreshed_id == original_id, // ubs:ignore -- Equality of public transcript record IDs verifies append stability; neither value grants authority.
        "append replaced the durable EE session"
    );
    let replay = archive.ee_import(&ee_binary, "cline", CLINE_ID)?;
    ensure!(replay.get("sessionsSkipped").and_then(Value::as_u64) == Some(1));
    ensure!(
        source_bytes(&archive)? == appended,
        "refresh changed native history"
    );

    // A separate process under the other native agent identity must retrieve
    // the reviewed memory after the original session has grown.
    let reused = archive.ee(&ee_binary, "opencode", &["search", "cargo fmt release tag"])?;
    ensure!(
        reused.to_string().contains(lesson),
        "reviewed memory was not reusable after append"
    );
    let db = Connection::open(
        archive
            .workspace
            .join(".ee/ee.db")
            .to_string_lossy()
            .as_ref(),
    )?;
    let count: i64 = db.query_row_map("SELECT COUNT(*) FROM sessions", params![], |row| {
        row.get_typed(0)
    })?;
    ensure!(count == 3, "native import duplicated or merged sessions");
    db.close()?;
    Ok(())
}
