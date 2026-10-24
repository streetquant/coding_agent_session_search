//! Ledger oracle for `cass analytics` rollups, raw fallbacks, CLI projections,
//! and the TUI chart projection.
//!
//! The expected values here are derived from the source message ledger, not
//! from the analytics tables: conversations are inserted through the real
//! storage API, provider token counts are extracted with the same provider
//! connector functions ingest uses, and expected costs are computed from rates
//! independently of the pricing helper that writes the ledger.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use assert_cmd::cargo::cargo_bin_cmd;
use chrono::{NaiveDate, TimeZone, Utc};
use coding_agent_search::analytics::query::{
    query_breakdown, query_cost_timeseries, query_session_scatter, query_status,
    query_tokens_timeseries, query_tools, query_unpriced_models,
};
use coding_agent_search::analytics::{
    AnalyticsFilter, Dim, GroupBy, Metric, SourceFilter, UsageBucket,
};
use coding_agent_search::connectors::{extract_tokens_for_agent, normalize_model};
use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::storage::sqlite::{FrankenStorage, PricingTable};
use coding_agent_search::ui::analytics_charts::load_chart_data;
use coding_agent_search::ui::app::AnalyticsFilterState;
use serde_json::{Value, json};
use tempfile::TempDir;

fn utc_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32, ms: u32) -> i64 {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, s)
        .single()
        .expect("valid UTC timestamp")
        .timestamp_millis()
        + i64::from(ms)
}

fn day_label(day_id: i64) -> String {
    Utc.timestamp_millis_opt(FrankenStorage::millis_from_day_id(day_id))
        .single()
        .expect("valid day boundary")
        .format("%Y-%m-%d")
        .to_string()
}

fn pad_to_quad(content: &str) -> String {
    let mut out = content.to_string();
    while !out.len().is_multiple_of(4) {
        out.push(' ');
    }
    out
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn role_str(role: &MessageRole) -> &str {
    match role {
        MessageRole::User => "user",
        MessageRole::Agent => "agent",
        MessageRole::Tool => "tool",
        MessageRole::System => "system",
        MessageRole::Other(value) => value.as_str(),
    }
}

fn is_assistant_role(role: &str) -> bool {
    role == "assistant" || role == "agent"
}

#[derive(Clone)]
struct MessageSpec {
    role: MessageRole,
    created_at: Option<i64>,
    content: String,
    extra: Value,
    plan: bool,
}

#[derive(Clone)]
struct ConvSpec {
    agent: &'static str,
    workspace: &'static str,
    source_id: &'static str,
    origin_host: Option<&'static str>,
    external_id: &'static str,
    started_at: i64,
    messages: Vec<MessageSpec>,
    /// When set, overwrites `conversations.grand_total_tokens` to exercise the
    /// scatter query's max(conversation rollup, message ledger) choice.
    grand_total_override: Option<i64>,
}

#[derive(Debug, Clone)]
struct Fact {
    source_id: String,
    agent: &'static str,
    workspace_id: i64,
    conversation_id: i64,
    source_path: String,
    role: String,
    timestamp_ms: i64,
    day_id: i64,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_creation: i64,
    thinking: i64,
    total: i64,
    cost: Option<f64>,
    api: bool,
    tool_calls: i64,
    content_chars: i64,
    content_est: i64,
    plan: bool,
    model_name: Option<String>,
    model_family: String,
}

impl Fact {
    fn api_total(&self) -> i64 {
        self.input + self.output + self.cache_read + self.cache_creation + self.thinking
    }

    fn total(&self) -> i64 {
        self.total
    }

    fn track_a_bucket(&self) -> UsageBucket {
        let content = self.content_est;
        UsageBucket {
            message_count: 1,
            user_message_count: i64::from(self.role == "user"),
            assistant_message_count: i64::from(is_assistant_role(&self.role)),
            tool_call_count: self.tool_calls,
            plan_message_count: i64::from(self.plan),
            api_coverage_message_count: i64::from(self.api),
            content_tokens_est_total: content,
            content_tokens_est_user: if self.role == "user" { content } else { 0 },
            content_tokens_est_assistant: if is_assistant_role(&self.role) {
                content
            } else {
                0
            },
            api_tokens_total: if self.api { self.api_total() } else { 0 },
            api_input_tokens_total: if self.api { self.input } else { 0 },
            api_output_tokens_total: if self.api { self.output } else { 0 },
            api_cache_read_tokens_total: if self.api { self.cache_read } else { 0 },
            api_cache_creation_tokens_total: if self.api { self.cache_creation } else { 0 },
            api_thinking_tokens_total: if self.api { self.thinking } else { 0 },
            plan_content_tokens_est_total: if self.plan { content } else { 0 },
            plan_api_tokens_total: if self.plan && self.api {
                self.api_total()
            } else {
                0
            },
            estimated_cost_usd: 0.0,
        }
    }

    fn track_b_bucket_rollup(&self) -> UsageBucket {
        // `token_daily_stats` stores SUM(content_chars); the rollup query
        // divides once. Fixture content is padded to 4-byte multiples so the
        // per-row and aggregate estimates agree.
        UsageBucket {
            message_count: 1,
            user_message_count: i64::from(self.role == "user"),
            assistant_message_count: i64::from(is_assistant_role(&self.role)),
            tool_call_count: self.tool_calls,
            api_coverage_message_count: 1,
            content_tokens_est_total: self.content_chars / 4,
            api_tokens_total: self.total,
            api_input_tokens_total: self.input,
            api_output_tokens_total: self.output,
            api_cache_read_tokens_total: self.cache_read,
            api_cache_creation_tokens_total: self.cache_creation,
            api_thinking_tokens_total: self.thinking,
            estimated_cost_usd: self.cost.unwrap_or(0.0),
            ..Default::default()
        }
    }
}

fn sum_facts<'a>(facts: impl Iterator<Item = &'a Fact>, track_b: bool) -> UsageBucket {
    let mut total = UsageBucket::default();
    for fact in facts {
        let bucket = if track_b {
            fact.track_b_bucket_rollup()
        } else {
            fact.track_a_bucket()
        };
        total.merge(&bucket);
    }
    total
}

fn bucket_map<'a>(
    facts: impl Iterator<Item = &'a Fact>,
    track_b: bool,
    key: impl Fn(&Fact) -> String,
) -> BTreeMap<String, UsageBucket> {
    let mut map: BTreeMap<String, UsageBucket> = BTreeMap::new();
    for fact in facts {
        let bucket = if track_b {
            fact.track_b_bucket_rollup()
        } else {
            fact.track_a_bucket()
        };
        map.entry(key(fact)).or_default().merge(&bucket);
    }
    map
}

fn assert_f64_close(actual: f64, expected: f64, ctx: &str) {
    let tolerance = (expected.abs() * 1e-12).max(1e-9);
    assert!(
        (actual - expected).abs() <= tolerance,
        "{ctx}: actual {actual} expected {expected} (tolerance {tolerance})"
    );
}

fn assert_bucket_eq(actual: &UsageBucket, expected: &UsageBucket, ctx: &str) {
    macro_rules! field {
        ($name:ident) => {
            assert_eq!(
                actual.$name,
                expected.$name,
                "{ctx}: field {} actual {} expected {}",
                stringify!($name),
                actual.$name,
                expected.$name
            );
        };
    }
    field!(message_count);
    field!(user_message_count);
    field!(assistant_message_count);
    field!(tool_call_count);
    field!(plan_message_count);
    field!(api_coverage_message_count);
    field!(content_tokens_est_total);
    field!(content_tokens_est_user);
    field!(content_tokens_est_assistant);
    field!(api_tokens_total);
    field!(api_input_tokens_total);
    field!(api_output_tokens_total);
    field!(api_cache_read_tokens_total);
    field!(api_cache_creation_tokens_total);
    field!(api_thinking_tokens_total);
    field!(plan_content_tokens_est_total);
    field!(plan_api_tokens_total);
    assert_f64_close(actual.estimated_cost_usd, expected.estimated_cost_usd, ctx);
}

/// Independently recompute provider cost from the seeded per-MTok rates.
fn oracle_cost(
    model: Option<&str>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_creation: i64,
) -> Option<f64> {
    let model = model?;
    let lower = model.to_lowercase();
    let (input_rate, output_rate, cache_read_rate, cache_creation_rate) =
        if lower.starts_with("claude-sonnet-4") {
            (3.0, 15.0, Some(0.3), Some(3.75))
        } else if lower.starts_with("claude-haiku-4") {
            (0.8, 4.0, Some(0.08), Some(1.0))
        } else if lower.starts_with("gpt-4o") {
            (2.5, 10.0, None, None)
        } else {
            return None;
        };
    let non_cache_input = input
        .saturating_sub(cache_read)
        .saturating_sub(cache_creation)
        .max(0);
    let mut cost = non_cache_input as f64 * input_rate / 1_000_000.0;
    cost += output as f64 * output_rate / 1_000_000.0;
    if let Some(rate) = cache_read_rate {
        cost += cache_read as f64 * rate / 1_000_000.0;
    }
    if let Some(rate) = cache_creation_rate {
        cost += cache_creation as f64 * rate / 1_000_000.0;
    }
    Some(cost)
}

fn claude_usage(
    model: &str,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_creation: i64,
    tools: u32,
) -> Value {
    json!({
        "cass": {
            "model": model,
            "tool_call_count": tools,
            "token_usage": {
                "input_tokens": input,
                "output_tokens": output,
                "cache_read_tokens": cache_read,
                "cache_creation_tokens": cache_creation,
                "data_source": "api"
            }
        }
    })
}

fn codex_usage(model: &str, input: i64, output: i64) -> Value {
    json!({
        "cass": {
            "model": model,
            "token_usage": {
                "input_tokens": input,
                "output_tokens": output,
                "data_source": "api"
            }
        }
    })
}

struct LedgerFixture {
    _tmp: TempDir,
    db_path: PathBuf,
    facts: Vec<Fact>,
    message_count: usize,
    grand_totals: BTreeMap<i64, i64>,
}

impl LedgerFixture {
    fn build() -> Self {
        let tmp = TempDir::new().expect("temp dir");
        let db_path = tmp.path().join("agent_search.db");
        let storage = FrankenStorage::open(&db_path).expect("open storage");

        let mut agents = BTreeMap::new();
        for (slug, name) in [("claude_code", "Claude Code"), ("codex", "Codex")] {
            let id = storage
                .ensure_agent(&Agent {
                    id: None,
                    slug: slug.to_string(),
                    name: name.to_string(),
                    version: Some("1.0.0".to_string()),
                    kind: AgentKind::Cli,
                })
                .expect("ensure agent");
            agents.insert(slug, id);
        }
        let mut workspaces = BTreeMap::new();
        for name in ["workspace-a", "workspace-b"] {
            let path = PathBuf::from(format!("/srv/{name}"));
            let id = storage
                .ensure_workspace(&path, Some(name))
                .expect("ensure workspace");
            workspaces.insert(name, (id, path));
        }

        let day1 = utc_ms(2026, 1, 15, 0, 0, 0, 0);
        let day1_end = utc_ms(2026, 1, 15, 23, 59, 59, 999);
        let day2 = utc_ms(2026, 1, 16, 12, 0, 0, 0);
        let day3 = utc_ms(2026, 1, 17, 0, 0, 0, 0);
        let day4 = utc_ms(2026, 1, 18, 0, 0, 0, 0);

        let specs = vec![
            ConvSpec {
                agent: "claude_code",
                workspace: "workspace-a",
                source_id: "local",
                origin_host: None,
                external_id: "conv-claude-api",
                started_at: day1,
                grand_total_override: Some(0),
                messages: vec![
                    MessageSpec {
                        role: MessageRole::User,
                        created_at: Some(day1),
                        content: pad_to_quad("Please summarize the analytics ledger for me."),
                        extra: json!({}),
                        plan: false,
                    },
                    MessageSpec {
                        role: MessageRole::Agent,
                        created_at: Some(day1 + 1_000),
                        content: pad_to_quad(
                            "## Plan\n1. Inspect ledger\n2. Rebuild rollups\n3. Assert totals",
                        ),
                        extra: claude_usage("claude-sonnet-4-20250514", 1000, 500, 200, 100, 2),
                        plan: true,
                    },
                    MessageSpec {
                        role: MessageRole::Agent,
                        created_at: Some(day1_end),
                        content: pad_to_quad("Boundary response at the edge of the day."),
                        extra: claude_usage("claude-sonnet-4-20250514", 300, 100, 0, 0, 0),
                        plan: false,
                    },
                ],
            },
            ConvSpec {
                agent: "codex",
                workspace: "workspace-b",
                source_id: "work-laptop",
                origin_host: Some("work-laptop"),
                external_id: "conv-codex-api",
                started_at: day2,
                grand_total_override: None,
                messages: vec![
                    MessageSpec {
                        role: MessageRole::User,
                        created_at: Some(day2),
                        content: pad_to_quad("Compare the two agents in this workspace."),
                        extra: json!({}),
                        plan: false,
                    },
                    MessageSpec {
                        role: MessageRole::Agent,
                        created_at: Some(day2 + 1_000),
                        content: pad_to_quad(
                            "Implemented the rollup oracle and compared every bucket.",
                        ),
                        extra: codex_usage("gpt-4o", 2000, 800),
                        plan: false,
                    },
                    MessageSpec {
                        role: MessageRole::Agent,
                        created_at: Some(day2 + 2_000),
                        content: pad_to_quad("Unpriced model response for coverage accounting."),
                        extra: codex_usage("mystery-model-x", 10_000, 5_000),
                        plan: false,
                    },
                ],
            },
            ConvSpec {
                agent: "claude_code",
                workspace: "workspace-a",
                source_id: "local",
                origin_host: None,
                external_id: "conv-estimated-only",
                started_at: day3,
                grand_total_override: Some(0),
                messages: vec![MessageSpec {
                    role: MessageRole::Agent,
                    created_at: Some(day3),
                    content: pad_to_quad(
                        "Estimated only: tokens were inferred from content length.",
                    ),
                    extra: json!({"cass": {"model": "claude-haiku-4-20250514"}}),
                    plan: false,
                }],
            },
            ConvSpec {
                agent: "codex",
                workspace: "workspace-b",
                source_id: "local",
                origin_host: None,
                external_id: "conv-huge-tokens",
                started_at: day4,
                grand_total_override: Some(0),
                messages: vec![MessageSpec {
                    role: MessageRole::Agent,
                    created_at: Some(day4),
                    content: pad_to_quad("Huge token response."),
                    extra: codex_usage("gpt-4o", 6_000_000_000_000, 3_000_000_000_000),
                    plan: false,
                }],
            },
        ];

        let pricing = PricingTable::franken_load(storage.raw()).expect("load pricing");
        let mut facts = Vec::new();
        let mut message_count = 0usize;
        let mut token_sql = Vec::new();

        for spec in &specs {
            let agent_id = agents[spec.agent];
            let (workspace_id, workspace_path) = &workspaces[spec.workspace];
            let conversation = Conversation {
                id: None,
                agent_slug: spec.agent.to_string(),
                workspace: Some(workspace_path.clone()),
                external_id: Some(spec.external_id.to_string()),
                title: Some(spec.external_id.to_string()),
                source_path: PathBuf::from(format!("/fixtures/{}.jsonl", spec.external_id)),
                started_at: Some(spec.started_at),
                ended_at: Some(spec.started_at + 86_400_000),
                approx_tokens: None,
                metadata_json: json!({}),
                messages: spec
                    .messages
                    .iter()
                    .enumerate()
                    .map(|(idx, message)| Message {
                        id: None,
                        idx: idx as i64,
                        role: message.role.clone(),
                        author: None,
                        created_at: message.created_at,
                        content: message.content.clone(),
                        extra_json: message.extra.clone(),
                        snippets: Vec::new(),
                    })
                    .collect(),
                source_id: spec.source_id.to_string(),
                origin_host: spec.origin_host.map(str::to_string),
            };
            let outcome = storage
                .insert_conversation_tree(agent_id, Some(*workspace_id), &conversation)
                .expect("insert conversation");
            let stored_messages = storage
                .fetch_messages(outcome.conversation_id)
                .expect("fetch messages");
            assert_eq!(stored_messages.len(), spec.messages.len());

            for (spec_message, stored) in spec.messages.iter().zip(stored_messages.iter()) {
                let role = role_str(&stored.role);
                let usage =
                    extract_tokens_for_agent(spec.agent, &stored.extra_json, &stored.content, role);
                let model_info = usage.model_name.as_deref().map(normalize_model);
                let model_family = model_info
                    .as_ref()
                    .map(|info| info.family.clone())
                    .unwrap_or_else(|| "unknown".to_string());
                let model_tier = model_info
                    .as_ref()
                    .map(|info| info.tier.clone())
                    .unwrap_or_else(|| "unknown".to_string());
                let provider = usage
                    .provider
                    .clone()
                    .or_else(|| model_info.as_ref().map(|info| info.provider.clone()))
                    .unwrap_or_else(|| "unknown".to_string());
                let timestamp_ms = spec_message.created_at.unwrap_or(spec.started_at);
                let day_id = if timestamp_ms > 0 {
                    FrankenStorage::day_id_from_millis(timestamp_ms)
                } else {
                    FrankenStorage::day_id_from_millis(spec.started_at)
                };
                let input = usage.input_tokens.unwrap_or(0);
                let output = usage.output_tokens.unwrap_or(0);
                let cache_read = usage.cache_read_tokens.unwrap_or(0);
                let cache_creation = usage.cache_creation_tokens.unwrap_or(0);
                let thinking = usage.thinking_tokens.unwrap_or(0);
                let total = usage.total_tokens().unwrap_or(0);
                let cost = pricing.compute_cost(
                    usage.model_name.as_deref(),
                    day_id,
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_read_tokens,
                    usage.cache_creation_tokens,
                );
                let expected_cost = oracle_cost(
                    usage.model_name.as_deref(),
                    input,
                    output,
                    cache_read,
                    cache_creation,
                );
                assert_f64_close(
                    cost.unwrap_or(0.0),
                    expected_cost.unwrap_or(0.0),
                    "independent cost oracle",
                );
                assert_eq!(cost.is_some(), expected_cost.is_some(), "pricing presence");

                let content_chars = stored.content.len() as i64;
                let content_est = content_chars / 4;
                assert_eq!(
                    content_chars % 4,
                    0,
                    "fixture content must stay quad-aligned"
                );
                let api = usage.data_source.as_str() == "api";
                let fact = Fact {
                    source_id: spec.source_id.to_string(),
                    agent: spec.agent,
                    workspace_id: *workspace_id,
                    conversation_id: outcome.conversation_id,
                    source_path: stored_path(spec.external_id),
                    role: role.to_string(),
                    timestamp_ms,
                    day_id,
                    input,
                    output,
                    cache_read,
                    cache_creation,
                    thinking,
                    total,
                    cost,
                    api,
                    tool_calls: i64::from(usage.tool_call_count),
                    content_chars,
                    content_est,
                    plan: spec_message.plan,
                    model_name: usage.model_name.clone(),
                    model_family: model_family.clone(),
                };
                token_sql.push(format!(
                    "({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
                    stored.id.expect("stored message id"),
                    outcome.conversation_id,
                    agent_id,
                    *workspace_id,
                    quote(spec.source_id),
                    timestamp_ms,
                    day_id,
                    opt_quote(usage.model_name.as_deref()),
                    quote(&model_family),
                    quote(&model_tier),
                    opt_quote(usage.service_tier.as_deref()),
                    quote(&provider),
                    opt_int(usage.input_tokens),
                    opt_int(usage.output_tokens),
                    opt_int(usage.cache_read_tokens),
                    opt_int(usage.cache_creation_tokens),
                    opt_int(usage.thinking_tokens),
                    opt_int(usage.total_tokens()),
                    cost.map_or_else(|| "NULL".to_string(), |value| format!("{value:.17}")),
                    quote(role),
                    content_chars,
                    i64::from(usage.has_tool_calls),
                    i64::from(usage.tool_call_count),
                    quote(usage.data_source.as_str()),
                ));
                facts.push(fact);
                message_count += 1;
            }
        }

        assert!(!token_sql.is_empty());
        // Replace any inline-analytics ledger rows so this test owns the ledger.
        storage
            .raw()
            .execute_batch("DELETE FROM token_usage;")
            .expect("clear token ledger");
        storage
            .raw()
            .execute_batch(&format!(
                "INSERT INTO token_usage (
                    message_id, conversation_id, agent_id, workspace_id, source_id,
                    timestamp_ms, day_id, model_name, model_family, model_tier,
                    service_tier, provider, input_tokens, output_tokens,
                    cache_read_tokens, cache_creation_tokens, thinking_tokens,
                    total_tokens, estimated_cost_usd, role, content_chars,
                    has_tool_calls, tool_call_count, data_source
                ) VALUES {};",
                token_sql.join(",")
            ))
            .expect("insert token ledger");

        // Rebuild both tracks from the ledger so inline analytics disposition
        // cannot influence the fixture.
        storage.rebuild_analytics().expect("rebuild track A");
        storage
            .rebuild_token_daily_stats()
            .expect("rebuild track B");

        // The scatter query takes the max of the conversation rollup and the
        // message ledger. Give one conversation an intentionally larger
        // rollup and force the others to fall back to the ledger.
        let mut grand_totals = BTreeMap::new();
        for spec in &specs {
            let detailed: i64 = facts
                .iter()
                .filter(|fact| {
                    fact.conversation_id == conversation_id_for(&facts, spec.external_id)
                })
                .map(Fact::api_total)
                .sum();
            let value = spec.grand_total_override.unwrap_or(detailed + 1_000);
            let conv_id = conversation_id_for(&facts, spec.external_id);
            grand_totals.insert(conv_id, value);
            storage
                .raw()
                .execute_batch(&format!(
                    "UPDATE conversations SET grand_total_tokens = {value} WHERE id = {conv_id};"
                ))
                .expect("set conversation rollup");
        }

        Self {
            _tmp: tmp,
            db_path,
            facts,
            message_count,
            grand_totals,
        }
    }

    fn read(&self) -> FrankenStorage {
        FrankenStorage::open_readonly(&self.db_path).expect("open readonly")
    }
}

fn stored_path(external_id: &str) -> String {
    format!("/fixtures/{external_id}.jsonl")
}

fn conversation_id_for(facts: &[Fact], external_id: &str) -> i64 {
    facts
        .iter()
        .find(|fact| fact.source_path == stored_path(external_id))
        .expect("conversation fact")
        .conversation_id
}

fn opt_quote(value: Option<&str>) -> String {
    value.map_or_else(|| "NULL".to_string(), quote)
}

fn opt_int(value: Option<i64>) -> String {
    value.map_or_else(|| "NULL".to_string(), |value| value.to_string())
}

fn filter(since: Option<i64>, until: Option<i64>) -> AnalyticsFilter {
    AnalyticsFilter {
        since_ms: since,
        until_ms: until,
        ..AnalyticsFilter::default()
    }
}

#[test]
fn oracle_ledger_matches_rollups_raw_fallbacks_and_status() {
    let fixture = LedgerFixture::build();
    let db = fixture.read();
    let conn = db.raw();
    let all = AnalyticsFilter::default();

    let track_a_expected = sum_facts(fixture.facts.iter(), false);
    let track_b_expected = sum_facts(fixture.facts.iter(), true);

    // Day rollups: per-day buckets and totals must match the ledger.
    for group_by in [GroupBy::Day, GroupBy::Week, GroupBy::Month, GroupBy::Hour] {
        let result = query_tokens_timeseries(conn, &all, group_by).expect("token timeseries");
        assert_eq!(result.path, "rollup", "{group_by:?}");
        assert_bucket_eq(
            &result.totals,
            &track_a_expected,
            &format!("track A totals {group_by:?}"),
        );
        if matches!(group_by, GroupBy::Day) {
            let expected = bucket_map(fixture.facts.iter(), false, |fact| day_label(fact.day_id));
            assert_eq!(result.buckets.len(), expected.len(), "day bucket count");
            for (label, bucket) in &result.buckets {
                assert_bucket_eq(bucket, expected.get(label).expect("expected bucket"), label);
            }
        }
    }

    // Exact range around 2026-01-16 exercises the raw fallback and must
    // preserve the same ledger totals as the rollup path for that slice.
    let day2_since = utc_ms(2026, 1, 16, 0, 0, 0, 0);
    let day2_until = utc_ms(2026, 1, 16, 23, 59, 59, 999);
    let day2_filter = filter(Some(day2_since), Some(day2_until));
    let raw = query_tokens_timeseries(conn, &day2_filter, GroupBy::Day).expect("raw tokens");
    assert_eq!(raw.path, "raw");
    let raw_expected = sum_facts(
        fixture
            .facts
            .iter()
            .filter(|fact| (day2_since..=day2_until).contains(&fact.timestamp_ms)),
        false,
    );
    assert_bucket_eq(&raw.totals, &raw_expected, "track A raw day2");
    assert!(
        raw.totals.assistant_message_count > 0,
        "raw path must count agent roles"
    );

    // Agent breakdown from rollups and from the raw fallback.
    let rollup =
        query_breakdown(conn, &all, Dim::Agent, Metric::ApiTotal, 10).expect("agent breakdown");
    assert_eq!(rollup.source_table, "usage_daily");
    let expected_agents = bucket_map(fixture.facts.iter(), false, |fact| fact.agent.to_string());
    assert_eq!(rollup.rows.len(), expected_agents.len());
    for row in &rollup.rows {
        assert_eq!(
            row.value, expected_agents[&row.key].api_tokens_total,
            "agent {}",
            row.key
        );
        assert_eq!(row.message_count, expected_agents[&row.key].message_count);
    }
    let raw_agents =
        query_breakdown(conn, &day2_filter, Dim::Agent, Metric::ApiTotal, 10).expect("raw agents");
    assert_eq!(raw_agents.source_table, "message_metrics");
    let expected_day2_agents = bucket_map(
        fixture
            .facts
            .iter()
            .filter(|fact| (day2_since..=day2_until).contains(&fact.timestamp_ms)),
        false,
        |fact| fact.agent.to_string(),
    );
    for row in &raw_agents.rows {
        assert_eq!(row.value, expected_day2_agents[&row.key].api_tokens_total);
    }

    // Source and workspace breakdowns are Track A, with source always raw.
    let sources = query_breakdown(conn, &all, Dim::Source, Metric::ApiTotal, 10).expect("sources");
    let expected_sources = bucket_map(fixture.facts.iter(), false, |fact| fact.source_id.clone());
    assert_eq!(sources.rows.len(), expected_sources.len());
    for row in &sources.rows {
        assert_eq!(
            row.value, expected_sources[&row.key].api_tokens_total,
            "source {}",
            row.key
        );
    }
    let workspaces =
        query_breakdown(conn, &all, Dim::Workspace, Metric::MessageCount, 10).expect("workspaces");
    let expected_workspaces = bucket_map(fixture.facts.iter(), false, |fact| {
        fact.workspace_id.to_string()
    });
    assert_eq!(workspaces.rows.len(), expected_workspaces.len());
    for row in &workspaces.rows {
        assert_eq!(row.value, expected_workspaces[&row.key].message_count);
    }

    // Track B model breakdown groups by normalized family.
    let models = query_breakdown(conn, &all, Dim::Model, Metric::ApiTotal, 10).expect("models");
    let expected_models = bucket_map(fixture.facts.iter(), true, |fact| fact.model_family.clone());
    assert_eq!(models.source_table, "token_daily_stats");
    assert_eq!(models.rows.len(), expected_models.len());
    for row in &models.rows {
        assert_eq!(
            row.value, expected_models[&row.key].api_tokens_total,
            "model {}",
            row.key
        );
    }

    // Cost timeseries must equal the independently priced ledger.
    for group_by in [GroupBy::Day, GroupBy::Week, GroupBy::Month] {
        let costs = query_cost_timeseries(conn, &all, group_by).expect("cost rollup");
        assert_eq!(costs.path, "rollup");
        assert_bucket_eq(
            &costs.totals,
            &track_b_expected,
            &format!("track B cost totals {group_by:?}"),
        );
    }
    let cost_raw = query_cost_timeseries(conn, &day2_filter, GroupBy::Day).expect("cost raw");
    assert_eq!(cost_raw.path, "raw");
    let expected_day2_cost = sum_facts(
        fixture
            .facts
            .iter()
            .filter(|fact| (day2_since..=day2_until).contains(&fact.timestamp_ms)),
        true,
    );
    assert_bucket_eq(&cost_raw.totals, &expected_day2_cost, "track B raw day2");
    let hourly = query_cost_timeseries(conn, &all, GroupBy::Hour).expect("cost hourly");
    assert_eq!(hourly.path, "raw");
    assert_bucket_eq(&hourly.totals, &track_b_expected, "track B hourly totals");

    // Unknown-cost truth: only priced rows contribute USD; every unpriced
    // token is still accounted for by the unpriced-models report.
    let expected_unpriced: i64 = fixture
        .facts
        .iter()
        .filter(|fact| fact.cost.is_none() && fact.total > 0)
        .map(Fact::total)
        .sum();
    let unpriced = query_unpriced_models(conn, 10).expect("unpriced");
    assert_eq!(unpriced.total_unpriced_tokens, expected_unpriced);
    let mystery = unpriced
        .models
        .iter()
        .find(|model| model.model_name == "mystery-model-x")
        .expect("unpriced mystery model is reported");
    assert_eq!(mystery.total_tokens, 15_000);
    assert_eq!(mystery.row_count, 1);
    let expected_priced: i64 = fixture
        .facts
        .iter()
        .filter(|fact| fact.cost.is_some())
        .map(Fact::total)
        .sum();
    assert_eq!(unpriced.total_priced_tokens, expected_priced);

    // Tool report from the rollup, including the limit's effect on rows only.
    let tools = query_tools(conn, &all, GroupBy::Day, 1).expect("tools");
    assert_eq!(tools.rows.len(), 1);
    assert_eq!(tools.total_tool_calls, 2);
    assert_eq!(tools.rows[0].key, "claude_code");
    assert_eq!(tools.rows[0].tool_call_count, 2);

    // Scatter uses max(conversation rollup, detailed ledger).
    let scatter = query_session_scatter(conn, &all, 10).expect("scatter");
    assert_eq!(scatter.len(), 4);
    let expected_scatter: BTreeMap<String, (i64, i64)> = fixture
        .grand_totals
        .iter()
        .map(|(conversation_id, grand_total)| {
            let conv_facts: Vec<&Fact> = fixture
                .facts
                .iter()
                .filter(|fact| fact.conversation_id == *conversation_id)
                .collect();
            // Scatter uses API fields for API rows and the estimated total for
            // rows the provider parser only estimated.
            let detailed: i64 = conv_facts
                .iter()
                .map(|fact| {
                    if fact.api {
                        fact.api_total()
                    } else {
                        fact.total
                    }
                })
                .sum();
            let source_path = conv_facts[0].source_path.clone();
            let tokens = (*grand_total).max(detailed);
            (source_path, (conv_facts.len() as i64, tokens))
        })
        .collect();
    for point in &scatter {
        let (messages, tokens) = expected_scatter[&point.source_path];
        assert_eq!(point.message_count, messages, "{}", point.source_path);
        assert_eq!(point.api_tokens_total, tokens, "{}", point.source_path);
    }
    let remote_filter = AnalyticsFilter {
        source: SourceFilter::Remote,
        ..AnalyticsFilter::default()
    };
    let remote = query_session_scatter(conn, &remote_filter, 10).expect("remote scatter");
    assert_eq!(remote.len(), 1);
    assert_eq!(remote[0].source_path, stored_path("conv-codex-api"));

    // Status coverage must be derived from the same ledger.
    let status = query_status(conn, &all).expect("status");
    assert_eq!(
        status.coverage.total_messages as usize,
        fixture.message_count
    );
    assert_eq!(
        status.coverage.message_metrics_coverage_pct.as_value(),
        Some(100.0)
    );
    let api_rows = fixture.facts.iter().filter(|fact| fact.api).count();
    assert_f64_close(
        status
            .coverage
            .api_token_coverage_pct
            .as_value()
            .expect("coverage value"),
        100.0 * api_rows as f64 / fixture.message_count as f64,
        "api coverage",
    );
    let named_rows = fixture
        .facts
        .iter()
        .filter(|fact| {
            fact.model_name
                .as_deref()
                .is_some_and(|name| !name.is_empty())
        })
        .count();
    assert_f64_close(
        status
            .coverage
            .model_name_coverage_pct
            .as_value()
            .expect("model coverage"),
        100.0 * named_rows as f64 / fixture.message_count as f64,
        "model coverage",
    );
    let estimated_rows = fixture.facts.iter().filter(|fact| !fact.api).count();
    assert_f64_close(
        status
            .coverage
            .estimate_only_pct
            .as_value()
            .expect("estimate pct"),
        100.0 * estimated_rows as f64 / fixture.message_count as f64,
        "estimate pct",
    );
    assert_eq!(
        status.recommended_action, "none",
        "fresh rebuild should need no repair"
    );
    for table in &status.tables {
        assert!(table.exists, "{} exists", table.table);
    }
}

#[test]
fn duplicate_and_updated_events_do_not_double_count() {
    let tmp = TempDir::new().expect("temp dir");
    let db_path = tmp.path().join("agent_search.db");
    let storage = FrankenStorage::open(&db_path).expect("open storage");
    let agent_id = storage
        .ensure_agent(&Agent {
            id: None,
            slug: "claude_code".to_string(),
            name: "Claude Code".to_string(),
            version: None,
            kind: AgentKind::Cli,
        })
        .expect("agent");
    let workspace = Path::new("/srv/duplicate-events");
    let workspace_id = storage
        .ensure_workspace(workspace, Some("duplicate-events"))
        .expect("workspace");
    let started_at = utc_ms(2026, 3, 1, 12, 0, 0, 0);

    let message = |idx: i64, role: MessageRole, content: &str, extra: Value| Message {
        id: None,
        idx,
        role,
        author: None,
        created_at: Some(started_at + idx * 1_000),
        content: pad_to_quad(content),
        extra_json: extra,
        snippets: Vec::new(),
    };
    let conversation = |messages: Vec<Message>| Conversation {
        id: None,
        agent_slug: "claude_code".to_string(),
        workspace: Some(workspace.to_path_buf()),
        external_id: Some("conv-duplicate-events".to_string()),
        title: Some("duplicate events".to_string()),
        source_path: PathBuf::from("/fixtures/conv-duplicate-events.jsonl"),
        started_at: Some(started_at),
        ended_at: Some(started_at + 86_400_000),
        approx_tokens: None,
        metadata_json: json!({}),
        messages,
        source_id: "local".to_string(),
        origin_host: None,
    };

    let first = conversation(vec![message(
        0,
        MessageRole::User,
        "Count this message exactly once.",
        json!({}),
    )]);
    storage
        .insert_conversation_tree(agent_id, Some(workspace_id), &first)
        .expect("first insert");
    // A provider re-scan can hand the archive the same conversation again.
    storage
        .insert_conversation_tree(agent_id, Some(workspace_id), &first)
        .expect("duplicate insert");
    storage.rebuild_analytics().expect("rebuild track A");
    storage
        .rebuild_token_daily_stats()
        .expect("rebuild track B");

    let initial = query_tokens_timeseries(storage.raw(), &AnalyticsFilter::default(), GroupBy::Day)
        .expect("initial tokens");
    assert_eq!(initial.totals.message_count, 1, "duplicate insert once");
    assert_eq!(initial.totals.user_message_count, 1);
    assert_eq!(initial.totals.assistant_message_count, 0);

    // A later update appends one new assistant turn; the existing user turn
    // is not duplicated or re-counted.
    let updated = conversation(vec![
        message(
            0,
            MessageRole::User,
            "Count this message exactly once.",
            json!({}),
        ),
        message(
            1,
            MessageRole::Agent,
            "Appended after the archive was revisited.",
            claude_usage("claude-sonnet-4-20250514", 100, 50, 0, 0, 0),
        ),
    ]);
    let outcome = storage
        .insert_conversation_tree(agent_id, Some(workspace_id), &updated)
        .expect("updated insert");
    assert_eq!(outcome.inserted_indices, vec![1]);
    let conversation_id = outcome.conversation_id;
    let stored = storage
        .fetch_messages(conversation_id)
        .expect("stored messages");
    assert_eq!(stored.len(), 2, "no duplicate append");
    let assistant = &stored[1];
    let assistant_id = assistant.id.expect("stored assistant id");
    let assistant_at = assistant.created_at.expect("assistant timestamp");
    let assistant_day = FrankenStorage::day_id_from_millis(assistant_at);
    let pricing = PricingTable::franken_load(storage.raw()).expect("pricing");
    let cost = pricing
        .compute_cost(
            Some("claude-sonnet-4-20250514"),
            assistant_day,
            Some(100),
            Some(50),
            Some(0),
            Some(0),
        )
        .expect("priced assistant");
    // The indexer materializes the appended turn's Track B ledger row.
    storage
        .raw()
        .execute_batch(&format!(
            "INSERT INTO token_usage (
                message_id, conversation_id, agent_id, workspace_id, source_id,
                timestamp_ms, day_id, model_name, model_family, model_tier,
                service_tier, provider, input_tokens, output_tokens,
                cache_read_tokens, cache_creation_tokens, thinking_tokens,
                total_tokens, estimated_cost_usd, role, content_chars,
                has_tool_calls, tool_call_count, data_source
             ) VALUES (
                {assistant_id}, {conversation_id}, {agent_id}, {workspace_id},
                'local', {assistant_at}, {assistant_day},
                'claude-sonnet-4-20250514', 'claude', 'sonnet', NULL,
                'anthropic', 100, 50, 0, 0, 0, 150, {cost:.17},
                'agent', {}, 0, 0, 'api'
             );",
            assistant.content.len()
        ))
        .expect("append token ledger");
    storage.rebuild_analytics().expect("rebuild track A");
    storage
        .rebuild_token_daily_stats()
        .expect("rebuild track B");

    let tokens = query_tokens_timeseries(storage.raw(), &AnalyticsFilter::default(), GroupBy::Day)
        .expect("updated tokens");
    assert_eq!(tokens.totals.message_count, 2);
    assert_eq!(tokens.totals.user_message_count, 1);
    assert_eq!(tokens.totals.assistant_message_count, 1);
    assert_eq!(tokens.totals.api_tokens_total, 150);

    let costs = query_cost_timeseries(storage.raw(), &AnalyticsFilter::default(), GroupBy::Day)
        .expect("updated costs");
    assert_f64_close(
        costs.totals.estimated_cost_usd,
        0.001_05,
        "updated cost total",
    );

    let status = query_status(storage.raw(), &AnalyticsFilter::default()).expect("status");
    assert_eq!(status.coverage.total_messages, 2);

    let agents = query_breakdown(
        storage.raw(),
        &AnalyticsFilter::default(),
        Dim::Agent,
        Metric::ApiTotal,
        10,
    )
    .expect("agent breakdown");
    let claude = agents
        .rows
        .iter()
        .find(|row| row.key == "claude_code")
        .expect("claude row");
    assert_eq!(claude.value, 150);
    assert_eq!(claude.message_count, 2);

    let scatter =
        query_session_scatter(storage.raw(), &AnalyticsFilter::default(), 10).expect("scatter");
    assert_eq!(scatter.len(), 1);
    assert_eq!(scatter[0].message_count, 2);
    assert!(scatter[0].api_tokens_total >= 150);
}

#[test]
fn timestamp_boundaries_and_timezone_edges_are_exact() {
    let tmp = TempDir::new().expect("temp dir");
    let db_path = tmp.path().join("agent_search.db");
    let storage = FrankenStorage::open(&db_path).expect("open storage");
    let agent_id = storage
        .ensure_agent(&Agent {
            id: None,
            slug: "claude_code".to_string(),
            name: "Claude Code".to_string(),
            version: None,
            kind: AgentKind::Cli,
        })
        .expect("agent");
    let day_start = utc_ms(2026, 2, 1, 0, 0, 0, 0); // Sunday, ISO 2026-W05
    let day_end = utc_ms(2026, 2, 1, 23, 59, 59, 999);
    let next_start = utc_ms(2026, 2, 2, 0, 0, 0, 0); // Monday, ISO 2026-W06
    let conv_started = utc_ms(2026, 2, 2, 12, 0, 0, 0);
    let huge = 253_402_300_799_000i64; // 9999-12-31T23:59:59Z

    let messages = vec![
        Message {
            id: None,
            idx: 0,
            role: MessageRole::Agent,
            author: None,
            created_at: Some(day_start),
            content: pad_to_quad("First instant of the UTC day."),
            extra_json: json!({}),
            snippets: Vec::new(),
        },
        Message {
            id: None,
            idx: 1,
            role: MessageRole::Agent,
            author: None,
            created_at: Some(day_end),
            content: pad_to_quad("Last millisecond of the UTC day."),
            extra_json: json!({}),
            snippets: Vec::new(),
        },
        Message {
            id: None,
            idx: 2,
            role: MessageRole::Agent,
            author: None,
            created_at: Some(next_start),
            content: pad_to_quad("First instant of the next UTC day."),
            extra_json: json!({}),
            snippets: Vec::new(),
        },
        Message {
            id: None,
            idx: 3,
            role: MessageRole::Agent,
            author: None,
            created_at: None,
            content: pad_to_quad("Missing timestamp falls back to the conversation."),
            extra_json: json!({}),
            snippets: Vec::new(),
        },
        Message {
            id: None,
            idx: 4,
            role: MessageRole::Agent,
            author: None,
            // Negative timestamps are preserved by the canonicalizer but the
            // rollup day falls back to the conversation's started_at.
            created_at: Some(-1_000),
            content: pad_to_quad("Pre-epoch timestamp stays canonical."),
            extra_json: json!({}),
            snippets: Vec::new(),
        },
        Message {
            id: None,
            idx: 5,
            role: MessageRole::Agent,
            author: None,
            created_at: Some(huge),
            content: pad_to_quad("Year 9999 timestamp remains exact."),
            extra_json: json!({}),
            snippets: Vec::new(),
        },
    ];
    let conv = Conversation {
        id: None,
        agent_slug: "claude_code".to_string(),
        workspace: None,
        external_id: Some("boundary".to_string()),
        title: Some("boundary".to_string()),
        source_path: PathBuf::from("/fixtures/boundary.jsonl"),
        started_at: Some(conv_started),
        ended_at: Some(huge),
        approx_tokens: None,
        metadata_json: json!({}),
        messages,
        source_id: "local".to_string(),
        origin_host: None,
    };
    storage
        .insert_conversation_tree(agent_id, None, &conv)
        .expect("insert");
    storage.rebuild_analytics().expect("rebuild");

    let db = FrankenStorage::open_readonly(&db_path).expect("readonly");
    let conn = db.raw();

    let first_day = filter(Some(day_start), Some(day_end));
    let result = query_tokens_timeseries(conn, &first_day, GroupBy::Day).expect("day range");
    assert_eq!(result.path, "raw");
    assert_eq!(
        result.totals.message_count, 2,
        "00:00:00.000 and 23:59:59.999 belong to the day"
    );
    assert_eq!(result.buckets.len(), 1);
    assert_eq!(result.buckets[0].0, "2026-02-01");

    let next_day = filter(Some(next_start), Some(next_start + 86_400_000 - 1));
    let result = query_tokens_timeseries(conn, &next_day, GroupBy::Day).expect("next day");
    assert_eq!(
        result.totals.message_count, 2,
        "explicit next-day message + null fallback"
    );
    assert_eq!(result.buckets[0].0, "2026-02-02");

    let empty_since = utc_ms(2026, 2, 3, 0, 0, 0, 0);
    let empty_until = utc_ms(2026, 2, 4, 0, 0, 0, 0);
    let empty = query_tokens_timeseries(
        conn,
        &filter(Some(empty_since), Some(empty_until)),
        GroupBy::Day,
    )
    .expect("empty range");
    assert_eq!(empty.buckets.len(), 0);
    assert_eq!(empty.totals.message_count, 0);
    assert!(
        !empty.metric_outcome.is_numeric() || empty.metric_outcome.as_value() == Some(0.0),
        "empty range must not be reported as a positive value"
    );

    // Pre-epoch timestamps stay out of exact positive ranges and form their
    // own negative day bucket in the rollup.
    let unfiltered =
        query_tokens_timeseries(conn, &AnalyticsFilter::default(), GroupBy::Day).expect("all");
    assert_eq!(
        unfiltered.buckets.len(),
        4,
        "1969-12-31, 2026-02-01, 2026-02-02, and 9999-12-31"
    );
    assert!(
        unfiltered
            .buckets
            .iter()
            .any(|(label, _)| label == "1969-12-31"),
        "pre-epoch bucket is preserved"
    );
    let all_messages: i64 = unfiltered
        .buckets
        .iter()
        .map(|(_, b)| b.message_count)
        .sum();
    assert_eq!(all_messages, 6);

    // Week and month labels are UTC / ISO (Monday-start) and cover the whole
    // ledger, not a local-time shifted window.
    let weeks =
        query_tokens_timeseries(conn, &AnalyticsFilter::default(), GroupBy::Week).expect("weeks");
    let week_labels: Vec<&str> = weeks
        .buckets
        .iter()
        .map(|(label, _)| label.as_str())
        .collect();
    assert!(week_labels.contains(&"2026-W05"), "labels: {week_labels:?}");
    assert!(week_labels.contains(&"2026-W06"), "labels: {week_labels:?}");
    let months =
        query_tokens_timeseries(conn, &AnalyticsFilter::default(), GroupBy::Month).expect("months");
    assert!(months.buckets.iter().any(|(label, _)| label == "2026-02"));
    assert!(months.buckets.iter().any(|(label, _)| label == "9999-12"));

    // The 9999 day is addressable with an exact raw filter.
    let far_future = filter(Some(huge - 1_000), Some(huge));
    let result = query_tokens_timeseries(conn, &far_future, GroupBy::Day).expect("far future");
    assert_eq!(result.totals.message_count, 1);
    assert_eq!(result.buckets[0].0, "9999-12-31");

    // Hour bucketing uses the same UTC boundary discipline.
    let first_hour = filter(Some(day_start), Some(day_start + 3_599_999));
    let hourly = query_tokens_timeseries(conn, &first_hour, GroupBy::Hour).expect("hour");
    assert_eq!(hourly.totals.message_count, 1);
    assert_eq!(hourly.buckets[0].0, "2026-02-01T00:00Z");
}

#[test]
fn missing_rollups_and_ledgers_are_truthful_and_read_only() {
    let tmp = TempDir::new().expect("temp dir");
    let db_path = tmp.path().join("agent_search.db");
    let storage = FrankenStorage::open(&db_path).expect("open storage");
    let agent_id = storage
        .ensure_agent(&Agent {
            id: None,
            slug: "codex".to_string(),
            name: "Codex".to_string(),
            version: None,
            kind: AgentKind::Cli,
        })
        .expect("agent");
    // More than the no-analytics drift threshold.
    let messages: Vec<Message> = (0..101)
        .map(|idx| Message {
            id: None,
            idx,
            role: MessageRole::Agent,
            author: None,
            created_at: Some(utc_ms(2026, 3, 1, 12, 0, 0, 0) + idx * 1_000),
            content: pad_to_quad("Canonical message without any analytics ledger."),
            extra_json: json!({}),
            snippets: Vec::new(),
        })
        .collect();
    let conv = Conversation {
        id: None,
        agent_slug: "codex".to_string(),
        workspace: None,
        external_id: Some("missing-ledger".to_string()),
        title: Some("missing-ledger".to_string()),
        source_path: PathBuf::from("/fixtures/missing.jsonl"),
        started_at: Some(utc_ms(2026, 3, 1, 12, 0, 0, 0)),
        ended_at: None,
        approx_tokens: None,
        metadata_json: json!({}),
        messages,
        source_id: "local".to_string(),
        origin_host: None,
    };
    storage
        .insert_conversation_tree(agent_id, None, &conv)
        .expect("insert");

    // Simulate pre-analytics archives / interrupted rebuilds.
    storage
        .raw()
        .execute_batch(
            "DROP TABLE usage_daily;
             DROP TABLE usage_hourly;
             DROP TABLE usage_models_daily;
             DROP TABLE message_metrics;
             DROP TABLE token_daily_stats;
             DROP TABLE token_usage;",
        )
        .expect("drop analytics tables");

    let before: Vec<i64> = storage
        .raw()
        .query_map_collect(
            "SELECT (SELECT COUNT(*) FROM messages), (SELECT COUNT(*) FROM conversations)",
            &[],
            |row| row.get_typed::<i64>(0),
        )
        .expect("counts");
    let db = FrankenStorage::open_readonly(&db_path).expect("readonly");
    let conn = db.raw();

    let status = query_status(conn, &AnalyticsFilter::default()).expect("status");
    assert_eq!(status.coverage.total_messages, 101);
    assert_eq!(
        status.coverage.message_metrics_coverage_pct.kind_str(),
        "rebuild-required"
    );
    assert_eq!(
        status.recommended_action, "rebuild_all",
        "missing analytics with a populated archive must ask for a rebuild"
    );
    assert!(
        status
            .tables
            .iter()
            .any(|table| table.table == "usage_daily" && !table.exists),
        "usage_daily is truthfully absent"
    );

    let tokens =
        query_tokens_timeseries(conn, &AnalyticsFilter::default(), GroupBy::Day).expect("tokens");
    assert_eq!(tokens.metric_outcome.kind_str(), "rebuild-required");
    assert_eq!(tokens.path, "none");

    let breakdown = query_breakdown(
        conn,
        &AnalyticsFilter::default(),
        Dim::Agent,
        Metric::ApiTotal,
        10,
    )
    .expect("breakdown");
    assert_eq!(breakdown.metric_outcome.kind_str(), "rebuild-required");

    let tools = query_tools(conn, &AnalyticsFilter::default(), GroupBy::Day, 10).expect("tools");
    assert_eq!(tools.metric_outcome.kind_str(), "rebuild-required");

    // Scatter is canonical-ledger only and remains truthful with zero tokens.
    let scatter = query_session_scatter(conn, &AnalyticsFilter::default(), 10).expect("scatter");
    assert_eq!(scatter.len(), 1);
    assert_eq!(scatter[0].message_count, 101);
    assert_eq!(scatter[0].api_tokens_total, 0);

    // Missing Track B tables must not be reported as a true zero.
    let costs =
        query_cost_timeseries(conn, &AnalyticsFilter::default(), GroupBy::Day).expect("costs");
    assert!(
        costs.metric_outcome.kind_str() != "true_zero",
        "missing ledger must not be a true zero: {:?}",
        costs.metric_outcome
    );
    let unpriced = query_unpriced_models(conn, 10).expect("unpriced");
    assert!(unpriced.models.is_empty());
    assert_eq!(unpriced.total_unpriced_tokens, 0);

    // Read-only status must not mutate or recreate anything.
    let after: Vec<i64> = db
        .raw()
        .query_map_collect(
            "SELECT (SELECT COUNT(*) FROM messages), (SELECT COUNT(*) FROM conversations)",
            &[],
            |row| row.get_typed::<i64>(0),
        )
        .expect("counts");
    assert_eq!(before, after);
    let exists: i64 = db
        .raw()
        .query_map_collect(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='usage_daily'",
            &[],
            |row| row.get_typed::<i64>(0),
        )
        .expect("sqlite_master")
        .into_iter()
        .next()
        .expect("one row");
    assert_eq!(exists, 0, "status inspection must not create rollups");
}

fn run_cli(args: &[&str], data_dir: &Path, home: &Path) -> Value {
    let mut cmd = cargo_bin_cmd!("cass");
    cmd.args(args)
        .arg("--data-dir")
        .arg(data_dir)
        .env("HOME", home)
        .env("CASS_DATA_DIR", data_dir)
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    serde_json::from_str(stdout.trim()).expect("valid JSON stdout")
}

fn run_cli_toon(args: &[&str], data_dir: &Path, home: &Path) -> Value {
    let mut cmd = cargo_bin_cmd!("cass");
    cmd.args(args)
        .arg("--data-dir")
        .arg(data_dir)
        .env("HOME", home)
        .env("CASS_DATA_DIR", data_dir)
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let json_text = toon::toon_to_json(stdout.trim()).expect("valid TOON stdout");
    serde_json::from_str(&json_text).expect("TOON decodes to JSON")
}

fn canonical_numbers(value: &Value) -> Value {
    match value {
        Value::Number(number) => {
            if let Some(float) = number.as_f64() {
                json!(float)
            } else {
                value.clone()
            }
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_numbers).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), canonical_numbers(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[test]
fn cli_json_toon_and_tui_projections_agree_with_the_ledger() {
    let fixture = LedgerFixture::build();
    let db = fixture.read();
    let conn = db.raw();
    let all = AnalyticsFilter::default();
    let home = fixture._tmp.path();

    let status_json = run_cli(
        &["analytics", "status", "--json"],
        fixture._tmp.path(),
        home,
    );
    let status_toon = run_cli_toon(
        &["analytics", "status", "--robot-format", "toon"],
        fixture._tmp.path(),
        home,
    );
    let status = query_status(conn, &all).expect("status");
    assert_eq!(status_json["command"], "analytics/status");
    assert_eq!(
        status_json["data"]["coverage"]["total_messages"].as_i64(),
        Some(status.coverage.total_messages)
    );
    assert_eq!(
        canonical_numbers(&status_json["data"]["coverage"]),
        canonical_numbers(&status_toon["data"]["coverage"]),
        "status JSON and TOON must project the same coverage"
    );
    assert_eq!(
        canonical_numbers(&status_json["data"]["drift"]),
        canonical_numbers(&status_toon["data"]["drift"])
    );

    let tokens_json = run_cli(
        &["analytics", "tokens", "--json", "--group-by", "day"],
        fixture._tmp.path(),
        home,
    );
    let tokens_toon = run_cli_toon(
        &[
            "analytics",
            "tokens",
            "--robot-format",
            "toon",
            "--group-by",
            "day",
        ],
        fixture._tmp.path(),
        home,
    );
    let tokens = query_tokens_timeseries(conn, &all, GroupBy::Day).expect("tokens");
    assert_eq!(
        tokens_json["data"]["totals"]["counts"]["message_count"].as_i64(),
        Some(tokens.totals.message_count)
    );
    assert_eq!(
        tokens_json["data"]["totals"]["api_tokens"]["total"].as_i64(),
        Some(tokens.totals.api_tokens_total)
    );
    assert_eq!(
        tokens_json["data"]["bucket_count"].as_u64(),
        Some(tokens.buckets.len() as u64)
    );
    assert_eq!(
        canonical_numbers(&tokens_json["data"]["totals"]),
        canonical_numbers(&tokens_toon["data"]["totals"])
    );

    let tools_json = run_cli(
        &["analytics", "tools", "--json", "--limit", "10"],
        fixture._tmp.path(),
        home,
    );
    let tools_toon = run_cli_toon(
        &[
            "analytics",
            "tools",
            "--robot-format",
            "toon",
            "--limit",
            "10",
        ],
        fixture._tmp.path(),
        home,
    );
    let tools = query_tools(conn, &all, GroupBy::Day, 10).expect("tools");
    assert_eq!(
        tools_json["data"]["totals"]["tool_call_count"].as_i64(),
        Some(tools.total_tool_calls)
    );
    assert_eq!(
        canonical_numbers(&tools_json["data"]["totals"]),
        canonical_numbers(&tools_toon["data"]["totals"])
    );

    let models_json = run_cli(
        &["analytics", "models", "--json"],
        fixture._tmp.path(),
        home,
    );
    let models_toon = run_cli_toon(
        &["analytics", "models", "--robot-format", "toon"],
        fixture._tmp.path(),
        home,
    );
    let models = query_breakdown(conn, &all, Dim::Model, Metric::ApiTotal, 10).expect("models");
    assert_eq!(
        models_json["data"]["by_api_tokens"]["row_count"].as_u64(),
        Some(models.rows.len() as u64)
    );
    assert_eq!(
        canonical_numbers(&models_json["data"]["by_api_tokens"]["rows"]),
        canonical_numbers(&models_toon["data"]["by_api_tokens"]["rows"])
    );

    // TUI projection agrees with the same library queries and CLI totals.
    let state = AnalyticsFilterState::default();
    let chart = load_chart_data(&db, &state, GroupBy::Day);
    assert_eq!(chart.total_messages, tokens.totals.message_count);
    assert_eq!(chart.total_api_tokens, tokens.totals.api_tokens_total);
    assert_eq!(chart.total_tool_calls, tools.total_tool_calls);
    let agent_breakdown =
        query_breakdown(conn, &all, Dim::Agent, Metric::ApiTotal, 20).expect("agents");
    let chart_value = |rows: &[(String, f64)], key: &str| {
        rows.iter()
            .find(|(row_key, _)| row_key == key)
            .map(|(_, value)| *value)
            .unwrap_or_default()
    };
    for row in &agent_breakdown.rows {
        assert_eq!(
            chart_value(&chart.agent_tokens, &row.key),
            row.value as f64,
            "TUI agent tokens for {}",
            row.key
        );
        assert_eq!(
            chart_value(&chart.agent_messages, &row.key),
            row.message_count as f64
        );
    }
    for row in &models.rows {
        assert_eq!(
            chart_value(&chart.model_tokens, &row.key),
            row.value as f64,
            "TUI model tokens for {}",
            row.key
        );
    }
    let sources = query_breakdown(conn, &all, Dim::Source, Metric::ApiTotal, 20).expect("sources");
    for row in &sources.rows {
        assert_eq!(
            chart_value(&chart.source_tokens, &row.key),
            row.value as f64
        );
    }
    let scatter = query_session_scatter(conn, &all, 20).expect("scatter");
    assert_eq!(chart.session_scatter.len(), scatter.len());
    for (chart_point, query_point) in chart.session_scatter.iter().zip(scatter.iter()) {
        assert_eq!(chart_point.message_count, query_point.message_count);
        assert_eq!(chart_point.api_tokens_total, query_point.api_tokens_total);
    }
    let expected_daily: BTreeMap<String, i64> =
        bucket_map(fixture.facts.iter(), false, |fact| day_label(fact.day_id))
            .into_iter()
            .map(|(label, bucket)| (label, bucket.api_tokens_total))
            .collect();
    assert_eq!(chart.daily_tokens.len(), expected_daily.len());
    for (label, value) in &chart.daily_tokens {
        assert_eq!(
            *value,
            expected_daily.get(label).copied().unwrap_or_default() as f64,
            "TUI daily tokens at {label}"
        );
    }
}

#[test]
fn scaled_skewed_ledger_queries_are_bounded_and_correct() {
    const DAYS: i64 = 60;
    const PER_DAY: i64 = 500;
    const MESSAGES: i64 = DAYS * PER_DAY;
    let tmp = TempDir::new().expect("temp dir");
    let db_path = tmp.path().join("agent_search.db");
    let storage = FrankenStorage::open(&db_path).expect("open storage");
    let start = utc_ms(2026, 1, 1, 0, 0, 0, 0);
    let agents = ["claude_code", "codex", "gemini_cli"];
    let mut agent_ids = Vec::new();
    for (index, slug) in agents.iter().enumerate() {
        agent_ids.push(
            storage
                .ensure_agent(&Agent {
                    id: None,
                    slug: (*slug).to_string(),
                    name: (*slug).to_string(),
                    version: None,
                    kind: AgentKind::Cli,
                })
                .expect("agent"),
        );
        let _ = index;
    }
    let mut workspace_ids = Vec::new();
    for index in 0..3 {
        let path = format!("/scaled/workspace-{index}");
        let name = format!("scaled-{index}");
        workspace_ids.push(
            storage
                .ensure_workspace(Path::new(&path), Some(&name))
                .expect("workspace"),
        );
    }

    // 200 conversations (one per 100 messages), with a deliberate skew:
    // 45% of all messages land on three days, 80% come from one agent.
    let per_conv = 100i64;
    let convs = MESSAGES / per_conv;
    let mut conv_sql = String::new();
    let mut msg_sql = String::new();
    let mut mm_sql = String::new();
    let mut tu_sql = String::new();
    // Expected rollups accumulated directly from the generated facts.
    let mut track_a_daily: BTreeMap<(i64, String, String), UsageBucket> = BTreeMap::new();
    let mut track_b_daily: BTreeMap<(i64, String, String, String), UsageBucket> = BTreeMap::new();
    let mut conv_day: BTreeMap<(i64, i64), (i64, i64)> = BTreeMap::new();
    let mut message_id = 1i64;
    for conv in 0..convs {
        let conv_id = conv + 1;
        let agent_index = if conv % 10 < 8 {
            0
        } else if conv % 10 < 9 {
            1
        } else {
            2
        };
        let source = if conv % 20 == 0 {
            "work-laptop"
        } else {
            "local"
        };
        let day = if conv % 10 < 8 {
            conv % 3
        } else {
            (conv * 7) % DAYS
        };
        let started = start + day * 86_400_000 + conv * 1_000;
        conv_sql.push_str(&format!(
            "({}, {}, {}, '{}', 'conv-{conv}', '/scaled/{conv}.jsonl', {started}, {}, 0),",
            conv_id,
            agent_ids[agent_index as usize],
            workspace_ids[agent_index as usize],
            source,
            started + 1_000
        ));
        let conversation_agent = agents[agent_index as usize];
        for idx in 0..per_conv {
            let agent_index = (conv + idx) % 10;
            let agent_index = if agent_index < 8 {
                0
            } else if agent_index < 9 {
                1
            } else {
                2
            };
            let message_source = if (conv + idx) % 20 == 0 {
                "work-laptop"
            } else {
                "local"
            };
            let day_offset = if (conv + idx) % 10 < 8 {
                (conv + idx) % 3
            } else {
                (conv * 7 + idx) % DAYS
            };
            let ts = start + day_offset * 86_400_000 + idx * 60_000 + 1_000;
            let role = if idx % 5 == 0 { "user" } else { "agent" };
            let model = if agent_index == 0 {
                "claude-sonnet-4-20250514"
            } else {
                "gpt-4o"
            };
            let family = if agent_index == 0 { "claude" } else { "gpt" };
            let input = 100 + idx;
            let output = 50 + idx;
            let total = input + output;
            let tool_calls = i64::from(idx % 3 == 0 && role == "agent");
            let content_chars = 40 + (idx % 4) * 4;
            let day_id = FrankenStorage::day_id_from_millis(ts);
            let hour_id = FrankenStorage::hour_id_from_millis(ts);
            let cost = oracle_cost(Some(model), input, output, 0, 0).expect("priced");
            msg_sql.push_str(&format!(
                "({message_id}, {conv_id}, {idx}, '{}', NULL, {ts}, '{}', NULL),",
                role,
                "x".repeat(content_chars as usize)
            ));
            mm_sql.push_str(&format!(
                "({message_id}, {ts}, {hour_id}, {day_id}, '{}', {}, '{}', '{}', {content_chars}, {}, {input}, {output}, 0, 0, 0, 0, 'api', {tool_calls}, {}, 0, '{model}', '{family}', 'unknown', 'unknown'),",
                agents[agent_index as usize],
                workspace_ids[agent_index as usize],
                message_source,
                role,
                content_chars / 4,
                i64::from(tool_calls > 0),
            ));
            tu_sql.push_str(&format!(
                "({message_id}, {conv_id}, {}, {}, '{}', {ts}, {day_id}, '{model}', '{family}', 'unknown', 'unknown', 'unknown', {input}, {output}, 0, 0, 0, {total}, {cost:.17}, '{}', {content_chars}, {}, {tool_calls}, 'api'),",
                agent_ids[agent_index as usize],
                workspace_ids[agent_index as usize],
                message_source,
                role,
                i64::from(tool_calls > 0),
            ));
            let content_est = content_chars / 4;
            let is_user = role == "user";
            let bucket = UsageBucket {
                message_count: 1,
                user_message_count: i64::from(is_user),
                assistant_message_count: i64::from(!is_user),
                tool_call_count: tool_calls,
                api_coverage_message_count: 1,
                content_tokens_est_total: content_est,
                content_tokens_est_user: if is_user { content_est } else { 0 },
                content_tokens_est_assistant: if is_user { 0 } else { content_est },
                api_tokens_total: total,
                api_input_tokens_total: input,
                api_output_tokens_total: output,
                estimated_cost_usd: 0.0,
                ..Default::default()
            };
            track_a_daily
                .entry((day_id, conversation_agent.to_string(), source.to_string()))
                .or_default()
                .merge(&bucket);
            let mut cost_bucket = bucket;
            // The token ledger stores one content-character count per row;
            // Track B does not attribute it to user vs assistant.
            cost_bucket.content_tokens_est_user = 0;
            cost_bucket.content_tokens_est_assistant = 0;
            cost_bucket.estimated_cost_usd = cost;
            track_b_daily
                .entry((
                    day_id,
                    agents[agent_index as usize].to_string(),
                    message_source.to_string(),
                    family.to_string(),
                ))
                .or_default()
                .merge(&cost_bucket);
            let conv_entry = conv_day.entry((conv_id, day_id)).or_default();
            conv_entry.0 += 1;
            conv_entry.1 += total;
            message_id += 1;
        }
    }
    conv_sql.pop();
    msg_sql.pop();
    mm_sql.pop();
    tu_sql.pop();

    let insert_start = Instant::now();
    storage
        .raw()
        .execute_batch(
            "INSERT OR IGNORE INTO sources (id, kind, host_label, created_at, updated_at)
             VALUES ('work-laptop', 'remote', 'work-laptop', 0, 0);",
        )
        .expect("remote source");
    storage
        .raw()
        .execute_batch(&format!(
            "INSERT INTO conversations (id, agent_id, workspace_id, source_id, external_id, source_path, started_at, ended_at, grand_total_tokens)
             VALUES {conv_sql};"
        ))
        .expect("insert conversations");
    storage.raw().execute_batch(&format!("INSERT INTO messages (id, conversation_id, idx, role, author, created_at, content, extra_json) VALUES {msg_sql};")).expect("messages");
    storage.raw().execute_batch(&format!("INSERT INTO message_metrics (message_id, created_at_ms, hour_id, day_id, agent_slug, workspace_id, source_id, role, content_chars, content_tokens_est, api_input_tokens, api_output_tokens, api_cache_read_tokens, api_cache_creation_tokens, api_thinking_tokens, api_service_tier, api_data_source, tool_call_count, has_tool_calls, has_plan, model_name, model_family, model_tier, provider) VALUES {mm_sql};")).expect("message_metrics");
    storage.raw().execute_batch(&format!("INSERT INTO token_usage (message_id, conversation_id, agent_id, workspace_id, source_id, timestamp_ms, day_id, model_name, model_family, model_tier, service_tier, provider, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, thinking_tokens, total_tokens, estimated_cost_usd, role, content_chars, has_tool_calls, tool_call_count, data_source) VALUES {tu_sql};")).expect("token_usage");
    eprintln!(
        "scaled insert: {:?} for {MESSAGES} messages",
        insert_start.elapsed()
    );

    // Materialize the Track A daily rollup from the oracle buckets, and let the
    // production rebuild derive Track B from token_usage.
    let now_ms = Utc::now().timestamp_millis();
    let usage_rows: Vec<String> = track_a_daily
        .iter()
        .map(|((day_id, agent, source), bucket)| {
            format!(
                "({day_id}, {}, 0, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {now_ms})",
                quote(agent),
                quote(source),
                bucket.message_count,
                bucket.user_message_count,
                bucket.assistant_message_count,
                bucket.tool_call_count,
                bucket.plan_message_count,
                bucket.api_coverage_message_count,
                bucket.content_tokens_est_total,
                bucket.content_tokens_est_user,
                bucket.content_tokens_est_assistant,
                bucket.api_tokens_total,
                bucket.api_input_tokens_total,
                bucket.api_output_tokens_total,
                bucket.api_cache_read_tokens_total,
                bucket.api_cache_creation_tokens_total,
                bucket.api_thinking_tokens_total,
                bucket.plan_content_tokens_est_total,
                bucket.plan_api_tokens_total,
            )
        })
        .collect();
    storage
        .raw()
        .execute_batch(&format!(
            "INSERT INTO usage_daily (
                day_id, agent_slug, workspace_id, source_id, message_count,
                user_message_count, assistant_message_count, tool_call_count,
                plan_message_count, api_coverage_message_count,
                content_tokens_est_total, content_tokens_est_user,
                content_tokens_est_assistant, api_tokens_total,
                api_input_tokens_total, api_output_tokens_total,
                api_cache_read_tokens_total, api_cache_creation_tokens_total,
                api_thinking_tokens_total, plan_content_tokens_est_total,
                plan_api_tokens_total, last_updated
             ) VALUES {};",
            usage_rows.join(",")
        ))
        .expect("insert usage_daily");
    storage
        .rebuild_token_daily_stats()
        .expect("rebuild token daily stats");
    eprintln!("scaled rollup build: {:?}", insert_start.elapsed());

    let db = FrankenStorage::open_readonly(&db_path).expect("readonly");
    let conn = db.raw();
    let day10 = start + 10 * 86_400_000;
    let day21 = start + 21 * 86_400_000;
    let range = filter(Some(day10), Some(day21 - 1));
    let in_range =
        |day_id: i64| (10..21).contains(&(day_id - FrankenStorage::day_id_from_millis(start)));

    let mut expected_range = UsageBucket::default();
    let mut expected_range_agents: BTreeMap<String, UsageBucket> = BTreeMap::new();
    for ((day_id, agent, _source), bucket) in &track_a_daily {
        if in_range(*day_id) {
            expected_range.merge(bucket);
            expected_range_agents
                .entry(agent.clone())
                .or_default()
                .merge(bucket);
        }
    }
    let mut expected_range_cost = UsageBucket::default();
    for ((day_id, _agent, _source, _family), bucket) in &track_b_daily {
        if in_range(*day_id) {
            expected_range_cost.merge(bucket);
        }
    }

    // Unfiltered rollup: bounded and oracle-exact.
    let start_time = Instant::now();
    let rollup = query_tokens_timeseries(conn, &AnalyticsFilter::default(), GroupBy::Day)
        .expect("scaled rollup tokens");
    let rollup_elapsed = start_time.elapsed();
    assert_eq!(rollup.path, "rollup");
    let expected_all = sum_buckets(track_a_daily.values());
    assert_bucket_eq(&rollup.totals, &expected_all, "scaled track A total");
    eprintln!(
        "scaled rollup: {} messages, {} buckets in {:?}",
        rollup.totals.message_count,
        rollup.buckets.len(),
        rollup_elapsed
    );

    // Exact filtered fallback: bounded, index-driven where possible, exact.
    let start_time = Instant::now();
    let filtered =
        query_tokens_timeseries(conn, &range, GroupBy::Day).expect("scaled filtered tokens");
    let filtered_elapsed = start_time.elapsed();
    assert_eq!(filtered.path, "raw");
    assert_bucket_eq(&filtered.totals, &expected_range, "scaled track A filtered");
    assert_eq!(filtered.totals.message_count, expected_range.message_count);
    assert!(expected_range.message_count > 0);
    eprintln!(
        "scaled filtered tokens: {} messages in {:?}",
        filtered.totals.message_count, filtered_elapsed
    );

    let start_time = Instant::now();
    let agents_breakdown = query_breakdown(conn, &range, Dim::Agent, Metric::ApiTotal, 2)
        .expect("scaled agent breakdown");
    let breakdown_elapsed = start_time.elapsed();
    assert_eq!(agents_breakdown.source_table, "message_metrics");
    assert_eq!(agents_breakdown.rows.len(), 2);
    for row in &agents_breakdown.rows {
        assert_eq!(row.value, expected_range_agents[&row.key].api_tokens_total);
    }
    assert!(
        agents_breakdown.rows[0].value >= agents_breakdown.rows[1].value,
        "agent rows sorted desc"
    );
    eprintln!("scaled filtered agent breakdown: {:?}", breakdown_elapsed);

    let start_time = Instant::now();
    let sources_breakdown = query_breakdown(conn, &range, Dim::Source, Metric::ApiTotal, 10)
        .expect("scaled source breakdown");
    let source_elapsed = start_time.elapsed();
    let source_sum: i64 = sources_breakdown.rows.iter().map(|row| row.value).sum();
    assert_eq!(source_sum, expected_range.api_tokens_total);
    assert_eq!(sources_breakdown.rows.len(), 2);
    eprintln!("scaled filtered source breakdown: {:?}", source_elapsed);

    let start_time = Instant::now();
    let costs = query_cost_timeseries(conn, &range, GroupBy::Day).expect("scaled costs");
    let cost_elapsed = start_time.elapsed();
    assert_eq!(costs.path, "raw");
    assert_bucket_eq(&costs.totals, &expected_range_cost, "scaled filtered cost");
    eprintln!(
        "scaled filtered cost: ${:.6} in {:?}",
        costs.totals.estimated_cost_usd, cost_elapsed
    );

    let start_time = Instant::now();
    let status = query_status(conn, &range).expect("scaled status");
    let status_elapsed = start_time.elapsed();
    assert_eq!(status.coverage.total_messages, expected_range.message_count);
    assert_eq!(
        status.coverage.message_metrics_coverage_pct.as_value(),
        Some(100.0)
    );
    eprintln!("scaled filtered status: {:?}", status_elapsed);

    let start_time = Instant::now();
    let scatter = query_session_scatter(conn, &range, 10).expect("scaled scatter");
    let scatter_elapsed = start_time.elapsed();
    assert_eq!(scatter.len(), 10, "scatter limit is enforced");
    assert!(
        scatter
            .windows(2)
            .all(|pair| pair[0].api_tokens_total >= pair[1].api_tokens_total),
        "scatter sorted desc"
    );
    for point in &scatter {
        let (_, tokens) = expected_range_conv(&conv_day, &point.source_path, 10, 21);
        assert_eq!(
            point.api_tokens_total, tokens,
            "scatter {}",
            point.source_path
        );
    }
    eprintln!("scaled filtered scatter: {:?}", scatter_elapsed);

    // Bounded-latency evidence. These are very generous ceilings; the test
    // also prints exact measurements for the bead.
    assert!(
        rollup_elapsed.as_millis() < 1_000,
        "rollup took {rollup_elapsed:?}"
    );
    assert!(
        filtered_elapsed.as_millis() < 30_000,
        "filtered tokens took {filtered_elapsed:?}"
    );
    assert!(
        breakdown_elapsed.as_millis() < 30_000,
        "breakdown took {breakdown_elapsed:?}"
    );
    assert!(
        source_elapsed.as_millis() < 30_000,
        "source breakdown took {source_elapsed:?}"
    );
    assert!(
        cost_elapsed.as_millis() < 30_000,
        "cost took {cost_elapsed:?}"
    );
    assert!(
        status_elapsed.as_millis() < 30_000,
        "status took {status_elapsed:?}"
    );
    assert!(
        scatter_elapsed.as_millis() < 30_000,
        "scatter took {scatter_elapsed:?}"
    );
}

fn sum_buckets<'a>(buckets: impl Iterator<Item = &'a UsageBucket>) -> UsageBucket {
    let mut total = UsageBucket::default();
    for bucket in buckets {
        total.merge(bucket);
    }
    total
}

fn expected_range_conv(
    conv_day: &BTreeMap<(i64, i64), (i64, i64)>,
    source_path: &str,
    first_day: i64,
    last_day_exclusive: i64,
) -> (i64, i64) {
    let conv_id: i64 = source_path
        .trim_start_matches("/scaled/")
        .trim_end_matches(".jsonl")
        .parse::<i64>()
        .expect("scaled conversation id")
        + 1;
    let base = FrankenStorage::day_id_from_millis(
        NaiveDate::from_ymd_opt(2026, 1, 1)
            .expect("date")
            .and_hms_opt(0, 0, 0)
            .expect("time")
            .and_utc()
            .timestamp_millis(),
    );
    let mut total = (0i64, 0i64);
    for ((id, day_id), (messages, tokens)) in conv_day {
        if *id == conv_id && (*day_id - base) >= first_day && (*day_id - base) < last_day_exclusive
        {
            total.0 += messages;
            total.1 += tokens;
        }
    }
    total
}
