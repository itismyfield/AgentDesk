//! Token usage receipt: JSONL log parsing, cost calculation, and HTML rendering.

use chrono::{DateTime, Datelike, Duration, Local, TimeZone, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::{cmp, fs};

// ── Public types ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ReceiptData {
    pub period_label: String,
    pub period_start: String,
    pub period_end: String,
    pub models: Vec<ModelLineItem>,
    pub subtotal: f64,
    pub cache_discount: f64,
    pub total: f64,
    pub stats: ReceiptStats,
    pub providers: Vec<ProviderShare>,
    pub agents: Vec<AgentShare>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelLineItem {
    pub model: String,
    pub display_name: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub total_tokens: u64,
    pub cost: f64,
    pub cost_without_cache: f64,
    pub provider: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReceiptStats {
    pub total_messages: u64,
    pub total_sessions: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderShare {
    pub provider: String,
    pub tokens: u64,
    pub percentage: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentShare {
    pub agent: String,
    pub tokens: u64,
    pub cost: f64,
    pub cost_without_cache: f64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub percentage: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenAnalyticsData {
    pub period: String,
    pub period_label: String,
    pub days: u32,
    pub generated_at: String,
    pub summary: TokenAnalyticsSummary,
    pub receipt: ReceiptData,
    pub daily: Vec<DailyTokenUsage>,
    pub heatmap: Vec<TokenHeatmapCell>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenAnalyticsSummary {
    pub total_tokens: u64,
    pub total_cost: f64,
    pub cache_discount: f64,
    pub total_messages: u64,
    pub total_sessions: u64,
    pub active_days: u32,
    pub average_daily_tokens: u64,
    pub peak_day: Option<TokenPeakDay>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenPeakDay {
    pub date: String,
    pub total_tokens: u64,
    pub cost: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DailyTokenUsage {
    pub date: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub total_tokens: u64,
    pub cost: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenHeatmapCell {
    pub date: String,
    pub week_index: usize,
    pub weekday: u32,
    pub total_tokens: u64,
    pub cost: f64,
    pub level: u8,
    pub future: bool,
}

// ── Internal types ─────────────────────────────────────────────

#[derive(Clone)]
struct UsageRecord {
    timestamp: DateTime<Utc>,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    provider: String,
    agent: String,
    session_id: Option<String>,
}

#[derive(Default)]
struct ModelAccum {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    provider: String,
}

#[derive(Default)]
struct DailyAccum {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    cost: f64,
}

#[derive(Default)]
struct AgentAccum {
    tokens: u64,
    cost: f64,
    cost_without_cache: f64,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
}

struct Pricing {
    input_per_m: f64,
    output_per_m: f64,
    cache_read_factor: f64,
    cache_create_factor: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CodexTokenUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
}

// ── Pricing table ──────────────────────────────────────────────

fn pricing_for(model: &str) -> Pricing {
    match model {
        // Gemini pricing (Google AI / ai.google.dev, USD per 1M tokens, ≤200K context)
        // Source: https://ai.google.dev/gemini-api/docs/pricing (checked 2026-04-15)
        m if m.contains("gemini-3.1-pro-preview") || m.contains("gemini-2.5-pro") => Pricing {
            input_per_m: 2.00,
            output_per_m: 12.00,
            cache_read_factor: 0.1,   // $0.20/$2.00
            cache_create_factor: 0.0, // cache storage billed per-hour, not per-token
        },
        m if m.contains("gemini-2.5-flash-lite") || m.contains("flash-lite") => Pricing {
            input_per_m: 0.10,
            output_per_m: 0.40,
            cache_read_factor: 0.1, // $0.01/$0.10
            cache_create_factor: 0.0,
        },
        m if m.contains("gemini-2.5-flash") || (m.contains("gemini") && m.contains("flash")) => {
            Pricing {
                input_per_m: 0.30,
                output_per_m: 2.50,
                cache_read_factor: 0.1, // $0.03/$0.30
                cache_create_factor: 0.0,
            }
        }
        // Gemini fallback (unknown Gemini model) — N/A
        m if m.contains("gemini") => Pricing {
            input_per_m: 0.0,
            output_per_m: 0.0,
            cache_read_factor: 0.0,
            cache_create_factor: 0.0,
        },
        // Qwen pricing — using $0 until official DashScope rates are confirmed
        m if m.contains("qwen") || m.contains("coder-model") => Pricing {
            input_per_m: 0.0,
            output_per_m: 0.0,
            cache_read_factor: 0.0,
            cache_create_factor: 0.0,
        },
        m if m.contains("opus-4-8") || m.contains("opus-4-6") || m.contains("opus-4-5") => {
            Pricing {
                input_per_m: 15.0,
                output_per_m: 75.0,
                cache_read_factor: 0.1,
                cache_create_factor: 1.25,
            }
        }
        m if m.contains("sonnet-4-6") || m.contains("sonnet-4-5") => Pricing {
            input_per_m: 3.0,
            output_per_m: 15.0,
            cache_read_factor: 0.1,
            cache_create_factor: 1.25,
        },
        m if m.contains("haiku-4-5") || m.contains("haiku-4") => Pricing {
            input_per_m: 0.80,
            output_per_m: 4.0,
            cache_read_factor: 0.1,
            cache_create_factor: 1.25,
        },
        m if m.contains("gpt-5") || m.contains("codex") => Pricing {
            input_per_m: 2.0,
            output_per_m: 8.0,
            cache_read_factor: 0.5,
            cache_create_factor: 1.0,
        },
        _ => Pricing {
            input_per_m: 3.0,
            output_per_m: 15.0,
            cache_read_factor: 0.1,
            cache_create_factor: 1.25,
        },
    }
}

fn actual_cost(acc: &ModelAccum, p: &Pricing) -> f64 {
    let inp = acc.input_tokens as f64 * p.input_per_m / 1e6;
    let cr = acc.cache_read_tokens as f64 * p.input_per_m * p.cache_read_factor / 1e6;
    let cc = acc.cache_creation_tokens as f64 * p.input_per_m * p.cache_create_factor / 1e6;
    let out = acc.output_tokens as f64 * p.output_per_m / 1e6;
    inp + cr + cc + out
}

fn no_cache_cost(acc: &ModelAccum, p: &Pricing) -> f64 {
    let all_input = acc.input_tokens + acc.cache_read_tokens + acc.cache_creation_tokens;
    all_input as f64 * p.input_per_m / 1e6 + acc.output_tokens as f64 * p.output_per_m / 1e6
}

fn record_total_tokens(record: &UsageRecord) -> u64 {
    record.input_tokens
        + record.output_tokens
        + record.cache_read_tokens
        + record.cache_creation_tokens
}

fn record_cost(record: &UsageRecord) -> f64 {
    let pricing = pricing_for(&record.model);
    let acc = ModelAccum {
        input_tokens: record.input_tokens,
        output_tokens: record.output_tokens,
        cache_read_tokens: record.cache_read_tokens,
        cache_creation_tokens: record.cache_creation_tokens,
        provider: record.provider.clone(),
    };
    actual_cost(&acc, &pricing)
}

fn record_no_cache_cost(record: &UsageRecord) -> f64 {
    let pricing = pricing_for(&record.model);
    let acc = ModelAccum {
        input_tokens: record.input_tokens,
        output_tokens: record.output_tokens,
        cache_read_tokens: record.cache_read_tokens,
        cache_creation_tokens: record.cache_creation_tokens,
        provider: record.provider.clone(),
    };
    no_cache_cost(&acc, &pricing)
}

fn parse_codex_usage(v: &Value) -> Option<CodexTokenUsage> {
    Some(CodexTokenUsage {
        input_tokens: v.get("input_tokens")?.as_u64()?,
        cached_input_tokens: v
            .get("cached_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        output_tokens: v.get("output_tokens")?.as_u64()?,
    })
}

fn parse_codex_total_usage(info: &Value) -> Option<CodexTokenUsage> {
    info.get("total_token_usage").and_then(parse_codex_usage)
}

fn codex_usage_delta(
    current: CodexTokenUsage,
    previous: Option<CodexTokenUsage>,
) -> CodexTokenUsage {
    let Some(previous) = previous else {
        return current;
    };

    CodexTokenUsage {
        input_tokens: current.input_tokens.saturating_sub(previous.input_tokens),
        cached_input_tokens: current
            .cached_input_tokens
            .saturating_sub(previous.cached_input_tokens),
        output_tokens: current.output_tokens.saturating_sub(previous.output_tokens),
    }
}

fn codex_usage_record(
    model: String,
    agent: String,
    session_id: Option<String>,
    timestamp: DateTime<Utc>,
    usage: CodexTokenUsage,
) -> UsageRecord {
    UsageRecord {
        timestamp,
        model,
        input_tokens: usage.input_tokens.saturating_sub(usage.cached_input_tokens),
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cached_input_tokens,
        cache_creation_tokens: 0,
        provider: "Codex".into(),
        agent,
        session_id,
    }
}

fn shorten_model(model: &str) -> String {
    match model {
        m if m.contains("gemini-3.1-pro-preview") || m.contains("gemini-2.5-pro") => {
            "Gemini 2.5 Pro".into()
        }
        m if m.contains("gemini-2.5-flash-lite") || m.contains("flash-lite") => {
            "Gemini 2.5 Flash Lite".into()
        }
        m if m.contains("gemini-2.5-flash") || (m.contains("gemini") && m.contains("flash")) => {
            "Gemini 2.5 Flash".into()
        }
        m if m.contains("gemini") => "Gemini".into(),
        m if m.contains("qwen3-max") || m.contains("qwen3:max") => "Qwen3 Max".into(),
        m if m.contains("qwen3.6-plus") || m.contains("qwen3.6:plus") => "Qwen3.6 Plus".into(),
        m if m.contains("qwen3.5-plus") || m.contains("qwen3.5:plus") => "Qwen3.5 Plus".into(),
        m if m.contains("qwen3.5-flash") || m.contains("qwen3.5:flash") => "Qwen3.5 Flash".into(),
        m if m.contains("qwen3.5") && m.contains("397b") => "Qwen3.5 397B".into(),
        m if m.contains("qwen") || m.contains("coder-model") => "Qwen".into(),
        m if m.contains("opus-4-8") => "Opus 4.8".into(),
        m if m.contains("opus-4-6") => "Opus 4.6".into(),
        m if m.contains("opus-4-5") => "Opus 4.5".into(),
        m if m.contains("sonnet-4-6") => "Sonnet 4.6".into(),
        m if m.contains("sonnet-4-5") => "Sonnet 4.5".into(),
        m if m.contains("haiku-4-5") => "Haiku 4.5".into(),
        m if m.contains("haiku-4") => "Haiku 4".into(),
        m if m.contains("gpt-5.4") => "GPT-5.4".into(),
        m if m.contains("gpt-5.3-codex") => "GPT-5.3 Codex".into(),
        m if m.contains("gpt-5.3") => "GPT-5.3".into(),
        m if m.contains("gpt-5") => "GPT-5".into(),
        _ => {
            let s = model.to_string();
            if s.len() > 20 {
                format!("{}...", s.chars().take(18).collect::<String>())
            } else {
                s
            }
        }
    }
}

// ── Agent name extraction ──────────────────────────────────────

/// Build a map from paths → agent names by scanning:
/// 1. `~/.adk/*/workspaces/` — canonical workspace directories
/// 2. `~/.adk/*/worktrees/`  — git worktrees, resolved to parent workspace via `.git` file
fn build_workspace_map() -> HashMap<PathBuf, String> {
    let mut map = HashMap::new();
    let home = dirs::home_dir().unwrap_or_default();
    let adk = home.join(".adk");
    let Ok(entries) = fs::read_dir(&adk) else {
        return map;
    };
    for entry in entries.flatten() {
        let env_dir = entry.path();

        // 1. Workspace directories
        let ws_dir = env_dir.join("workspaces");
        if ws_dir.is_dir() {
            if let Ok(ws_entries) = fs::read_dir(&ws_dir) {
                for ws in ws_entries.flatten() {
                    let ws_path = ws.path();
                    if ws_path.is_dir() {
                        if let Some(name) = ws_path.file_name().and_then(|n| n.to_str()) {
                            let name = name.to_string();
                            let canonical =
                                ws_path.canonicalize().unwrap_or_else(|_| ws_path.clone());
                            map.insert(canonical, name.clone());
                            map.insert(ws_path, name);
                        }
                    }
                }
            }
        }

        // 2. Worktree directories — resolve to parent workspace via .git file
        //    .git file contains: gitdir: /.../.adk/.../workspaces/{agent}/.git/worktrees/{name}
        let wt_dir = env_dir.join("worktrees");
        if wt_dir.is_dir() {
            if let Ok(wt_entries) = fs::read_dir(&wt_dir) {
                for wt in wt_entries.flatten() {
                    let wt_path = wt.path();
                    if !wt_path.is_dir() {
                        continue;
                    }
                    let git_file = wt_path.join(".git");
                    if let Ok(content) = fs::read_to_string(&git_file) {
                        // Parse: "gitdir: .../workspaces/{agent}/.git/worktrees/..."
                        if let Some(gitdir) = content.trim().strip_prefix("gitdir: ") {
                            let gp = Path::new(gitdir);
                            for (i, comp) in gp.components().enumerate() {
                                let s = comp.as_os_str().to_string_lossy();
                                if s == "workspaces" {
                                    if let Some(next) = gp.components().nth(i + 1) {
                                        let agent = next.as_os_str().to_string_lossy().to_string();
                                        let canonical = wt_path
                                            .canonicalize()
                                            .unwrap_or_else(|_| wt_path.clone());
                                        map.insert(canonical, agent.clone());
                                        map.insert(wt_path.clone(), agent);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    map
}

/// Extract agent name from a JSONL file path (Claude) or cwd (any provider).
///
/// Resolution order:
/// 1. Claude file path: encoded workspace dir always contains `workspaces-{name}`
/// 2. cwd path: match against known workspace/worktree paths from filesystem
///    (build_workspace_map resolves worktrees → parent workspace via .git file)
/// 3. cwd path: check `workspaces/` path marker (worktrees excluded — their
///    directory names are ephemeral and don't correspond to agent names)
/// 4. cwd path: find git repo root and use its directory name as project label
/// 5. Fallback to provider name
fn resolve_agent(
    file_path: &Path,
    cwd: Option<&str>,
    ws_map: &HashMap<PathBuf, String>,
    default: &str,
) -> String {
    // 1. Claude JSONL file path: encoded workspace dir always contains `workspaces-{name}`
    for ancestor in file_path.ancestors() {
        if let Some(name) = ancestor.file_name().and_then(|n| n.to_str()) {
            if name.starts_with('-') && name.contains("workspaces-") {
                if let Some(ws) = name.rsplit("workspaces-").next() {
                    if !ws.is_empty() {
                        return ws.to_string();
                    }
                }
            }
        }
        if ancestor.ends_with("projects") || ancestor.ends_with("sessions") {
            break;
        }
    }

    // 2-4. cwd-based resolution
    if let Some(cwd) = cwd {
        let cwd_path = Path::new(cwd);

        // 2. Match cwd against known workspace/worktree paths (most accurate —
        //    worktrees are resolved to their parent workspace by build_workspace_map)
        for (ws_path, agent_name) in ws_map {
            if cwd_path.starts_with(ws_path) {
                return agent_name.clone();
            }
        }

        // 3. Path marker heuristic — only `workspaces/` (NOT `worktrees/`)
        let mut found_marker = false;
        for component in cwd_path.components() {
            if found_marker {
                let name = component.as_os_str().to_string_lossy();
                if !name.is_empty() {
                    return name.to_string();
                }
            }
            let s = component.as_os_str().to_string_lossy();
            if s == "workspaces" {
                found_marker = true;
            }
        }

        // 4. Git repo root — walk cwd ancestors looking for .git dir/file.
        //    Uses the repo directory name as the project label so non-ADK
        //    sessions get a meaningful name instead of the provider fallback.
        for ancestor in cwd_path.ancestors() {
            if ancestor.join(".git").exists() {
                if let Some(name) = ancestor.file_name().and_then(|n| n.to_str()) {
                    return name.to_string();
                }
                break;
            }
        }
    }

    default.into()
}

// ── File discovery ─────────────────────────────────────────────

fn find_jsonl(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

fn scan_gemini_chats(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let root = home.join(".gemini").join("tmp");
    let Ok(entries) = fs::read_dir(&root) else {
        return out;
    };

    for entry in entries.flatten() {
        let chats_dir = entry.path().join("chats");
        let Ok(chat_entries) = fs::read_dir(&chats_dir) else {
            continue;
        };
        for chat in chat_entries.flatten() {
            let path = chat.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                out.push(path);
            }
        }
    }

    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
}

// ── Claude Code JSONL parsing ──────────────────────────────────

fn parse_claude(
    path: &Path,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    ws_map: &HashMap<PathBuf, String>,
) -> (Vec<UsageRecord>, u64, Option<String>) {
    // Resolve agent from file path first (Claude paths always encode workspace)
    let mut agent_name = {
        let resolved = resolve_agent(path, None, ws_map, "claude");
        if resolved != "claude" {
            Some(resolved)
        } else {
            None
        }
    };
    let mut sid: Option<String> = None;
    let mut by_request: HashMap<String, UsageRecord> = HashMap::new();
    let mut no_reqid_records: Vec<UsageRecord> = Vec::new();

    let Ok(file) = fs::File::open(path) else {
        return (Vec::new(), 0, None);
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        // Fallback: try cwd from records if file path didn't resolve
        if agent_name.is_none() {
            if let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) {
                let name = resolve_agent(path, Some(cwd), ws_map, "claude");
                if name != "claude" {
                    agent_name = Some(name);
                }
            }
        }

        let Some(ts_str) = v.get("timestamp").and_then(|t| t.as_str()) else {
            continue;
        };
        let Some(ts) = parse_ts(ts_str) else { continue };
        if ts < start || ts > end {
            continue;
        }

        if sid.is_none() {
            sid = v
                .get("sessionId")
                .and_then(|s| s.as_str())
                .map(String::from);
        }

        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let Some(message) = v.get("message") else {
            continue;
        };
        let Some(usage) = message.get("usage") else {
            continue;
        };
        let model = message
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown");
        if model == "<synthetic>" {
            continue;
        }

        let rec = UsageRecord {
            timestamp: ts,
            model: model.into(),
            input_tokens: usage
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            output_tokens: usage
                .get("output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            cache_read_tokens: usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            cache_creation_tokens: usage
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            provider: "Claude".into(),
            agent: agent_name.clone().unwrap_or_else(|| "claude".into()),
            session_id: sid.clone(),
        };

        // Use requestId to deduplicate — last entry wins (cumulative usage)
        if let Some(req_id) = v.get("requestId").and_then(|r| r.as_str()) {
            by_request.insert(req_id.to_string(), rec);
        } else {
            no_reqid_records.push(rec);
        }
    }

    let msgs = (by_request.len() + no_reqid_records.len()) as u64;
    let mut records: Vec<UsageRecord> = by_request.into_values().collect();
    records.extend(no_reqid_records);
    (records, msgs, sid)
}

// ── Codex JSONL parsing ────────────────────────────────────────

fn parse_codex(
    path: &Path,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    ws_map: &HashMap<PathBuf, String>,
) -> (Vec<UsageRecord>, u64, Option<String>) {
    let mut agent_name: Option<String> = None;
    let mut records = Vec::new();
    let mut sid: Option<String> = None;
    let mut current_model = String::from("codex");
    // Codex token_count snapshots are cumulative within the session.
    // Keep the final in-range total per turn and subtract the previous turn's
    // final total to recover that turn's actual usage.
    let mut previous_total: Option<CodexTokenUsage> = None;
    let mut pending_total: Option<CodexTokenUsage> = None;
    let mut pending_timestamp: Option<DateTime<Utc>> = None;

    let Ok(file) = fs::File::open(path) else {
        return (records, 0, None);
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let rtype = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

        if rtype == "session_meta" {
            sid = v
                .get("payload")
                .and_then(|p| p.get("id"))
                .and_then(|s| s.as_str())
                .or_else(|| v.get("id").and_then(|s| s.as_str()))
                .map(String::from);
            // Extract agent from session_meta.payload.cwd
            if agent_name.is_none() {
                if let Some(cwd) = v
                    .get("payload")
                    .and_then(|p| p.get("cwd"))
                    .and_then(|c| c.as_str())
                {
                    let name = resolve_agent(path, Some(cwd), ws_map, "codex");
                    if name != "codex" {
                        agent_name = Some(name);
                    }
                }
            }
            continue;
        }
        if rtype == "turn_context" {
            if let Some(total) = pending_total.take() {
                let delta = codex_usage_delta(total, previous_total);
                previous_total = Some(total);
                if delta.input_tokens > 0
                    || delta.cached_input_tokens > 0
                    || delta.output_tokens > 0
                {
                    records.push(codex_usage_record(
                        current_model.clone(),
                        agent_name.clone().unwrap_or_else(|| "codex".into()),
                        sid.clone(),
                        pending_timestamp.take().unwrap_or_else(Utc::now),
                        delta,
                    ));
                }
            }
            pending_timestamp = None;
            if let Some(m) = v
                .get("payload")
                .and_then(|p| p.get("model"))
                .and_then(|m| m.as_str())
            {
                current_model = m.into();
            }
            continue;
        }
        if rtype != "event_msg" {
            continue;
        }

        let Some(payload) = v.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(|t| t.as_str()) != Some("token_count") {
            continue;
        }

        let Some(ts_str) = v.get("timestamp").and_then(|t| t.as_str()) else {
            continue;
        };
        let Some(ts) = parse_ts(ts_str) else { continue };

        let Some(info) = payload.get("info") else {
            continue;
        };
        if info.is_null() {
            continue;
        }
        let Some(total_usage) = parse_codex_total_usage(info) else {
            continue;
        };
        if ts < start || ts > end {
            if pending_total.is_none() {
                previous_total = Some(total_usage);
            }
            continue;
        }

        // Overwrite the turn's pending total — only the final in-range snapshot matters.
        pending_total = Some(total_usage);
        pending_timestamp = Some(ts);
    }
    if let Some(total) = pending_total.take() {
        let delta = codex_usage_delta(total, previous_total);
        if delta.input_tokens > 0 || delta.cached_input_tokens > 0 || delta.output_tokens > 0 {
            records.push(codex_usage_record(
                current_model,
                agent_name.unwrap_or_else(|| "codex".into()),
                sid.clone(),
                pending_timestamp.unwrap_or_else(Utc::now),
                delta,
            ));
        }
    }
    let msgs = records.len() as u64;
    (records, msgs, sid)
}

fn parse_gemini(
    path: &Path,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> (Vec<UsageRecord>, u64, Option<String>) {
    let Ok(contents) = fs::read_to_string(path) else {
        return (Vec::new(), 0, None);
    };
    let Ok(root) = serde_json::from_str::<Value>(&contents) else {
        return (Vec::new(), 0, None);
    };

    let session_id = root
        .get("sessionId")
        .and_then(|value| value.as_str())
        .map(String::from);
    let start_time = root
        .get("startTime")
        .and_then(|value| value.as_str())
        .and_then(parse_ts);

    let Some(messages) = root.get("messages").and_then(|value| value.as_array()) else {
        return (Vec::new(), 0, session_id);
    };

    let mut records = Vec::new();
    for message in messages {
        if message.get("type").and_then(|value| value.as_str()) != Some("gemini") {
            continue;
        }
        let Some(tokens) = message.get("tokens") else {
            continue;
        };
        let timestamp = message
            .get("timestamp")
            .and_then(|value| value.as_str())
            .and_then(parse_ts)
            .or(start_time);
        let Some(timestamp) = timestamp else {
            continue;
        };
        if timestamp < start || timestamp > end {
            continue;
        }

        let input_tokens = tokens
            .get("input")
            .and_then(|value| value.as_u64())
            .unwrap_or(0)
            + tokens
                .get("tool")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
        let output_tokens = tokens
            .get("output")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let cache_read_tokens = tokens
            .get("cached")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let cache_creation_tokens = tokens
            .get("thoughts")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);

        if input_tokens == 0
            && output_tokens == 0
            && cache_read_tokens == 0
            && cache_creation_tokens == 0
        {
            continue;
        }

        records.push(UsageRecord {
            timestamp,
            model: message
                .get("model")
                .and_then(|value| value.as_str())
                .unwrap_or("gemini")
                .to_string(),
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            provider: "Gemini".into(),
            agent: "gemini".into(),
            session_id: session_id.clone(),
        });
    }

    let msgs = records.len() as u64;
    (records, msgs, session_id)
}

fn decode_qwen_project_path(encoded: &str) -> Option<String> {
    if encoded.is_empty() {
        return None;
    }

    let mut decoded = String::new();
    let mut chars = encoded.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '-' {
            if chars.peek() == Some(&'-') {
                chars.next();
                decoded.push('/');
                decoded.push('.');
            } else {
                decoded.push('/');
            }
        } else {
            decoded.push(ch);
        }
    }

    if decoded.is_empty() {
        None
    } else {
        Some(decoded)
    }
}

fn normalize_qwen_model(model: &str) -> String {
    match model {
        "" => "qwen".into(),
        "coder-model" => "qwen".into(),
        other => other.to_string(),
    }
}

fn parse_qwen(
    path: &Path,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    ws_map: &HashMap<PathBuf, String>,
) -> (Vec<UsageRecord>, u64, Option<String>) {
    let mut agent_name = path
        .parent()
        .and_then(Path::parent)
        .and_then(|project_dir| project_dir.file_name())
        .and_then(|value| value.to_str())
        .and_then(decode_qwen_project_path)
        .and_then(|cwd| {
            let resolved = resolve_agent(path, Some(&cwd), ws_map, "qwen");
            (resolved != "qwen").then_some(resolved)
        });
    let mut sid: Option<String> = None;
    let mut records = Vec::new();

    let Ok(file) = fs::File::open(path) else {
        return (records, 0, None);
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        if sid.is_none() {
            sid = v
                .get("sessionId")
                .and_then(|value| value.as_str())
                .map(String::from);
        }

        if agent_name.is_none() {
            if let Some(cwd) = v.get("cwd").and_then(|value| value.as_str()) {
                let resolved = resolve_agent(path, Some(cwd), ws_map, "qwen");
                if resolved != "qwen" {
                    agent_name = Some(resolved);
                }
            }
        }

        if v.get("type").and_then(|value| value.as_str()) != Some("assistant") {
            continue;
        }

        let Some(timestamp) = v
            .get("timestamp")
            .and_then(|value| value.as_str())
            .and_then(parse_ts)
        else {
            continue;
        };
        if timestamp < start || timestamp > end {
            continue;
        }

        let Some(usage) = v.get("usageMetadata") else {
            continue;
        };
        let prompt_tokens = usage
            .get("promptTokenCount")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let output_tokens = usage
            .get("candidatesTokenCount")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let cache_read_tokens = usage
            .get("cachedContentTokenCount")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let input_tokens = prompt_tokens.saturating_sub(cache_read_tokens);
        let cache_creation_tokens = usage
            .get("thoughtsTokenCount")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);

        if input_tokens == 0
            && output_tokens == 0
            && cache_read_tokens == 0
            && cache_creation_tokens == 0
        {
            continue;
        }

        records.push(UsageRecord {
            timestamp,
            model: normalize_qwen_model(
                v.get("model")
                    .and_then(|value| value.as_str())
                    .unwrap_or("qwen"),
            ),
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            provider: "Qwen".into(),
            agent: agent_name.clone().unwrap_or_else(|| "qwen".into()),
            session_id: sid.clone(),
        });
    }

    let msgs = records.len() as u64;
    (records, msgs, sid)
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                .map(|ndt| ndt.and_utc())
                .ok()
        })
}

// ── Rate limit window ──────────────────────────────────────────

fn parse_ratelimit_window_start_data(data: &str) -> Option<DateTime<Utc>> {
    let parsed: Value = serde_json::from_str(data).ok()?;
    let buckets = parsed.get("buckets")?.as_array()?;
    for b in buckets {
        let name = b.get("name")?.as_str()?;
        if name.contains("7d") || name.contains("week") {
            let reset = b.get("reset")?.as_i64()?;
            return DateTime::from_timestamp(reset, 0).map(|dt| dt - Duration::days(7));
        }
    }
    None
}

pub async fn ratelimit_window_start_pg(pool: &sqlx::PgPool) -> Option<DateTime<Utc>> {
    let row = sqlx::query("SELECT data FROM rate_limit_cache WHERE provider = 'claude' AND profile_id = 'default' LIMIT 1")
        .fetch_optional(pool)
        .await
        .ok()??;
    let data: String = row.get("data");
    parse_ratelimit_window_start_data(&data)
}

// ── Collection entry point ─────────────────────────────────────

fn scan_usage_records(start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<UsageRecord> {
    let home = dirs::home_dir().unwrap_or_default();
    let claude_files = find_jsonl(&home.join(".claude").join("projects"));
    let codex_files = find_jsonl(&home.join(".codex").join("sessions"));
    let gemini_files = scan_gemini_chats(&home);
    let qwen_files = find_jsonl(&home.join(".qwen").join("projects"));
    let ws_map = build_workspace_map();

    let mut all: Vec<UsageRecord> = Vec::new();

    for f in &claude_files {
        let (recs, _, _) = parse_claude(f, start, end, &ws_map);
        all.extend(recs);
    }
    for f in &codex_files {
        let (recs, _, _) = parse_codex(f, start, end, &ws_map);
        all.extend(recs);
    }
    for f in &gemini_files {
        let (recs, _, _) = parse_gemini(f, start, end);
        all.extend(recs);
    }
    for f in &qwen_files {
        let (recs, _, _) = parse_qwen(f, start, end, &ws_map);
        all.extend(recs);
    }
    all.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    all
}

fn build_receipt_data(
    records: &[UsageRecord],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    period_label: &str,
) -> ReceiptData {
    let mut total_msgs = 0u64;
    let mut sessions = HashSet::new();
    for record in records {
        total_msgs += 1;
        if let Some(session_id) = record.session_id.as_ref().filter(|value| !value.is_empty()) {
            sessions.insert(session_id.clone());
        }
    }

    // Aggregate by model
    let mut map: HashMap<(String, String), ModelAccum> = HashMap::new();
    for r in records {
        let acc = map
            .entry((r.provider.clone(), r.model.clone()))
            .or_default();
        acc.input_tokens += r.input_tokens;
        acc.output_tokens += r.output_tokens;
        acc.cache_read_tokens += r.cache_read_tokens;
        acc.cache_creation_tokens += r.cache_creation_tokens;
        if acc.provider.is_empty() {
            acc.provider.clone_from(&r.provider);
        }
    }

    // Sort by cost descending
    let mut entries: Vec<_> = map.into_iter().collect();
    entries.sort_by(|a, b| {
        let ca = actual_cost(&a.1, &pricing_for(&a.0.1));
        let cb = actual_cost(&b.1, &pricing_for(&b.0.1));
        cb.partial_cmp(&ca).unwrap_or(cmp::Ordering::Equal)
    });

    let mut models = Vec::new();
    let mut grand_sub = 0.0f64;
    let mut grand_total = 0.0f64;

    for ((provider, model), acc) in &entries {
        let p = pricing_for(model);
        let cost = actual_cost(acc, &p);
        let sub = no_cache_cost(acc, &p);
        grand_sub += sub;
        grand_total += cost;
        models.push(ModelLineItem {
            display_name: shorten_model(model),
            model: model.clone(),
            input_tokens: acc.input_tokens,
            output_tokens: acc.output_tokens,
            cache_read_tokens: acc.cache_read_tokens,
            cache_creation_tokens: acc.cache_creation_tokens,
            total_tokens: acc.input_tokens
                + acc.output_tokens
                + acc.cache_read_tokens
                + acc.cache_creation_tokens,
            cost,
            cost_without_cache: sub,
            provider: provider.clone(),
        });
    }

    // Agent shares — per-(agent, provider) for accurate provider-split filtering.
    let mut ap_stats: HashMap<(String, String), AgentAccum> = HashMap::new();
    for r in records {
        let stats = ap_stats
            .entry((r.agent.clone(), r.provider.clone()))
            .or_default();
        stats.tokens += record_total_tokens(r);
        stats.cost += record_cost(r);
        stats.cost_without_cache += record_no_cache_cost(r);
        stats.input_tokens += r.input_tokens;
        stats.cache_read_tokens += r.cache_read_tokens;
        stats.cache_creation_tokens += r.cache_creation_tokens;
    }
    // Collapse to per-agent for the combined receipt
    let mut agent_stats: HashMap<String, AgentAccum> = HashMap::new();
    for ((agent, _prov), stats) in &ap_stats {
        let entry = agent_stats.entry(agent.clone()).or_default();
        entry.tokens += stats.tokens;
        entry.cost += stats.cost;
        entry.cost_without_cache += stats.cost_without_cache;
        entry.input_tokens += stats.input_tokens;
        entry.cache_read_tokens += stats.cache_read_tokens;
        entry.cache_creation_tokens += stats.cache_creation_tokens;
    }
    let agent_total_tok: u64 = agent_stats.values().map(|stats| stats.tokens).sum();
    let mut agents: Vec<AgentShare> = agent_stats
        .into_iter()
        .map(|(agent, stats)| AgentShare {
            percentage: if agent_total_tok > 0 {
                stats.tokens as f64 / agent_total_tok as f64 * 100.0
            } else {
                0.0
            },
            agent,
            tokens: stats.tokens,
            cost: stats.cost,
            cost_without_cache: stats.cost_without_cache,
            input_tokens: stats.input_tokens,
            cache_read_tokens: stats.cache_read_tokens,
            cache_creation_tokens: stats.cache_creation_tokens,
        })
        .collect();
    agents.sort_by(|a, b| {
        b.percentage
            .partial_cmp(&a.percentage)
            .unwrap_or(cmp::Ordering::Equal)
    });

    // Provider shares
    let mut prov_tokens: HashMap<String, u64> = HashMap::new();
    let mut total_tok = 0u64;
    for m in &models {
        *prov_tokens.entry(m.provider.clone()).or_default() += m.total_tokens;
        total_tok += m.total_tokens;
    }
    let mut providers: Vec<ProviderShare> = prov_tokens
        .into_iter()
        .map(|(prov, tok)| ProviderShare {
            provider: prov,
            tokens: tok,
            percentage: if total_tok > 0 {
                tok as f64 / total_tok as f64 * 100.0
            } else {
                0.0
            },
        })
        .collect();
    providers.sort_by(|a, b| {
        b.percentage
            .partial_cmp(&a.percentage)
            .unwrap_or(cmp::Ordering::Equal)
    });

    let start_local = start.with_timezone(&Local);
    let end_local = end.with_timezone(&Local);

    ReceiptData {
        period_label: period_label.into(),
        period_start: start_local.format("%Y-%m-%d").to_string(),
        period_end: end_local.format("%Y-%m-%d").to_string(),
        models,
        subtotal: grand_sub,
        cache_discount: (grand_sub - grand_total).max(0.0),
        total: grand_total,
        agents,
        stats: ReceiptStats {
            total_messages: total_msgs,
            total_sessions: sessions.len() as u64,
        },
        providers,
    }
}

fn build_daily_usage(
    records: &[UsageRecord],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Vec<DailyTokenUsage> {
    let start_date = start.with_timezone(&Local).date_naive();
    let end_date = end.with_timezone(&Local).date_naive();
    let mut by_date: HashMap<chrono::NaiveDate, DailyAccum> = HashMap::new();

    for record in records {
        let day = record.timestamp.with_timezone(&Local).date_naive();
        if day < start_date || day > end_date {
            continue;
        }
        let entry = by_date.entry(day).or_default();
        entry.input_tokens += record.input_tokens;
        entry.output_tokens += record.output_tokens;
        entry.cache_read_tokens += record.cache_read_tokens;
        entry.cache_creation_tokens += record.cache_creation_tokens;
        entry.cost += record_cost(record);
    }

    let mut daily = Vec::new();
    let days = (end_date - start_date).num_days();
    for offset in 0..=days {
        let day = start_date + Duration::days(offset);
        let accum = by_date.remove(&day).unwrap_or_default();
        daily.push(DailyTokenUsage {
            date: day.format("%Y-%m-%d").to_string(),
            input_tokens: accum.input_tokens,
            output_tokens: accum.output_tokens,
            cache_read_tokens: accum.cache_read_tokens,
            cache_creation_tokens: accum.cache_creation_tokens,
            total_tokens: accum.input_tokens
                + accum.output_tokens
                + accum.cache_read_tokens
                + accum.cache_creation_tokens,
            cost: accum.cost,
        });
    }
    daily
}

fn heatmap_level(value: u64, quantiles: &[u64; 3]) -> u8 {
    if value == 0 {
        0
    } else if value <= quantiles[0] {
        1
    } else if value <= quantiles[1] {
        2
    } else if value <= quantiles[2] {
        3
    } else {
        4
    }
}

fn build_heatmap(records: &[UsageRecord], end: DateTime<Utc>) -> Vec<TokenHeatmapCell> {
    let end_date = end.with_timezone(&Local).date_naive();
    let week_start = end_date - Duration::days(end_date.weekday().num_days_from_monday() as i64);
    let start_date = week_start - Duration::days(12 * 7);
    let mut by_date: HashMap<chrono::NaiveDate, (u64, f64)> = HashMap::new();

    for record in records {
        let day = record.timestamp.with_timezone(&Local).date_naive();
        if day < start_date || day > week_start + Duration::days(6) {
            continue;
        }
        let entry = by_date.entry(day).or_insert((0, 0.0));
        entry.0 += record_total_tokens(record);
        entry.1 += record_cost(record);
    }

    let mut positive = Vec::new();
    for week in 0..13 {
        for weekday in 0..7 {
            let day = start_date + Duration::days((week * 7 + weekday) as i64);
            if day <= end_date {
                positive.push(by_date.get(&day).map(|value| value.0).unwrap_or(0));
            }
        }
    }
    positive.retain(|value| *value > 0);
    positive.sort_unstable();
    let quantiles = if positive.is_empty() {
        [0, 0, 0]
    } else {
        let last = positive.len() - 1;
        [
            positive[last / 4],
            positive[(last * 2) / 4],
            positive[(last * 3) / 4],
        ]
    };

    let mut cells = Vec::with_capacity(91);
    for week in 0..13 {
        for weekday in 0..7 {
            let day = start_date + Duration::days((week * 7 + weekday) as i64);
            let future = day > end_date;
            let (tokens, cost) = by_date.get(&day).copied().unwrap_or((0, 0.0));
            cells.push(TokenHeatmapCell {
                date: day.format("%Y-%m-%d").to_string(),
                week_index: week,
                weekday: weekday as u32,
                total_tokens: if future { 0 } else { tokens },
                cost: if future { 0.0 } else { cost },
                level: if future {
                    0
                } else {
                    heatmap_level(tokens, &quantiles)
                },
                future,
            });
        }
    }
    cells
}

pub fn collect(start: DateTime<Utc>, end: DateTime<Utc>, period_label: &str) -> ReceiptData {
    let records = scan_usage_records(start, end);
    build_receipt_data(&records, start, end, period_label)
}

pub fn collect_token_analytics(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    period_label: &str,
    period: &str,
) -> TokenAnalyticsData {
    let end_date = end.with_timezone(&Local).date_naive();
    let heatmap_week_start =
        end_date - Duration::days(end_date.weekday().num_days_from_monday() as i64);
    let heatmap_start_local = heatmap_week_start - Duration::days(12 * 7);
    let heatmap_start = Local
        .from_local_datetime(&heatmap_start_local.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|| start - Duration::days(84));

    let records = scan_usage_records(heatmap_start, end);
    let filtered: Vec<UsageRecord> = records
        .iter()
        .filter(|record| record.timestamp >= start)
        .cloned()
        .collect();
    let receipt = build_receipt_data(&filtered, start, end, period_label);
    let daily = build_daily_usage(&filtered, start, end);
    let active_days = daily.iter().filter(|day| day.total_tokens > 0).count() as u32;
    let total_tokens: u64 = receipt.models.iter().map(|model| model.total_tokens).sum();
    let average_daily_tokens = if daily.is_empty() {
        0
    } else {
        total_tokens / daily.len() as u64
    };
    let peak_day = daily.iter().max_by(|left, right| {
        left.total_tokens
            .cmp(&right.total_tokens)
            .then_with(|| left.date.cmp(&right.date))
    });

    TokenAnalyticsData {
        period: period.to_string(),
        period_label: period_label.to_string(),
        days: daily.len() as u32,
        generated_at: end.with_timezone(&Local).to_rfc3339(),
        summary: TokenAnalyticsSummary {
            total_tokens,
            total_cost: receipt.total,
            cache_discount: receipt.cache_discount,
            total_messages: receipt.stats.total_messages,
            total_sessions: receipt.stats.total_sessions,
            active_days,
            average_daily_tokens,
            peak_day: peak_day.map(|day| TokenPeakDay {
                date: day.date.clone(),
                total_tokens: day.total_tokens,
                cost: day.cost,
            }),
        },
        receipt,
        daily,
        heatmap: build_heatmap(&records, end),
    }
}
