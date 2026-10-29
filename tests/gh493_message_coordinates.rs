//! Search ordinals and physical lines must never be silently interchanged.
use coding_agent_search::franken_sync::compat::{
    ConnectionExt, OpenFlags, RowExt, open_with_flags,
};
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

// Rebased onto cass.pack.v2: keep its one-based selectors and every previously
// landed regression while checking the remaining session-identity boundaries.
mod pack_identity_regressions {
    use super::*;
    use coding_agent_search::search::pack_planner::{
        PackCandidate, PackFreshnessPolicy, PackOmittedReason, PackPlanRequest, PackPlannerLimits,
        PackRenderFormat, PackRenderRequest, PlannedAnswerPack, plan_answer_pack,
        render_answer_pack_value_without_trust_correlation,
        render_answer_pack_without_trust_correlation,
    };
    use coding_agent_search::search::query::{MatchType, SearchHit};
    use std::collections::HashSet;
    use std::time::Duration;

    fn hit(conversation_id: Option<i64>, number: Option<usize>, content_hash: u64) -> SearchHit {
        SearchHit {
            title: "pack identity".into(),
            message_anchor: None,
            snippet: format!("distinct evidence {content_hash}"),
            content: format!("distinct evidence {content_hash}"),
            content_hash,
            conversation_id,
            score: 1.0,
            source_path: "/shared/provider.db".into(),
            agent: "codex".into(),
            workspace: "/work".into(),
            workspace_original: None,
            created_at: Some(1_000_000),
            line_number: number,
            match_type: MatchType::Exact,
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
        }
    }

    fn candidate(hit: &SearchHit) -> PackCandidate {
        PackCandidate::from_search_hit(hit, 1, 0)
    }

    fn plan(candidates: Vec<PackCandidate>, max_sessions: usize) -> PlannedAnswerPack {
        plan_answer_pack(PackPlanRequest {
            now_ms: 1_000_000,
            candidates,
            limits: PackPlannerLimits {
                max_sessions,
                ..PackPlannerLimits::default()
            },
            freshness_policy: PackFreshnessPolicy::AllowStale,
            ..PackPlanRequest::default()
        })
        .unwrap()
    }

    fn evidence_id(candidate: PackCandidate) -> String {
        let pack = plan(vec![candidate], 1);
        assert_eq!(pack.evidence.len(), 1);
        pack.evidence[0].id.clone()
    }

    #[test]
    fn pack_session_limits_distinguish_conversations_in_one_provider_file() {
        let first = candidate(&hit(Some(42), Some(8), 1));
        let second = candidate(&hit(Some(43), Some(8), 2));
        let mut candidates = vec![first.clone(), second.clone()];
        let full = plan(candidates.clone(), 2);
        assert_eq!(full.selected_evidence_count, 2);
        assert_eq!(full.selected_session_count, 2);
        assert_ne!(first.candidate_id, second.candidate_id);
        candidates.reverse();
        assert_eq!(plan(candidates, 2), full, "input order is not identity");

        let limited = plan(vec![first.clone(), second], 1);
        assert_eq!(limited.selected_evidence_count, 1);
        assert_eq!(limited.selected_session_count, 1);
        assert_eq!(limited.omitted.len(), 1);
        assert_eq!(
            limited.omitted[0].reason,
            PackOmittedReason::MaxSessionsReached
        );

        let same_session = candidate(&hit(Some(42), Some(13), 3));
        let pack = plan(vec![first, same_session], 1);
        assert_eq!(pack.selected_evidence_count, 2);
        assert_eq!(pack.selected_session_count, 1);
    }

    #[test]
    fn pack_unverified_spans_do_not_suppress_canonical_evidence_but_content_dedup_remains() {
        let mut first = candidate(&hit(Some(42), Some(8), 1));
        first.line_start = Some(10);
        first.line_end = Some(12);
        let mut remote_hit = hit(Some(42), Some(8), 2);
        remote_hit.source_id = "remote-host".into();
        remote_hit.origin_kind = "ssh".into();
        let mut remote = candidate(&remote_hit);
        remote.line_start = first.line_start;
        remote.line_end = first.line_end;
        let mut sibling = candidate(&hit(Some(43), Some(8), 3));
        sibling.line_start = first.line_start;
        sibling.line_end = first.line_end;
        let pack = plan(vec![first.clone(), remote, sibling], 3);
        assert_eq!(pack.selected_evidence_count, 3);
        assert_eq!(pack.selected_session_count, 3);

        let mut same_session = candidate(&hit(Some(42), Some(13), 4));
        same_session.line_start = Some(12);
        same_session.line_end = Some(14);
        let unverified = plan(vec![first.clone(), same_session.clone()], 3);
        assert_eq!(unverified.selected_evidence_count, 2);
        assert!(unverified.evidence.iter().all(|item| {
            item.candidate.line_start.is_none() && item.candidate.line_end.is_none()
        }));
        let mut verified_first = first.clone();
        verified_first.citation_verified = true;
        same_session.citation_verified = true;
        let overlapping = plan(vec![verified_first, same_session], 3);
        assert_eq!(overlapping.selected_evidence_count, 1);
        assert_eq!(
            overlapping.omitted[0].reason,
            PackOmittedReason::DuplicateContent
        );

        // Exact content dedup is a separate policy, not a session-identity test.
        let identical_content = candidate(&hit(Some(99), Some(100), 1));
        let duplicate = plan(vec![first, identical_content], 3);
        assert_eq!(duplicate.selected_evidence_count, 1);
        assert_eq!(
            duplicate.omitted[0].reason,
            PackOmittedReason::DuplicateContent
        );
    }

    #[test]
    fn pack_content_dedup_chooses_a_stable_known_canonical_anchor_under_ties() {
        let first = candidate(&hit(Some(42), Some(8), 1));
        let second = candidate(&hit(Some(43), Some(8), 1));
        let forward = plan(vec![first.clone(), second.clone()], 2);
        let reverse = plan(vec![second, first.clone()], 2);
        assert_eq!(
            forward, reverse,
            "text ties must not pick input-order-dependent citations"
        );
        assert_eq!(forward.evidence[0].candidate.conversation_id, Some(42));
        let unknown = candidate(&hit(None, Some(8), 1));
        for candidates in [vec![unknown.clone(), first.clone()], vec![first, unknown]] {
            let selected = plan(candidates, 2);
            assert_eq!(selected.evidence[0].candidate.conversation_id, Some(42));
        }
    }

    #[test]
    fn pack_invalid_physical_spans_cannot_gain_authority_or_erase_canonical_evidence() {
        let clean = candidate(&hit(Some(42), Some(8), 1));
        let clean_pack = plan(vec![clean.clone()], 1);
        let clean_id = clean_pack.evidence[0].id.clone();
        for (verified, start, end) in [
            (false, Some(10), Some(12)),
            (true, None, Some(12)),
            (true, Some(0), Some(12)),
            (true, Some(12), Some(10)),
            (true, None, None),
        ] {
            let mut invalid = clean.clone();
            invalid.citation_verified = verified;
            invalid.line_start = start;
            invalid.line_end = end;
            let mut other = candidate(&hit(Some(43), Some(13), 2));
            other.citation_verified = true;
            other.line_start = Some(10);
            other.line_end = Some(12);
            for items in [
                vec![invalid.clone(), other.clone()],
                vec![other, invalid.clone()],
            ] {
                let selected = plan(items, 2);
                assert_eq!(selected.selected_evidence_count, 2);
                let retained = selected
                    .evidence
                    .iter()
                    .find(|item| item.candidate.conversation_id == Some(42))
                    .unwrap();
                assert_eq!(retained.id, clean_id);
                assert_eq!(retained.selection.citation_quality_score, 0.75);
                assert_eq!(retained.candidate.message_index, Some(8));
                assert!(!retained.candidate.citation_verified);
                assert!(retained.candidate.line_start.is_none());
                assert!(retained.candidate.line_end.is_none());
                let output = render_answer_pack_value_without_trust_correlation(
                    &selected,
                    &PackRenderRequest::default(),
                )
                .unwrap();
                let citation = &output["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|item| item["citation"]["conversation_id"] == 42)
                    .unwrap()["citation"];
                assert_eq!(citation["message_index"], 8);
                assert_eq!(citation["message_index_base"], 1);
                assert_eq!(citation["verified"], false);
                assert!(citation["line_start"].is_null());
                assert!(citation["line_end"].is_null());
            }
        }
    }

    #[test]
    fn pack_verified_file_overlap_is_independent_of_canonical_provider_and_conversation() {
        let mut first = candidate(&hit(Some(42), Some(8), 1));
        first.citation_verified = true;
        first.line_start = Some(10);
        first.line_end = Some(12);
        let mut sibling = candidate(&hit(Some(43), Some(13), 2));
        sibling.agent = "claude_code".into();
        sibling.citation_verified = true;
        sibling.line_start = Some(12);
        // Missing end means a single verified physical line, not a dense
        // canonical message position and not an unknown start.
        sibling.line_end = None;
        for items in [
            vec![first.clone(), sibling.clone()],
            vec![sibling.clone(), first.clone()],
        ] {
            let selected = plan(items, 2);
            assert_eq!(selected.selected_evidence_count, 1);
            assert_eq!(
                selected.omitted[0].reason,
                PackOmittedReason::DuplicateContent
            );
        }
        sibling.line_start = Some(13);
        assert_eq!(
            plan(vec![first.clone(), sibling.clone()], 2).selected_evidence_count,
            2
        );
        sibling.line_start = Some(12);
        sibling.source_id = "another-host".into();
        assert_eq!(
            plan(vec![first.clone(), sibling.clone()], 2).selected_evidence_count,
            2
        );
        sibling.source_id = first.source_id.clone();
        sibling.source_path = "/different/provider.db".into();
        assert_eq!(plan(vec![first, sibling], 2).selected_evidence_count, 2);
    }

    #[test]
    fn pack_ids_bind_canonical_coordinates_and_frame_untrusted_strings() {
        let base = hit(Some(42), Some(8), 7);
        let mut variants = vec![base.clone()];
        for conversation in [None, Some(0), Some(43)] {
            let mut variant = base.clone();
            variant.conversation_id = conversation;
            variants.push(variant);
        }
        for number in [None, Some(1), Some(13)] {
            let mut variant = base.clone();
            variant.line_number = number;
            variants.push(variant);
        }
        let mut other_agent = base.clone();
        other_agent.agent = "claude_code".into();
        variants.push(other_agent);
        for (source, path) in [("a:b", "c"), ("a", "b:c"), ("a\nb", "c"), ("a", "b\nc")] {
            let mut variant = base.clone();
            variant.source_id = source.into();
            variant.source_path = path.into();
            variants.push(variant);
        }
        let mut candidates = HashSet::new();
        let mut evidence = HashSet::new();
        for variant in variants {
            let item = candidate(&variant);
            assert!(
                candidates.insert(item.candidate_id.clone()),
                "candidate identity aliased"
            );
            let id = evidence_id(item);
            assert!(evidence.insert(id.clone()), "citation identity aliased");
            assert_eq!(evidence_id(candidate(&variant)), id);
            assert!(id.starts_with("ev_"));
            assert_eq!(id.len(), 55);
        }
        let first = candidate(&hit(Some(42), Some(1), 7));
        let unknown = candidate(&hit(Some(42), None, 7));
        assert_eq!(first.message_index, Some(1));
        assert_eq!(unknown.message_index, None);
        assert_ne!(evidence_id(first), evidence_id(unknown));

        let mut span = candidate(&base);
        let absent = evidence_id(span.clone());
        span.line_start = Some(0);
        span.line_end = Some(0);
        assert_eq!(
            evidence_id(span.clone()),
            absent,
            "invalid physical spans are not evidence"
        );
        span.line_start = Some(10);
        span.line_end = Some(12);
        span.citation_verified = true;
        assert_ne!(
            evidence_id(span.clone()),
            absent,
            "a verified span is part of citation identity"
        );
        let first = evidence_id(span.clone());
        span.conversation_id = Some(43);
        assert_ne!(evidence_id(span), first);
    }

    #[test]
    fn pack_redacted_paths_do_not_collapse_session_counts_or_leak_private_keys() {
        // Unknown IDs make path preservation essential: distinct known IDs would
        // mask the redacted-path collision in the old source-summary code.
        for conversations in [[None, None], [Some(42), Some(43)]] {
            let mut one = hit(conversations[0], Some(8), 1);
            one.source_path = "/home/alice/history.jsonl".into();
            let mut two = hit(conversations[1], Some(8), 2);
            two.source_path = "/home/bob/history.jsonl".into();
            let pack = plan(vec![candidate(&one), candidate(&two)], 2);
            let mut render = PackRenderRequest::default();
            let value = render_answer_pack_value_without_trust_correlation(&pack, &render).unwrap();
            assert_eq!(value["schema_version"], "cass.pack.v2");
            assert_eq!(value["realized"]["selected_session_count"], 2);
            assert_eq!(value["pack"]["source_summary"][0]["session_count"], 2);
            assert_eq!(
                value["evidence"][0]["citation"]["source_path"],
                value["evidence"][1]["citation"]["source_path"]
            );
            let encoded = value.to_string();
            for private in ["/home/alice", "/home/bob", "session_key"] {
                assert!(
                    !encoded.contains(private),
                    "private accounting leaked: {private}"
                );
            }
            render.format = PackRenderFormat::Markdown;
            let markdown = render_answer_pack_without_trust_correlation(&pack, &render).unwrap();
            assert!(markdown.contains("message_index=8 (1-based)"));
            if conversations[0].is_some() {
                assert!(markdown.contains("conversation_id=42"));
                assert!(markdown.contains("conversation_id=43"));
            }
            assert!(!markdown.contains("/home/alice"));
            assert!(!markdown.contains("/home/bob"));
            assert!(value["evidence"].as_array().unwrap().iter().all(|item| {
                item["citation"]["message_index"] == 8
                    && item["citation"]["message_index_base"] == 1
                    && item["citation"]["line_start"].is_null()
            }));
        }
    }

    #[test]
    fn pack_unknown_conversations_keep_provider_identity_without_inventing_a_number() {
        let one = hit(None, Some(8), 1);
        let mut two = hit(None, Some(13), 2);
        two.agent = "claude_code".into();
        let pack = plan(vec![candidate(&one), candidate(&two)], 2);
        assert_eq!(pack.selected_session_count, 2);
        let render = PackRenderRequest {
            format: PackRenderFormat::Markdown,
            ..PackRenderRequest::default()
        };
        let markdown = render_answer_pack_without_trust_correlation(&pack, &render).unwrap();
        assert!(!markdown.contains("conversation_id="));
        let value = render_answer_pack_value_without_trust_correlation(&pack, &render).unwrap();
        assert_eq!(value["pack"]["source_summary"][0]["session_count"], 2);
        assert!(
            value["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| { item["citation"]["conversation_id"].is_null() })
        );
    }

    fn bounded_output(command: Command) -> Output {
        assert_cmd::Command::from_std(command)
            .timeout(Duration::from_secs(30))
            .output()
            .unwrap()
    }

    #[test]
    fn actual_pack_shared_path_limits_and_citations_round_trip_to_their_own_messages() {
        let fixture = Fixture::new(&[0, 7, 12]);
        let storage = FrankenStorage::open(&fixture.db).unwrap();
        let other = seed(&storage, &fixture.path, "codex", &[0, 7, 12]);
        let agents = storage
            .raw()
            .query_map_collect(
                "SELECT agent_id FROM conversations WHERE id = ?1",
                coding_agent_search::franken_sync::params![fixture.conversation_id],
                |row| row.get_typed::<i64>(0),
            )
            .unwrap();
        storage
            .raw()
            .execute_compat(
                "UPDATE conversations SET agent_id = ?1 WHERE id = ?2",
                coding_agent_search::franken_sync::params![agents[0], other],
            )
            .unwrap();
        let expected = [
            (
                fixture.conversation_id,
                "PACK493IDENTITY alpha conversation evidence",
            ),
            (other, "PACK493IDENTITY beta conversation evidence"),
        ];
        for (conversation_id, content) in expected {
            storage
                .raw()
                .execute_compat(
                    "UPDATE messages SET content = ?1 WHERE conversation_id = ?2 AND idx = 7",
                    coding_agent_search::franken_sync::params![content, conversation_id],
                )
                .unwrap();
        }
        drop(storage);
        let retained_source = fixture._root.path().join("retained-source.jsonl");
        std::fs::rename(&fixture.path, &retained_source).unwrap();
        for (conversation_id, _) in expected {
            let mut index = fixture.command("index");
            index
                .arg("--data-dir")
                .arg(&fixture.data)
                .arg("--reconcile-conversation")
                .arg(conversation_id.to_string())
                .arg("--json");
            decode(bounded_output(index));
        }
        let db_before = std::fs::read(&fixture.db).unwrap();
        let source_before = std::fs::read(&retained_source).unwrap();
        for max_sessions in [1_usize, 2] {
            let mut command = fixture.command("pack");
            command
                .args([
                    "PACK493IDENTITY",
                    "--mode",
                    "lexical",
                    "--json",
                    "--freshness-policy",
                    "allow-stale",
                    "--require-evidence",
                    "--max-sessions",
                    &max_sessions.to_string(),
                    "--max-evidence",
                    "4",
                    "--max-tokens",
                    "12000",
                    "--timeout",
                    "20000",
                ])
                .arg("--data-dir")
                .arg(&fixture.data);
            let payload = decode(bounded_output(command));
            assert_eq!(payload["schema_version"], "cass.pack.v2");
            let evidence = payload["evidence"].as_array().unwrap();
            assert_eq!(evidence.len(), max_sessions, "{payload}");
            assert_eq!(payload["realized"]["selected_session_count"], max_sessions);
            assert_eq!(
                payload["pack"]["source_summary"][0]["session_count"],
                max_sessions
            );
            let mut identities = HashSet::new();
            let mut ids = HashSet::new();
            for item in evidence {
                let citation = &item["citation"];
                let conversation = citation["conversation_id"].as_i64().unwrap();
                assert!(identities.insert(conversation));
                assert!(ids.insert(item["id"].as_str().unwrap()));
                assert_eq!(citation["message_index"], 8);
                assert_eq!(citation["message_index_base"], 1);
                assert!(citation["line_start"].is_null());
                assert_eq!(citation["verified"], false);
                let content = expected
                    .iter()
                    .find(|(id, _)| *id == conversation)
                    .unwrap()
                    .1;
                assert_eq!(item["excerpt"], content);
                for followup in ["view", "expand"] {
                    let mut command = fixture.command(followup);
                    let number = citation["message_index"].as_u64().unwrap().to_string();
                    command
                        .arg(citation["source_path"].as_str().unwrap())
                        .args([
                            "--source",
                            citation["source_id"].as_str().unwrap(),
                            "--conversation-id",
                            &conversation.to_string(),
                            "--message-index",
                            &number,
                            "--message-anchor",
                            citation["message_anchor"].as_str().unwrap(),
                            "-C",
                            "0",
                            "--json",
                        ]);
                    let payload = decode(bounded_output(command));
                    let rows = if followup == "view" {
                        &payload["lines"]
                    } else {
                        &payload
                    };
                    assert_eq!(rows.as_array().unwrap().len(), 1);
                    assert_eq!(rows[0]["conversation_id"], conversation);
                    assert_eq!(rows[0]["message_index"], 8);
                    assert_eq!(rows[0]["is_target"], true);
                    assert_eq!(rows[0]["content"], content);
                }
            }
        }
        assert!(!fixture.path.exists());
        assert_eq!(std::fs::read(&fixture.db).unwrap(), db_before);
        assert_eq!(std::fs::read(retained_source).unwrap(), source_before);
    }
}

struct Fixture {
    _root: TempDir,
    db: PathBuf,
    data: PathBuf,
    path: PathBuf,
    conversation_id: i64,
}

fn seed(storage: &FrankenStorage, path: &Path, agent: &str, indices: &[i64]) -> i64 {
    let agent_id = storage
        .ensure_agent(&Agent {
            id: None,
            slug: agent.into(),
            name: agent.into(),
            version: None,
            kind: AgentKind::Cli,
        })
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            None,
            &Conversation {
                id: None,
                agent_slug: agent.into(),
                workspace: None,
                external_id: Some(format!("gh493-{agent}")),
                title: Some("GH493 message anchor regression".into()),
                source_path: path.into(),
                started_at: Some(1_733_000_000_000),
                ended_at: Some(1_733_000_002_000),
                approx_tokens: None,
                metadata_json: json!({}),
                messages: indices
                    .iter()
                    .enumerate()
                    .map(|(position, &idx)| Message {
                        id: None,
                        idx,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: Some(1_733_000_000_000 + position as i64),
                        content: if position == 1 {
                            "ANCHOR493TARGET"
                        } else {
                            "canonical neighbour"
                        }
                        .into(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    })
                    .collect(),
                source_id: "local".into(),
                origin_host: None,
            },
        )
        .unwrap();
    let ids = storage
        .raw()
        .query_map_collect(
            "SELECT id FROM conversations WHERE agent_id = ?1",
            coding_agent_search::franken_sync::params![agent_id],
            |row| row.get_typed::<i64>(0),
        )
        .unwrap();
    assert_eq!(ids.len(), 1);
    ids[0]
}

impl Fixture {
    fn new(indices: &[i64]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("cass");
        std::fs::create_dir(&data).unwrap();
        let db = data.join("agent_search.db");
        let path = root.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"queue-operation\"}\n",
                "{\"role\":\"assistant\",\"content\":\"PLAUSIBLE WRONG MESSAGE\"}\n",
                "\n",
                "{\"type\":\"attachment\"}\n",
                "{\"role\":\"assistant\",\"content\":\"ANCHOR493TARGET\"}\n",
            ),
        )
        .unwrap();
        let storage = FrankenStorage::open(&db).unwrap();
        let conversation_id = seed(&storage, &path, "claude_code", indices);
        drop(storage);
        Self {
            _root: root,
            db,
            data,
            path,
            conversation_id,
        }
    }

    fn command(&self, subcommand: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cass"));
        command
            .arg("--db")
            .arg(&self.db)
            .arg(subcommand)
            .env("HOME", self._root.path())
            .env("XDG_CONFIG_HOME", self._root.path().join("config"))
            .env("XDG_DATA_HOME", self._root.path().join("data"))
            .env("XDG_CACHE_HOME", self._root.path().join("cache"))
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env_remove("CASS_OUTPUT_FORMAT")
            .env_remove("TOON_DEFAULT_FORMAT")
            .env_remove("CASS_TEST_VIEW_SLOW_MS")
            .env("CASS_VIEW_BUDGET_MS", "30000");
        command
    }

    fn follow(&self, subcommand: &str, index: usize, extra: &[&str]) -> Output {
        let mut command = self.command(subcommand);
        command
            .arg(&self.path)
            .args(["--message-index", &index.to_string(), "-C", "0", "--json"]);
        if !extra.contains(&"--message-anchor") && !extra.contains(&"--message_anchor") {
            command.arg("--message-anchor").arg(self.anchor(index));
        }
        command.args(extra).output().unwrap()
    }

    fn anchor(&self, index: usize) -> String {
        let storage =
            open_with_flags(self.db.to_str().unwrap(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let source_id = storage
            .query_map_collect(
                "SELECT source_id FROM conversations WHERE id = ?1",
                coding_agent_search::franken_sync::params![self.conversation_id],
                |row| row.get_typed::<String>(0),
            )
            .unwrap()
            .into_iter()
            .next()
            .expect("fixture conversation source");
        let body = i64::try_from(index)
            .ok()
            .and_then(|number| {
                storage
                    .query_map_collect(
                        "SELECT content FROM messages WHERE conversation_id = ?1 AND idx = ?2",
                        coding_agent_search::franken_sync::params![
                            self.conversation_id,
                            number - 1
                        ],
                        |row| row.get_typed::<String>(0),
                    )
                    .unwrap()
                    .into_iter()
                    .next()
            })
            .unwrap_or_default();
        coding_agent_search::search::query::message_anchor_for_content(
            &source_id,
            self.path.to_str().unwrap(),
            Some(self.conversation_id),
            Some(index),
            &body,
        )
        .unwrap_or_else(|| format!("v1:{}", "0".repeat(64)))
    }
}

fn decode(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_target(payload: &Value, command: &str, number: usize, cid: i64) {
    let rows = if command == "view" {
        &payload["lines"]
    } else {
        payload
    };
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{payload}");
    assert_eq!(rows[0]["content"], "ANCHOR493TARGET");
    assert_eq!(rows[0]["is_target"], true);
    assert_eq!(rows[0]["message_index"], number);
    assert_eq!(rows[0]["conversation_id"], cid);
    assert_eq!(rows[0]["source_id"], "local");
    assert_eq!(rows[0]["coordinate_space"], "message_index");
    assert_eq!(rows[0]["content_source"], "archive");
    assert!(rows[0]["message_id"].as_i64().unwrap() > 0);
}

#[test]
fn physical_line_and_indexed_message_are_distinct_explicit_coordinates() {
    let fixture = Fixture::new(&[0, 1, 2]);
    for command in ["expand", "view"] {
        assert_target(
            &decode(fixture.follow(command, 2, &[])),
            command,
            2,
            fixture.conversation_id,
        );
    }
    let raw = decode(
        fixture
            .command("expand")
            .arg(&fixture.path)
            .args(["--line", "2", "-C", "0", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(raw[0]["content"], "PLAUSIBLE WRONG MESSAGE");
}

#[test]
fn actual_search_hit_round_trips_through_both_followup_commands() {
    let fixture = Fixture::new(&[0, 1, 2]);
    let indexed = fixture
        .command("index")
        .arg("--data-dir")
        .arg(&fixture.data)
        .arg("--reconcile-conversation")
        .arg(fixture.conversation_id.to_string())
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    // Exercise the optimized full JSON serializer, general metadata path,
    // and explicit field projection: all must carry the same archive key.
    for extra in [
        vec![],
        vec!["--robot-meta"],
        vec![
            "--fields",
            "conversation_id,line_number,source_id,source_path,content,message_anchor",
        ],
    ] {
        let found = decode(
            fixture
                .command("search")
                .args([
                    "ANCHOR493TARGET",
                    "--mode",
                    "lexical",
                    "--json",
                    "--limit",
                    "5",
                ])
                .args(extra)
                .arg("--data-dir")
                .arg(&fixture.data)
                .output()
                .unwrap(),
        );
        let hits = found["hits"].as_array().expect("search hits");
        let hit = hits
            .iter()
            .find(|hit| hit["content"] == "ANCHOR493TARGET")
            .expect("canonical hit");
        let index = hit["line_number"].as_u64().unwrap() as usize;
        assert_eq!(index, 2);
        let hit_conversation_id = hit["conversation_id"]
            .as_i64()
            .expect("search conversation id");
        assert_eq!(hit_conversation_id, fixture.conversation_id);
        assert_eq!(hit["source_path"], fixture.path.to_string_lossy().as_ref());
        let anchor = hit["message_anchor"]
            .as_str()
            .expect("complete message anchor");
        for command in ["expand", "view"] {
            let output = fixture.follow(
                command,
                index,
                &[
                    "--source",
                    hit["source_id"].as_str().unwrap(),
                    "--conversation-id",
                    &hit_conversation_id.to_string(),
                    "--message-anchor",
                    anchor,
                ],
            );
            assert_target(&decode(output), command, index, fixture.conversation_id);
        }
    }
}

#[test]
fn sparse_indices_are_not_vector_positions_and_missing_indices_fail() {
    let fixture = Fixture::new(&[0, 7, 12]);
    for command in ["expand", "view"] {
        assert_target(
            &decode(fixture.follow(command, 8, &[])),
            command,
            8,
            fixture.conversation_id,
        );
        for missing in [0, 2, 7, 99, usize::MAX] {
            let output = fixture.follow(command, missing, &[]);
            assert!(!output.status.success(), "accepted missing index {missing}");
            assert!(output.stdout.is_empty(), "emitted a target on error");
        }
    }
}

#[test]
fn changed_source_file_never_replaces_archived_hit_content() {
    let fixture = Fixture::new(&[0, 1]);
    std::fs::write(
        &fixture.path,
        "{\"messages\":[{\"content\":\"unrelated replacement\"}]}\n",
    )
    .unwrap();
    for command in ["expand", "view"] {
        assert_target(
            &decode(fixture.follow(command, 2, &["--source", "local"])),
            command,
            2,
            fixture.conversation_id,
        );
    }
}

#[test]
fn wrong_source_or_conversation_never_falls_back_to_live_file() {
    let fixture = Fixture::new(&[0, 1]);
    for command in ["expand", "view"] {
        for flags in [
            vec!["--source", "work-laptop"],
            vec!["--conversation-id", "99999"],
            vec!["--source", ""],
        ] {
            let output = fixture.follow(command, 2, &flags);
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
        }
    }
}

#[test]
fn shared_path_requires_explicit_conversation_identity() {
    let fixture = Fixture::new(&[0, 1]);
    let storage = FrankenStorage::open(&fixture.db).unwrap();
    let other = seed(&storage, &fixture.path, "codex", &[0, 1]);
    assert_ne!(other, fixture.conversation_id);
    drop(storage);
    for command in ["expand", "view"] {
        let ambiguous = fixture.follow(command, 2, &["--source", "local"]);
        assert!(!ambiguous.status.success());
        assert!(ambiguous.stdout.is_empty());
        assert_target(
            &decode(fixture.follow(
                command,
                2,
                &[
                    "--source",
                    "local",
                    "--conversation-id",
                    &fixture.conversation_id.to_string(),
                ],
            )),
            command,
            2,
            fixture.conversation_id,
        );
    }
}

#[test]
fn missing_archive_is_not_created_and_does_not_use_file_lines() {
    let fixture = Fixture::new(&[0, 1]);
    let missing = fixture.data.join("does-not-exist.db");
    for command in ["expand", "view"] {
        let output = Command::new(env!("CARGO_BIN_EXE_cass"))
            .arg("--db")
            .arg(&missing)
            .arg(command)
            .arg(&fixture.path)
            .args(["--message-index", "2", "--json"])
            .arg("--message-anchor")
            .arg(fixture.anchor(2))
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!missing.exists());
    }
}

#[test]
fn raw_expand_never_snaps_blank_malformed_or_past_eof_to_a_target() {
    let fixture = Fixture::new(&[0, 1]);
    std::fs::write(
        &fixture.path,
        "{\"content\":\"first\"}\n\nmalformed\n{\"content\":\"last\"}\n",
    )
    .unwrap();
    for line in ["0", "2", "3", "99"] {
        let output = fixture
            .command("expand")
            .arg(&fixture.path)
            .args(["--line", line, "-C", "0", "--json"])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "accepted nonexistent raw message {line}"
        );
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn conflicting_selectors_are_rejected_and_huge_context_does_not_overflow() {
    let fixture = Fixture::new(&[0, 1, 2]);
    for command in ["expand", "view"] {
        let output = fixture.follow(command, 2, &["--line", "2"]);
        assert!(!output.status.success());
        let payload = decode(
            fixture
                .command(command)
                .arg(&fixture.path)
                .args([
                    "--message-index",
                    "2",
                    "-C",
                    &usize::MAX.to_string(),
                    "--json",
                ])
                .arg("--message-anchor")
                .arg(fixture.anchor(2))
                .output()
                .unwrap(),
        );
        let rows = if command == "view" {
            &payload["lines"]
        } else {
            &payload
        };
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter().filter(|row| row["is_target"] == true).count(),
            1
        );
    }
}

#[test]
fn pasted_search_fields_and_snake_case_selectors_use_canonical_messages() {
    let fixture = Fixture::new(&[0, 7, 12]);
    for subcommand in ["expand", "view"] {
        for selector in [
            "line_number=8",
            "line-number=8",
            "message_index=8",
            "message-index=8",
        ] {
            let payload = decode(
                fixture
                    .command(subcommand)
                    .arg(format!("source_path={}", fixture.path.display()))
                    .arg("source_id=local")
                    .arg(format!("conversation_id={}", fixture.conversation_id))
                    .arg(format!("message_anchor={}", fixture.anchor(8)))
                    .args([selector, "context=0", "--json"])
                    .output()
                    .unwrap(),
            );
            assert_target(&payload, subcommand, 8, fixture.conversation_id);
        }
        let payload = decode(
            fixture
                .command(subcommand)
                .arg(&fixture.path)
                .args(["--message_index", "8", "-C", "0", "--json"])
                .arg("--message_anchor")
                .arg(fixture.anchor(8))
                .output()
                .unwrap(),
        );
        assert_target(&payload, subcommand, 8, fixture.conversation_id);
    }
}

#[test]
fn pasted_search_coordinates_cannot_override_explicit_raw_coordinates() {
    let fixture = Fixture::new(&[0, 1]);
    for subcommand in ["expand", "view"] {
        let output = fixture
            .command(subcommand)
            .arg(&fixture.path)
            .args(["--line", "2", "line_number=2", "--json"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn missing_source_still_resolves_the_archived_message_without_mutation() {
    let fixture = Fixture::new(&[0, 1]);
    let moved = fixture.path.with_extension("saved-jsonl");
    std::fs::rename(&fixture.path, &moved).unwrap();
    let db_before = std::fs::read(&fixture.db).unwrap();
    let source_before = std::fs::read(&moved).unwrap();
    for subcommand in ["expand", "view"] {
        let payload = decode(fixture.follow(subcommand, 2, &["--source", "LOCAL"]));
        assert_target(&payload, subcommand, 2, fixture.conversation_id);
        if subcommand == "view" {
            assert_eq!(payload["archive_only"], true);
            assert_eq!(payload["source_exists"], false);
        }
    }
    assert_eq!(std::fs::read(&fixture.db).unwrap(), db_before);
    assert_eq!(std::fs::read(&moved).unwrap(), source_before);
    assert!(!fixture.path.exists());
}

#[test]
fn an_empty_conversation_cannot_hide_shared_path_ambiguity() {
    let fixture = Fixture::new(&[0, 1]);
    let storage = FrankenStorage::open(&fixture.db).unwrap();
    seed(&storage, &fixture.path, "codex", &[]);
    drop(storage);
    for subcommand in ["expand", "view"] {
        let output = fixture.follow(subcommand, 2, &[]);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "ambiguous-source");
    }
}

#[test]
fn canonical_indices_are_discoverable_as_integers() {
    let fixture = Fixture::new(&[0, 1]);
    let capabilities = decode(
        fixture
            .command("capabilities")
            .arg("--json")
            .output()
            .unwrap(),
    );
    for name in ["expand", "view"] {
        let command = capabilities["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|command| command["name"] == name)
            .unwrap();
        let index = command["arguments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|arg| arg["name"] == "message-index")
            .unwrap();
        assert_eq!(index["value_type"], "integer");
        assert!(
            command["description"]
                .as_str()
                .unwrap()
                .contains("--message-index")
        );
    }
}

#[test]
fn malformed_archive_is_not_repaired_or_replaced_with_a_live_file() {
    let fixture = Fixture::new(&[0, 1]);
    let broken = fixture.data.join("broken.db");
    let bytes = b"not an SQLite database";
    std::fs::write(&broken, bytes).unwrap();
    for subcommand in ["expand", "view"] {
        let output = Command::new(env!("CARGO_BIN_EXE_cass"))
            .env_remove("CASS_OUTPUT_FORMAT")
            .env_remove("TOON_DEFAULT_FORMAT")
            .arg("--db")
            .arg(&broken)
            .arg(subcommand)
            .arg(&fixture.path)
            .args(["--message-index", "2", "--json"])
            .arg("--message-anchor")
            .arg(fixture.anchor(2))
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(std::fs::read(&broken).unwrap(), bytes);
        assert!(!fixture.data.join("broken.db-wal").exists());
        assert!(!fixture.data.join("broken.db-shm").exists());
    }
}

#[test]
fn physical_lines_never_fall_back_to_archive_when_source_is_missing() {
    let fixture = Fixture::new(&[0, 7, 12]);
    std::fs::rename(&fixture.path, fixture.path.with_extension("retained")).unwrap();
    let before = std::fs::read(&fixture.db).unwrap();
    for command in ["view", "expand"] {
        for json in [false, true] {
            let mut process = fixture.command(command);
            process.arg(&fixture.path).args(["--line", "2", "-C", "0"]);
            if json {
                process.arg("--json");
            }
            let output = process.output().unwrap();
            assert_eq!(output.status.code(), Some(3));
            assert!(
                output.stdout.is_empty(),
                "an archive row became a physical target"
            );
            let diagnostic = String::from_utf8_lossy(&output.stderr);
            assert!(diagnostic.contains("--message-index"), "{diagnostic}");
        }
        assert_target(
            &decode(fixture.follow(command, 8, &[])),
            command,
            8,
            fixture.conversation_id,
        );
    }
    assert_eq!(std::fs::read(&fixture.db).unwrap(), before);
}

#[test]
fn physical_read_error_cannot_turn_into_a_successful_archive_target() {
    let fixture = Fixture::new(&[0, 1, 2]);
    let broken = b"{\"content\":\"first physical record\"}\n\xff\n";
    std::fs::write(&fixture.path, broken).unwrap();
    let before = std::fs::read(&fixture.db).unwrap();
    for command in ["view", "expand"] {
        let output = fixture
            .command(command)
            .arg(&fixture.path)
            .args(["--line", "2", "-C", "0", "--json"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(9));
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "file-read");
        assert_target(
            &decode(fixture.follow(command, 2, &[])),
            command,
            2,
            fixture.conversation_id,
        );
    }
    assert_eq!(std::fs::read(&fixture.db).unwrap(), before);
    assert_eq!(std::fs::read(&fixture.path).unwrap(), broken);
}

#[test]
fn indexed_non_jsonl_files_do_not_change_physical_coordinate_meaning() {
    let mut fixture = Fixture::new(&[0, 1, 2]);
    fixture.path = fixture.path.with_extension("json");
    std::fs::write(
        &fixture.path,
        "[\n  {\"content\":\"physical second line\"}\n]\n",
    )
    .unwrap();
    let storage = FrankenStorage::open(&fixture.db).unwrap();
    storage
        .raw()
        .execute_compat(
            "UPDATE conversations SET source_path = ?1 WHERE id = ?2",
            coding_agent_search::franken_sync::params![
                fixture.path.to_string_lossy().as_ref(),
                fixture.conversation_id
            ],
        )
        .unwrap();
    drop(storage);
    let before = std::fs::read(&fixture.db).unwrap();
    let payload = decode(
        fixture
            .command("view")
            .arg(&fixture.path)
            .args(["--line", "2", "-C", "0", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(payload["coordinate_space"], "file_line");
    assert_eq!(payload["content_source"], "file");
    assert_eq!(payload["total_lines"], 3);
    assert_eq!(
        payload["lines"][0]["content"],
        "  {\"content\":\"physical second line\"}"
    );
    assert_eq!(payload["lines"][0]["is_target"], true);
    let refused = fixture
        .command("expand")
        .arg(&fixture.path)
        .args(["--line", "2", "-C", "0", "--json"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(9));
    assert!(refused.stdout.is_empty());
    assert_target(
        &decode(fixture.follow("expand", 2, &[])),
        "expand",
        2,
        fixture.conversation_id,
    );
    assert_eq!(std::fs::read(&fixture.db).unwrap(), before);
}

#[test]
fn physical_targets_cannot_claim_remote_or_conversation_identity() {
    let fixture = Fixture::new(&[0, 7, 12]);
    // A real remote archive row shares the same path as a plausible local file.
    let storage = FrankenStorage::open(&fixture.db).unwrap();
    storage
        .raw()
        .execute(
            "INSERT INTO sources (id, kind, host_label, created_at, updated_at)
             VALUES ('work-laptop', 'ssh', 'work-laptop', 1, 1)",
        )
        .unwrap();
    storage
        .raw()
        .execute_compat(
            "UPDATE conversations SET source_id = 'work-laptop', origin_host = 'work-laptop' WHERE id = ?1",
            coding_agent_search::franken_sync::params![fixture.conversation_id],
        )
        .unwrap();
    drop(storage);
    let before = std::fs::read(&fixture.db).unwrap();
    let source = std::fs::read(&fixture.path).unwrap();
    for command in ["view", "expand"] {
        for flags in [
            vec!["--source".to_string(), "work-laptop".to_string()],
            vec![
                "--conversation-id".to_string(),
                fixture.conversation_id.to_string(),
            ],
        ] {
            let output = fixture
                .command(command)
                .arg(&fixture.path)
                .args(["--line", "2", "-C", "0", "--json"])
                .args(flags)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(2));
            assert!(output.stdout.is_empty());
            let diagnostic = String::from_utf8_lossy(&output.stderr);
            assert!(diagnostic.contains("--message-index"), "{diagnostic}");
        }
        let payload = decode(fixture.follow(command, 8, &["--source", "work-laptop"]));
        let rows = if command == "view" {
            &payload["lines"]
        } else {
            &payload
        };
        assert_eq!(rows[0]["content"], "ANCHOR493TARGET");
        assert_eq!(rows[0]["source_id"], "work-laptop");
        assert_eq!(rows[0]["is_target"], true);
    }
    assert_eq!(std::fs::read(&fixture.db).unwrap(), before);
    assert_eq!(std::fs::read(&fixture.path).unwrap(), source);
}

#[test]
fn physical_context_keeps_file_numbers_while_expand_skips_non_records() {
    let fixture = Fixture::new(&[0, 1]);
    std::fs::write(
        &fixture.path,
        concat!(
            "{\"content\":\"before\"}\n",
            "\n",
            "malformed\n",
            "{\"content\":\"physical target\"}\n",
            "\n",
            "malformed\n",
            "{\"content\":\"after\"}\n",
            "\n",
        ),
    )
    .unwrap();
    for command in ["view", "expand"] {
        let payload = decode(
            fixture
                .command(command)
                .arg(&fixture.path)
                .args(["--line", "4", "-C", "1", "--json"])
                .output()
                .unwrap(),
        );
        let rows = if command == "view" {
            &payload["lines"]
        } else {
            &payload
        };
        let rows = rows.as_array().unwrap();
        let numbers: Vec<_> = rows
            .iter()
            .map(|row| row["line"].as_u64().unwrap())
            .collect();
        assert_eq!(
            numbers,
            if command == "view" {
                vec![3, 4, 5]
            } else {
                vec![1, 4, 7]
            }
        );
        assert_eq!(
            rows.iter().filter(|row| row["is_target"] == true).count(),
            1
        );
        assert_eq!(rows[1]["line"], 4);
        for row in rows {
            assert_eq!(row["coordinate_space"], "file_line");
            assert_eq!(row["content_source"], "file");
            assert_eq!(row["file_line"], row["line"]);
            assert!(row.get("message_index").is_none());
            assert!(row.get("conversation_id").is_none());
        }
    }
}

#[test]
fn unanchored_archive_browsing_remains_available_without_a_physical_target() {
    let fixture = Fixture::new(&[0, 7, 12]);
    std::fs::rename(&fixture.path, fixture.path.with_extension("retained")).unwrap();
    let payload = decode(
        fixture
            .command("view")
            .arg(&fixture.path)
            .args([
                "--conversation-id",
                &fixture.conversation_id.to_string(),
                "--source",
                "local",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(payload["target_line"], Value::Null);
    assert_eq!(payload["archive_only"], true);
    let rows = payload["lines"].as_array().unwrap();
    assert!(!rows.is_empty());
    assert!(
        rows.iter()
            .all(|row| row["highlighted"] != true && row["is_target"] != true)
    );
}

#[test]
fn search_hit_serialization_preserves_available_identity_without_inventing_one() {
    use coding_agent_search::search::query::{MatchType, SearchHit};
    let mut hit = SearchHit {
        title: "identity".into(),
        message_anchor: None,
        snippet: "anchor".into(),
        content: "anchor".into(),
        content_hash: 123,
        conversation_id: Some(42),
        score: 1.0,
        source_path: "/shared/provider.db".into(),
        agent: "codex".into(),
        workspace: "/work".into(),
        workspace_original: None,
        created_at: None,
        line_number: Some(8),
        match_type: MatchType::Exact,
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
    };
    let value = serde_json::to_value(&hit).unwrap();
    assert_eq!(value["conversation_id"], 42);
    assert_eq!(value["line_number"], 8);
    assert!(value.get("content_hash").is_none());
    hit.conversation_id = None;
    let value = serde_json::to_value(&hit).unwrap();
    assert_eq!(value.get("conversation_id"), Some(&Value::Null));
}

#[test]
fn shared_path_search_hits_round_trip_in_every_robot_projection() {
    let fixture = Fixture::new(&[0, 7, 12]);
    let storage = FrankenStorage::open(&fixture.db).unwrap();
    let other = seed(&storage, &fixture.path, "codex", &[0, 7, 12]);
    drop(storage);
    assert_ne!(fixture.conversation_id, other);
    for conversation_id in [fixture.conversation_id, other] {
        let indexed = fixture
            .command("index")
            .arg("--data-dir")
            .arg(&fixture.data)
            .arg("--reconcile-conversation")
            .arg(conversation_id.to_string())
            .arg("--json")
            .output()
            .unwrap();
        decode(indexed);
    }
    let db_before = std::fs::read(&fixture.db).unwrap();
    let source_before = std::fs::read(&fixture.path).unwrap();
    let expected: std::collections::BTreeSet<_> =
        [fixture.conversation_id, other].into_iter().collect();
    for format in ["json", "compact", "jsonl"] {
        for fields in [
            None,
            Some("minimal"),
            Some("minimal,message_anchor"),
            Some("summary"),
            Some("summary,message_anchor"),
            Some("all"),
            Some("source_path,line_number,source_id,conversation_id"),
            Some("source_path,line_number,source_id,conversation_id,message_anchor"),
        ] {
            // Plain JSON exercises the handwritten fast serializers. Truncation
            // forces the general projection; JSONL and compact use that path too.
            for truncate in [false, true] {
                let mut command = fixture.command("search");
                command
                    .args([
                        "ANCHOR493TARGET",
                        "--mode",
                        "lexical",
                        "--robot-format",
                        format,
                        "--limit",
                        "5",
                        "--no-maintenance",
                    ])
                    .arg("--data-dir")
                    .arg(&fixture.data);
                if let Some(fields) = fields {
                    command.args(["--fields", fields]);
                }
                if truncate {
                    command.args(["--max-content-length", "8"]);
                }
                let output = command.output().unwrap();
                assert!(
                    output.status.success(),
                    "format={format} fields={fields:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let hits: Vec<Value> = if format == "jsonl" {
                    String::from_utf8(output.stdout)
                        .unwrap()
                        .lines()
                        .map(|line| serde_json::from_str::<Value>(line).unwrap())
                        .filter(|value| value.get("source_path").is_some())
                        .collect()
                } else {
                    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
                    payload["hits"].as_array().expect("search hits").clone()
                };
                assert_eq!(hits.len(), 2, "format={format} fields={fields:?}: {hits:?}");
                let mut seen = std::collections::BTreeSet::new();
                for hit in hits {
                    let cid = hit["conversation_id"]
                        .as_i64()
                        .expect("canonical identity must survive output");
                    assert!(
                        seen.insert(cid),
                        "shared paths must not collapse distinct conversations"
                    );
                    assert_eq!(hit["source_id"], "local");
                    assert_eq!(hit["source_path"], fixture.path.to_string_lossy().as_ref());
                    assert_eq!(hit["line_number"], 8);
                    if fields == Some("minimal") {
                        assert_eq!(hit.as_object().unwrap().len(), 5);
                        assert!(hit.get("content").is_none());
                    } else if fields == Some("summary") {
                        assert_eq!(
                            hit.as_object().unwrap().len(),
                            7 + usize::from(hit.get("title_truncated").is_some())
                        );
                        assert!(hit.get("content").is_none());
                    } else if fields == Some("source_path,line_number,source_id,conversation_id") {
                        assert_eq!(hit.as_object().unwrap().len(), 4);
                    }
                    for subcommand in ["view", "expand"] {
                        if let Some(anchor) = hit["message_anchor"].as_str() {
                            let payload = decode(fixture.follow(
                                subcommand,
                                8,
                                &[
                                    "--source",
                                    hit["source_id"].as_str().unwrap(),
                                    "--conversation-id",
                                    &cid.to_string(),
                                    "--message-anchor",
                                    anchor,
                                ],
                            ));
                            assert_target(&payload, subcommand, 8, cid);
                        } else {
                            let output = fixture
                                .command(subcommand)
                                .arg(&fixture.path)
                                .args([
                                    "--message-index",
                                    "8",
                                    "--source",
                                    hit["source_id"].as_str().unwrap(),
                                    "--conversation-id",
                                    &cid.to_string(),
                                    "--json",
                                ])
                                .output()
                                .unwrap();
                            assert!(!output.status.success());
                            assert!(output.stdout.is_empty());
                            assert!(
                                String::from_utf8_lossy(&output.stderr).contains("message-anchor")
                            );
                        }
                    }
                }
                assert_eq!(seen, expected);
            }
        }
    }
    // Caller-selected masks may deliberately omit identity; do not silently
    // expand them or invent a conversation id to make the follow-up succeed.
    let payload = decode(
        fixture
            .command("search")
            .args([
                "ANCHOR493TARGET",
                "--mode",
                "lexical",
                "--json",
                "--fields",
                "source_path,line_number",
                "--limit",
                "5",
                "--no-maintenance",
            ])
            .arg("--data-dir")
            .arg(&fixture.data)
            .output()
            .unwrap(),
    );
    let hits = payload["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2);
    for hit in hits {
        assert_eq!(hit.as_object().unwrap().len(), 2);
    }
    assert_eq!(std::fs::read(&fixture.db).unwrap(), db_before);
    assert_eq!(std::fs::read(&fixture.path).unwrap(), source_before);
}

#[test]
fn corrected_robot_failures_are_one_error_envelope_and_success_still_teaches() {
    let fixture = Fixture::new(&[0, 7, 12]);
    let formats: &[&[&str]] = &[
        &["--json"],
        &["--robot"],
        &["--robot-format", "json"],
        &["--robot-format", "compact"],
        &["--robot-format", "jsonl"],
        &["--format=json"],
        &[], // Environment-selected structured output follows the same contract.
    ];
    for subcommand in ["view", "expand"] {
        for flags in formats {
            let mut command = fixture.command(subcommand);
            command
                .arg(format!("source_path={}", fixture.path.display()))
                .args([
                    "source_id=local",
                    "conversation_id=999999",
                    "line_number=8",
                    "context=0",
                ])
                .arg(format!("message_anchor={}", fixture.anchor(8)))
                .args(*flags);
            if flags.is_empty() {
                command.env("CASS_OUTPUT_FORMAT", "json");
            }
            let output = command.output().unwrap();
            assert!(!output.status.success());
            assert!(output.stdout.is_empty(), "failed recovery emitted a target");
            let error: Value = serde_json::from_slice(&output.stderr).unwrap_or_else(|err| {
                panic!(
                    "{subcommand} {flags:?}: {err}; stderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                )
            });
            assert_eq!(error["error"]["kind"], "indexed-session-required");
            assert_eq!(error["error"]["retryable"], false);
            assert!(
                error["error"]["hint"]
                    .as_str()
                    .is_some_and(|hint| hint.contains("same search hit"))
            );
        }
        let output = fixture
            .command(subcommand)
            .arg(format!("source_path={}", fixture.path.display()))
            .args(["source_id=local", "line_number=8", "context=0", "--json"])
            .arg(format!("message_anchor={}", fixture.anchor(8)))
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("note: auto-corrected:"),
            "successful robot recovery still teaches the canonical syntax"
        );
        assert_target(&decode(output), subcommand, 8, fixture.conversation_id);

        let human = fixture
            .command(subcommand)
            .arg(format!("source_path={}", fixture.path.display()))
            .args(["line_number=8", "conversation_id=999999", "context=0"])
            .arg(format!("message_anchor={}", fixture.anchor(8)))
            .output()
            .unwrap();
        assert!(!human.status.success());
        assert!(
            String::from_utf8_lossy(&human.stderr)
                .contains("Note: Your command was auto-corrected:"),
            "human diagnostics must retain their teaching note"
        );
    }
}

fn pack_candidate(
    source: &str,
    path: &str,
    conversation_id: Option<i64>,
    number: Option<usize>,
    hash: u64,
) -> coding_agent_search::search::pack_planner::PackCandidate {
    use coding_agent_search::search::pack_planner::PackCandidate;
    use coding_agent_search::search::query::{MatchType, SearchHit};
    PackCandidate::from_search_hit(
        &SearchHit {
            title: "pack identity".into(),
            message_anchor: None,
            snippet: "pack evidence".into(),
            content: format!("distinct pack evidence {hash}"),
            content_hash: hash,
            conversation_id,
            score: 1.0,
            source_path: path.into(),
            agent: "claude_code".into(),
            workspace: "/work".into(),
            workspace_original: None,
            created_at: Some(1_733_000_000_000),
            line_number: number,
            match_type: MatchType::Exact,
            source_id: source.into(),
            origin_kind: "local".into(),
            origin_host: None,
        },
        1,
        0,
    )
}

#[test]
fn pack_indices_and_evidence_ids_bind_the_complete_canonical_identity() {
    use coding_agent_search::search::pack_planner::{PackPlanRequest, plan_answer_pack};
    for number in [None, Some(0), Some(1), Some(8), Some(129), Some(541)] {
        let candidate = pack_candidate("local", "/shared/provider.db", Some(42), number, 1);
        assert_eq!(candidate.message_index, number.filter(|&value| value != 0));
        assert!(candidate.line_start.is_none());
        assert!(!candidate.citation_verified);
    }
    // Identical content and source spans in separate single-item packs must
    // still identify the right conversation and message. Content deduplication
    // WITHIN a pack is intentionally unchanged.
    let candidates = vec![
        pack_candidate("local", "/shared/provider.db", Some(42), Some(8), 1),
        pack_candidate("local", "/shared/provider.db", Some(43), Some(8), 1),
        pack_candidate("local", "/shared/provider.db", Some(42), Some(9), 1),
        pack_candidate("remote-host", "/shared/provider.db", Some(42), Some(8), 1),
        pack_candidate("local", "/shared/provider.db", None, Some(8), 1),
        pack_candidate("local", "/shared/provider.db", Some(42), None, 1),
        // These collided with newline-delimited citation hashing.
        pack_candidate("local\n/shared", "provider.db", Some(42), Some(8), 1),
        pack_candidate("local", "/shared\nprovider.db", Some(42), Some(8), 1),
    ];
    let mut candidate_ids = std::collections::HashSet::new();
    let mut evidence_ids = std::collections::HashSet::new();
    for candidate in candidates {
        assert!(candidate_ids.insert(candidate.candidate_id.clone()));
        let request = PackPlanRequest {
            candidates: vec![candidate],
            ..Default::default()
        };
        let plan = plan_answer_pack(request.clone()).unwrap();
        let repeat = plan_answer_pack(request).unwrap();
        assert_eq!(plan.evidence.len(), 1);
        assert_eq!(plan.evidence[0].id, repeat.evidence[0].id);
        assert!(evidence_ids.insert(plan.evidence[0].id.clone()));
    }
}

#[test]
fn pack_session_caps_and_source_summaries_count_shared_path_conversations() {
    use coding_agent_search::search::pack_planner::{
        PackOmittedReason, PackPlanRequest, PackRenderRequest, plan_answer_pack,
        render_answer_pack_value_without_trust_correlation,
    };
    let candidates = vec![
        pack_candidate("local", "/shared/provider.db", Some(11), Some(1), 11),
        pack_candidate("local", "/shared/provider.db", Some(22), Some(1), 22),
        pack_candidate("local", "/shared/provider.db", Some(11), Some(2), 12),
    ];
    for cap in [1, 2] {
        let mut request = PackPlanRequest {
            candidates: candidates.clone(),
            ..Default::default()
        };
        request.limits.max_sessions = cap;
        let plan = plan_answer_pack(request.clone()).unwrap();
        assert_eq!(plan.selected_session_count, cap);
        assert_eq!(plan.selected_evidence_count, if cap == 1 { 2 } else { 3 });
        if cap == 1 {
            assert_eq!(plan.omitted.len(), 1);
            assert_eq!(
                plan.omitted[0].reason,
                PackOmittedReason::MaxSessionsReached
            );
        }
        request.candidates.reverse();
        let reversed = plan_answer_pack(request).unwrap();
        assert_eq!(
            plan.evidence
                .iter()
                .map(|item| &item.id)
                .collect::<Vec<_>>(),
            reversed
                .evidence
                .iter()
                .map(|item| &item.id)
                .collect::<Vec<_>>(),
            "equal-ranked shared paths must not make selection depend on input order",
        );
        let value = render_answer_pack_value_without_trust_correlation(
            &plan,
            &PackRenderRequest::default(),
        )
        .unwrap();
        assert_eq!(value["realized"]["selected_session_count"], cap);
        assert_eq!(value["pack"]["source_summary"][0]["session_count"], cap);
    }
}

#[test]
fn pack_physical_overlap_deduplication_is_scoped_to_the_source() {
    use coding_agent_search::search::pack_planner::{PackPlanRequest, plan_answer_pack};
    let mut left = pack_candidate("local", "/shared/session.jsonl", Some(11), Some(8), 11);
    left.line_start = Some(4);
    left.line_end = Some(7);
    left.citation_verified = true;
    let mut right = pack_candidate(
        "work-laptop",
        "/shared/session.jsonl",
        Some(22),
        Some(8),
        22,
    );
    right.line_start = left.line_start;
    right.line_end = left.line_end;
    right.citation_verified = true;
    let plan = plan_answer_pack(PackPlanRequest {
        candidates: vec![left.clone(), right.clone()],
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        plan.selected_evidence_count, 2,
        "same pathname on different hosts is not one file"
    );
    right.source_id = "local".into();
    let plan = plan_answer_pack(PackPlanRequest {
        candidates: vec![left, right],
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        plan.selected_evidence_count, 1,
        "actual same-source physical overlap is still deduplicated"
    );
}

#[test]
fn pack_renderer_coordinates_are_self_describing_without_bypassing_privacy() {
    use coding_agent_search::search::pack_planner::{
        PackPlanRequest, PackRenderFormat, PackRenderRequest, plan_answer_pack,
        render_answer_pack_without_trust_correlation,
    };
    let mut candidate = pack_candidate(
        "alice.internal",
        "/home/alice/history.jsonl",
        Some(42),
        Some(8),
        1,
    );
    candidate.origin_kind = "ssh".into();
    candidate.origin_host = Some("alice.internal".into());
    candidate.message_anchor = coding_agent_search::search::query::message_anchor_for_content(
        &candidate.source_id,
        &candidate.source_path,
        candidate.conversation_id,
        candidate.message_index,
        &candidate.excerpt,
    );
    let anchor = candidate.message_anchor.clone().unwrap();
    let plan = plan_answer_pack(PackPlanRequest {
        candidates: vec![candidate],
        ..Default::default()
    })
    .unwrap();
    for format in [
        PackRenderFormat::Json,
        PackRenderFormat::CompactJson,
        PackRenderFormat::Jsonl,
        PackRenderFormat::Markdown,
    ] {
        let request = PackRenderRequest {
            format,
            ..Default::default()
        };
        let output = render_answer_pack_without_trust_correlation(&plan, &request).unwrap();
        assert!(!output.contains("alice.internal"));
        assert!(!output.contains("/home/alice"));
        if format == PackRenderFormat::Markdown {
            assert!(output.contains("conversation_id=42 message_index=8 (1-based)"));
            assert!(output.contains(&format!("message_anchor={anchor}")));
            assert!(output.contains("followup_unavailable_reason=redacted-source-coordinates"));
        } else if format == PackRenderFormat::Jsonl {
            let lines: Vec<Value> = output
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(lines[0]["schema_version"], "cass.pack.v2");
            let citation = &lines
                .iter()
                .find(|line| line.get("evidence").is_some())
                .unwrap()["evidence"]["citation"];
            assert_eq!(citation["message_index_base"], 1);
            assert_eq!(citation["message_index"], 8);
            assert_eq!(citation["message_anchor"], anchor);
            assert_eq!(
                citation["followup_unavailable_reason"],
                "redacted-source-coordinates"
            );
        } else {
            let payload: Value = serde_json::from_str(&output).unwrap();
            assert_eq!(payload["schema_version"], "cass.pack.v2");
            let citation = &payload["evidence"][0]["citation"];
            assert_eq!(citation["message_index_base"], 1);
            assert_eq!(citation["message_index"], 8);
            assert_eq!(citation["message_anchor"], anchor);
            assert_eq!(
                citation["followup_unavailable_reason"],
                "redacted-source-coordinates"
            );
            assert_eq!(citation["conversation_id"], 42);
            assert!(citation["line_start"].is_null());
        }
    }
}

#[test]
fn actual_pack_citations_round_trip_unchanged_with_shared_paths_and_missing_sources() {
    let fixture = Fixture::new(&[0, 1, 7]);
    let storage = FrankenStorage::open(&fixture.db).unwrap();
    let other = seed(&storage, &fixture.path, "codex", &[0, 1, 7]);
    let targets = [
        (
            fixture.conversation_id,
            "PACK493TARGET first conversation evidence",
        ),
        (other, "PACK493TARGET second conversation evidence"),
    ];
    for (conversation_id, content) in targets {
        storage
            .raw()
            .execute_compat(
                "UPDATE messages SET content = ?1 WHERE conversation_id = ?2 AND idx = 1",
                coding_agent_search::franken_sync::params![content, conversation_id],
            )
            .unwrap();
    }
    drop(storage);
    for (conversation_id, _) in targets {
        let indexed = fixture
            .command("index")
            .arg("--data-dir")
            .arg(&fixture.data)
            .arg("--reconcile-conversation")
            .arg(conversation_id.to_string())
            .arg("--json")
            .output()
            .unwrap();
        assert!(
            indexed.status.success(),
            "{}",
            String::from_utf8_lossy(&indexed.stderr)
        );
    }
    // Both the previous canonical message and physical file line 2 exist and
    // are plausible WRONG targets. A mistaken offset must not pass this test.
    let wrong = decode(fixture.follow(
        "view",
        1,
        &["--conversation-id", &fixture.conversation_id.to_string()],
    ));
    assert_eq!(wrong["lines"][0]["content"], "canonical neighbour");
    let source = std::fs::read(&fixture.path).unwrap();
    let retained_source = fixture.path.with_extension("retained");
    std::fs::rename(&fixture.path, &retained_source).unwrap();
    let before = std::fs::read(&fixture.db).unwrap();
    for (format, args) in [
        ("json", vec!["--json"]),
        ("compact", vec!["--robot-format", "compact"]),
        ("jsonl", vec!["--robot-format", "jsonl"]),
        ("minimal", vec!["--json", "--fields", "minimal"]),
        (
            "custom",
            vec![
                "--json",
                "--fields",
                "schema_version,evidence[].citation,evidence[].excerpt",
            ],
        ),
    ] {
        let output = fixture
            .command("pack")
            .args([
                "PACK493TARGET",
                "--mode",
                "lexical",
                "--freshness-policy",
                "allow-stale",
                "--max-sessions",
                "2",
                "--max-tokens",
                "20000",
                "--timeout",
                "30000",
                "--require-evidence",
            ])
            .arg("--data-dir")
            .arg(&fixture.data)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{format}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let items: Vec<Value> = if format == "jsonl" {
            let records: Vec<Value> = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(records[0]["schema_version"], "cass.pack.v2");
            records
                .into_iter()
                .filter_map(|mut row| row.get_mut("evidence").map(Value::take))
                .collect()
        } else {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["schema_version"], "cass.pack.v2");
            if format == "json" || format == "compact" {
                assert_eq!(value["realized"]["selected_session_count"], 2);
                assert_eq!(value["pack"]["source_summary"][0]["session_count"], 2);
            }
            value["evidence"].as_array().unwrap().clone()
        };
        assert_eq!(
            items.len(),
            2,
            "{format}: distinct conversations cannot be collapsed"
        );
        let mut seen = std::collections::HashSet::new();
        for item in items {
            let citation = &item["citation"];
            assert_eq!(citation["message_index_base"], 1);
            assert_eq!(citation["message_index"], 2);
            assert!(citation["line_start"].is_null());
            let cid = citation["conversation_id"].as_i64().unwrap();
            assert!(seen.insert(cid));
            let expected = targets.iter().find(|(id, _)| *id == cid).unwrap().1;
            assert_eq!(item["excerpt"], expected);
            for command in ["view", "expand"] {
                let value = decode(
                    fixture
                        .command(command)
                        .arg(citation["source_path"].as_str().unwrap())
                        .args(["--source", citation["source_id"].as_str().unwrap()])
                        .arg("--conversation-id")
                        .arg(cid.to_string())
                        .arg("--message-index")
                        .arg(citation["message_index"].as_u64().unwrap().to_string())
                        .arg("--message-anchor")
                        .arg(citation["message_anchor"].as_str().unwrap())
                        .args(["-C", "0", "--json"])
                        .output()
                        .unwrap(),
                );
                let rows = if command == "view" {
                    &value["lines"]
                } else {
                    &value
                };
                assert_eq!(rows.as_array().unwrap().len(), 1);
                assert_eq!(rows[0]["content"], expected);
                assert_eq!(rows[0]["conversation_id"], cid);
                assert_eq!(rows[0]["message_index"], 2);
                assert_eq!(rows[0]["is_target"], true);
            }
        }
    }
    let capped = decode(
        fixture
            .command("pack")
            .args([
                "PACK493TARGET",
                "--mode",
                "lexical",
                "--freshness-policy",
                "allow-stale",
                "--json",
                "--max-sessions",
                "1",
                "--max-tokens",
                "20000",
                "--timeout",
                "30000",
                "--require-evidence",
            ])
            .arg("--data-dir")
            .arg(&fixture.data)
            .output()
            .unwrap(),
    );
    assert_eq!(capped["realized"]["selected_session_count"], 1);
    assert_eq!(capped["evidence"].as_array().unwrap().len(), 1);
    assert_eq!(std::fs::read(&fixture.db).unwrap(), before);
    assert_eq!(std::fs::read(&retained_source).unwrap(), source);
    assert!(!fixture.path.exists());
}

/// Synthetic native connector acceptance, not the reporter's private archive.
/// Expected rows come from the canonical archive; every outward identity field
/// is checked in addition to content and the immutable anchor.
mod native_anchor_journey {
    use super::*;
    use coding_agent_search::search::query::message_anchor_for_content;
    use sha2::{Digest, Sha256};
    use std::collections::HashSet;
    use std::time::Duration;

    const QUERY: &str = "GH493NATIVE";
    const DUPLICATE: &str = "GH493NATIVE same text at several positions and sessions";

    #[derive(Clone, Debug)]
    struct Expected {
        message_id: i64,
        conversation_id: i64,
        index: usize,
        source: String,
        path: String,
        agent: String,
        role: String,
        content: String,
    }

    impl Expected {
        fn anchor(&self) -> String {
            message_anchor_for_content(
                &self.source,
                &self.path,
                Some(self.conversation_id),
                Some(self.index),
                &self.content,
            )
            .unwrap()
        }

        fn assert_followup(&self, payload: &Value, command: &str) {
            let rows = if command == "view" {
                &payload["lines"]
            } else {
                payload
            };
            let rows = rows.as_array().unwrap();
            assert_eq!(rows.len(), 1);
            let target = &rows[0];
            assert_eq!(target["message_id"], self.message_id);
            assert_eq!(target["conversation_id"], self.conversation_id);
            assert_eq!(target["source_id"], self.source);
            assert_eq!(target["message_index"], self.index);
            assert_eq!(target["line"], self.index);
            assert_eq!(target["message_anchor"], self.anchor());
            assert_eq!(target["content"], self.content);
            assert_eq!(target["role"], self.role);
            assert_eq!(target["is_target"], true);
            assert_eq!(target["highlighted"], true);
            assert_eq!(target["content_source"], "archive");
            assert_eq!(target["coordinate_space"], "message_index");
            if command == "view" {
                assert_eq!(payload["path"], self.path);
                assert_eq!(payload["source_id"], self.source);
                assert_eq!(payload["conversation_id"], self.conversation_id);
                assert_eq!(payload["target_message_index"], self.index);
                assert_eq!(payload["target_line"], self.index);
            }
        }
    }

    struct Corpus {
        root: TempDir,
        data: PathBuf,
        db: PathBuf,
        codex: PathBuf,
        claude: PathBuf,
        source: PathBuf,
        binary: PathBuf,
        binary_sha256: String,
    }

    fn output(command: Command) -> Output {
        assert_cmd::Command::from_std(command)
            .timeout(Duration::from_secs(90))
            .output()
            .unwrap()
    }

    fn binary_sha256(path: &Path) -> String {
        use std::io::Read;
        let mut file = std::fs::File::open(path).expect("selected candidate must exist");
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
        hex::encode(digest.finalize())
    }

    impl Corpus {
        fn new() -> Self {
            // Release qualification supplies both the exact candidate and its
            // attested SHA. An invalid supplied override never falls back.
            let override_path = match dotenvy::var("CASS_TEST_CANDIDATE_BIN") {
                Ok(value) => Some(value),
                Err(dotenvy::Error::EnvVar(std::env::VarError::NotPresent)) => None,
                Err(error) => panic!("invalid candidate override: {error}"),
            };
            let binary = PathBuf::from(
                override_path
                    .as_deref()
                    .unwrap_or(env!("CARGO_BIN_EXE_cass")),
            );
            let binary_sha256 = binary_sha256(&binary);
            if override_path.is_some() {
                let expected = dotenvy::var("CASS_TEST_CANDIDATE_SHA256")
                    .expect("candidate override requires its attested SHA256");
                assert_eq!(binary_sha256, expected, "selected candidate hash mismatch");
            }
            eprintln!(
                "GH493_BINARY SHA256={binary_sha256} OVERRIDE={}",
                override_path.is_some()
            );
            let root = tempfile::tempdir().unwrap();
            // RCH can place tempdirs inside an ignored checkout subtree.
            // This boundary keeps the native corpus independent of that policy.
            std::fs::create_dir(root.path().join(".git")).unwrap();
            let data = root.path().join("data");
            let codex = root.path().join("codex");
            let claude = root.path().join("claude");
            let sessions = codex.join("sessions/2024/11/20");
            let projects = claude.join("projects/gh493-native");
            std::fs::create_dir_all(&sessions).unwrap();
            std::fs::create_dir_all(&projects).unwrap();
            std::fs::create_dir_all(&data).unwrap();
            let source = sessions.join("rollout-gh493-native.jsonl");
            // This value is deliberately synthetic and must be redacted by ingestion.
            let credential_assignment = format!("password={}", "gh493_synthetic_fixture_value");
            let records = [
                json!({"timestamp":"2024-11-20T00:00:00Z","type":"session_meta","payload":{"id":"gh493-native-codex","cwd":"/gh493-work","cli_version":"0.42.0"}}),
                json!({"timestamp":"2024-11-20T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":DUPLICATE}]}}),
                json!({"timestamp":"2024-11-20T00:00:02Z","type":"event_msg","payload":{"type":"token_count","input_tokens":17}}),
                json!({"timestamp":"2024-11-20T00:00:03Z","type":"response_item","payload":{"type":"function_call_output","call_id":"synthetic-tool","output":"GH493NATIVE tool-only result"}}),
                json!({"timestamp":"2024-11-20T00:00:04Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"GH493NATIVE multi block naïve café 日本語"},{"type":"output_text","text":"second block 😀"}]}}),
                json!({"timestamp":"2024-11-20T00:00:05Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":DUPLICATE}]}}),
                json!({"timestamp":"2024-11-20T00:00:06Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":format!("GH493NATIVE redacted {credential_assignment}")}]}}),
                json!({"timestamp":"2024-11-20T00:00:07Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":format!("GH493NATIVE long {}", "complete text 日本語 ".repeat(80))}]}}),
            ];
            let mut bytes = String::new();
            for record in records {
                bytes.push_str(&record.to_string());
                bytes.push('\n');
            }
            std::fs::write(&source, bytes).unwrap();
            let claude_records = [
                json!({"type":"user","cwd":"/gh493-work","sessionId":"gh493-native-claude","timestamp":"2024-11-20T00:00:01Z","message":{"role":"user","content":DUPLICATE}}),
                json!({"type":"summary","summary":"Skipped metadata has no canonical ordinal"}),
                json!({"type":"assistant","timestamp":"2024-11-20T00:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"GH493NATIVE Claude Unicode Ελληνικά"}]}}),
            ];
            let mut bytes = String::new();
            for record in claude_records {
                bytes.push_str(&record.to_string());
                bytes.push('\n');
            }
            std::fs::write(projects.join("session.jsonl"), bytes).unwrap();
            let corpus = Self {
                root,
                db: data.join("agent_search.db"),
                data,
                codex,
                claude,
                source,
                binary,
                binary_sha256,
            };
            let mut index = corpus.command("index");
            index
                .args(["--full", "--semantic", "--embedder", "hash", "--json"])
                .arg("--data-dir")
                .arg(&corpus.data);
            decode(output(index));

            // A remote session deliberately shares the exact local source path.
            let storage = FrankenStorage::open(&corpus.db).unwrap();
            let agent = storage
                .ensure_agent(&Agent {
                    id: None,
                    slug: "codex".into(),
                    name: "Codex".into(),
                    version: None,
                    kind: AgentKind::Cli,
                })
                .unwrap();
            let remote = storage
                .insert_conversation_tree(
                    agent,
                    None,
                    &Conversation {
                        id: None,
                        agent_slug: "codex".into(),
                        workspace: None,
                        external_id: Some("gh493-native-remote".into()),
                        title: Some("GH493NATIVE remote".into()),
                        source_path: corpus.source.clone(),
                        started_at: Some(1_733_011_200_000),
                        ended_at: None,
                        approx_tokens: None,
                        metadata_json: json!({}),
                        messages: vec![Message {
                            id: None,
                            idx: 0,
                            role: MessageRole::Agent,
                            author: None,
                            created_at: Some(1_733_011_200_000),
                            content: "GH493NATIVE remote evidence".into(),
                            extra_json: json!({}),
                            snippets: Vec::new(),
                        }],
                        source_id: "gh493-remote".into(),
                        origin_host: Some("gh493-remote".into()),
                    },
                )
                .unwrap();
            drop(storage);
            let mut reconcile = corpus.command("index");
            reconcile
                .args([
                    "--reconcile-conversation",
                    &remote.conversation_id.to_string(),
                    "--json",
                ])
                .arg("--data-dir")
                .arg(&corpus.data);
            decode(output(reconcile));
            let mut index = corpus.command("index");
            index
                .args(["--semantic", "--embedder", "hash", "--json"])
                .arg("--data-dir")
                .arg(&corpus.data);
            decode(output(index));
            corpus
        }

        fn command(&self, command: &str) -> Command {
            // ubs:ignore[rust.security.command-executable] -- Test-only Cargo executable or explicit SHA256-checked release candidate; archive and query inputs never select this path.
            let mut cmd = Command::new(&self.binary);
            let empty = self.root.path().join("empty");
            cmd.arg("--db")
                .arg(&self.db)
                .arg(command)
                .current_dir(self.root.path())
                .env("HOME", self.root.path())
                .env("CODEX_HOME", &self.codex)
                .env("CLAUDE_CONFIG_DIR", &self.claude)
                .env("XDG_CONFIG_HOME", self.root.path().join("config"))
                .env("XDG_DATA_HOME", self.root.path().join("xdg"))
                .env("XDG_CACHE_HOME", self.root.path().join("cache"))
                .env("CASS_OPENWEBUI_DATA_ROOT", &empty)
                .env("CASS_CLOUDMCP_DATA_ROOT", &empty)
                .env("PI_SESSIONS_DIR", &empty)
                .env("PI_CODING_AGENT_DIR", &empty)
                .env("PI_CODING_AGENT_SESSION_DIR", &empty)
                .env_remove("PI_CONFIG_DIR")
                .env_remove("PI_PROFILE")
                .env("CASS_RAW_MIRROR", "0")
                .env("CASS_IGNORE_SOURCES_CONFIG", "1")
                .env_remove("CASS_EXCLUDE_PATHS")
                .env("CASS_AUTO_REFRESH", "0")
                .env("CASS_SEMANTIC_EMBEDDER", "hash")
                .env("CASS_VIEW_BUDGET_MS", "30000")
                .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
                .env_remove("CASS_OUTPUT_FORMAT")
                .env_remove("TOON_DEFAULT_FORMAT")
                .env_remove("CASS_TEST_VIEW_SLOW_MS");
            cmd
        }

        fn expected(&self) -> Vec<Expected> {
            let storage =
                open_with_flags(self.db.to_str().unwrap(), OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .unwrap();
            storage.query_map_collect(
                "SELECT m.id, c.id, m.idx, c.source_id, c.source_path, a.slug, m.role, m.content
                 FROM messages m JOIN conversations c ON c.id = m.conversation_id
                 JOIN agents a ON a.id = c.agent_id ORDER BY c.id, m.idx",
                &[], |row| {
                    let role = row.get_typed::<String>(6)?;
                    Ok(Expected { message_id: row.get_typed(0)?, conversation_id: row.get_typed(1)?,
                        index: usize::try_from(row.get_typed::<i64>(2)?).unwrap() + 1,
                        source: row.get_typed(3)?, path: row.get_typed(4)?, agent: row.get_typed(5)?,
                        role: if role == "agent" { "assistant".into() } else { role }, content: row.get_typed(7)? })
                }).unwrap()
        }

        fn search(&self, mode: &str, extra: &[&str]) -> Value {
            let mut command = self.command("search");
            command
                .args([
                    QUERY,
                    "--mode",
                    mode,
                    "--model",
                    "hash",
                    "--json",
                    "--robot-meta",
                    "--no-maintenance",
                    "--limit",
                    "30",
                    "--timeout",
                    "60000",
                ])
                .args(extra)
                .arg("--data-dir")
                .arg(&self.data);
            decode(output(command))
        }

        fn follow(&self, command: &str, expected: &Expected, anchor: &str) -> Output {
            let mut cmd = self.command(command);
            cmd.arg(&expected.path).args([
                "--source",
                &expected.source,
                "--conversation-id",
                &expected.conversation_id.to_string(),
                "--message-index",
                &expected.index.to_string(),
                "--message-anchor",
                anchor,
                "-C",
                "0",
                "--json",
            ]);
            output(cmd)
        }

        fn pack(&self, mode: &str, max_excerpt: &str) -> Value {
            let mut command = self.command("pack");
            command
                .args([
                    QUERY,
                    "--mode",
                    mode,
                    "--json",
                    "--freshness-policy",
                    "allow-stale",
                    "--max-tokens",
                    "24000",
                    "--max-sessions",
                    "8",
                    "--max-evidence",
                    "20",
                    "--max-excerpt-chars",
                    max_excerpt,
                    "--require-evidence",
                    "--timeout",
                    "60000",
                ])
                .arg("--data-dir")
                .arg(&self.data);
            decode(output(command))
        }
    }

    #[test]
    fn native_pack_requires_the_selected_semantic_producer_and_reports_unfinished_work() {
        let corpus = Corpus::new();
        let original = corpus.expected();
        // Hash vectors exist, but cannot stand in for the explicitly selected
        // native producer, whose model and vectors were never installed here.
        let mut semantic = corpus.command("pack");
        semantic
            .env("CASS_SEMANTIC_EMBEDDER", "minilm")
            .args([
                QUERY,
                "--mode",
                "semantic",
                "--json",
                "--timeout",
                "60000",
                "--data-dir",
            ])
            .arg(&corpus.data);
        let refused = output(semantic);
        assert_eq!(refused.status.code(), Some(15));
        let stderr = String::from_utf8_lossy(&refused.stderr);
        let error: Value = serde_json::from_str(
            stderr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(error["error"]["kind"], "semantic-unavailable");

        let mut hybrid = corpus.command("pack");
        hybrid
            .env("CASS_SEMANTIC_EMBEDDER", "minilm")
            .args([
                QUERY,
                "--mode",
                "hybrid",
                "--json",
                "--freshness-policy",
                "allow-stale",
                "--timeout",
                "60000",
                "--data-dir",
            ])
            .arg(&corpus.data);
        let fallback = decode(output(hybrid));
        assert_eq!(fallback["realized"]["search_mode"], "lexical");
        assert_eq!(fallback["realized"]["fallback_mode"], "lexical");
        assert_eq!(fallback["realized"]["semantic_joined"], false);
        assert!(
            fallback["evidence"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty())
        );
        assert!(
            fallback["realized"]["fallback_reason"]
                .as_str()
                .is_some_and(|reason| {
                    reason.starts_with("semantic_") && reason != "semantic_not_applied"
                })
        );

        let mut delayed = corpus.command("pack");
        delayed
            .env("CASS_TEST_SEARCH_SEMANTIC_SETUP_SLOW_MS", "60000")
            .args([
                QUERY,
                "--mode",
                "semantic",
                "--json",
                "--timeout",
                "1000",
                "--data-dir",
            ])
            .arg(&corpus.data);
        let started = std::time::Instant::now();
        let delayed = output(delayed);
        assert_eq!(delayed.status.code(), Some(10));
        let error: Value = serde_json::from_slice(&delayed.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "timeout");
        assert_eq!(error["error"]["retryable"], true);
        let partial: Value = serde_json::from_slice(&delayed.stdout).unwrap();
        assert!(started.elapsed() < Duration::from_secs(30));
        assert_eq!(partial["budget"]["timed_out"], true);
        assert_eq!(partial["realized"]["semantic_joined"], false);
        assert!(partial["evidence"].as_array().is_some_and(Vec::is_empty));

        let after = corpus.expected();
        assert_eq!(after.len(), original.len());
        for (before, after) in original.iter().zip(&after) {
            assert_eq!(before.message_id, after.message_id);
            assert_eq!(before.conversation_id, after.conversation_id);
            assert_eq!(before.source, after.source);
            assert_eq!(before.index, after.index);
            assert_eq!(before.content, after.content);
        }
        eprintln!(
            "GH493_PACK_NEGATIVES EXPLICIT_NATIVE_REFUSAL=15 HYBRID_FALLBACK=lexical TIMEOUT_SEMANTIC_JOINED=false ARCHIVE_MESSAGES={}",
            after.len()
        );
    }

    #[test]
    fn native_index_search_followups_and_packs_preserve_complete_content_anchors() {
        let started = std::time::Instant::now();
        let corpus = Corpus::new();
        let expected = corpus.expected();
        let conversations = expected
            .iter()
            .map(|row| row.conversation_id)
            .collect::<HashSet<_>>()
            .len();
        eprintln!(
            "GH493_CORPUS KIND=synthetic_native PLATFORM={}/{} CONVERSATIONS={conversations} MESSAGES={} CODEX_SHA256={} CLAUDE_SHA256={}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            expected.len(),
            binary_sha256(&corpus.source),
            binary_sha256(&corpus.claude.join("projects/gh493-native/session.jsonl"))
        );
        assert!(expected.iter().any(|row| row.agent == "claude_code"));
        assert!(expected.iter().any(|row| row.agent == "codex"));
        assert!(expected.iter().any(|row| row.role == "tool"));
        assert!(
            expected
                .iter()
                .any(|row| row.content.contains("naïve café 日本語")
                    && row.content.contains("second block 😀"))
        );
        assert!(
            expected
                .iter()
                .any(|row| row.content == "GH493NATIVE redacted [REDACTED]")
        );
        let duplicates: HashSet<_> = expected
            .iter()
            .filter(|row| row.content == DUPLICATE)
            .map(|row| (row.conversation_id, row.index))
            .collect();
        assert_eq!(
            duplicates.len(),
            3,
            "same text must survive in two native sessions"
        );
        let local = expected
            .iter()
            .find(|row| row.path == corpus.source.to_string_lossy() && row.source == "local")
            .unwrap();
        let remote = expected
            .iter()
            .find(|row| row.source == "gh493-remote")
            .unwrap();
        assert_eq!(local.path, remote.path);
        assert_ne!(local.conversation_id, remote.conversation_id);
        assert_ne!(local.anchor(), remote.anchor());

        for mode in ["lexical", "semantic", "hybrid"] {
            let payload = corpus.search(mode, &["--fields", "source_path,source_id,conversation_id,line_number,agent,content,message_anchor"]);
            assert_eq!(payload["_meta"]["search_mode"], mode);
            if mode == "hybrid" {
                assert_eq!(payload["_meta"]["semantic_refinement"], true);
            }
            let hits = payload["hits"].as_array().unwrap();
            assert!(
                !hits.is_empty(),
                "{mode} must execute a real retrieval tier"
            );
            eprintln!(
                "GH493_SEARCH MODE={mode} HITS={} GENERATION={} ELAPSED_MS={}",
                hits.len(),
                payload["_meta"]["query_plan"]["index_generation"]["lexical_shard_generation"],
                started.elapsed().as_millis()
            );
            let mut found = HashSet::new();
            for hit in hits {
                let row = expected
                    .iter()
                    .find(|row| {
                        hit["conversation_id"] == row.conversation_id
                            && hit["line_number"] == row.index
                    })
                    .unwrap();
                assert_eq!(hit["source_id"], row.source);
                assert_eq!(hit["source_path"], row.path);
                assert_eq!(hit["agent"], row.agent);
                assert_eq!(hit["content"], row.content);
                assert_eq!(hit["message_anchor"], row.anchor());
                assert!(found.insert((row.conversation_id, row.index)));
                for command in ["view", "expand"] {
                    row.assert_followup(
                        &decode(corpus.follow(command, row, &row.anchor())),
                        command,
                    );
                }
            }
            if mode == "lexical" {
                let query_rows: HashSet<_> = expected
                    .iter()
                    .filter(|row| row.content.contains(QUERY))
                    .map(|row| (row.conversation_id, row.index))
                    .collect();
                assert_eq!(
                    found, query_rows,
                    "lexical must retrieve every native query row, including tool-only and Unicode multiblock messages"
                );
                assert!(duplicates.is_subset(&found));
            }
            let packed = corpus.pack(mode, "8000");
            assert_eq!(packed["realized"]["search_mode"], mode);
            assert!(packed["realized"]["fallback_mode"].is_null());
            let evidence = packed["evidence"].as_array().unwrap();
            assert!(!evidence.is_empty());
            let packed_identities: HashSet<_> = evidence
                .iter()
                .map(|item| {
                    (
                        item["citation"]["conversation_id"].as_i64().unwrap(),
                        item["citation"]["message_index"].as_u64().unwrap() as usize,
                    )
                })
                .collect();
            if mode == "lexical" {
                for row in expected.iter().filter(|row| {
                    row.role == "tool"
                        || row.content.contains("naïve café 日本語")
                        || row.content == "GH493NATIVE redacted [REDACTED]"
                }) {
                    assert!(
                        packed_identities.contains(&(row.conversation_id, row.index)),
                        "native special-case row must reach actual pack hydration"
                    );
                }
            }
            eprintln!(
                "GH493_PACK MODE={mode} EVIDENCE={} GENERATION={} ELAPSED_MS={}",
                evidence.len(),
                packed["health"]["index_generation"],
                started.elapsed().as_millis()
            );
            for item in evidence {
                let citation = &item["citation"];
                let row = expected
                    .iter()
                    .find(|row| {
                        citation["conversation_id"] == row.conversation_id
                            && citation["message_index"] == row.index
                    })
                    .unwrap();
                assert!(found.contains(&(row.conversation_id, row.index)));
                assert_eq!(citation["message_index_base"], 1);
                if citation["followup_unavailable_reason"].is_null() {
                    assert_eq!(citation["source_id"], row.source);
                    assert_eq!(citation["source_path"], row.path);
                } else {
                    assert_eq!(
                        citation["followup_unavailable_reason"],
                        "redacted-source-coordinates"
                    );
                    assert!(
                        citation["source_id"] != row.source || citation["source_path"] != row.path
                    );
                    assert!(item["redactions"].is_array());
                }
                assert_eq!(citation["agent"], row.agent);
                assert_eq!(citation["message_anchor"], row.anchor());
                assert_eq!(item["excerpt"], row.content);
                assert_eq!(
                    citation["verified"],
                    row.source == "local" && row.agent == "codex",
                    "local Codex header provenance must verify; remote and unsupported formats must remain unverified"
                );
                assert_eq!(
                    citation["excerpt_sha256"],
                    hex::encode(Sha256::digest(row.content.as_bytes()))
                );
                if citation["verified"] == true {
                    assert_eq!(
                        row.source, "local",
                        "remote paths cannot be verified as local bytes"
                    );
                    let line = citation["line_start"].as_u64().unwrap() as usize;
                    assert!(line > 0);
                    assert_eq!(citation["line_end"], line);
                    let source = std::fs::read_to_string(&row.path).unwrap();
                    let session: Value =
                        serde_json::from_str(source.lines().next().unwrap()).unwrap();
                    assert_eq!(session["type"], "session_meta");
                    assert_eq!(session["payload"]["id"], "gh493-native-codex");
                    let record = source.lines().nth(line - 1).unwrap();
                    assert_eq!(
                        citation["span_hash"],
                        blake3::hash(record.as_bytes()).to_hex().to_string()
                    );
                    let raw: Value = serde_json::from_str(record).unwrap();
                    let raw_role = if raw["payload"]["type"] == "function_call_output" {
                        "tool"
                    } else {
                        raw["payload"]["role"].as_str().unwrap()
                    };
                    assert_eq!(raw_role, row.role);
                    let body = if let Some(blocks) = raw["payload"]["content"].as_array() {
                        blocks
                            .iter()
                            .filter_map(|block| block["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        let call_id = raw["payload"]["call_id"]
                            .as_str()
                            .expect("verified tool output call ID");
                        let output = raw["payload"]["output"]
                            .as_str()
                            .expect("verified tool output");
                        format!("[Tool output: {call_id}]\n{output}")
                    };
                    assert_eq!(
                        coding_agent_search::indexer::redact_secrets::redact_text(&body).as_ref(),
                        row.content
                    );
                } else {
                    assert!(citation["line_start"].is_null());
                    assert!(citation["line_end"].is_null());
                }
                for command in ["view", "expand"] {
                    row.assert_followup(
                        &decode(corpus.follow(
                            command,
                            row,
                            citation["message_anchor"].as_str().unwrap(),
                        )),
                        command,
                    );
                }
            }
        }

        let long = expected
            .iter()
            .find(|row| row.content.starts_with("GH493NATIVE long "))
            .unwrap();
        for command in ["view", "expand"] {
            let output = output({
                let mut process = corpus.command(command);
                process.arg(&long.path).args([
                    "--message-index",
                    &long.index.to_string(),
                    "--source",
                    &long.source,
                    "--conversation-id",
                    &long.conversation_id.to_string(),
                    "--json",
                ]);
                process
            });
            assert!(
                !output.status.success(),
                "unanchored target must be refused"
            );
            assert!(output.stdout.is_empty());
            assert!(String::from_utf8_lossy(&output.stderr).contains("message-anchor"));
        }
        let projected = corpus.search(
            "lexical",
            &[
                "--fields",
                "conversation_id,line_number,message_anchor,content",
                "--max-content-length",
                "16",
            ],
        );
        let hit = projected["hits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|hit| {
                hit["conversation_id"] == long.conversation_id && hit["line_number"] == long.index
            })
            .unwrap();
        assert_ne!(hit["content"], long.content);
        assert_eq!(
            hit["message_anchor"],
            long.anchor(),
            "anchor must precede display truncation"
        );
        let shortened = corpus.pack("lexical", "80");
        let shortened = shortened["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| {
                item["citation"]["conversation_id"] == long.conversation_id
                    && item["citation"]["message_index"] == long.index
            })
            .expect("long evidence must be retained with a shortened excerpt");
        assert_eq!(shortened["excerpt_truncated"], true);
        assert_ne!(shortened["excerpt"], long.content);
        assert_eq!(shortened["citation"]["message_anchor"], long.anchor());
        for command in ["view", "expand"] {
            long.assert_followup(
                &decode(corpus.follow(command, long, &long.anchor())),
                command,
            );
        }

        let redacted = expected
            .iter()
            .find(|row| row.content == "GH493NATIVE redacted [REDACTED]")
            .unwrap();
        assert!(
            !std::fs::read_to_string(&corpus.source)
                .unwrap()
                .contains(&redacted.content)
        );
        let original_raw = std::fs::read_to_string(&corpus.source).unwrap();
        for changed_source in [
            original_raw.replace("gh493-native-codex", "gh493-unrelated-session"),
            original_raw.replace("\"role\":\"assistant\"", "\"role\":\"user\""),
        ] {
            assert_ne!(changed_source, original_raw);
            std::fs::write(&corpus.source, changed_source).unwrap();
            let changed_pack = corpus.pack("lexical", "8000");
            let cited = changed_pack["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| {
                    item["citation"]["conversation_id"] == redacted.conversation_id
                        && item["citation"]["message_index"] == redacted.index
                })
                .expect("canonical evidence survives a changed raw session or role");
            assert_eq!(cited["citation"]["verified"], false);
            assert!(cited["citation"]["line_start"].is_null());
            assert!(cited["citation"]["line_end"].is_null());
            assert_eq!(cited["citation"]["message_anchor"], redacted.anchor());
            assert_eq!(cited["excerpt"], redacted.content);
        }
        std::fs::write(&corpus.source, &original_raw).unwrap();
        std::fs::write(
            &corpus.source,
            "{\"role\":\"assistant\",\"content\":\"GH493NATIVE unrelated raw replacement\"}\n",
        )
        .unwrap();
        for command in ["view", "expand"] {
            redacted.assert_followup(
                &decode(corpus.follow(command, redacted, &redacted.anchor())),
                command,
            );
        }
        std::fs::rename(
            &corpus.source,
            corpus.source.with_extension("retained-jsonl"),
        )
        .unwrap();
        for command in ["view", "expand"] {
            redacted.assert_followup(
                &decode(corpus.follow(command, redacted, &redacted.anchor())),
                command,
            );
            remote.assert_followup(
                &decode(corpus.follow(command, remote, &remote.anchor())),
                command,
            );
        }
        let missing_source_pack = corpus.pack("lexical", "8000");
        for item in missing_source_pack["evidence"].as_array().unwrap() {
            let citation = &item["citation"];
            let row = expected
                .iter()
                .find(|row| {
                    citation["conversation_id"] == row.conversation_id
                        && citation["message_index"] == row.index
                })
                .unwrap();
            if row.path == corpus.source.to_string_lossy().as_ref() {
                assert_eq!(citation["verified"], false);
                assert!(citation["line_start"].is_null());
                assert!(citation["line_end"].is_null());
                assert_eq!(citation["message_anchor"], row.anchor());
                assert_eq!(item["excerpt"], row.content);
                for command in ["view", "expand"] {
                    row.assert_followup(
                        &decode(corpus.follow(command, row, &row.anchor())),
                        command,
                    );
                }
            }
        }

        let storage = FrankenStorage::open(&corpus.db).unwrap();
        storage
            .raw()
            .execute_compat(
                "UPDATE messages SET content = ?1 WHERE id = ?2",
                coding_agent_search::franken_sync::params![
                    "GH493NATIVE different text at the same ordinal",
                    redacted.message_id
                ],
            )
            .unwrap();
        drop(storage);
        for command in ["view", "expand"] {
            let failed = corpus.follow(command, redacted, &redacted.anchor());
            assert!(!failed.status.success());
            assert!(failed.stdout.is_empty(), "no stale target may escape");
            let error: Value = serde_json::from_slice(&failed.stderr).unwrap();
            assert_eq!(error["error"]["kind"], "stale-message-anchor");
            for invalid in [
                "",
                "v1:broken",
                "v2:0000000000000000000000000000000000000000000000000000000000000000",
            ] {
                let failed = corpus.follow(command, redacted, invalid);
                assert!(!failed.status.success());
                assert!(failed.stdout.is_empty());
                let error: Value = serde_json::from_slice(&failed.stderr).unwrap();
                assert_eq!(error["error"]["kind"], "invalid-message-anchor");
            }
            // A valid anchor copied from a different session/source also fails.
            let failed = corpus.follow(command, local, &remote.anchor());
            assert!(!failed.status.success());
            assert!(failed.stdout.is_empty());
        }
        assert_eq!(
            binary_sha256(&corpus.binary),
            corpus.binary_sha256,
            "executed candidate changed during acceptance"
        );
        eprintln!(
            "GH493_NEGATIVES REWRITE=passed MISSING_SOURCE=passed STALE_ANCHOR=passed MALFORMED_ANCHOR=passed SOURCE_COLLISION=passed ELAPSED_MS={}",
            started.elapsed().as_millis()
        );
        eprintln!("GH493_BINARY_AFTER SHA256={}", corpus.binary_sha256);
    }

    #[test]
    fn anchor_frames_every_identity_component_and_exact_unicode_bytes() {
        let anchor = |source, path, conversation, index, content| {
            message_anchor_for_content(source, path, conversation, index, content)
        };
        let baseline = anchor("local", "/session", Some(1), Some(2), "café\n日本語").unwrap();
        let alternatives = [
            anchor("remote", "/session", Some(1), Some(2), "café\n日本語"),
            anchor("local", "/other", Some(1), Some(2), "café\n日本語"),
            anchor("local", "/session", Some(2), Some(2), "café\n日本語"),
            anchor("local", "/session", Some(1), Some(3), "café\n日本語"),
            anchor("local", "/session", Some(1), Some(2), "café 日本語"),
        ];
        assert!(
            alternatives
                .into_iter()
                .all(|value| value.unwrap() != baseline)
        );
        assert_ne!(
            anchor("a\nb", "c", Some(1), Some(2), "body"),
            anchor("a", "b\nc", Some(1), Some(2), "body")
        );
        assert!(anchor("local", "/session", None, Some(2), "body").is_none());
        assert!(anchor("local", "/session", Some(1), None, "body").is_none());
        assert!(anchor("local", "/session", Some(0), Some(2), "body").is_none());
        assert!(anchor("local", "/session", Some(1), Some(0), "body").is_none());
    }
}
