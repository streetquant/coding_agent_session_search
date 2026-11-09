use super::*;

pub(super) struct SnapshotOutcome {
    pub conversation: Conversation,
    pub conversation_id: i64,
    pub conversation_inserted: bool,
    pub messages_changed: bool,
    pub inserted_indices: Vec<i64>,
    pub updated_indices: Vec<i64>,
    pub rebuild_analytics: bool,
}

struct CodexProjectionPlan {
    conversation: Conversation,
    duplicates: Vec<i64>,
}

#[allow(dead_code)] // Codex legacy-projection repair is exercised by the module tests.
fn empty_extra(message: &Message) -> bool {
    message.extra_json.is_null()
        || message
            .extra_json
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
}

#[allow(dead_code)] // Codex legacy-projection repair is exercised by the module tests.
fn clone_conversation_header(conversation: &Conversation) -> Conversation {
    let mut header = conversation.clone();
    header.messages.clear();
    header
}

fn codex_projection_metadata_matches(
    previous: &serde_json::Value,
    incoming: &serde_json::Value,
) -> bool {
    if previous == incoming {
        return true;
    }
    let (Some(previous), Some(incoming)) = (previous.as_object(), incoming.as_object()) else {
        return false;
    };
    // The connector now certifies this identity from the original session
    // header. Legacy archives lack it; adding it must preserve every archived
    // field and may never replace an existing identity. The projection plan
    // independently verifies the original full bodies before reaching here.
    !previous.contains_key("native_session_id")
        && incoming
            .get("native_session_id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| !id.trim().is_empty() && id.len() <= 4096)
        && incoming.len() == previous.len() + 1
        && previous
            .iter()
            .all(|(key, value)| incoming.get(key) == Some(value))
}

/// Repair only the historical compacted Codex projection, not arbitrary
/// mutable transcripts. Each shortened result needs its original full body
/// still present in the archive as independent evidence of the omitted text.
#[allow(dead_code)] // Codex legacy-projection repair is exercised by the module tests.
fn codex_projection_plan(
    incoming: &Conversation,
    previous: &[Message],
) -> Result<Option<CodexProjectionPlan>> {
    const LEGACY_TOOL_CHARS: usize = 128 * 1024;
    if !previous.iter().any(|message| {
        message.role == MessageRole::Tool
            && empty_extra(message)
            && message
                .content
                .ends_with(" additional chars from tool output]")
    }) {
        return Ok(None);
    }
    let mut by_body = HashMap::<_, Vec<usize>>::new();
    for (position, message) in previous.iter().enumerate() {
        by_body
            .entry(message_replay_fingerprint(message))
            .or_default()
            .push(position);
    }
    let mut conversation = clone_conversation_header(incoming);
    let mut used = HashSet::new();
    let mut native_ids = HashSet::new();
    let mut witnesses = HashMap::<_, Vec<i64>>::new();
    for (position, message) in incoming.messages.iter().enumerate() {
        if message.idx != i64::try_from(position)? {
            return Ok(None);
        }
        let exact = message_replay_fingerprint(message);
        let exact_match = by_body.get(&exact).and_then(|positions| {
            positions.iter().copied().find(|index| {
                !used.contains(index)
                    && previous.get(*index).is_some_and(|old| {
                        old.extra_json == message.extra_json
                            || (empty_extra(old) && empty_extra(message))
                    })
            })
        });
        let mut old_position = exact_match;
        if let Some(call_id) = message
            .extra_json
            .pointer("/cass/tool_result_call_id")
            .and_then(serde_json::Value::as_str)
        {
            if message.role != MessageRole::Tool
                || call_id.is_empty()
                || !native_ids.insert(call_id)
            {
                return Ok(None);
            }
            let prefix = format!("[Tool output: {call_id}]\n"); // ubs:ignore[rust.perf.string-alloc-in-loop] -- The historical call-ID prefix is required to verify the archived projection.
            let Some(body) = message.content.strip_prefix(&prefix) else {
                return Ok(None);
            };
            if old_position.is_none() {
                let trimmed = body.trim();
                let mut characters = trimmed.chars();
                let mut projection = prefix;
                projection.extend(characters.by_ref().take(LEGACY_TOOL_CHARS));
                let omitted = characters.count();
                if omitted == 0 {
                    return Ok(None);
                }
                let notice = format!("\n[truncated {omitted} additional chars from tool output]"); // ubs:ignore[rust.perf.string-alloc-in-loop] -- Exact historical notice required for evidence comparison.
                projection.push_str(&notice);
                let projected = MessageReplayFingerprint {
                    content_hash: *blake3::hash(projection.as_bytes()).as_bytes(),
                    ..exact
                };
                old_position = by_body.get(&projected).and_then(|positions| {
                    positions.iter().copied().find(|index| {
                        !used.contains(index) && previous.get(*index).is_some_and(empty_extra)
                    })
                });
                let Some(old) = old_position.and_then(|index| previous.get(index)) else {
                    return Ok(None);
                };
                witnesses
                    .entry(MessageReplayFingerprint {
                        content_hash: *blake3::hash(body.as_bytes()).as_bytes(),
                        ..message_replay_fingerprint(message)
                    })
                    .or_default()
                    .push(old.id.context("legacy Codex result has no canonical ID")?);
            }
        }
        let Some(old_position) = old_position else {
            // An incomplete packet must never retire messages outside it.
            return Ok(None);
        };
        used.insert(old_position);
        let old = previous
            .get(old_position)
            .context("legacy message position")?;
        // ubs:ignore[rust.collections.clone-in-loop] -- Only a fully verified repair owns a replacement snapshot; ordinary replay returns before this loop.
        let mut revised = message.clone();
        revised.id = old.id;
        conversation.messages.push(revised);
    }
    if witnesses.is_empty() {
        return Ok(None);
    }
    let mut duplicates = Vec::new();
    let mut audit = Vec::new();
    for (position, old) in previous.iter().enumerate() {
        if used.contains(&position) {
            continue;
        }
        let fingerprint = message_replay_fingerprint(old);
        let Some(replacements) = witnesses.remove(&fingerprint) else {
            return Ok(None);
        };
        if old.role != MessageRole::Tool || !empty_extra(old) {
            return Ok(None);
        }
        let identity = old
            .id
            .context("legacy Codex duplicate has no canonical ID")?;
        duplicates.push(identity);
        audit.push(serde_json::json!({
            "message_id": identity, "idx": old.idx,
            "body_blake3": blake3::hash(old.content.as_bytes()).to_hex().as_str(),
            "preserved_in_message_ids": replacements,
        }));
    }
    if !witnesses.is_empty() {
        return Ok(None);
    }
    if conversation.metadata_json.is_null() {
        conversation.metadata_json = serde_json::json!({});
    }
    if !conversation.metadata_json.is_object() {
        return Ok(None);
    }
    // ubs:ignore[rust.panic.direct-indexing] -- The object check above makes this a JSON object insertion.
    conversation.metadata_json["codex_compacted_projection_repair"] = serde_json::json!({
        "version": 1, "retired_duplicate_projections": audit,
        "original_positions": previous.iter().map(|message| (message.id, message.idx)).collect::<Vec<_>>()
    });
    Ok(Some(CodexProjectionPlan {
        conversation,
        duplicates,
    }))
}

#[allow(dead_code)] // Codex legacy-projection repair is exercised by the module tests.
pub(super) fn reconcile_codex_projection(
    storage: &FrankenStorage,
    tx: &FrankenTransaction<'_>,
    agent_id: i64,
    workspace_id: Option<i64>,
    incoming: &Conversation,
    defer_lexical: bool,
    defer_analytics: bool,
) -> Result<Option<SnapshotOutcome>> {
    if incoming.agent_slug != "codex"
        || !incoming.messages.iter().any(|message| {
            message
                .extra_json
                .pointer("/cass/tool_result_call_id")
                .is_some()
        })
    {
        return Ok(None);
    }
    let Some(external_id) = incoming.external_id.as_deref() else {
        return Ok(None);
    };
    let existing: Option<i64> = tx
        .query_row_map(
            "SELECT id FROM conversations WHERE source_id=?1 AND agent_id=?2 AND external_id=?3",
            fparams![incoming.source_id.as_str(), agent_id, external_id],
            |row| row.get_typed(0),
        )
        .optional()?;
    let Some(conversation_id) = existing else {
        return Ok(None);
    };
    let legacy: i64 = tx.query_row_map(
        "SELECT EXISTS(SELECT 1 FROM messages
         WHERE conversation_id=?1 AND role='tool'
           AND content LIKE '[Tool output:%'
           AND content LIKE '% additional chars from tool output]')",
        fparams![conversation_id],
        |row| row.get_typed(0),
    )?;
    if legacy == 0 {
        return Ok(None);
    }
    let previous = read_messages(tx, conversation_id)?;
    let Some(plan) = codex_projection_plan(incoming, &previous)? else {
        return Ok(None);
    };
    // A compacted projection never had source snippets or extra metadata to
    // discard. Refuse instead of silently dropping later canonical additions.
    for message in &previous {
        if !franken_codebuff_snippets_match(
            tx,
            message.id.context("legacy Codex canonical message ID")?,
            &[],
        )? {
            return Ok(None);
        }
    }
    reconcile_inner(
        storage,
        tx,
        agent_id,
        workspace_id,
        incoming,
        defer_lexical,
        defer_analytics,
        Some(plan),
    )
    .map(Some)
}

fn required_text<'value>(value: &'value serde_json::Value, key: &str) -> Result<&'value str> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.is_empty())
        .with_context(|| format!("missing native OpenWebUI {key}"))
}

fn message_identity<'message>(
    conversation: &Conversation,
    message: &'message Message,
) -> Result<&'message str> {
    let identity = message
        .extra_json
        .pointer("/openwebui/id")
        .and_then(serde_json::Value::as_str)
        .filter(|identity| !identity.is_empty())
        .context("missing original OpenWebUI message ID")?;
    anyhow::ensure!(
        message
            .extra_json
            .pointer("/openwebui/message/id")
            .and_then(serde_json::Value::as_str)
            == Some(identity),
        "native OpenWebUI projected message ID mismatch"
    );
    for key in ["instance_id", "conversation_id", "user_id"] {
        anyhow::ensure!(
            required_text(&message.extra_json, key)?
                == required_text(&conversation.metadata_json, key)?,
            "native OpenWebUI message {key} mismatch"
        );
    }
    Ok(identity)
}

pub(super) fn validate_batch(conversations: &[(i64, Option<i64>, &Conversation)]) -> Result<()> {
    let mut identities = HashSet::new();
    for (agent_id, _, conversation) in conversations {
        if !is_openwebui_saved_snapshot(conversation) {
            continue;
        }
        let instance = required_text(&conversation.metadata_json, "instance_id")?;
        let chat = required_text(&conversation.metadata_json, "conversation_id")?;
        for key in ["user_id", "source_database", "workspace_root"] {
            required_text(&conversation.metadata_json, key)?;
        }
        let external_id = serde_json::to_string(&[instance, chat])?;
        anyhow::ensure!(
            conversation.external_id.as_deref() == Some(external_id.as_str()),
            "native OpenWebUI conversation ID mismatch"
        );
        anyhow::ensure!(
            identities.insert((*agent_id, &conversation.source_id, external_id)),
            "multiple OpenWebUI snapshots for one original identity in a batch"
        );
        let mut messages = HashSet::new();
        for (position, message) in conversation.messages.iter().enumerate() {
            anyhow::ensure!(
                message.idx == i64::try_from(position)?,
                "noncanonical OpenWebUI message order"
            );
            anyhow::ensure!(
                messages.insert(message_identity(conversation, message)?),
                "duplicate original OpenWebUI message ID"
            );
        }
    }
    Ok(())
}

fn read_messages(tx: &FrankenTransaction<'_>, conversation_id: i64) -> Result<Vec<Message>> {
    tx.query_map_collect(
        "SELECT id, idx, role, author, created_at, content, extra_json, extra_bin
         FROM messages WHERE conversation_id = ?1 ORDER BY idx",
        fparams![conversation_id],
        |row| {
            let role: String = row.get_typed(2)?;
            Ok(Message {
                id: Some(row.get_typed(0)?),
                idx: row.get_typed(1)?,
                role: role_from_str(&role),
                author: row.get_typed(3)?,
                created_at: row.get_typed(4)?,
                content: row.get_typed(5)?,
                extra_json: franken_read_message_extra_compat(row, 6, 7),
                snippets: Vec::new(),
            })
        },
    )
    .map_err(Into::into)
}

fn same_message(previous: &Message, current: &Message) -> bool {
    previous.idx == current.idx
        && previous.role == current.role
        && previous.author == current.author
        && previous.created_at == current.created_at
        && previous.content == current.content
        && previous.extra_json == current.extra_json
}

#[allow(clippy::too_many_arguments)]
pub(super) fn reconcile(
    storage: &FrankenStorage,
    tx: &FrankenTransaction<'_>,
    agent_id: i64,
    workspace_id: Option<i64>,
    incoming: &Conversation,
    defer_lexical: bool,
    defer_analytics: bool,
) -> Result<SnapshotOutcome> {
    reconcile_inner(
        storage,
        tx,
        agent_id,
        workspace_id,
        incoming,
        defer_lexical,
        defer_analytics,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn reconcile_inner(
    storage: &FrankenStorage,
    tx: &FrankenTransaction<'_>,
    agent_id: i64,
    workspace_id: Option<i64>,
    incoming: &Conversation,
    defer_lexical: bool,
    defer_analytics: bool,
    codex_plan: Option<CodexProjectionPlan>,
) -> Result<SnapshotOutcome> {
    let is_codex_projection = codex_plan.is_some();
    let (conversation_id, conversation_inserted) =
        match franken_insert_conversation_or_get_existing(tx, agent_id, workspace_id, incoming)? {
            ConversationInsertStatus::Inserted(identity) => (identity, true),
            ConversationInsertStatus::Existing(identity) => (identity, false),
        };
    let (previous_metadata, previous_started, previous_title, slug, workspace, source, path, host) =
        tx.query_row_map(
            "SELECT c.metadata_json, c.metadata_bin, c.started_at, c.title,
                a.slug, c.workspace_id, c.source_id, c.source_path, c.origin_host
         FROM conversations c JOIN agents a ON a.id = c.agent_id WHERE c.id = ?1",
            fparams![conversation_id],
            |row| {
                let slug: String = row.get_typed(4)?;
                let workspace: Option<i64> = row.get_typed(5)?;
                let source: String = row.get_typed(6)?;
                let path: String = row.get_typed(7)?;
                let host: Option<String> = row.get_typed(8)?;
                Ok((
                    franken_read_metadata_compat(row, 0, 1),
                    row.get_typed::<Option<i64>>(2)?,
                    row.get_typed::<Option<String>>(3)?,
                    slug,
                    workspace,
                    source,
                    path,
                    host,
                ))
            },
        )?;
    anyhow::ensure!(
        slug == incoming.agent_slug
            && workspace == workspace_id
            && source == incoming.source_id
            && path == path_to_string(&incoming.source_path)
            && host == incoming.origin_host,
        "native OpenWebUI canonical identity changed"
    );
    for key in [
        "schema_version",
        "instance_id",
        "conversation_id",
        "user_id",
        "source_database",
        "workspace_root",
    ] {
        if is_codex_projection {
            break;
        }
        anyhow::ensure!(
            required_text(&previous_metadata, key)? == required_text(&incoming.metadata_json, key)?,
            "native OpenWebUI canonical {key} changed"
        );
    }
    let previous = read_messages(tx, conversation_id)?;
    let mut previous_by_identity = HashMap::new();
    for message in &previous {
        if is_codex_projection {
            break;
        }
        anyhow::ensure!(
            message.idx >= 0,
            "negative canonical OpenWebUI message index"
        );
        anyhow::ensure!(
            previous_by_identity
                .insert(message_identity(incoming, message)?, message)
                .is_none(),
            "duplicate canonical OpenWebUI message ID"
        );
    }
    let (mut conversation, duplicates) = match codex_plan {
        Some(plan) => {
            anyhow::ensure!(
                codex_projection_metadata_matches(&previous_metadata, &incoming.metadata_json)
                    && previous_started == incoming.started_at
                    && previous_title == incoming.title,
                "legacy Codex canonical metadata changed"
            );
            (plan.conversation, Some(plan.duplicates))
        }
        None => (incoming.clone(), None),
    };
    let mut inserted_indices = Vec::new();
    let mut seen = HashSet::new();
    let mut messages_changed = conversation_inserted
        || previous_started != incoming.started_at
        || previous_title != incoming.title
        || is_codex_projection;
    for message in &mut conversation.messages {
        if is_codex_projection {
            break;
        }
        // ubs:ignore[rust.perf.string-alloc-in-loop] -- The owned identity is the lookup key retained in the seen set.
        let identity = message_identity(incoming, message)?.to_owned();
        if let Some(previous_message) = previous_by_identity.get(identity.as_str()) {
            message.id = previous_message.id;
            messages_changed |= !same_message(previous_message, message);
        } else {
            message.id = None;
            inserted_indices.push(message.idx);
            messages_changed = true;
        }
        seen.insert(identity);
    }
    let mut retained = Vec::new();
    for message in &previous {
        if is_codex_projection {
            break;
        }
        let identity = message_identity(incoming, message)?;
        if seen.contains(identity) {
            continue;
        }
        // ubs:ignore[rust.collections.clone-in-loop] -- The retained message is an owned copy pushed into the conversation.
        let mut retained_message = message.clone();
        retained_message.idx = i64::try_from(conversation.messages.len())?;
        // ubs:ignore[rust.panic.direct-indexing] -- serde_json::Value indexing inserts the key and cannot panic.
        retained_message.extra_json["cass_openwebui_source_present"] = serde_json::json!(false);
        messages_changed |= !same_message(message, &retained_message);
        // ubs:ignore[rust.perf.string-alloc-in-loop] -- The retained identity is owned because it is stored in the metadata list.
        retained.push(identity.to_owned());
        conversation.messages.push(retained_message);
    }
    if !is_codex_projection {
        // ubs:ignore[rust.panic.direct-indexing] -- serde_json::Value indexing inserts the key and cannot panic.
        conversation.metadata_json["retained_absent_message_ids"] = serde_json::json!(retained);
    }
    let lexical_key = if is_codex_projection {
        "codex_projection_lexical_indices"
    } else {
        "openwebui_lexical_indices"
    };
    let mut lexical_indices = std::collections::BTreeSet::new();
    if let Some(indices) = previous_metadata.get(lexical_key) {
        for index in indices
            .as_array()
            .context("invalid native lexical identity ledger")?
        {
            lexical_indices.insert(
                index
                    .as_i64()
                    .filter(|index| *index >= 0)
                    .context("invalid native lexical message index")?,
            );
        }
    }
    lexical_indices.extend(previous.iter().map(|message| message.idx));
    lexical_indices.extend(conversation.messages.iter().map(|message| message.idx));
    // ubs:ignore[rust.panic.direct-indexing] -- serde_json::Value indexing inserts the key and cannot panic.
    conversation.metadata_json[lexical_key] = serde_json::json!(lexical_indices);
    let (defer_analytics, previous_analytics) = if is_codex_projection {
        let (metrics, tokens): (i64, i64) = tx.query_row_map(
            "SELECT (SELECT COUNT(*) FROM message_metrics WHERE message_id IN
                (SELECT id FROM messages WHERE conversation_id=?1)),
                (SELECT COUNT(*) FROM token_usage WHERE conversation_id=?1)",
            fparams![conversation_id],
            |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
        )?;
        let complete = metrics == i64::try_from(previous.len())? && tokens == metrics;
        anyhow::ensure!(
            complete || (metrics == 0 && tokens == 0),
            "legacy Codex analytics require repair before replacing projections"
        );
        (defer_analytics && !complete, complete)
    } else {
        (defer_analytics, !conversation_inserted)
    };
    let updated_indices = if messages_changed {
        conversation
            .messages
            .iter()
            .filter(|message| message.id.is_some())
            .map(|message| message.idx)
            .collect()
    } else {
        Vec::new()
    };

    if messages_changed {
        let previous_started = previous_started.or_else(|| {
            previous
                .iter()
                .filter_map(|message| message.created_at)
                .min()
        });
        if !defer_analytics {
            if previous_analytics {
                subtract_analytics(tx, incoming, conversation_id, previous_started, &previous)?;
                franken_update_daily_stats_in_tx(
                    storage,
                    tx,
                    &incoming.agent_slug,
                    &incoming.source_id,
                    previous_started,
                    StatsDelta {
                        session_count_delta: -1,
                        message_count_delta: -i64::try_from(previous.len())?,
                        total_chars_delta: -message_chars(&previous)?,
                    },
                )?;
            }
            franken_update_daily_stats_in_tx(
                storage,
                tx,
                &incoming.agent_slug,
                &incoming.source_id,
                conversation_effective_started_at(&conversation),
                StatsDelta {
                    session_count_delta: 1,
                    message_count_delta: i64::try_from(conversation.messages.len())?,
                    total_chars_delta: message_chars(&conversation.messages)?,
                },
            )?;
        }
        if !defer_lexical
            && !storage.fts_inline_writes_suspended()
            && storage.fts_messages_present_cached(tx)
        {
            for chunk in previous.chunks(128) {
                let parameters: Vec<SqliteValue> = chunk
                    .iter()
                    .map(|message| {
                        message
                            .id
                            .context("canonical message ID")
                            .map(SqliteValue::from)
                    })
                    .collect::<Result<_>>()?;
                if let Err(error) = tx.execute_with_params_skip_statement_savepoint(
                    // ubs:ignore[rust.perf.string-alloc-in-loop] -- The statement text is formatted once per batch, not once per row.
                    &format!(
                        "DELETE FROM fts_messages WHERE rowid IN ({})",
                        sql_placeholders(parameters.len())
                    ),
                    &parameters,
                ) {
                    // ubs:ignore[rust.perf.string-alloc-in-loop] -- The error text is materialized only on the error path to classify a missing FTS table.
                    if error.to_string().contains("no such table: fts_messages") {
                        storage.set_fts_messages_present_cache(false);
                        break;
                    }
                    return Err(error).context("retiring native snapshot fallback lexical rows");
                }
            }
        }
        if let Some(duplicates) = duplicates {
            for message_id in duplicates {
                tx.execute_compat(
                    "DELETE FROM messages WHERE id=?1 AND conversation_id=?2",
                    fparams![message_id, conversation_id],
                )?;
            }
            let generation = format!("codex_projection:{:032x}", rand::random::<u128>());
            for tier in [SemanticIdentityTier::Fast, SemanticIdentityTier::Quality] {
                tx.execute_compat(
                    "INSERT OR REPLACE INTO meta(key,value) VALUES(?1,?2)",
                    fparams![tier.meta_key(), generation.as_str()],
                )?;
            }
            clear_semantic_embed_watermark_after_deletion(tx)?;
            tx.execute_compat(
                "INSERT OR REPLACE INTO meta(key,value) VALUES(?1,?2)",
                fparams![
                    TOKEN_USAGE_REVISION_GENERATION_META_KEY,
                    generation.as_str()
                ],
            )?;
            tx.execute_compat(
                "DELETE FROM meta WHERE key=?1",
                fparams![TOKEN_DAILY_STATS_REBUILD_CURSOR_KEY],
            )?;
        }
        if let Some(maximum) = previous.iter().map(|message| message.idx).max() {
            let offset = maximum
                .max(i64::try_from(conversation.messages.len())?)
                .checked_add(1)
                .context("OpenWebUI index offset overflow")?;
            maximum
                .checked_add(offset)
                .context("OpenWebUI temporary index overflow")?;
            tx.execute_compat(
                "UPDATE messages SET idx = idx + ?1 WHERE conversation_id = ?2",
                fparams![offset, conversation_id],
            )?;
        }
        for message in &mut conversation.messages {
            if let Some(message_id) = message.id {
                let (json, binary) = franken_message_insert_payload(message)?;
                tx.execute_compat(
                    "UPDATE messages SET idx=?1, role=?2, author=?3, created_at=?4, content=?5, extra_json=?6, extra_bin=?7 WHERE id=?8 AND conversation_id=?9",
                    fparams![message.idx, role_as_str(&message.role), message.author.as_deref(), message.created_at,
                        message.content.as_str(), json.as_deref(), binary.as_deref(), message_id, conversation_id],
                )?;
            } else {
                let message_id = franken_insert_new_message(tx, conversation_id, message)?;
                franken_insert_snippets(tx, message_id, &message.snippets)?;
                message.id = Some(message_id);
            }
        }
    }
    if previous_metadata != conversation.metadata_json
        || previous_title != conversation.title
        || previous_started != conversation.started_at
    {
        let (json, binary) = franken_metadata_insert_payload(&conversation.metadata_json)?;
        tx.execute_compat("UPDATE conversations SET title=?1, started_at=?2, ended_at=?3, metadata_json=?4, metadata_bin=?5 WHERE id=?6",
            fparams![conversation.title.as_deref(), conversation.started_at, conversation.ended_at, json.as_deref(), binary.as_deref(), conversation_id])?;
    }
    let (last_idx, last_created_at) = conversation_tail_state(&conversation);
    tx.execute_compat(
        "UPDATE conversations SET ended_at=?1, last_message_idx=?2, last_message_created_at=?3 WHERE id=?4",
        fparams![conversation.ended_at, last_idx, last_created_at, conversation_id],
    )?;
    tx.execute_compat(
        "INSERT OR REPLACE INTO conversation_tail_state(conversation_id, ended_at, last_message_idx, last_message_created_at) VALUES(?1,?2,?3,?4)",
        fparams![conversation_id, conversation.ended_at, last_idx, last_created_at],
    )?;
    if let Some(lookup) = conversation_external_lookup_key_for_conv(agent_id, &conversation) {
        let changed = tx.execute_compat(
            "UPDATE conversation_external_tail_lookup SET ended_at=?1, last_message_idx=?2, last_message_created_at=?3 WHERE lookup_key=?4 AND conversation_id=?5",
            fparams![conversation.ended_at, last_idx, last_created_at, lookup, conversation_id],
        )?;
        anyhow::ensure!(
            changed == 1,
            "missing native OpenWebUI external tail identity"
        );
    }
    Ok(SnapshotOutcome {
        conversation,
        conversation_id,
        conversation_inserted,
        messages_changed,
        inserted_indices,
        updated_indices,
        rebuild_analytics: !defer_analytics,
    })
}

fn message_chars(messages: &[Message]) -> Result<i64> {
    messages.iter().try_fold(0_i64, |total, message| {
        total
            .checked_add(i64::try_from(message.content.len())?)
            .context("OpenWebUI character count overflow")
    })
}

fn subtract_analytics(
    tx: &FrankenTransaction<'_>,
    conversation: &Conversation,
    conversation_id: i64,
    previous_started: Option<i64>,
    previous: &[Message],
) -> Result<()> {
    let mut rollups = AnalyticsRollupAggregator::new();
    let mut metrics_count = 0usize;
    for chunk in previous.chunks(128) {
        let parameters: Vec<ParamValue> = chunk
            .iter()
            .map(|message| {
                message
                    .id
                    .context("canonical message ID")
                    .map(ParamValue::from)
            })
            .collect::<Result<_>>()?;
        // ubs:ignore[rust.perf.string-alloc-in-loop] -- The query text is formatted once per batch, not once per row.
        let entries = tx.query_map_collect(&format!(
            "SELECT message_id, created_at_ms, hour_id, day_id, agent_slug, workspace_id, source_id, role,
                    content_chars, content_tokens_est, model_name, model_family, model_tier, provider,
                    api_input_tokens, api_output_tokens, api_cache_read_tokens, api_cache_creation_tokens,
                    api_thinking_tokens, api_service_tier, api_data_source, tool_call_count, has_tool_calls, has_plan
             FROM message_metrics WHERE message_id IN ({})", sql_placeholders(parameters.len())), &parameters, |row| {
            Ok(MessageMetricsEntry {
                message_id: row.get_typed(0)?, created_at_ms: row.get_typed(1)?, hour_id: row.get_typed(2)?, day_id: row.get_typed(3)?,
                agent_slug: row.get_typed(4)?, workspace_id: row.get_typed(5)?, source_id: row.get_typed(6)?, role: row.get_typed(7)?,
                content_chars: row.get_typed(8)?, content_tokens_est: row.get_typed(9)?, model_name: row.get_typed(10)?, model_family: row.get_typed(11)?,
                model_tier: row.get_typed(12)?, provider: row.get_typed(13)?, api_input_tokens: row.get_typed(14)?, api_output_tokens: row.get_typed(15)?,
                api_cache_read_tokens: row.get_typed(16)?, api_cache_creation_tokens: row.get_typed(17)?, api_thinking_tokens: row.get_typed(18)?,
                api_service_tier: row.get_typed(19)?, api_data_source: row.get_typed(20)?, tool_call_count: row.get_typed(21)?,
                has_tool_calls: row.get_typed::<i64>(22)? != 0, has_plan: row.get_typed::<i64>(23)? != 0,
            })
        })?;
        metrics_count += entries.len();
        for entry in entries {
            rollups.record(&entry);
        }
        tx.execute_compat(
            // ubs:ignore[rust.perf.string-alloc-in-loop] -- The statement text is formatted once per batch, not once per row.
            &format!(
                "DELETE FROM message_metrics WHERE message_id IN ({})",
                sql_placeholders(parameters.len())
            ),
            &parameters,
        )?;
    }
    anyhow::ensure!(
        metrics_count == previous.len(),
        "native OpenWebUI analytics need repair before replacement"
    );
    for (table, bucket, deltas) in [
        ("usage_hourly", "hour_id", &rollups.hourly),
        ("usage_daily", "day_id", &rollups.daily),
    ] {
        for ((period, agent, workspace, source), delta) in deltas {
            franken_relocate_rollup_subtract(
                tx,
                table,
                vec![
                    (bucket, ParamValue::from(*period)),
                    ("agent_slug", ParamValue::from(agent.as_str())),
                    ("workspace_id", ParamValue::from(*workspace)),
                    ("source_id", ParamValue::from(source.as_str())),
                ],
                *workspace,
                delta,
            )?;
        }
    }
    for ((day, agent, workspace, source, family, tier), delta) in &rollups.models_daily {
        franken_relocate_rollup_subtract(
            tx,
            "usage_models_daily",
            vec![
                ("day_id", ParamValue::from(*day)),
                ("agent_slug", ParamValue::from(agent.as_str())),
                ("workspace_id", ParamValue::from(*workspace)),
                ("source_id", ParamValue::from(source.as_str())),
                ("model_family", ParamValue::from(family.as_str())),
                ("model_tier", ParamValue::from(tier.as_str())),
            ],
            *workspace,
            delta,
        )?;
    }
    let rows = tx.query_map_collect(
        "SELECT message_id, day_id, model_family, role, input_tokens, output_tokens, cache_read_tokens,
                cache_creation_tokens, thinking_tokens, total_tokens, content_chars, tool_call_count, estimated_cost_usd
         FROM token_usage WHERE conversation_id = ?1", fparams![conversation_id], |row| {
            let role: String = row.get_typed(3)?;
            Ok((row.get_typed::<i64>(0)?, row.get_typed::<i64>(1)?, row.get_typed::<Option<String>>(2)?.unwrap_or_else(|| "unknown".to_owned()), TokenStatsDelta {
                api_call_count: 1, user_message_count: i64::from(role == "user"),
                assistant_message_count: i64::from(role == "assistant" || role == "agent"), tool_message_count: i64::from(role == "tool"),
                total_input_tokens: row.get_typed::<Option<i64>>(4)?.unwrap_or(0), total_output_tokens: row.get_typed::<Option<i64>>(5)?.unwrap_or(0),
                total_cache_read_tokens: row.get_typed::<Option<i64>>(6)?.unwrap_or(0), total_cache_creation_tokens: row.get_typed::<Option<i64>>(7)?.unwrap_or(0),
                total_thinking_tokens: row.get_typed::<Option<i64>>(8)?.unwrap_or(0), grand_total_tokens: row.get_typed::<Option<i64>>(9)?.unwrap_or(0),
                total_content_chars: row.get_typed::<i64>(10)?, total_tool_calls: row.get_typed::<i64>(11)?,
                estimated_cost_usd: row.get_typed::<Option<f64>>(12)?.unwrap_or(0.0), session_count: 0,
            }))
        })?;
    anyhow::ensure!(
        rows.len() == previous.len(),
        "native OpenWebUI token ledger needs repair before replacement"
    );
    let positions: HashMap<i64, i64> = previous
        .iter()
        .map(|message| {
            message
                .id
                .context("canonical message ID")
                .map(|identity| (identity, message.idx))
        })
        .collect::<Result<_>>()?;
    let session_family = rows
        .iter()
        .filter(|(_, _, family, _)| family != "unknown")
        .max_by_key(|(identity, _, _, _)| positions.get(identity).copied())
        .map_or("unknown", |(_, _, family, _)| family.as_str())
        .to_owned();
    let mut token_stats = TokenStatsAggregator::new();
    for (_, day, family, delta) in rows {
        let accumulated = token_stats
            .deltas
            .entry((
                day,
                // ubs:ignore[rust.collections.clone-in-loop] -- The rollup key owns the agent slug.
                conversation.agent_slug.clone(),
                // ubs:ignore[rust.collections.clone-in-loop] -- The rollup key owns the source id.
                conversation.source_id.clone(),
                family,
            ))
            .or_default();
        for (total, contribution) in [
            (&mut accumulated.api_call_count, delta.api_call_count),
            (
                &mut accumulated.user_message_count,
                delta.user_message_count,
            ),
            (
                &mut accumulated.assistant_message_count,
                delta.assistant_message_count,
            ),
            (
                &mut accumulated.tool_message_count,
                delta.tool_message_count,
            ),
            (
                &mut accumulated.total_input_tokens,
                delta.total_input_tokens,
            ),
            (
                &mut accumulated.total_output_tokens,
                delta.total_output_tokens,
            ),
            (
                &mut accumulated.total_cache_read_tokens,
                delta.total_cache_read_tokens,
            ),
            (
                &mut accumulated.total_cache_creation_tokens,
                delta.total_cache_creation_tokens,
            ),
            (
                &mut accumulated.total_thinking_tokens,
                delta.total_thinking_tokens,
            ),
            (
                &mut accumulated.grand_total_tokens,
                delta.grand_total_tokens,
            ),
            (
                &mut accumulated.total_content_chars,
                delta.total_content_chars,
            ),
            (&mut accumulated.total_tool_calls, delta.total_tool_calls),
        ] {
            anyhow::ensure!(
                contribution >= 0,
                "negative native OpenWebUI token contribution"
            );
            *total = total
                .checked_sub(contribution)
                .context("native OpenWebUI token contribution overflow")?;
        }
        anyhow::ensure!(
            delta.estimated_cost_usd.is_finite() && delta.estimated_cost_usd >= 0.0,
            "invalid native OpenWebUI token cost"
        );
        accumulated.estimated_cost_usd -= delta.estimated_cost_usd;
        anyhow::ensure!(
            accumulated.estimated_cost_usd.is_finite(),
            "native OpenWebUI token cost overflow"
        );
    }
    let day = previous_started
        .map(FrankenStorage::day_id_from_millis)
        .unwrap_or(0);
    token_stats
        .deltas
        .entry((
            day,
            conversation.agent_slug.clone(),
            conversation.source_id.clone(),
            session_family,
        ))
        .or_default()
        .session_count -= 1;
    franken_update_token_daily_stats_batched_in_tx(tx, &token_stats.expand())?;
    tx.execute_compat(
        "DELETE FROM token_usage WHERE conversation_id = ?1",
        fparams![conversation_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn codex_projection_fixture() -> (Conversation, Conversation) {
        let body = format!("\n{}尾\n", "λ".repeat(128 * 1024 + 19));
        let mut current = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: None,
            external_id: Some("original-rollout".into()),
            title: Some("Head".into()),
            source_path: PathBuf::from("/profile/.codex/sessions/original.jsonl"),
            started_at: None,
            ended_at: None,
            approx_tokens: None,
            metadata_json: serde_json::json!({"source":"rollout"}),
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: None,
                content: "Head".into(),
                extra_json: serde_json::json!({}),
                snippets: Vec::new(),
            }],
            source_id: "local".into(),
            origin_host: None,
        };
        for idx in 1..=2 {
            current.messages.push(Message {
                id: None, idx, role: MessageRole::Tool, author: None, created_at: None,
                content: format!("[Tool output: original-call-{idx}]\n{body}"),
                extra_json: serde_json::json!({"cass":{"tool_result_call_id":format!("original-call-{idx}")}}),
                snippets: Vec::new(),
            });
        }
        let mut tail = current.messages[0].clone();
        tail.idx = 3;
        tail.content = "Tail".into();
        current.messages.push(tail.clone());
        let mut legacy = current.clone();
        for message in legacy
            .messages
            .iter_mut()
            .filter(|message| message.role == MessageRole::Tool)
        {
            message.content = format!(
                "[Tool output: original-call-{}]\n{}\n[truncated 20 additional chars from tool output]",
                message.idx,
                "λ".repeat(128 * 1024),
            );
            message.extra_json = serde_json::json!({});
        }
        legacy.messages.pop();
        legacy.messages.push(Message {
            id: None,
            idx: 3,
            role: MessageRole::Tool,
            author: None,
            created_at: None,
            content: body,
            extra_json: serde_json::json!({}),
            snippets: Vec::new(),
        });
        tail.idx = 5;
        legacy.messages.push(tail);
        (legacy, current)
    }

    #[test]
    fn codex_legacy_projection_upgrade_preserves_ids_full_bodies_and_replay() -> Result<()> {
        for (materialized, enrich_identity) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let directory = TempDir::new()?;
            let storage = FrankenStorage::open(&directory.path().join("archive.db"))?;
            let agent = storage.ensure_agent(&Agent {
                id: None,
                slug: "codex".into(),
                name: "Codex".into(),
                version: None,
                kind: AgentKind::Cli,
            })?;
            let (legacy, mut current) = codex_projection_fixture();
            if enrich_identity {
                current.metadata_json["native_session_id"] =
                    serde_json::json!("original-native-session");
            }
            let initial = storage.insert_conversation_tree_with_analytics(
                agent,
                None,
                &legacy,
                !materialized,
            )?;
            let before = storage.fetch_messages(initial.conversation_id)?;
            assert_eq!(before.len(), 5);
            let completion = SourceIngestLedgerEntry {
                key: "source_ingest_v1:codex-projection-upgrade".into(),
                observation: "unchanged original native bytes".into(),
            };
            // Explicitly deferred callers must still repair already-published
            // analytics rather than leave the old projection counted.
            let outcome = storage
                .insert_conversations_batched_with_completion(
                    &[(agent, None, &current)],
                    true,
                    Some(&completion),
                )?
                .pop()
                .context("Codex repair outcome")?;
            assert_eq!(outcome.conversation_id, initial.conversation_id);
            assert!(outcome.inserted_indices.is_empty());
            assert_eq!(outcome.updated_indices, vec![0, 1, 2, 3]);
            let after = storage.fetch_messages(initial.conversation_id)?;
            assert_eq!(after.len(), 4);
            for ((actual, expected), old_position) in
                after.iter().zip(&current.messages).zip([0, 1, 2, 4])
            {
                assert_eq!(actual.id, before[old_position].id);
                assert_eq!(actual.content, expected.content);
                assert_eq!(actual.extra_json, expected.extra_json);
                assert_eq!(actual.idx, expected.idx);
            }
            assert_eq!(
                storage.source_ingest_ledger_entries()?.get(&completion.key),
                Some(&completion.observation)
            );
            let snapshot =
                storage.fetch_native_projection_lexical_snapshot(initial.conversation_id)?;
            assert_eq!(
                snapshot.metadata_json.get("native_session_id"),
                current.metadata_json.get("native_session_id")
            );
            assert_eq!(
                snapshot.metadata_json["codex_projection_lexical_indices"],
                serde_json::json!([0, 1, 2, 3, 5])
            );
            assert_eq!(
                snapshot.metadata_json["codex_compacted_projection_repair"]["retired_duplicate_projections"]
                    [0]["message_id"],
                before[3].id.unwrap()
            );
            for table in ["message_metrics", "token_usage"] {
                let (count, chars): (i64, i64) = storage.raw().query_row_map(
                    &format!("SELECT COUNT(*), COALESCE(SUM(content_chars),0) FROM {table}"),
                    fparams![],
                    |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
                )?;
                assert_eq!(count, if materialized { 4 } else { 0 }, "{table}");
                assert_eq!(
                    chars,
                    if materialized {
                        message_chars(&after)?
                    } else {
                        0
                    },
                    "{table}"
                );
            }
            if materialized {
                let (count, chars): (i64,i64) = storage.raw().query_row_map(
                    "SELECT SUM(message_count),SUM(total_chars) FROM daily_stats WHERE agent_slug='codex' AND source_id='local'",
                    fparams![], |row| Ok((row.get_typed(0)?,row.get_typed(1)?)),
                )?;
                assert_eq!((count, chars), (4, message_chars(&after)?));
            }
            let replay =
                storage.insert_conversation_tree_with_analytics(agent, None, &current, true)?;
            assert!(replay.inserted_indices.is_empty() && replay.updated_indices.is_empty());
            assert_eq!(
                serde_json::to_value(storage.fetch_messages(initial.conversation_id)?)?,
                serde_json::to_value(after)?
            );
        }
        Ok(())
    }

    #[test]
    fn codex_legacy_projection_metadata_conflicts_do_not_commit() -> Result<()> {
        for case in 0..10 {
            let directory = TempDir::new()?;
            let storage = FrankenStorage::open(&directory.path().join("archive.db"))?;
            let agent = storage.ensure_agent(&Agent {
                id: None,
                slug: "codex".into(),
                name: "Codex".into(),
                version: None,
                kind: AgentKind::Cli,
            })?;
            let (mut legacy, mut current) = codex_projection_fixture();
            current.metadata_json["native_session_id"] =
                serde_json::json!("original-native-session");
            match case {
                0 => {
                    legacy.metadata_json["native_session_id"] =
                        serde_json::json!("different-native-session");
                }
                1 => current.metadata_json["native_session_id"] = serde_json::json!(""),
                2 => current.metadata_json["native_session_id"] = serde_json::json!(" \t"),
                3 => current.metadata_json["native_session_id"] = serde_json::json!(17),
                4 => {
                    current.metadata_json["native_session_id"] =
                        serde_json::json!("x".repeat(4097));
                }
                5 => current.metadata_json["source"] = serde_json::json!("changed-source"),
                6 => {
                    legacy.metadata_json["retained"] = serde_json::json!("original metadata");
                }
                7 => current.metadata_json["unrelated"] = serde_json::json!("new metadata"),
                8 => current.title = Some("Changed title".into()),
                9 => current.started_at = Some(1),
                _ => unreachable!(),
            }
            let initial = storage.insert_conversation_tree(agent, None, &legacy)?;
            let before = storage
                .list_conversations(1, 0)?
                .pop()
                .context("legacy canonical conversation")?;
            let before_messages =
                serde_json::to_value(storage.fetch_messages(initial.conversation_id)?)?;
            let completion = SourceIngestLedgerEntry {
                key: format!("source_ingest_v1:codex-metadata-conflict-{case}"),
                observation: "must not advance".into(),
            };
            let error = storage
                .insert_conversations_batched_with_completion(
                    &[(agent, None, &current)],
                    true,
                    Some(&completion),
                )
                .err()
                .context("conflicting canonical metadata must reject the repair")?;
            assert!(
                error
                    .to_string()
                    .contains("legacy Codex canonical metadata changed"),
                "case {case}: {error:#}"
            );
            let after = storage
                .list_conversations(1, 0)?
                .pop()
                .context("retained legacy canonical conversation")?;
            assert_eq!(
                serde_json::to_value(after)?,
                serde_json::to_value(before)?,
                "case {case}"
            );
            assert_eq!(
                serde_json::to_value(storage.fetch_messages(initial.conversation_id)?)?,
                before_messages,
                "case {case}"
            );
            assert!(
                !storage
                    .source_ingest_ledger_entries()?
                    .contains_key(&completion.key),
                "case {case}"
            );
        }
        Ok(())
    }

    #[test]
    fn codex_legacy_projection_refuses_missing_or_changed_evidence() -> Result<()> {
        let (mut legacy, current) = codex_projection_fixture();
        for (position, message) in legacy.messages.iter_mut().enumerate() {
            message.id = Some(i64::try_from(position)? + 1);
        }
        assert!(codex_projection_plan(&current, &legacy.messages)?.is_some());
        for case in 0..9 {
            let mut incoming = current.clone();
            let mut previous = legacy.messages.clone();
            match case {
                0 => {
                    previous.remove(3);
                }
                1 => previous[3].content.push_str("changed omitted source text"),
                2 => previous[1].content.push('!'),
                3 => previous[1].extra_json = serde_json::json!({"retained":"original metadata"}),
                4 => previous[3].extra_json = serde_json::json!({"retained":"original metadata"}),
                5 => incoming.messages[1].created_at = Some(1),
                6 => {
                    incoming.messages.pop();
                }
                7 => {
                    incoming.messages[2].extra_json["cass"]["tool_result_call_id"] =
                        serde_json::json!("original-call-1")
                }
                8 => previous[4].content = "different original user message".into(),
                _ => unreachable!(),
            }
            assert!(
                codex_projection_plan(&incoming, &previous)?.is_none(),
                "case {case}"
            );
        }
        Ok(())
    }

    struct NativeFixture {
        _directory: TempDir,
        storage: FrankenStorage,
        agent_id: i64,
        workspace_id: i64,
        conversation: Conversation,
    }

    impl NativeFixture {
        fn new() -> Result<Self> {
            let directory = TempDir::new()?;
            let storage = FrankenStorage::open(&directory.path().join("archive.db"))?;
            let agent_id = storage.ensure_agent(&Agent {
                id: None,
                slug: "openwebui".to_owned(),
                name: "OpenWebUI".to_owned(),
                version: None,
                kind: AgentKind::Hybrid,
            })?;
            let workspace = PathBuf::from("/work/webui");
            let workspace_id = storage.ensure_workspace(&workspace, None)?;
            let conversation = Conversation {
                id: None, agent_slug: "openwebui".to_owned(), workspace: Some(workspace),
                external_id: Some("[\"original-instance\",\"original-chat\"]".to_owned()),
                title: Some("Native snapshot".to_owned()), source_path: directory.path().join("snapshot.json"),
                started_at: Some(1_700_000_000_000), ended_at: None, approx_tokens: None,
                metadata_json: serde_json::json!({
                    "schema_version": "openwebui.saved-conversation.v1", "instance_id": "original-instance",
                    "conversation_id": "original-chat", "user_id": "original-owner",
                    "workspace_root": "/work/webui", "source_database": "/work/webui/webui.db"
                }),
                messages: (0..2).map(|index| Message {
                    id: None, idx: index, role: MessageRole::User, author: None,
                    created_at: Some(1_700_000_000_000 + index), content: format!("Original message {index}"),
                    extra_json: serde_json::json!({
                        "instance_id": "original-instance", "conversation_id": "original-chat", "user_id": "original-owner",
                        "openwebui": {"id": format!("native-{index}"), "message": {"id": format!("native-{index}"), "done": false}}
                    }), snippets: Vec::new(),
                }).collect(),
                source_id: "local".to_owned(), origin_host: None,
            };
            Ok(Self {
                _directory: directory,
                storage,
                agent_id,
                workspace_id,
                conversation,
            })
        }

        fn insert(&self, conversation: &Conversation, batched: bool) -> Result<InsertOutcome> {
            if batched {
                self.storage
                    .insert_conversations_batched_with_analytics(
                        &[(self.agent_id, Some(self.workspace_id), conversation)],
                        false,
                    )?
                    .pop()
                    .context("native batch outcome")
            } else {
                self.storage.insert_conversation_tree_with_analytics(
                    self.agent_id,
                    Some(self.workspace_id),
                    conversation,
                    false,
                )
            }
        }

        fn assert_analytics(&self, conversation_id: i64) -> Result<()> {
            let messages = self.storage.fetch_messages(conversation_id)?;
            let expected_count = i64::try_from(messages.len())?;
            let expected_chars = message_chars(&messages)?;
            let daily: (i64, i64, i64) = self.storage.raw().query_row_map(
                "SELECT SUM(session_count), SUM(message_count), SUM(total_chars) FROM daily_stats WHERE agent_slug='openwebui' AND source_id='local'",
                fparams![], |row| Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?)),
            )?;
            assert_eq!(daily, (1, expected_count, expected_chars));
            for table in ["message_metrics", "token_usage"] {
                let actual: (i64, i64) = self.storage.raw().query_row_map(
                    &format!("SELECT COUNT(*), SUM(content_chars) FROM {table}"),
                    fparams![],
                    |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
                )?;
                assert_eq!(actual, (expected_count, expected_chars), "{table}");
            }
            let tokens: (i64, i64, i64) = self.storage.raw().query_row_map(
                "SELECT SUM(session_count), SUM(api_call_count), SUM(total_content_chars) FROM token_daily_stats WHERE agent_slug='openwebui' AND source_id='local' AND model_family='all'",
                fparams![], |row| Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?)),
            )?;
            assert_eq!(tokens, (1, expected_count, expected_chars));
            for table in ["usage_hourly", "usage_daily", "usage_models_daily"] {
                let actual: i64 = self.storage.raw().query_row_map(
                    &format!("SELECT SUM(message_count) FROM {table} WHERE agent_slug='openwebui' AND source_id='local'"),
                    fparams![], |row| row.get_typed(0),
                )?;
                assert_eq!(actual, expected_count, "{table}");
            }
            let expected_tail = messages
                .iter()
                .filter_map(|message| message.created_at)
                .max();
            for table in [
                "conversations",
                "conversation_tail_state",
                "conversation_external_tail_lookup",
            ] {
                let actual: (Option<i64>, Option<i64>) = self.storage.raw().query_row_map(
                    &format!("SELECT ended_at, last_message_created_at FROM {table}"),
                    fparams![],
                    |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
                )?;
                assert_eq!(actual, (None, expected_tail), "{table}");
            }
            Ok(())
        }
    }

    fn reset_indices(conversation: &mut Conversation) -> Result<()> {
        for (index, message) in conversation.messages.iter_mut().enumerate() {
            message.idx = i64::try_from(index)?;
        }
        Ok(())
    }

    #[test]
    fn native_snapshot_edits_reorder_retention_and_analytics_converge() -> Result<()> {
        for batched in [false, true] {
            let fixture = NativeFixture::new()?;
            let mut snapshot = fixture.conversation.clone();
            let initial = fixture.insert(&snapshot, batched)?;
            let original = fixture.storage.fetch_messages(initial.conversation_id)?;
            let original_ids: HashMap<String, Option<i64>> = original
                .iter()
                .map(|message| Ok((message_identity(&snapshot, message)?.to_owned(), message.id)))
                .collect::<Result<_>>()?;
            fixture.assert_analytics(initial.conversation_id)?;
            snapshot.messages.swap(0, 1);
            snapshot.started_at = Some(1_699_913_600_000);
            snapshot.title = Some("Edited native snapshot".to_owned());
            for message in &mut snapshot.messages {
                message.created_at = snapshot.started_at;
            }
            let edited = snapshot.messages.first_mut().context("edited message")?;
            edited.content = "Replacement content with corrected tool output".to_owned();
            edited.role = MessageRole::Tool;
            edited.author = Some("headless-tool".to_owned());
            edited.extra_json["openwebui"]["message"]["done"] = serde_json::json!(true);
            reset_indices(&mut snapshot)?;
            let changed = fixture.insert(&snapshot, batched)?;
            assert!(changed.inserted_indices.is_empty());
            assert_eq!(changed.updated_indices, vec![0, 1]);
            let stored = fixture.storage.fetch_messages(initial.conversation_id)?;
            for (expected, actual) in snapshot.messages.iter().zip(&stored) {
                assert!(same_message(expected, actual));
                assert_eq!(
                    actual.id,
                    original_ids[message_identity(&snapshot, actual)?]
                );
            }
            fixture.assert_analytics(initial.conversation_id)?;
            let returned_message = snapshot
                .messages
                .first()
                .context("returning message")?
                .clone();
            let mut appended = returned_message.clone();
            appended.content = "Appended native output".to_owned();
            appended.extra_json["openwebui"]["id"] = serde_json::json!("appended-native");
            appended.extra_json["openwebui"]["message"]["id"] =
                serde_json::json!("appended-native");
            snapshot.messages.push(appended);
            reset_indices(&mut snapshot)?;
            assert_eq!(
                fixture.insert(&snapshot, batched)?.inserted_indices,
                vec![2]
            );
            fixture.assert_analytics(initial.conversation_id)?;
            snapshot
                .messages
                .retain(|message| message.extra_json["openwebui"]["id"] != "native-1");
            reset_indices(&mut snapshot)?;
            let retained = fixture.insert(&snapshot, batched)?;
            assert!(retained.inserted_indices.is_empty());
            let mut stored = fixture
                .storage
                .fetch_native_projection_lexical_snapshot(initial.conversation_id)?;
            stored.messages = fixture.storage.fetch_messages(initial.conversation_id)?;
            assert_eq!(
                stored.metadata_json["retained_absent_message_ids"],
                serde_json::json!(["native-1"])
            );
            assert_eq!(
                stored
                    .messages
                    .last()
                    .context("retained message")?
                    .extra_json["cass_openwebui_source_present"],
                false
            );
            assert_eq!(
                stored.messages.last().context("retained ID")?.id,
                original_ids["native-1"]
            );
            fixture.assert_analytics(initial.conversation_id)?;
            let replay = fixture.insert(&snapshot, batched)?;
            assert!(replay.inserted_indices.is_empty() && replay.updated_indices.is_empty());
            snapshot.messages.push(returned_message);
            reset_indices(&mut snapshot)?;
            assert!(
                fixture
                    .insert(&snapshot, batched)?
                    .inserted_indices
                    .is_empty()
            );
            let mut stored = fixture
                .storage
                .fetch_native_projection_lexical_snapshot(initial.conversation_id)?;
            stored.messages = fixture.storage.fetch_messages(initial.conversation_id)?;
            assert_eq!(
                stored.metadata_json["retained_absent_message_ids"],
                serde_json::json!([])
            );
            assert!(
                stored
                    .messages
                    .last()
                    .context("returned message")?
                    .extra_json
                    .get("cass_openwebui_source_present")
                    .is_none()
            );
            let replay = fixture.insert(&snapshot, batched)?;
            assert!(replay.inserted_indices.is_empty() && replay.updated_indices.is_empty());
            fixture.assert_analytics(initial.conversation_id)?;
        }
        Ok(())
    }

    #[test]
    fn native_snapshot_identity_changes_and_invalid_batches_leave_canonical_rows_unchanged()
    -> Result<()> {
        let fixture = NativeFixture::new()?;
        let initial = fixture.insert(&fixture.conversation, true)?;
        let original =
            serde_json::to_value(fixture.storage.fetch_messages(initial.conversation_id)?)?;
        for key in ["user_id", "workspace_root", "source_database"] {
            let mut changed = fixture.conversation.clone();
            changed.metadata_json[key] = serde_json::json!("different binding");
            if key == "user_id" {
                for message in &mut changed.messages {
                    message.extra_json[key] = serde_json::json!("different binding");
                }
            }
            assert!(fixture.insert(&changed, true).is_err(), "accepted {key}");
        }
        let mut moved = fixture.conversation.clone();
        moved.source_path = PathBuf::from("/different/snapshot.json");
        assert!(fixture.insert(&moved, true).is_err());
        let other_workspace = fixture
            .storage
            .ensure_workspace(Path::new("/work/other"), None)?;
        assert!(
            fixture
                .storage
                .insert_conversations_batched_with_analytics(
                    &[(
                        fixture.agent_id,
                        Some(other_workspace),
                        &fixture.conversation
                    )],
                    false,
                )
                .is_err()
        );
        assert!(
            fixture
                .storage
                .insert_conversations_batched_with_analytics(
                    &[
                        (
                            fixture.agent_id,
                            Some(fixture.workspace_id),
                            &fixture.conversation
                        ),
                        (
                            fixture.agent_id,
                            Some(fixture.workspace_id),
                            &fixture.conversation
                        )
                    ],
                    false,
                )
                .is_err()
        );
        let mut invalid = fixture.conversation.clone();
        invalid.messages.first_mut().context("invalid message")?.idx = 100;
        assert!(fixture.insert(&invalid, true).is_err());
        let mut duplicate = fixture.conversation.clone();
        duplicate.messages = vec![
            duplicate
                .messages
                .first()
                .context("duplicate source")?
                .clone();
            2
        ];
        reset_indices(&mut duplicate)?;
        assert!(fixture.insert(&duplicate, true).is_err());
        assert_eq!(
            serde_json::to_value(fixture.storage.fetch_messages(initial.conversation_id)?)?,
            original
        );
        fixture.assert_analytics(initial.conversation_id)?;
        Ok(())
    }

    #[test]
    fn native_snapshot_corrupt_token_contribution_rolls_back_without_panicking() -> Result<()> {
        let fixture = NativeFixture::new()?;
        let initial = fixture.insert(&fixture.conversation, true)?;
        let original = fixture.storage.fetch_messages(initial.conversation_id)?;
        let message_id = original
            .first()
            .context("first message")?
            .id
            .context("canonical ID")?;
        fixture.storage.raw().execute_compat(
            "UPDATE token_usage SET input_tokens=?1 WHERE message_id=?2",
            fparams![i64::MIN, message_id],
        )?;
        let mut changed = fixture.conversation.clone();
        changed
            .messages
            .first_mut()
            .context("edited message")?
            .content = "Must not commit".to_owned();
        let error = fixture
            .insert(&changed, true)
            .err()
            .context("accepted invalid token contribution")?;
        assert!(
            error
                .to_string()
                .contains("negative native OpenWebUI token contribution"),
            "{error:#}"
        );
        assert_eq!(
            serde_json::to_value(fixture.storage.fetch_messages(initial.conversation_id)?)?,
            serde_json::to_value(original)?
        );
        fixture.assert_analytics(initial.conversation_id)?;
        Ok(())
    }
}
