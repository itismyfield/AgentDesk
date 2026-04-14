//! Token usage receipt: JSONL log parsing, cost calculation, and HTML rendering.

use chrono::{DateTime, Datelike, Duration, Local, TimeZone, Utc};
use serde::Serialize;
use serde_json::Value;
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
    /// Per-provider message and session counts (provider → (messages, sessions)).
    #[serde(skip)]
    pub per_provider: HashMap<String, (u64, u64)>,
    /// Provider → list of AgentShare for that provider (pre-computed for split).
    #[serde(skip)]
    pub per_provider_agents: HashMap<String, Vec<AgentShare>>,
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
        m if m.contains("opus-4-6") || m.contains("opus-4-5") => Pricing {
            input_per_m: 15.0,
            output_per_m: 75.0,
            cache_read_factor: 0.1,
            cache_create_factor: 1.25,
        },
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
                format!("{}...", &s[..18])
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

pub fn ratelimit_window_start(conn: &rusqlite::Connection) -> Option<DateTime<Utc>> {
    let data: String = conn
        .query_row(
            "SELECT data FROM rate_limit_cache WHERE provider = 'claude' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok()?;
    let parsed: Value = serde_json::from_str(&data).ok()?;
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

// ── Collection entry point ─────────────────────────────────────

fn scan_usage_records(start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<UsageRecord> {
    let home = dirs::home_dir().unwrap_or_default();
    let claude_files = find_jsonl(&home.join(".claude").join("projects"));
    let codex_files = find_jsonl(&home.join(".codex").join("sessions"));
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
    let mut prov_msgs: HashMap<String, u64> = HashMap::new();
    let mut prov_sessions: HashMap<String, HashSet<String>> = HashMap::new();
    for record in records {
        total_msgs += 1;
        *prov_msgs.entry(record.provider.clone()).or_default() += 1;
        if let Some(session_id) = record.session_id.as_ref().filter(|value| !value.is_empty()) {
            sessions.insert(session_id.clone());
            prov_sessions
                .entry(record.provider.clone())
                .or_default()
                .insert(session_id.clone());
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
            per_provider: prov_msgs
                .into_iter()
                .map(|(prov, msgs)| {
                    let sess = prov_sessions
                        .get(&prov)
                        .map(|s| s.len() as u64)
                        .unwrap_or(0);
                    (prov, (msgs, sess))
                })
                .collect(),
            per_provider_agents: {
                let mut by_prov: HashMap<String, Vec<(String, AgentAccum)>> = HashMap::new();
                for ((agent, prov), stats) in &ap_stats {
                    by_prov.entry(prov.clone()).or_default().push((
                        agent.clone(),
                        AgentAccum {
                            tokens: stats.tokens,
                            cost: stats.cost,
                            cost_without_cache: stats.cost_without_cache,
                            input_tokens: stats.input_tokens,
                            cache_read_tokens: stats.cache_read_tokens,
                            cache_creation_tokens: stats.cache_creation_tokens,
                        },
                    ));
                }
                by_prov
                    .into_iter()
                    .map(|(prov, items)| {
                        let prov_total: u64 = items.iter().map(|(_, stats)| stats.tokens).sum();
                        let mut shares: Vec<AgentShare> = items
                            .into_iter()
                            .map(|(agent, stats)| AgentShare {
                                agent,
                                tokens: stats.tokens,
                                cost: stats.cost,
                                cost_without_cache: stats.cost_without_cache,
                                input_tokens: stats.input_tokens,
                                cache_read_tokens: stats.cache_read_tokens,
                                cache_creation_tokens: stats.cache_creation_tokens,
                                percentage: if prov_total > 0 {
                                    stats.tokens as f64 / prov_total as f64 * 100.0
                                } else {
                                    0.0
                                },
                            })
                            .collect();
                        shares.sort_by(|a, b| {
                            b.percentage
                                .partial_cmp(&a.percentage)
                                .unwrap_or(cmp::Ordering::Equal)
                        });
                        (prov, shares)
                    })
                    .collect()
            },
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

/// Split a receipt into per-provider receipts — each contains only models from
/// one provider.  This is used by the Discord command to render separate PNG
/// receipts per provider.
pub fn split_by_provider(data: &ReceiptData) -> Vec<ReceiptData> {
    use std::collections::HashMap;

    let mut order: Vec<String> = Vec::new();
    let mut by_prov: HashMap<String, Vec<ModelLineItem>> = HashMap::new();
    for m in &data.models {
        by_prov
            .entry(m.provider.clone())
            .or_default()
            .push(m.clone());
        if !order.contains(&m.provider) {
            order.push(m.provider.clone());
        }
    }

    if order.len() <= 1 {
        return vec![data.clone()];
    }

    order
        .into_iter()
        .map(|prov| {
            let models = by_prov.remove(&prov).unwrap_or_default();
            let total: f64 = models.iter().map(|m| m.cost).sum();
            let subtotal: f64 = models.iter().map(|m| m.cost_without_cache).sum();
            let total_tokens: u64 = models.iter().map(|m| m.total_tokens).sum();
            let (prov_msgs, prov_sess) = data
                .stats
                .per_provider
                .get(&prov)
                .copied()
                .unwrap_or((0, 0));
            ReceiptData {
                period_label: data.period_label.clone(),
                period_start: data.period_start.clone(),
                period_end: data.period_end.clone(),
                subtotal,
                cache_discount: (subtotal - total).max(0.0),
                total,
                stats: ReceiptStats {
                    total_messages: prov_msgs,
                    total_sessions: prov_sess,
                    per_provider: HashMap::new(),
                    per_provider_agents: HashMap::new(),
                },
                providers: vec![ProviderShare {
                    provider: prov.clone(),
                    tokens: total_tokens,
                    percentage: 100.0,
                }],
                agents: data
                    .stats
                    .per_provider_agents
                    .get(&prov)
                    .cloned()
                    .unwrap_or_default(),
                models,
            }
        })
        .collect()
}

// ── HTML rendering ─────────────────────────────────────────────

pub fn render_html(data: &ReceiptData) -> String {
    // Group models by provider
    let mut providers_order: Vec<String> = Vec::new();
    let mut by_provider: HashMap<String, Vec<&ModelLineItem>> = HashMap::new();
    for m in &data.models {
        by_provider.entry(m.provider.clone()).or_default().push(m);
        if !providers_order.contains(&m.provider) {
            providers_order.push(m.provider.clone());
        }
    }

    // Build provider sections
    let mut provider_sections = String::new();
    for prov in &providers_order {
        let items = &by_provider[prov];
        let prov_cost: f64 = items.iter().map(|m| m.cost).sum();
        let prov_tokens: u64 = items.iter().map(|m| m.total_tokens).sum();

        provider_sections.push_str(&format!(
            r#"<div class="ph"><span>{prov}</span><span>{tokens} / {cost}</span></div>"#,
            prov = esc(prov),
            tokens = fmt_tok(prov_tokens),
            cost = fmt_cost(prov_cost),
        ));

        for m in items {
            provider_sections.push_str(&format!(
                r#"<div class="li"><span class="mn">{}</span><span class="dots"></span><span class="tk">{}</span><span class="ct">{}</span></div>
"#,
                esc(&m.display_name), fmt_tok(m.total_tokens), fmt_cost(m.cost),
            ));
        }
    }

    let today = Local::now().format("%Y-%m-%d (%a)").to_string();
    let subscription_cost = 200.0f64; // Max plan

    // Model usage percentage breakdown
    let total_cost = data.total;
    let mut model_pct_rows = String::new();
    for m in &data.models {
        if total_cost > 0.0 {
            let pct = m.cost / total_cost * 100.0;
            if pct >= 0.1 {
                model_pct_rows.push_str(&format!(
                    r#"<div class="sl sm"><span>{}</span><span>{:.1}%</span></div>
"#,
                    esc(&m.display_name),
                    pct,
                ));
            }
        }
    }

    // Agent usage percentage breakdown
    let mut agent_pct_rows = String::new();
    for a in &data.agents {
        if a.percentage >= 0.1 {
            agent_pct_rows.push_str(&format!(
                r#"<div class="sl sm"><span>{}</span><span>{:.1}%</span></div>
"#,
                esc(&a.agent),
                a.percentage,
            ));
        }
    }

    // Savings calculation — clamp negative to show "UNDER BUDGET" instead of
    // a misleading "YOU SAVED -$150".
    let raw_savings = data.total - subscription_cost;
    let savings_multiplier = if subscription_cost > 0.0 {
        data.total / subscription_cost
    } else {
        0.0
    };

    format!(
        r##"<!DOCTYPE html>
<html><head><meta charset="utf-8"><style>
*{{margin:0;padding:0;box-sizing:border-box}}
body{{font-family:'Courier New',Courier,monospace;background:transparent;padding:0}}
.r{{width:400px;background:#fefdf8;color:#1a1a1a;padding:20px 16px;font-size:12px;line-height:1.6}}
.hd{{text-align:center;font-size:15px;font-weight:700;letter-spacing:2px;margin-bottom:2px}}
.dt{{text-align:center;font-size:10px;color:#666;margin-bottom:4px}}
.pl{{text-align:center;font-size:11px;color:#444;margin-bottom:4px}}
.pr{{text-align:center;font-size:9px;color:#888}}
.sp{{border:none;border-top:1px dashed #bbb;margin:10px 0;opacity:.6}}
.ds{{border:none;border-top:2px double #bbb;margin:10px 0;opacity:.6}}
.ch{{display:flex;justify-content:space-between;font-size:10px;color:#888;font-weight:700;margin-bottom:4px;letter-spacing:1px}}
.ph{{display:flex;justify-content:space-between;font-size:11px;font-weight:700;color:#7C5CFC;margin:8px 0 4px;padding-bottom:2px;border-bottom:1px solid rgba(124,92,252,.2)}}
.li{{display:flex;align-items:baseline;margin-bottom:3px;font-size:12px}}
.li .mn{{flex-shrink:0;font-weight:600}}
.li .dots{{flex:1;border-bottom:1px dotted #ccc;margin:0 4px;min-width:8px;height:10px}}
.li .tk{{flex-shrink:0;width:70px;text-align:right;color:#555;font-size:11px}}
.li .ct{{flex-shrink:0;width:80px;text-align:right;font-weight:600}}
.sl{{display:flex;justify-content:space-between;font-size:12px;margin-bottom:2px}}
.sl.b{{font-weight:700}}
.sv{{color:#059669;font-weight:600}}
.tl{{display:flex;justify-content:space-between;font-size:15px;font-weight:700;margin:4px 0}}
.ss{{margin-top:6px}}
.st{{font-size:11px;font-weight:700;color:#444;margin-bottom:4px}}
.sl.sm{{font-size:11px;color:#555}}
.nc{{color:#888;font-size:10px}}
.ft{{text-align:center;font-size:10px;color:#888;margin-top:8px}}
.bc{{text-align:center;font-size:14px;letter-spacing:1px;color:#1a1a1a;opacity:.2;margin-top:6px;overflow:hidden;white-space:nowrap}}
.vr{{text-align:center;font-size:8px;color:#bbb;margin-top:4px}}
</style></head><body>
<div class="r">
<div class="hd">{title}</div>
<div class="dt">{today}</div>
<hr class="ds">
<div class="pl">{period_label}</div>
<div class="pr">{period_start} ~ {period_end}</div>
<hr class="sp">
<div class="ch"><span class="cm">MODEL</span><span class="ct">TOKENS</span><span class="cc">API COST</span></div>
{provider_sections}<hr class="sp">
<div class="sl b"><span>SUBTOTAL</span><span>{no_cache_cost}</span></div>
{cache_discount_row}
<hr class="ds">
<div class="tl"><span>API COST</span><span>{api_cost}</span></div>
<hr class="sp">
<div class="sl b"><span>SUBSCRIPTION</span><span>$200</span></div>
{savings_row}
<hr class="ds">
<div class="ss">
<div class="st">MODEL USAGE</div>
{model_pct_rows}</div>
{agent_section}<div class="ss">
<div class="st">STATISTICS</div>
<div class="sl sm"><span>Requests</span><span>{messages}</span></div>
<div class="sl sm"><span>Sessions</span><span>{sessions}</span></div>
</div>
<hr class="sp">
<div class="ft">Thank you for using AgentDesk!</div>
<div class="bc">||||| || ||| || |||| || ||| | |||| ||| ||</div>
<div class="vr">AgentDesk v{version}</div>
</div>
</body></html>"##,
        title = if providers_order.len() == 1 && !providers_order[0].is_empty() {
            format!("{} TOKEN RECEIPT", esc(&providers_order[0]).to_uppercase())
        } else {
            "AI TOKEN RECEIPT".into()
        },
        today = esc(&today),
        period_label = esc(&data.period_label),
        period_start = esc(&data.period_start),
        period_end = esc(&data.period_end),
        provider_sections = provider_sections,
        cache_discount_row = if data.cache_discount > 0.001 {
            format!(
                r#"<div class="sl sv"><span>CACHE DISCOUNT</span><span>-{}</span></div>"#,
                fmt_cost(data.cache_discount)
            )
        } else {
            String::new()
        },
        api_cost = fmt_cost(data.total),
        no_cache_cost = fmt_cost(data.subtotal),
        savings_row = if raw_savings > 0.0 {
            format!(
                r#"<div class="sl sv"><span>YOU SAVED</span><span>{} ({:.0}x)</span></div>"#,
                fmt_cost(raw_savings),
                savings_multiplier,
            )
        } else {
            format!(
                r#"<div class="sl sv"><span>UNDER BUDGET</span><span>{}</span></div>"#,
                fmt_cost(subscription_cost - data.total),
            )
        },
        model_pct_rows = model_pct_rows,
        agent_section = if !agent_pct_rows.is_empty() {
            format!(
                r#"<hr class="sp">
<div class="ss">
<div class="st">AGENT USAGE</div>
{agent_pct_rows}</div>
"#,
                agent_pct_rows = agent_pct_rows,
            )
        } else {
            String::new()
        },
        messages = fmt_num(data.stats.total_messages),
        sessions = fmt_num(data.stats.total_sessions),
        version = env!("CARGO_PKG_VERSION"),
    )
}

// ── Helpers ────────────────────────────────────────────────────

fn fmt_tok(t: u64) -> String {
    if t >= 1_000_000_000 {
        format!("{:.1}B", t as f64 / 1e9)
    } else if t >= 1_000_000 {
        format!("{:.1}M", t as f64 / 1e6)
    } else if t >= 1_000 {
        format!("{:.1}K", t as f64 / 1e3)
    } else {
        t.to_string()
    }
}

fn fmt_cost(c: f64) -> String {
    if c >= 100.0 {
        format!("${:.0}", c)
    } else if c >= 1.0 {
        format!("${:.2}", c)
    } else if c >= 0.01 {
        format!("${:.3}", c)
    } else {
        format!("${:.4}", c)
    }
}

fn fmt_num(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{},{:03}", n / 1000, n % 1000)
    } else {
        n.to_string()
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::io::Write;

    fn write_jsonl(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("create temp jsonl");
        for line in lines {
            writeln!(file, "{line}").expect("write jsonl line");
        }
        file
    }

    #[test]
    fn parse_codex_uses_total_token_usage_deltas_per_turn() {
        let file = write_jsonl(&[
            r#"{"type":"session_meta","payload":{"id":"sess-1","cwd":"/tmp/codex-agent"}}"#,
            r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
            r#"{"timestamp":"2026-04-03T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10000,"cached_input_tokens":8000,"output_tokens":100},"last_token_usage":{"input_tokens":10000,"cached_input_tokens":8000,"output_tokens":100}}}}"#,
            r#"{"timestamp":"2026-04-03T10:00:10Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":26000,"cached_input_tokens":22000,"output_tokens":250},"last_token_usage":{"input_tokens":16000,"cached_input_tokens":14000,"output_tokens":150}}}}"#,
            r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
            r#"{"timestamp":"2026-04-03T10:05:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":38000,"cached_input_tokens":33000,"output_tokens":400},"last_token_usage":{"input_tokens":12000,"cached_input_tokens":11000,"output_tokens":150}}}}"#,
        ]);

        let start = Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).single().unwrap();
        let end = Utc
            .with_ymd_and_hms(2026, 4, 6, 23, 59, 59)
            .single()
            .unwrap();
        let (records, msgs, sid) = parse_codex(file.path(), start, end, &HashMap::new());

        assert_eq!(sid.as_deref(), Some("sess-1"));
        assert_eq!(msgs, 2);
        assert_eq!(records.len(), 2);

        assert_eq!(records[0].model, "gpt-5.4");
        assert_eq!(records[0].input_tokens, 4000);
        assert_eq!(records[0].cache_read_tokens, 22000);
        assert_eq!(records[0].output_tokens, 250);

        assert_eq!(records[1].model, "gpt-5.4");
        assert_eq!(records[1].input_tokens, 1000);
        assert_eq!(records[1].cache_read_tokens, 11000);
        assert_eq!(records[1].output_tokens, 150);
    }

    #[test]
    fn parse_codex_subtracts_pre_window_cumulative_baseline() {
        let file = write_jsonl(&[
            r#"{"type":"session_meta","payload":{"id":"sess-2","cwd":"/tmp/codex-agent"}}"#,
            r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
            r#"{"timestamp":"2026-03-31T23:59:50Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":20000,"cached_input_tokens":16000,"output_tokens":100},"last_token_usage":{"input_tokens":20000,"cached_input_tokens":16000,"output_tokens":100}}}}"#,
            r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
            r#"{"timestamp":"2026-04-01T00:01:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":32000,"cached_input_tokens":25000,"output_tokens":180},"last_token_usage":{"input_tokens":12000,"cached_input_tokens":9000,"output_tokens":80}}}}"#,
        ]);

        let start = Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).single().unwrap();
        let end = Utc
            .with_ymd_and_hms(2026, 4, 6, 23, 59, 59)
            .single()
            .unwrap();
        let (records, msgs, sid) = parse_codex(file.path(), start, end, &HashMap::new());

        assert_eq!(sid.as_deref(), Some("sess-2"));
        assert_eq!(msgs, 1);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].input_tokens, 3000);
        assert_eq!(records[0].cache_read_tokens, 9000);
        assert_eq!(records[0].output_tokens, 80);
    }

    #[test]
    fn build_receipt_data_includes_agent_cache_metrics() {
        let start = Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).single().unwrap();
        let end = Utc.with_ymd_and_hms(2026, 4, 2, 0, 0, 0).single().unwrap();
        let records = vec![
            UsageRecord {
                timestamp: start,
                model: "gpt-5.4".into(),
                input_tokens: 200,
                output_tokens: 80,
                cache_read_tokens: 600,
                cache_creation_tokens: 40,
                provider: "Codex".into(),
                agent: "dash-agent".into(),
                session_id: Some("sess-a".into()),
            },
            UsageRecord {
                timestamp: start + Duration::minutes(5),
                model: "gpt-5.4".into(),
                input_tokens: 100,
                output_tokens: 20,
                cache_read_tokens: 200,
                cache_creation_tokens: 0,
                provider: "Codex".into(),
                agent: "dash-agent".into(),
                session_id: Some("sess-a".into()),
            },
        ];

        let receipt = build_receipt_data(&records, start, end, "Test");
        let agent = receipt
            .agents
            .iter()
            .find(|item| item.agent == "dash-agent")
            .expect("agent share");

        assert_eq!(agent.tokens, 1_240);
        assert_eq!(agent.input_tokens, 300);
        assert_eq!(agent.cache_read_tokens, 800);
        assert_eq!(agent.cache_creation_tokens, 40);
        assert!(agent.cost_without_cache > agent.cost);
    }
}
