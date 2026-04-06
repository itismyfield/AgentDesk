use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};

use super::AppState;

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct GenerateBody {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub issue_numbers: Option<Vec<i64>>,
    pub mode: Option<String>, // "priority-sort" (default), "dependency-aware", "similarity-aware", or "pm-assisted"
    pub parallel: Option<bool>,
    pub max_concurrent_threads: Option<i64>,
    pub max_concurrent_per_agent: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ActivateBody {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub unified_thread: Option<bool>,
    /// Internal-only: continue only already-active runs, never promote generated drafts.
    pub active_only: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct StatusQuery {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReorderBody {
    #[serde(rename = "orderedIds")]
    pub ordered_ids: Vec<String>,
    #[serde(rename = "agentId")]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateRunBody {
    pub status: Option<String>,
    pub unified_thread: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct EnqueueBody {
    pub repo: String,
    pub issue_number: i64,
    pub agent_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResetBody {
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone)]
struct GenerateCandidate {
    card_id: String,
    agent_id: String,
    priority: String,
    description: Option<String>,
    metadata: Option<String>,
    github_issue_number: Option<i64>,
}

#[derive(Debug, Clone)]
struct PlannedEntry {
    card_idx: usize,
    thread_group: i64,
    priority_rank: i64,
    reason: String,
}

#[derive(Debug, Clone)]
struct GroupPlan {
    entries: Vec<PlannedEntry>,
    thread_group_count: i64,
    recommended_parallel_threads: i64,
    dependency_edges: usize,
    similarity_edges: usize,
    path_backed_card_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupKind {
    Independent,
    Similarity,
    Dependency,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenerateMode {
    PrioritySort,
    DependencyAware,
    SimilarityAware,
    PmAssisted,
}

impl GenerateMode {
    fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.unwrap_or("priority-sort") {
            "priority-sort" => Ok(Self::PrioritySort),
            "dependency-aware" => Ok(Self::DependencyAware),
            "similarity-aware" => Ok(Self::SimilarityAware),
            "pm-assisted" => Ok(Self::PmAssisted),
            other => Err(format!(
                "mode must be one of: priority-sort, dependency-aware, similarity-aware, pm-assisted (got {other})"
            )),
        }
    }

    fn uses_similarity(self) -> bool {
        matches!(self, Self::SimilarityAware)
    }

    /// Whether auto-grouping should be used. Requires `parallel=true` explicitly;
    /// `parallel=None` (unspecified) always defaults to sequential to preserve
    /// backward compatibility with existing callers like auto-queue.js.
    fn enables_auto_grouping(self, parallel: Option<bool>) -> bool {
        parallel.unwrap_or(false)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

pub(crate) fn ensure_tables(conn: &rusqlite::Connection) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS auto_queue_runs (
            id          TEXT PRIMARY KEY,
            repo        TEXT,
            agent_id    TEXT,
            status      TEXT DEFAULT 'active',
            ai_model    TEXT,
            ai_rationale TEXT,
            timeout_minutes INTEGER DEFAULT 120,
            unified_thread  INTEGER DEFAULT 0,
            unified_thread_id TEXT,
            unified_thread_channel_id TEXT,
            created_at  DATETIME DEFAULT CURRENT_TIMESTAMP,
            completed_at DATETIME
        );
        CREATE TABLE IF NOT EXISTS auto_queue_entries (
            id              TEXT PRIMARY KEY,
            run_id          TEXT REFERENCES auto_queue_runs(id),
            kanban_card_id  TEXT REFERENCES kanban_cards(id),
            agent_id        TEXT,
            priority_rank   INTEGER DEFAULT 0,
            reason          TEXT,
            status          TEXT DEFAULT 'pending',
            dispatch_id     TEXT,
            created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
            dispatched_at   DATETIME,
            completed_at    DATETIME
        );",
    )
    .ok();
    // #137: upgrade path for existing DBs
    let has_unified: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_runs') WHERE name = 'unified_thread'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if !has_unified {
        conn.execute_batch(
            "ALTER TABLE auto_queue_runs ADD COLUMN unified_thread INTEGER DEFAULT 0;
             ALTER TABLE auto_queue_runs ADD COLUMN unified_thread_id TEXT;
             ALTER TABLE auto_queue_runs ADD COLUMN unified_thread_channel_id TEXT;",
        )
        .ok();
    }
    // #140: thread_group on entries — parallel thread group assignment
    let has_thread_group: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_entries') WHERE name = 'thread_group'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if !has_thread_group {
        conn.execute_batch(
            "ALTER TABLE auto_queue_entries ADD COLUMN thread_group INTEGER DEFAULT 0;",
        )
        .ok();
    }
    // #140: parallel dispatch columns on runs
    let has_max_concurrent: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_runs') WHERE name = 'max_concurrent_threads'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if !has_max_concurrent {
        conn.execute_batch(
            "ALTER TABLE auto_queue_runs ADD COLUMN max_concurrent_threads INTEGER DEFAULT 1;
             ALTER TABLE auto_queue_runs ADD COLUMN max_concurrent_per_agent INTEGER DEFAULT 1;
             ALTER TABLE auto_queue_runs ADD COLUMN thread_group_count INTEGER DEFAULT 1;",
        )
        .ok();
    }

    // #145: dispatch_id on entries — direct dispatch→run association
    let has_dispatch_id: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_entries') WHERE name = 'dispatch_id'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if !has_dispatch_id {
        conn.execute_batch("ALTER TABLE auto_queue_entries ADD COLUMN dispatch_id TEXT;")
            .ok();
        // Backfill dispatch_id for the LATEST entry per card+agent only (#145).
        // Only the most recent entry gets the dispatch_id to avoid stamping the same
        // dispatch_id onto old done entries from previous runs, which would make
        // is_unified_thread_active() ambiguous via LIMIT 1.
        conn.execute_batch(
            "UPDATE auto_queue_entries SET dispatch_id = (
                SELECT td.id FROM task_dispatches td
                WHERE td.kanban_card_id = auto_queue_entries.kanban_card_id
                  AND td.to_agent_id = auto_queue_entries.agent_id
                  AND td.dispatch_type = 'implementation'
                ORDER BY td.created_at DESC LIMIT 1
            )
            WHERE auto_queue_entries.status IN ('dispatched', 'done')
              AND auto_queue_entries.dispatch_id IS NULL
              AND auto_queue_entries.rowid = (
                  SELECT e.rowid FROM auto_queue_entries e
                  WHERE e.kanban_card_id = auto_queue_entries.kanban_card_id
                    AND e.agent_id = auto_queue_entries.agent_id
                    AND e.status IN ('dispatched', 'done')
                  ORDER BY e.created_at DESC LIMIT 1
              );",
        )
        .ok();
    }
}

fn enqueueable_states_for(pipeline: &crate::pipeline::PipelineConfig) -> Vec<String> {
    let mut states: Vec<String> = pipeline
        .dispatchable_states()
        .iter()
        .map(|s| s.to_string())
        .collect();
    // Requested is a pre-execution staging state in the default pipeline. Allow
    // enqueueing it directly so callers can queue already-requested work.
    if pipeline.is_valid_state("requested") && !states.iter().any(|s| s == "requested") {
        states.push("requested".to_string());
    }
    // Ready is an explicit preparation state. Backlog is intentionally excluded:
    // auto-queue should only accept work that has already been prepared.
    if pipeline.is_valid_state("ready") && !states.iter().any(|s| s == "ready") {
        states.push("ready".to_string());
    }
    states
}

fn build_card_filters(
    alias: &str,
    repo: Option<&String>,
    agent_id: Option<&String>,
    issue_numbers: Option<&Vec<i64>>,
) -> (Vec<String>, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let prefix = if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    };
    let mut conditions = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(repo) = repo {
        conditions.push(format!("{}repo_id = ?{}", prefix, params.len() + 1));
        params.push(Box::new(repo.clone()));
    }
    if let Some(agent_id) = agent_id {
        conditions.push(format!(
            "{}assigned_agent_id = ?{}",
            prefix,
            params.len() + 1
        ));
        params.push(Box::new(agent_id.clone()));
    }
    if let Some(issue_numbers) = issue_numbers.filter(|nums| !nums.is_empty()) {
        let base_idx = params.len() + 1;
        let placeholders = issue_numbers
            .iter()
            .enumerate()
            .map(|(idx, _)| format!("?{}", base_idx + idx))
            .collect::<Vec<_>>()
            .join(",");
        conditions.push(format!(
            "{}github_issue_number IN ({})",
            prefix, placeholders
        ));
        for issue_number in issue_numbers {
            params.push(Box::new(*issue_number));
        }
    }

    (conditions, params)
}

fn priority_sort_key(priority: &str) -> i32 {
    match priority {
        "urgent" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

fn extract_dependency_numbers(card: &GenerateCandidate) -> Vec<i64> {
    let mut deps = HashSet::new();
    let sources = [card.description.as_deref(), card.metadata.as_deref()];
    let re = regex::Regex::new(r"#(\d+)").expect("dependency regex must compile");
    for text in sources.into_iter().flatten() {
        for cap in re.captures_iter(text) {
            if let Ok(num) = cap[1].parse::<i64>() {
                if Some(num) != card.github_issue_number {
                    deps.insert(num);
                }
            }
        }
    }
    let mut out: Vec<i64> = deps.into_iter().collect();
    out.sort_unstable();
    out
}

fn normalize_similarity_path(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim_matches(|ch: char| matches!(ch, '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}'))
        .trim_end_matches(|ch: char| matches!(ch, '.' | ',' | ':' | ';'));
    if trimmed.is_empty() || !trimmed.contains('/') {
        return None;
    }
    Some(trimmed.to_string())
}

fn extract_file_paths_from_text(text: &str) -> HashSet<String> {
    let re = regex::Regex::new(
        r"(?:src|dashboard|policies|tests|scripts|docs|crates|migrations|assets|prompts|templates|examples|references)/[A-Za-z0-9_./-]+",
    )
    .expect("file path regex must compile");
    re.find_iter(text)
        .filter_map(|m| normalize_similarity_path(m.as_str()))
        .collect()
}

fn similarity_paths(card: &GenerateCandidate) -> HashSet<String> {
    let description_paths = card
        .description
        .as_deref()
        .map(extract_file_paths_from_text)
        .unwrap_or_default();
    if !description_paths.is_empty() {
        return description_paths;
    }
    card.metadata
        .as_deref()
        .map(extract_file_paths_from_text)
        .unwrap_or_default()
}

fn similarity_edge_allowed(left: &GenerateCandidate, right: &GenerateCandidate) -> bool {
    // Allow cross-agent similarity edges — file overlap determines conflict,
    // not agent assignment. Cards touching the same files should be grouped
    // regardless of which agent they're assigned to.
    !left.agent_id.is_empty() && !right.agent_id.is_empty()
}

/// Compute file-path-based similarity between two sets of extracted paths.
///
/// Each element is a full file path string (e.g. `src/server/routes/auto_queue.rs`)
/// extracted from issue description text by [`extract_file_paths_from_text()`].
/// This is NOT token-level similarity — paths are compared as atomic strings.
///
/// Returns `(shared_count, score)` where score = max(Jaccard, Overlap coefficient):
/// - **Jaccard index**: |intersection| / |union| — penalizes sets of very different sizes.
/// - **Overlap coefficient**: |intersection| / min(|left|, |right|) — captures "subset" overlap.
///   e.g. if issue A touches {X, Y} and issue B touches {X, Z}, overlap = 1/2 = 0.5.
///
/// Using max() ensures that two issues sharing a file are grouped even when their
/// total file counts differ significantly.
fn path_similarity(left: &HashSet<String>, right: &HashSet<String>) -> (usize, f64) {
    if left.is_empty() || right.is_empty() {
        return (0, 0.0);
    }
    let shared = left.intersection(right).count();
    if shared == 0 {
        return (0, 0.0);
    }
    let union = left.union(right).count();
    let overlap = shared as f64 / left.len().min(right.len()) as f64;
    let jaccard = shared as f64 / union as f64;
    (shared, overlap.max(jaccard))
}

fn compact_path_label(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 2 {
        path.to_string()
    } else {
        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    }
}

fn group_path_labels(members: &[usize], paths: &[HashSet<String>]) -> Vec<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for &member in members {
        for path in &paths[member] {
            *counts.entry(path.clone()).or_insert(0) += 1;
        }
    }

    let mut ranked: Vec<(String, usize)> = counts
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
        .into_iter()
        .take(3)
        .map(|(path, _)| compact_path_label(&path))
        .collect()
}

fn build_group_reason(
    kind: GroupKind,
    path_labels: &[String],
    dependency_issue_nums: &[i64],
    member_count: usize,
) -> String {
    let path_suffix = if path_labels.is_empty() {
        String::new()
    } else {
        format!(" [{}]", path_labels.join(", "))
    };
    match kind {
        GroupKind::Mixed => format!(
            "의존성 + 유사도 그룹{} ({}개 카드)",
            path_suffix, member_count
        ),
        GroupKind::Dependency => {
            if dependency_issue_nums.is_empty() {
                format!("의존성 그룹 ({}개 카드)", member_count)
            } else {
                let refs = dependency_issue_nums
                    .iter()
                    .map(|num| format!("#{num}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("의존성 그룹 · 선행 {refs}")
            }
        }
        GroupKind::Similarity => {
            if path_labels.is_empty() {
                format!("유사도 그룹 ({}개 카드)", member_count)
            } else {
                format!("유사도 그룹 [{}]", path_labels.join(", "))
            }
        }
        GroupKind::Independent => "독립 그룹".to_string(),
    }
}

fn build_group_plan(cards: &[GenerateCandidate], enable_similarity: bool) -> GroupPlan {
    const SIMILARITY_THRESHOLD: f64 = 0.5;
    if cards.is_empty() {
        return GroupPlan {
            entries: Vec::new(),
            thread_group_count: 0,
            recommended_parallel_threads: 1,
            dependency_edges: 0,
            similarity_edges: 0,
            path_backed_card_count: 0,
        };
    }

    let mut issue_to_idx: HashMap<i64, usize> = HashMap::new();
    for (idx, card) in cards.iter().enumerate() {
        if let Some(num) = card.github_issue_number {
            issue_to_idx.insert(num, idx);
        }
    }

    let similarity_paths_per_card: Vec<HashSet<String>> = if enable_similarity {
        cards.iter().map(similarity_paths).collect()
    } else {
        vec![HashSet::new(); cards.len()]
    };
    let dependency_numbers: Vec<Vec<i64>> = cards.iter().map(extract_dependency_numbers).collect();
    let path_backed_card_count = similarity_paths_per_card
        .iter()
        .filter(|paths| !paths.is_empty())
        .count();

    let n = cards.len();
    let mut dependency_adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut parent: Vec<usize> = (0..n).collect();
    let mut dependency_edges = 0usize;
    let mut similarity_edges = 0usize;

    fn find(parent: &mut [usize], x: usize) -> usize {
        if parent[x] != x {
            parent[x] = find(parent, parent[x]);
        }
        parent[x]
    }

    fn union(parent: &mut [usize], a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent[rb] = ra;
        }
    }

    for (idx, deps) in dependency_numbers.iter().enumerate() {
        let mut seen = HashSet::new();
        for dep_num in deps {
            if let Some(&dep_idx) = issue_to_idx.get(dep_num) {
                if dep_idx != idx && seen.insert(dep_idx) {
                    dependency_adj[dep_idx].push(idx);
                    union(&mut parent, dep_idx, idx);
                    dependency_edges += 1;
                }
            }
        }
    }

    for left in 0..n {
        for right in (left + 1)..n {
            if !similarity_edge_allowed(&cards[left], &cards[right]) {
                continue;
            }
            let (shared, score) = path_similarity(
                &similarity_paths_per_card[left],
                &similarity_paths_per_card[right],
            );
            if shared == 0 || score < SIMILARITY_THRESHOLD {
                continue;
            }
            union(&mut parent, left, right);
            similarity_edges += 1;
        }
    }

    let mut components: HashMap<usize, Vec<usize>> = HashMap::new();
    for idx in 0..n {
        let root = find(&mut parent, idx);
        components.entry(root).or_default().push(idx);
    }

    let mut component_roots: Vec<usize> = components.keys().copied().collect();
    component_roots
        .sort_by_key(|root| components[root].iter().copied().min().unwrap_or(usize::MAX));

    let mut planned_entries = Vec::with_capacity(n);
    for (group_num, root) in component_roots.iter().enumerate() {
        let mut members = components[root].clone();
        members.sort_by_key(|idx| (priority_sort_key(&cards[*idx].priority), *idx));
        let member_set: HashSet<usize> = members.iter().copied().collect();

        let mut local_in_degree: HashMap<usize, usize> =
            members.iter().map(|idx| (*idx, 0)).collect();
        let mut group_dep_nums = HashSet::new();
        let mut group_dependency_edges = 0usize;
        let mut group_similarity_edges = 0usize;

        for &member in &members {
            for dep_num in &dependency_numbers[member] {
                if let Some(&dep_idx) = issue_to_idx.get(dep_num) {
                    if member_set.contains(&dep_idx) && dep_idx != member {
                        *local_in_degree.entry(member).or_insert(0) += 1;
                        group_dep_nums.insert(*dep_num);
                        group_dependency_edges += 1;
                    }
                }
            }
        }

        for pos in 0..members.len() {
            for next in (pos + 1)..members.len() {
                let left = members[pos];
                let right = members[next];
                if similarity_edge_allowed(&cards[left], &cards[right]) {
                    let (shared, score) = path_similarity(
                        &similarity_paths_per_card[left],
                        &similarity_paths_per_card[right],
                    );
                    if shared > 0 && score >= SIMILARITY_THRESHOLD {
                        group_similarity_edges += 1;
                    }
                }
            }
        }

        let mut available: Vec<usize> = members
            .iter()
            .copied()
            .filter(|member| local_in_degree.get(member).copied().unwrap_or(0) == 0)
            .collect();
        let mut sorted = Vec::with_capacity(members.len());
        while !available.is_empty() {
            available.sort_by_key(|idx| (priority_sort_key(&cards[*idx].priority), *idx));
            let current = available.remove(0);
            sorted.push(current);
            for &next in &dependency_adj[current] {
                if !member_set.contains(&next) {
                    continue;
                }
                if let Some(deg) = local_in_degree.get_mut(&next) {
                    if *deg > 0 {
                        *deg -= 1;
                        if *deg == 0 {
                            available.push(next);
                        }
                    }
                }
            }
        }

        if sorted.len() < members.len() {
            let seen: HashSet<usize> = sorted.iter().copied().collect();
            for member in &members {
                if !seen.contains(member) {
                    sorted.push(*member);
                }
            }
        }

        let path_labels = group_path_labels(&members, &similarity_paths_per_card);
        let mut dep_nums: Vec<i64> = group_dep_nums.into_iter().collect();
        dep_nums.sort_unstable();
        let kind = match (group_dependency_edges > 0, group_similarity_edges > 0) {
            (true, true) => GroupKind::Mixed,
            (true, false) => GroupKind::Dependency,
            (false, true) => GroupKind::Similarity,
            (false, false) => GroupKind::Independent,
        };
        let group_reason = build_group_reason(kind, &path_labels, &dep_nums, members.len());

        for (priority_rank, idx) in sorted.into_iter().enumerate() {
            let mut entry_reason = group_reason.clone();
            let deps_in_queue: Vec<i64> = dependency_numbers[idx]
                .iter()
                .copied()
                .filter(|dep_num| issue_to_idx.contains_key(dep_num))
                .collect();
            if !deps_in_queue.is_empty() {
                let refs = deps_in_queue
                    .iter()
                    .map(|dep_num| format!("#{dep_num}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                entry_reason = format!("{entry_reason} · 선행 {refs}");
            }
            planned_entries.push(PlannedEntry {
                card_idx: idx,
                thread_group: group_num as i64,
                priority_rank: priority_rank as i64,
                reason: entry_reason,
            });
        }
    }

    let distinct_agents = cards
        .iter()
        .filter(|card| !card.agent_id.is_empty())
        .map(|card| card.agent_id.clone())
        .collect::<HashSet<_>>()
        .len()
        .max(1) as i64;
    let thread_group_count = component_roots.len() as i64;
    let recommended_parallel_threads = if thread_group_count <= 1 {
        1
    } else {
        thread_group_count.min(distinct_agents).clamp(1, 4)
    };

    GroupPlan {
        entries: planned_entries,
        thread_group_count,
        recommended_parallel_threads,
        dependency_edges,
        similarity_edges,
        path_backed_card_count,
    }
}

fn entry_to_json(conn: &rusqlite::Connection, entry_id: &str) -> serde_json::Value {
    conn.query_row(
        "SELECT e.id, e.agent_id, e.kanban_card_id, e.priority_rank, e.reason, e.status,
                CAST(strftime('%s', e.created_at) AS INTEGER) * 1000,
                CASE WHEN e.dispatched_at IS NOT NULL THEN CAST(strftime('%s', e.dispatched_at) AS INTEGER) * 1000 END,
                CASE WHEN e.completed_at IS NOT NULL THEN CAST(strftime('%s', e.completed_at) AS INTEGER) * 1000 END,
                kc.title, kc.github_issue_number, kc.github_issue_url,
                COALESCE(e.thread_group, 0)
         FROM auto_queue_entries e
         LEFT JOIN kanban_cards kc ON e.kanban_card_id = kc.id
         WHERE e.id = ?1",
        [entry_id],
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "agent_id": row.get::<_, String>(1)?,
                "card_id": row.get::<_, String>(2)?,
                "priority_rank": row.get::<_, i64>(3)?,
                "reason": row.get::<_, Option<String>>(4)?,
                "status": row.get::<_, String>(5)?,
                "created_at": row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                "dispatched_at": row.get::<_, Option<i64>>(7)?,
                "completed_at": row.get::<_, Option<i64>>(8)?,
                "card_title": row.get::<_, Option<String>>(9)?,
                "github_issue_number": row.get::<_, Option<i64>>(10)?,
                "github_repo": row.get::<_, Option<String>>(11)?,
                "thread_group": row.get::<_, i64>(12)?,
            }))
        },
    )
    .unwrap_or(json!(null))
}

fn run_to_json(conn: &rusqlite::Connection, run_id: &str) -> serde_json::Value {
    conn.query_row(
        "SELECT id, repo, agent_id, status, timeout_minutes,
                ai_model, ai_rationale,
                CAST(strftime('%s', created_at) AS INTEGER) * 1000,
                CASE WHEN completed_at IS NOT NULL THEN CAST(strftime('%s', completed_at) AS INTEGER) * 1000 END,
                unified_thread, unified_thread_id,
                COALESCE(max_concurrent_threads, 1),
                COALESCE(max_concurrent_per_agent, 1),
                COALESCE(thread_group_count, 1)
         FROM auto_queue_runs WHERE id = ?1",
        [run_id],
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "repo": row.get::<_, Option<String>>(1)?,
                "agent_id": row.get::<_, Option<String>>(2)?,
                "status": row.get::<_, String>(3)?,
                "timeout_minutes": row.get::<_, i64>(4)?,
                "ai_model": row.get::<_, Option<String>>(5)?,
                "ai_rationale": row.get::<_, Option<String>>(6)?,
                "created_at": row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                "completed_at": row.get::<_, Option<i64>>(8)?,
                "unified_thread": row.get::<_, i64>(9).unwrap_or(0) != 0,
                "unified_thread_id": row.get::<_, Option<String>>(10)?,
                "max_concurrent_threads": row.get::<_, i64>(11)?,
                "max_concurrent_per_agent": row.get::<_, i64>(12)?,
                "thread_group_count": row.get::<_, i64>(13)?,
            }))
        },
    )
    .unwrap_or(json!(null))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueueEntryOrder {
    id: String,
    status: String,
    agent_id: String,
}

fn reorder_entry_ids(
    entries: &[QueueEntryOrder],
    ordered_ids: &[String],
    agent_id: Option<&str>,
) -> Result<Vec<String>, String> {
    if ordered_ids.is_empty() {
        return Err("orderedIds cannot be empty".to_string());
    }

    let scope_ids: Vec<String> = entries
        .iter()
        .filter(|entry| {
            entry.status == "pending"
                && agent_id
                    .map(|target| entry.agent_id == target)
                    .unwrap_or(true)
        })
        .map(|entry| entry.id.clone())
        .collect();
    if scope_ids.is_empty() {
        return Err("no pending entries found for reorder scope".to_string());
    }

    let scope_set: HashSet<&str> = scope_ids.iter().map(String::as_str).collect();
    let mut seen = HashSet::new();
    let mut replacement_ids = Vec::new();
    for id in ordered_ids {
        let id_str = id.as_str();
        if scope_set.contains(id_str) && seen.insert(id_str) {
            replacement_ids.push(id.clone());
        }
    }
    if replacement_ids.is_empty() {
        return Err("orderedIds do not match any pending entries in scope".to_string());
    }

    for id in &scope_ids {
        if !seen.contains(id.as_str()) {
            replacement_ids.push(id.clone());
        }
    }

    let mut replacement_iter = replacement_ids.into_iter();
    let mut reordered = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.status == "pending"
            && agent_id
                .map(|target| entry.agent_id == target)
                .unwrap_or(true)
        {
            let next_id = replacement_iter
                .next()
                .ok_or_else(|| "replacement sequence exhausted".to_string())?;
            reordered.push(next_id);
        } else {
            reordered.push(entry.id.clone());
        }
    }

    if replacement_iter.next().is_some() {
        return Err("replacement sequence was not fully consumed".to_string());
    }

    Ok(reordered)
}

// ── Endpoints ────────────────────────────────────────────────────────────────

/// POST /api/auto-queue/generate
/// Creates a queue run from ready cards, ordered by priority.
pub async fn generate(
    State(state): State<AppState>,
    Json(body): Json<GenerateBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(issue_numbers) = body.issue_numbers.as_ref().filter(|nums| !nums.is_empty()) {
        let conn = match state.db.separate_conn() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        ensure_tables(&conn);
        let (mut conditions, params) = build_card_filters(
            "kc",
            body.repo.as_ref(),
            body.agent_id.as_ref(),
            Some(issue_numbers),
        );
        conditions.push("kc.status = 'backlog'".to_string());
        let sql = format!(
            "SELECT kc.id, kc.repo_id, kc.assigned_agent_id
             FROM kanban_cards kc
             WHERE {}",
            conditions.join(" AND ")
        );
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = match conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        let backlog_cards: Vec<(String, Option<String>, Option<String>)> = stmt
            .query_map(param_refs.as_slice(), |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .ok()
            .map(|rows| rows.filter_map(|row| row.ok()).collect())
            .unwrap_or_default();
        drop(stmt);
        drop(conn);

        for (card_id, repo_id, assigned_agent_id) in backlog_cards {
            let conn = match state.db.separate_conn() {
                Ok(c) => c,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("{e}")})),
                    );
                }
            };
            crate::pipeline::ensure_loaded();
            let effective = crate::pipeline::resolve_for_card(
                &conn,
                repo_id.as_deref(),
                assigned_agent_id.as_deref(),
            );
            let prep_path = if effective.is_valid_state("ready") {
                effective
                    .free_path_to_state("backlog", "ready")
                    .or_else(|| effective.free_path_to_dispatchable("backlog"))
            } else {
                effective.free_path_to_dispatchable("backlog")
            };
            let Some(path) = prep_path else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": format!("card {card_id} has no free path from backlog to ready/dispatchable state"),
                    })),
                );
            };
            drop(conn);

            for step in &path {
                if let Err(e) = crate::kanban::transition_status_no_hooks(
                    &state.db,
                    &card_id,
                    step,
                    "auto-queue-generate",
                ) {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": format!("failed to auto-transition card {card_id} to {step}: {e}"),
                        })),
                    );
                }
            }
        }
    }

    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    // Build filter — pipeline-driven enqueueable states (dispatchable + prepared staging states)
    crate::pipeline::ensure_loaded();
    let enqueueable = crate::pipeline::try_get()
        .map(|p| {
            enqueueable_states_for(p)
                .iter()
                .map(|s| format!("'{}'", s))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_else(|| "'ready','requested'".to_string());
    let (mut conditions, params) = build_card_filters(
        "kc",
        body.repo.as_ref(),
        body.agent_id.as_ref(),
        body.issue_numbers.as_ref(),
    );
    conditions.insert(0, format!("kc.status IN ({})", enqueueable));

    let where_clause = conditions.join(" AND ");
    let sql = format!(
        "SELECT kc.id, kc.assigned_agent_id, kc.priority, kc.description, kc.metadata, kc.github_issue_number
         FROM kanban_cards kc
         WHERE {where_clause}
         ORDER BY
           CASE kc.priority
             WHEN 'urgent' THEN 0
             WHEN 'high' THEN 1
             WHEN 'medium' THEN 2
             WHEN 'low' THEN 3
             ELSE 4
           END,
           kc.created_at ASC"
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let cards: Vec<GenerateCandidate> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(GenerateCandidate {
                card_id: row.get::<_, String>(0)?,
                agent_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                priority: row
                    .get::<_, Option<String>>(2)?
                    .unwrap_or_else(|| "medium".to_string()),
                description: row.get::<_, Option<String>>(3)?,
                metadata: row.get::<_, Option<String>>(4)?,
                github_issue_number: row.get::<_, Option<i64>>(5)?,
            })
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    if cards.is_empty() {
        // Provide context: how many cards are in backlog vs other statuses
        // Uses the same repo + agent_id filters as the main ready query
        let count_with_filters = |status_val: &str| -> i64 {
            let mut sql = format!(
                "SELECT COUNT(*) FROM kanban_cards WHERE status = '{}'",
                status_val
            );
            let mut count_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            if let Some(ref repo) = body.repo {
                count_params.push(Box::new(repo.clone()));
                sql.push_str(&format!(" AND repo_id = ?{}", count_params.len()));
            }
            if let Some(ref agent_id) = body.agent_id {
                count_params.push(Box::new(agent_id.clone()));
                sql.push_str(&format!(" AND assigned_agent_id = ?{}", count_params.len()));
            }
            let refs: Vec<&dyn rusqlite::types::ToSql> =
                count_params.iter().map(|p| p.as_ref()).collect();
            conn.query_row(&sql, refs.as_slice(), |row| row.get(0))
                .unwrap_or(0)
        };
        // Pipeline-driven counts: report all non-terminal states
        let mut counts_map = serde_json::Map::new();
        if let Some(pipeline) = crate::pipeline::try_get() {
            for state in &pipeline.states {
                if !state.terminal {
                    let c: i64 = count_with_filters(&state.id);
                    counts_map.insert(state.id.clone(), serde_json::json!(c));
                }
            }
        }
        return (
            StatusCode::OK,
            Json(json!({
                "run": null,
                "entries": [],
                "message": "No dispatchable cards found",
                "hint": "Move cards to a dispatchable state before generating a queue.",
                "counts": counts_map,
            })),
        );
    }

    let mode = match GenerateMode::parse(body.mode.as_deref()) {
        Ok(mode) => mode,
        Err(err) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": err })));
        }
    };

    // PM-assisted mode: send card list to PMD for async analysis
    if mode == GenerateMode::PmAssisted {
        // Create pending run
        let run_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, ai_model, ai_rationale) VALUES (?1, ?2, ?3, 'pending', 'pm-assisted', ?4)",
            rusqlite::params![run_id, body.repo, body.agent_id, format!("PMD 분석 대기 중 — {}개 카드 제출", cards.len())],
        ).ok();

        // Collect card info for PMD request
        let mut card_summaries = Vec::new();
        for card in &cards {
            let (title, issue_num): (String, Option<i64>) = conn
                .query_row(
                    "SELECT title, github_issue_number FROM kanban_cards WHERE id = ?1",
                    [&card.card_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap_or_default();
            card_summaries.push(format!(
                "- #{} {} (priority: {}, agent: {})",
                issue_num.unwrap_or(0),
                title,
                card.priority,
                card.agent_id
            ));
        }

        let run_id_for_spawn = run_id.clone();
        let card_list_text = card_summaries.join("\n");
        let repo_name = body.repo.clone().unwrap_or_else(|| "all".to_string());
        drop(stmt);
        let run = run_to_json(&conn, &run_id);
        drop(conn);

        // Async: send PMD request via announce bot
        tokio::spawn(async move {
            let token = match crate::credential::read_bot_token("announce") {
                Some(t) => t,
                None => return,
            };

            // Kanban manager channel from config (kv_meta)
            let km_channel: Option<String> = {
                let conn = state.db.separate_conn().ok();
                conn.and_then(|c| {
                    c.query_row(
                        "SELECT value FROM kv_meta WHERE key = 'kanban_manager_channel_id'",
                        [],
                        |row| row.get(0),
                    )
                    .ok()
                })
            };
            let Some(km_channel) = km_channel else {
                tracing::warn!(
                    "[auto-queue] No kanban_manager_channel_id configured, skipping PM request"
                );
                return;
            };

            // Resolve channel name to ID if needed
            let km_channel_num: u64 = match km_channel.parse() {
                Ok(n) => n,
                Err(_) => {
                    match crate::server::routes::dispatches::resolve_channel_alias_pub(&km_channel)
                    {
                        Some(n) => n,
                        None => return,
                    }
                }
            };

            let message = format!(
                "[칸반매니저] 자동큐 순서 분석 요청\n\n\
                 repo: {}\n\
                 run_id: {}\n\n\
                 아래 일감들의 실행 순서를 분석해주세요.\n\
                 의존관계, 긴급도, 작업 내용을 고려하여 순서를 결정하고,\n\
                 `POST /api/auto-queue/runs/{}/order`에 결과를 전달해주세요.\n\n\
                 {}",
                repo_name, run_id_for_spawn, run_id_for_spawn, card_list_text
            );

            let client = reqwest::Client::new();
            let _ = client
                .post(format!(
                    "https://discord.com/api/v10/channels/{km_channel_num}/messages"
                ))
                .header("Authorization", format!("Bot {}", token))
                .json(&serde_json::json!({"content": message}))
                .send()
                .await;
        });

        return (
            StatusCode::OK,
            Json(json!({
                "run": run,
                "entries": [],
                "message": "PMD 분석 요청 전송됨. 응답 대기 중.",
            })),
        );
    }

    // Dependency-aware mode: filter out cards with incomplete dependencies.
    // SimilarityAware does NOT filter — it uses dependency edges only for
    // grouping/ordering inside build_group_plan() union-find, not exclusion.
    let (filtered_cards, excluded_count) = if mode == GenerateMode::DependencyAware {
        let mut filtered = Vec::new();
        let mut excluded = 0usize;
        for card in &cards {
            let dep_numbers = extract_dependency_numbers(card);
            let mut all_deps_done = true;
            for dep_num in &dep_numbers {
                let dep_status: Option<String> = conn
                    .query_row(
                        "SELECT status FROM kanban_cards WHERE github_issue_number = ?1",
                        [dep_num],
                        |row| row.get(0),
                    )
                    .ok();
                if dep_status.as_deref() != Some("done") {
                    all_deps_done = false;
                    break;
                }
            }

            if all_deps_done {
                filtered.push(card.clone());
            } else {
                excluded += 1;
            }
        }
        (filtered, excluded)
    } else {
        (cards.clone(), 0)
    };

    if filtered_cards.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({
                "run": null,
                "entries": [],
                "message": format!("No cards available ({}개 의존성 미충족으로 제외)", excluded_count)
            })),
        );
    }

    let force_sequential = body.parallel == Some(false);
    let auto_group = !force_sequential && mode.enables_auto_grouping(body.parallel);
    let analyzed_plan =
        auto_group.then(|| build_group_plan(&filtered_cards, mode.uses_similarity()));
    let max_concurrent = match analyzed_plan.as_ref() {
        Some(plan) => body
            .max_concurrent_threads
            .unwrap_or(plan.recommended_parallel_threads)
            .clamp(1, 10)
            .min(plan.thread_group_count.max(1)),
        None => 1,
    };
    let max_per_agent = body.max_concurrent_per_agent.unwrap_or(1).clamp(1, 10);

    let (
        grouped_entries,
        thread_group_count,
        recommended_parallel_threads,
        dependency_edges,
        similarity_edges,
        path_backed_card_count,
    ) = if let Some(plan) = analyzed_plan.as_ref() {
        (
            plan.entries.clone(),
            plan.thread_group_count,
            plan.recommended_parallel_threads,
            plan.dependency_edges,
            plan.similarity_edges,
            plan.path_backed_card_count,
        )
    } else {
        let result: Vec<PlannedEntry> = filtered_cards
            .iter()
            .enumerate()
            .map(|(idx, _)| PlannedEntry {
                card_idx: idx,
                thread_group: 0,
                priority_rank: idx as i64,
                reason: "순차 실행".to_string(),
            })
            .collect();
        (result, 1i64, 1i64, 0usize, 0usize, 0usize)
    };

    let ai_rationale = match (mode, auto_group, force_sequential) {
        (GenerateMode::SimilarityAware, _, true) => format!(
            "파일 경로 유사도 분석을 건너뛰고 순차 실행으로 고정 ({}개 카드)",
            filtered_cards.len()
        ),
        (GenerateMode::DependencyAware, _, true) => format!(
            "의존관계 기반 필터링 후 순차 실행. {}개 큐잉, {}개 의존성 미충족 제외",
            filtered_cards.len(),
            excluded_count
        ),
        (GenerateMode::PrioritySort, _, true) => format!(
            "우선순위 기반 순차 실행 (urgent > high > medium > low), {}개 카드 큐잉",
            filtered_cards.len()
        ),
        (GenerateMode::SimilarityAware, true, false) if path_backed_card_count == 0 => format!(
            "description/metadata에서 파일 경로를 찾지 못해 dependency-only 그룹으로 fallback. 의존성 {}건, {}개 그룹, 추천 병렬 {}개, 적용 {}개",
            dependency_edges, thread_group_count, recommended_parallel_threads, max_concurrent
        ),
        (GenerateMode::SimilarityAware, true, false) => format!(
            "파일 경로 유사도 {}건 + 의존성 {}건으로 {}개 그룹 생성. 파일 경로 추출 카드 {}개, 추천 병렬 {}개, 적용 {}개",
            similarity_edges,
            dependency_edges,
            thread_group_count,
            path_backed_card_count,
            recommended_parallel_threads,
            max_concurrent
        ),
        (GenerateMode::DependencyAware, true, false) => format!(
            "의존관계 기반 필터링 + dependency 그룹 분석. {}개 큐잉, {}개 의존성 미충족 제외, {}개 그룹, 적용 {}개",
            filtered_cards.len(),
            excluded_count,
            thread_group_count,
            max_concurrent
        ),
        (GenerateMode::PrioritySort, true, false) => format!(
            "의존성 기반 그룹 분석. {}개 카드, {}개 그룹, 추천 병렬 {}개, 적용 {}개",
            filtered_cards.len(),
            thread_group_count,
            recommended_parallel_threads,
            max_concurrent
        ),
        (GenerateMode::DependencyAware, false, false) => format!(
            "의존관계 기반 필터링 + 우선순위 정렬. {}개 큐잉, {}개 의존성 미충족 제외",
            filtered_cards.len(),
            excluded_count
        ),
        (GenerateMode::SimilarityAware, false, false) => format!(
            "유사도 분석 모드이지만 parallel 미지정으로 순차 실행. {}개 카드, {}개 의존성 미충족 제외",
            filtered_cards.len(),
            excluded_count
        ),
        (GenerateMode::PrioritySort, false, false) => format!(
            "우선순위 기반 정렬 (urgent > high > medium > low), {}개 카드 큐잉",
            filtered_cards.len()
        ),
        (GenerateMode::PmAssisted, _, _) => unreachable!("pm-assisted handled above"),
    };

    // Create run
    let run_id = uuid::Uuid::new_v4().to_string();
    let ai_model = match (mode, auto_group, force_sequential) {
        (GenerateMode::SimilarityAware, _, true) => "similarity-aware-sequential",
        (GenerateMode::SimilarityAware, true, false) => "similarity-aware-thread-group",
        (GenerateMode::SimilarityAware, false, false) => "similarity-aware-sequential",
        (GenerateMode::DependencyAware, true, false) => "dependency-aware-thread-group",
        (GenerateMode::DependencyAware, _, _) => "dependency-aware-sort",
        (GenerateMode::PrioritySort, true, false) => "parallel-thread-group",
        (GenerateMode::PrioritySort, _, _) => "priority-sort",
        (GenerateMode::PmAssisted, _, _) => unreachable!("pm-assisted handled above"),
    };
    let ai_model_str = ai_model.to_string();
    conn.execute(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status, ai_model, ai_rationale, max_concurrent_threads, max_concurrent_per_agent, thread_group_count) \
         VALUES (?1, ?2, ?3, 'generated', ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![run_id, body.repo, body.agent_id, ai_model_str, ai_rationale, max_concurrent, max_per_agent, thread_group_count],
    )
    .ok();

    // Create entries
    let mut entries = Vec::new();
    for planned in &grouped_entries {
        let card = &filtered_cards[planned.card_idx];
        let entry_id = uuid::Uuid::new_v4().to_string();
        let agent = if card.agent_id.is_empty() {
            body.agent_id.as_deref().unwrap_or("")
        } else {
            card.agent_id.as_str()
        };
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, priority_rank, thread_group, reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                entry_id,
                run_id,
                card.card_id,
                agent,
                planned.priority_rank,
                planned.thread_group,
                planned.reason
            ],
        )
        .ok();
        entries.push(entry_to_json(&conn, &entry_id));
    }

    let run = run_to_json(&conn, &run_id);

    (
        StatusCode::OK,
        Json(json!({ "run": run, "entries": entries })),
    )
}

/// POST /api/auto-queue/activate
/// Dispatches the next pending entry in the active run.
pub async fn activate(
    State(state): State<AppState>,
    Json(body): Json<ActivateBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    let active_only = body.active_only.unwrap_or(false);
    // Internal recovery paths must continue only active runs. Manual activation
    // may opt into promoting the latest generated draft.
    let mut run_filter = if active_only {
        "status = 'active'".to_string()
    } else {
        "status IN ('active', 'generated', 'pending')".to_string()
    };
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(ref repo) = body.repo {
        run_filter.push_str(&format!(
            " AND (repo = ?{} OR repo IS NULL OR repo = '')",
            params.len() + 1
        ));
        params.push(Box::new(repo.clone()));
    }
    if let Some(ref agent_id) = body.agent_id {
        run_filter.push_str(&format!(
            " AND (agent_id = ?{} OR agent_id IS NULL OR agent_id = '')",
            params.len() + 1
        ));
        params.push(Box::new(agent_id.clone()));
    }

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let run_id: Option<String> = conn
        .query_row(
            &format!(
                "SELECT id FROM auto_queue_runs WHERE {run_filter} ORDER BY created_at DESC LIMIT 1"
            ),
            param_refs.as_slice(),
            |row| row.get(0),
        )
        .ok();

    let Some(run_id) = run_id else {
        return (
            StatusCode::OK,
            Json(json!({ "dispatched": [], "count": 0, "message": "No active run" })),
        );
    };

    if !active_only {
        // Promote pending/generated → active on explicit activation.
        conn.execute(
            "UPDATE auto_queue_runs SET status = 'active' WHERE id = ?1 AND status IN ('generated', 'pending')",
            [&run_id],
        )
        .ok();
    }

    // #137: Apply unified_thread toggle if provided
    if let Some(unified) = body.unified_thread {
        conn.execute(
            "UPDATE auto_queue_runs SET unified_thread = ?1 WHERE id = ?2",
            rusqlite::params![unified as i32, run_id],
        )
        .ok();
    }

    // Stale empty run cleanup: after generate()/enqueue() fixes, normal paths never
    // leave an active run with 0 entries.  Any such run is legacy corruption — complete
    // it immediately instead of auto-populating with unrelated ready cards (#85).
    let entry_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = ?1",
            [&run_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    if entry_count == 0 {
        conn.execute(
            "UPDATE auto_queue_runs SET status = 'completed', completed_at = datetime('now') WHERE id = ?1",
            [&run_id],
        ).ok();
        tracing::info!(
            "[auto-queue] Completed stale empty run {run_id} — no entries, skipping fallback populate (#85)"
        );
        return (
            StatusCode::OK,
            Json(
                json!({ "dispatched": [], "count": 0, "message": "Stale empty run completed — no entries to dispatch" }),
            ),
        );
    }

    // #179: When agent_id is known, apply cross-run in-flight guard to prevent
    // double-dispatch. Respects repo filter to avoid cross-repo dispatch.
    let effective_agent: Option<String> = body.agent_id.clone();

    // Build card-level repo condition for agent-scoped queries (#179).
    // Uses kanban_cards.repo_id instead of auto_queue_runs.repo to handle
    // mixed global runs (repo=NULL) that contain cards from multiple repos.
    let card_repo_condition = if body.repo.is_some() {
        " AND kc.repo_id = ?2"
    } else {
        ""
    };

    if let Some(ref agt) = effective_agent {
        // In-flight guard: any dispatched entry for this agent across active runs,
        // filtered by card's actual repo_id (not run-level repo).
        let inflight_query = format!(
            "SELECT COUNT(*) > 0 FROM auto_queue_entries e \
             JOIN auto_queue_runs r ON e.run_id = r.id \
             JOIN kanban_cards kc ON e.kanban_card_id = kc.id \
             WHERE e.agent_id = ?1 AND e.status = 'dispatched' AND r.status = 'active'{}",
            card_repo_condition
        );
        let has_inflight: bool = if let Some(ref repo) = body.repo {
            conn.query_row(&inflight_query, rusqlite::params![agt, repo], |row| {
                row.get(0)
            })
        } else {
            conn.query_row(&inflight_query, rusqlite::params![agt], |row| row.get(0))
        }
        .unwrap_or(false);
        if has_inflight {
            tracing::info!(
                "[auto-queue] Skipping activate: agent {agt} already has a dispatched entry in-flight"
            );
            return (
                StatusCode::OK,
                Json(
                    json!({ "dispatched": [], "count": 0, "message": "Already has in-flight entry" }),
                ),
            );
        }
    }

    // #140: Read run parallel config
    let (max_concurrent, max_per_agent, _thread_group_count): (i64, i64, i64) = conn
        .query_row(
            "SELECT COALESCE(max_concurrent_threads, 1), COALESCE(max_concurrent_per_agent, 1), COALESCE(thread_group_count, 1) FROM auto_queue_runs WHERE id = ?1",
            [&run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap_or((1, 1, 1));

    // Count currently active groups (groups with at least one 'dispatched' entry)
    let active_groups: Vec<i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT COALESCE(thread_group, 0) FROM auto_queue_entries \
                 WHERE run_id = ?1 AND status = 'dispatched'",
            )
            .unwrap();
        stmt.query_map([&run_id], |row| row.get::<_, i64>(0))
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    };

    // Count per-agent active dispatches (across all groups in this run)
    let mut agent_dispatch_counts: HashMap<String, i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT agent_id, COUNT(*) FROM auto_queue_entries \
                 WHERE run_id = ?1 AND status = 'dispatched' GROUP BY agent_id",
            )
            .unwrap();
        stmt.query_map([&run_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    };

    let active_group_count = active_groups.len() as i64;

    // Find pending groups not currently active, ordered by group number
    let pending_groups: Vec<i64> = {
        let active_set: HashSet<i64> = active_groups.iter().copied().collect();
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT COALESCE(thread_group, 0) FROM auto_queue_entries \
                 WHERE run_id = ?1 AND status = 'pending' \
                 ORDER BY thread_group ASC",
            )
            .unwrap();
        stmt.query_map([&run_id], |row| row.get::<_, i64>(0))
            .ok()
            .map(|rows| {
                rows.filter_map(|r| r.ok())
                    .filter(|g| !active_set.contains(g))
                    .collect()
            })
            .unwrap_or_default()
    };

    drop(conn);

    let mut dispatched = Vec::new();
    let mut groups_to_dispatch: Vec<i64> = Vec::new();

    // Also dispatch next entry for active groups that have pending entries
    // (continuation within same group after prior entry completed)
    {
        let conn = state.db.separate_conn().unwrap();
        for &grp in &active_groups {
            let has_pending: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM auto_queue_entries \
                     WHERE run_id = ?1 AND COALESCE(thread_group, 0) = ?2 AND status = 'pending'",
                    rusqlite::params![run_id, grp],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            let has_dispatched: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM auto_queue_entries \
                     WHERE run_id = ?1 AND COALESCE(thread_group, 0) = ?2 AND status = 'dispatched'",
                    rusqlite::params![run_id, grp],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            // Only add group if it has pending entries AND no currently dispatched entries
            // (sequential within group)
            if has_pending && !has_dispatched {
                groups_to_dispatch.push(grp);
            }
        }
    }

    // Add new groups from available slots (dynamic — check remaining capacity)
    for &grp in &pending_groups {
        if (active_group_count + groups_to_dispatch.len() as i64) >= max_concurrent {
            break;
        }
        if !groups_to_dispatch.contains(&grp) {
            groups_to_dispatch.push(grp);
        }
    }

    for group in &groups_to_dispatch {
        // Get first pending entry in this group
        let conn = state.db.separate_conn().unwrap();
        let entry: Option<(String, String, String)> = conn
            .query_row(
                "SELECT e.id, e.kanban_card_id, e.agent_id \
                 FROM auto_queue_entries e \
                 WHERE e.run_id = ?1 AND COALESCE(e.thread_group, 0) = ?2 AND e.status = 'pending' \
                 ORDER BY e.priority_rank ASC LIMIT 1",
                rusqlite::params![run_id, group],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .ok();
        drop(conn);

        let Some((entry_id, card_id, agent_id)) = entry else {
            continue;
        };

        // Per-agent concurrency guard (#140)
        let current_agent_count = agent_dispatch_counts.get(&agent_id).copied().unwrap_or(0);
        if current_agent_count >= max_per_agent {
            tracing::info!(
                "[auto-queue] Skipping group {group} for {agent_id}: at max_concurrent_per_agent ({max_per_agent})"
            );
            continue;
        }

        // Busy-agent guard (#110): skip if agent has active cards outside auto-queue.
        // Exclude the card being dispatched (#162) and cards that belong to the
        // same auto-queue run — those work in isolated worktrees so parallel
        // execution is safe.
        let conn = state.db.separate_conn().unwrap();
        let busy: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM kanban_cards \
                 WHERE assigned_agent_id = ?1 AND status IN ('requested', 'in_progress', 'review') \
                 AND id != ?2 \
                 AND id NOT IN (SELECT kanban_card_id FROM auto_queue_entries WHERE run_id = ?3)",
                rusqlite::params![agent_id, card_id, run_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        drop(conn);

        if busy {
            tracing::info!(
                "[auto-queue] Skipping activate for {agent_id}: agent has active cards outside auto-queue"
            );
            continue;
        }

        // #162: If card is in a non-dispatchable state (e.g. backlog, requested),
        // walk it through free transitions to the dispatchable state using the
        // canonical reducer path (preserves ApplyClock, AuditLog, SyncReviewState)
        // but without firing policy hooks that would create side-dispatches.
        {
            let conn = state.db.separate_conn().unwrap();
            let card_status: String = conn
                .query_row(
                    "SELECT status FROM kanban_cards WHERE id = ?1",
                    [&card_id],
                    |row| row.get(0),
                )
                .unwrap_or_default();
            let (card_repo_id, card_assigned_agent_id): (Option<String>, Option<String>) = conn
                .query_row(
                    "SELECT repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
                    [&card_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap_or_default();
            crate::pipeline::ensure_loaded();
            let effective = crate::pipeline::resolve_for_card(
                &conn,
                card_repo_id.as_deref(),
                card_assigned_agent_id.as_deref(),
            );
            drop(conn);
            if let Some(path) = effective.free_path_to_dispatchable(&card_status) {
                tracing::info!(
                    "[auto-queue] Silent walk: card {} from '{}' through {:?} (canonical reducer, no hooks)",
                    card_id,
                    card_status,
                    path
                );
                let mut walk_failed = false;
                for step in &path {
                    if let Err(e) = crate::kanban::transition_status_no_hooks(
                        &state.db,
                        &card_id,
                        step,
                        "auto-queue-walk",
                    ) {
                        tracing::warn!(
                            "[auto-queue] Silent walk failed for card {} at step '{}': {e}",
                            card_id,
                            step
                        );
                        walk_failed = true;
                        break;
                    }
                }
                if walk_failed {
                    continue;
                }
            }
        }

        // Get card title
        let conn = state.db.separate_conn().unwrap();
        let title: String = conn
            .query_row(
                "SELECT title FROM kanban_cards WHERE id = ?1",
                [&card_id],
                |row| row.get(0),
            )
            .unwrap_or_else(|_| "Dispatch".to_string());
        drop(conn);

        // Create dispatch
        let dispatch_result = tokio::task::block_in_place(|| {
            crate::dispatch::create_dispatch(
                &state.db,
                &state.engine,
                &card_id,
                &agent_id,
                "implementation",
                &title,
                &json!({"auto_queue": true, "entry_id": entry_id, "thread_group": group}),
            )
        });

        if dispatch_result.is_err() {
            tracing::error!(
                "[auto-queue] create_dispatch failed for entry {entry_id} (group {group}), leaving as pending for retry"
            );
            continue;
        }

        // Mark entry with dispatch_id (#145)
        let dispatch_id = dispatch_result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let conn = state.db.separate_conn().unwrap();
        conn.execute(
            "UPDATE auto_queue_entries SET status = 'dispatched', dispatch_id = ?1, dispatched_at = datetime('now') WHERE id = ?2",
            rusqlite::params![dispatch_id, entry_id],
        )
        .ok();
        drop(conn);

        // #140: Update local per-agent count so subsequent iterations respect max_concurrent_per_agent
        *agent_dispatch_counts.entry(agent_id.clone()).or_insert(0) += 1;

        let conn = state.db.separate_conn().unwrap();
        dispatched.push(entry_to_json(&conn, &entry_id));
        drop(conn);
    }

    // Check if all entries are done — include 'dispatched' to avoid premature run completion (#179)
    let conn = state.db.separate_conn().unwrap();
    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = ?1 AND status IN ('pending', 'dispatched')",
            [&run_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    if remaining == 0 {
        let still_dispatched: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = ?1 AND status = 'dispatched'",
                [&run_id],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if still_dispatched == 0 {
            conn.execute(
                "UPDATE auto_queue_runs SET status = 'completed', completed_at = datetime('now') WHERE id = ?1",
                [&run_id],
            )
            .ok();
        }
    }

    // Build response with group info
    let active_group_count = {
        let mut stmt = conn
            .prepare(
                "SELECT COUNT(DISTINCT COALESCE(thread_group, 0)) FROM auto_queue_entries \
                 WHERE run_id = ?1 AND status = 'dispatched'",
            )
            .unwrap();
        stmt.query_row([&run_id], |row| row.get::<_, i64>(0))
            .unwrap_or(0)
    };
    let pending_group_count = {
        let mut stmt = conn
            .prepare(
                "SELECT COUNT(DISTINCT COALESCE(thread_group, 0)) FROM auto_queue_entries \
                 WHERE run_id = ?1 AND status = 'pending'",
            )
            .unwrap();
        stmt.query_row([&run_id], |row| row.get::<_, i64>(0))
            .unwrap_or(0)
    };

    (
        StatusCode::OK,
        Json(json!({
            "dispatched": dispatched,
            "count": dispatched.len(),
            "active_groups": active_group_count,
            "pending_groups": pending_group_count,
        })),
    )
}

/// GET /api/auto-queue/status
pub async fn status(
    State(state): State<AppState>,
    Query(query): Query<StatusQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    // Find latest run (NULL agent_id/repo matches any filter)
    let mut run_filter = "1=1".to_string();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(ref repo) = query.repo {
        run_filter.push_str(&format!(
            " AND (repo = ?{} OR repo IS NULL OR repo = '')",
            params.len() + 1
        ));
        params.push(Box::new(repo.clone()));
    }
    if let Some(ref agent_id) = query.agent_id {
        run_filter.push_str(&format!(
            " AND (agent_id = ?{} OR agent_id IS NULL OR agent_id = '')",
            params.len() + 1
        ));
        params.push(Box::new(agent_id.clone()));
    }

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let run_id: Option<String> = conn
        .query_row(
            &format!(
                "SELECT id FROM auto_queue_runs WHERE {run_filter} ORDER BY created_at DESC LIMIT 1"
            ),
            param_refs.as_slice(),
            |row| row.get(0),
        )
        .ok();

    let Some(run_id) = run_id else {
        return (
            StatusCode::OK,
            Json(json!({ "run": null, "entries": [], "agents": {} })),
        );
    };

    let run = run_to_json(&conn, &run_id);

    // Get entries (filtered by agent_id and repo if specified)
    let entry_ids: Vec<String> = {
        let mut entry_sql = String::from(
            "SELECT e.id FROM auto_queue_entries e \
             LEFT JOIN kanban_cards kc ON e.kanban_card_id = kc.id \
             WHERE e.run_id = ?1",
        );
        let mut entry_params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(run_id.clone())];
        if let Some(ref agent_id) = query.agent_id {
            entry_sql.push_str(&format!(" AND e.agent_id = ?{}", entry_params.len() + 1));
            entry_params.push(Box::new(agent_id.clone()));
        }
        if let Some(ref repo) = query.repo {
            entry_sql.push_str(&format!(" AND kc.repo_id = ?{}", entry_params.len() + 1));
            entry_params.push(Box::new(repo.clone()));
        }
        entry_sql.push_str(" ORDER BY e.priority_rank ASC");

        let entry_refs: Vec<&dyn rusqlite::types::ToSql> =
            entry_params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&entry_sql).unwrap();
        stmt.query_map(entry_refs.as_slice(), |row| row.get::<_, String>(0))
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    };

    let entries: Vec<serde_json::Value> = entry_ids
        .iter()
        .map(|id| entry_to_json(&conn, id))
        .collect();

    // Agent summary
    let mut agents: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    for entry in &entries {
        let agent = entry["agent_id"].as_str().unwrap_or("unknown").to_string();
        let entry_status = entry["status"].as_str().unwrap_or("pending");
        let counter = agents
            .entry(agent)
            .or_insert_with(|| json!({"pending": 0, "dispatched": 0, "done": 0, "skipped": 0}));
        if let Some(obj) = counter.as_object_mut() {
            if let Some(val) = obj.get_mut(entry_status) {
                *val = json!(val.as_i64().unwrap_or(0) + 1);
            }
        }
    }

    // #140: Thread group summary
    let mut thread_groups: std::collections::HashMap<i64, serde_json::Value> =
        std::collections::HashMap::new();
    for entry in &entries {
        let group = entry["thread_group"].as_i64().unwrap_or(0);
        let entry_status = entry["status"].as_str().unwrap_or("pending");
        let counter = thread_groups.entry(group).or_insert_with(|| {
            json!({
                "pending": 0,
                "dispatched": 0,
                "done": 0,
                "skipped": 0,
                "entries": [],
                "reason": serde_json::Value::Null,
            })
        });
        if let Some(obj) = counter.as_object_mut() {
            if let Some(val) = obj.get_mut(entry_status) {
                *val = json!(val.as_i64().unwrap_or(0) + 1);
            }
            if obj
                .get("reason")
                .and_then(|value| value.as_str())
                .map(|value| value.is_empty())
                .unwrap_or(true)
            {
                if let Some(reason) = entry["reason"].as_str() {
                    obj.insert("reason".to_string(), json!(reason));
                }
            }
            if let Some(arr) = obj.get_mut("entries").and_then(|v| v.as_array_mut()) {
                arr.push(json!({
                    "id": entry["id"],
                    "card_id": entry["card_id"],
                    "status": entry_status,
                    "github_issue_number": entry["github_issue_number"],
                }));
            }
        }
    }

    // Determine group-level statuses
    let mut group_statuses: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    for (group_num, summary) in &thread_groups {
        let dispatched_count = summary["dispatched"].as_i64().unwrap_or(0);
        let pending_count = summary["pending"].as_i64().unwrap_or(0);
        let group_status = if dispatched_count > 0 {
            "active"
        } else if pending_count > 0 {
            "pending"
        } else {
            "done"
        };
        let mut group_obj = summary.clone();
        group_obj["status"] = json!(group_status);
        group_statuses.insert(group_num.to_string(), group_obj);
    }

    (
        StatusCode::OK,
        Json(json!({
            "run": run,
            "entries": entries,
            "agents": agents,
            "thread_groups": group_statuses,
        })),
    )
}

/// PATCH /api/auto-queue/entries/{id}/skip
pub async fn skip_entry(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    let changed = conn
        .execute(
            "UPDATE auto_queue_entries SET status = 'skipped', completed_at = datetime('now') WHERE id = ?1 AND status = 'pending'",
            [&id],
        )
        .unwrap_or(0);

    if changed == 0 {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "entry not found or not pending"})),
        );
    }

    (StatusCode::OK, Json(json!({ "ok": true })))
}

/// PATCH /api/auto-queue/runs/{id}
pub async fn update_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateRunBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    let mut changed = 0usize;

    if let Some(ref status) = body.status {
        let completed_at = if status == "completed" {
            "datetime('now')"
        } else {
            "NULL"
        };
        changed += conn
            .execute(
                &format!(
                    "UPDATE auto_queue_runs SET status = ?1, completed_at = {completed_at} WHERE id = ?2"
                ),
                rusqlite::params![status, id],
            )
            .unwrap_or(0);
    }

    if let Some(unified) = body.unified_thread {
        changed += conn
            .execute(
                "UPDATE auto_queue_runs SET unified_thread = ?1 WHERE id = ?2",
                rusqlite::params![unified as i32, id],
            )
            .unwrap_or(0);
    }

    if changed == 0 && body.status.is_none() && body.unified_thread.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no fields to update"})),
        );
    }

    (StatusCode::OK, Json(json!({ "ok": true })))
}

/// POST /api/auto-queue/reset
/// Clear all entries and complete all non-terminal runs.
pub async fn reset(
    State(state): State<AppState>,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    let body: ResetBody = if body.is_empty() {
        ResetBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid reset body: {error}")})),
                );
            }
        }
    };

    let agent_id = body
        .agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let (deleted_entries, completed_runs, protected_active_runs, warning) = if let Some(agent_id) =
        agent_id
    {
        let deleted_entries = conn
            .execute(
                "DELETE FROM auto_queue_entries WHERE agent_id = ?1",
                rusqlite::params![agent_id],
            )
            .unwrap_or(0);
        let completed_runs = conn
                .execute(
                    "UPDATE auto_queue_runs SET status = 'completed', completed_at = datetime('now') \
                     WHERE status IN ('generated', 'pending', 'active', 'paused') AND agent_id = ?1",
                    rusqlite::params![agent_id],
                )
                .unwrap_or(0);
        (deleted_entries, completed_runs, 0usize, None)
    } else {
        let protected_active_runs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_runs WHERE status = 'active'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if protected_active_runs > 0 {
            tracing::warn!(
                "[auto-queue] Global reset requested without agent_id; preserving {protected_active_runs} active run(s)"
            );
        } else {
            tracing::warn!(
                "[auto-queue] Global reset requested without agent_id; applying unscoped reset"
            );
        }

        let deleted_entries = if protected_active_runs > 0 {
            conn.execute(
                "DELETE FROM auto_queue_entries \
                     WHERE run_id IS NULL \
                        OR run_id NOT IN (SELECT id FROM auto_queue_runs WHERE status = 'active')",
                [],
            )
            .unwrap_or(0)
        } else {
            conn.execute("DELETE FROM auto_queue_entries", [])
                .unwrap_or(0)
        };
        let completed_runs = if protected_active_runs > 0 {
            conn.execute(
                "UPDATE auto_queue_runs SET status = 'completed', completed_at = datetime('now') \
                     WHERE status IN ('generated', 'pending', 'paused')",
                [],
            )
            .unwrap_or(0)
        } else {
            conn.execute(
                "UPDATE auto_queue_runs SET status = 'completed', completed_at = datetime('now') \
                     WHERE status IN ('generated', 'pending', 'active', 'paused')",
                [],
            )
            .unwrap_or(0)
        };
        let warning = (protected_active_runs > 0).then(|| {
                format!(
                    "global reset preserved {protected_active_runs} active run(s); use agent_id to reset a specific queue"
                )
            });
        (
            deleted_entries,
            completed_runs,
            protected_active_runs as usize,
            warning,
        )
    };

    let mut response = json!({
        "ok": true,
        "deleted_entries": deleted_entries,
        "completed_runs": completed_runs,
        "protected_active_runs": protected_active_runs,
    });
    if let Some(warning) = warning {
        response["warning"] = json!(warning);
    }
    (StatusCode::OK, Json(response))
}

/// POST /api/auto-queue/pause — pause all active runs
pub async fn pause(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);
    let paused = conn
        .execute(
            "UPDATE auto_queue_runs SET status = 'paused' WHERE status = 'active'",
            [],
        )
        .unwrap_or(0);
    (
        StatusCode::OK,
        Json(json!({"ok": true, "paused_runs": paused})),
    )
}

/// POST /api/auto-queue/resume — resume paused runs and dispatch next entry
pub async fn resume_run(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);
    let resumed = conn
        .execute(
            "UPDATE auto_queue_runs SET status = 'active' WHERE status = 'paused'",
            [],
        )
        .unwrap_or(0);
    drop(conn);

    // Trigger dispatch of next pending entry
    if resumed > 0 {
        let (_status, body) = activate(
            State(state),
            Json(ActivateBody {
                repo: None,
                agent_id: None,
                unified_thread: None,
                active_only: Some(true),
            }),
        )
        .await;
        let dispatched = body.0.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        return (
            StatusCode::OK,
            Json(json!({"ok": true, "resumed_runs": resumed, "dispatched": dispatched})),
        );
    }

    (
        StatusCode::OK,
        Json(json!({"ok": true, "resumed_runs": 0, "message": "No paused runs"})),
    )
}

/// POST /api/auto-queue/cancel — cancel all active/paused runs and pending entries
pub async fn cancel(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);
    let cancelled_entries = conn
        .execute(
            "UPDATE auto_queue_entries SET status = 'skipped' WHERE status IN ('pending', 'dispatched')",
            [],
        )
        .unwrap_or(0);
    let cancelled_runs = conn
        .execute(
            "UPDATE auto_queue_runs SET status = 'cancelled', completed_at = datetime('now') WHERE status IN ('active', 'paused')",
            [],
        )
        .unwrap_or(0);
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "cancelled_entries": cancelled_entries,
            "cancelled_runs": cancelled_runs,
        })),
    )
}

/// PATCH /api/auto-queue/reorder
pub async fn reorder(
    State(state): State<AppState>,
    Json(body): Json<ReorderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    let run_id = body.ordered_ids.iter().find_map(|id| {
        conn.query_row(
            "SELECT run_id FROM auto_queue_entries WHERE id = ?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .ok()
    });
    let Some(run_id) = run_id else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no matching queue entries found"})),
        );
    };

    let current_entries: Vec<QueueEntryOrder> = {
        let mut stmt = match conn.prepare(
            "SELECT id, COALESCE(status, 'pending'), COALESCE(agent_id, '')
             FROM auto_queue_entries
             WHERE run_id = ?1
             ORDER BY priority_rank ASC, created_at ASC, id ASC",
        ) {
            Ok(stmt) => stmt,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        stmt.query_map([&run_id], |row| {
            Ok(QueueEntryOrder {
                id: row.get(0)?,
                status: row.get(1)?,
                agent_id: row.get(2)?,
            })
        })
        .ok()
        .map(|rows| rows.filter_map(|row| row.ok()).collect())
        .unwrap_or_default()
    };

    let reordered_ids = match reorder_entry_ids(
        &current_entries,
        &body.ordered_ids,
        body.agent_id.as_deref(),
    ) {
        Ok(ids) => ids,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": error })));
        }
    };

    let tx = match conn.transaction() {
        Ok(tx) => tx,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    for (rank, id) in reordered_ids.iter().enumerate() {
        if let Err(e) = tx.execute(
            "UPDATE auto_queue_entries SET priority_rank = ?1 WHERE id = ?2",
            rusqlite::params![rank as i64, id],
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    }

    if let Err(e) = tx.commit() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        );
    }

    (StatusCode::OK, Json(json!({ "ok": true })))
}

/// POST /api/auto-queue/enqueue
pub async fn enqueue(
    State(state): State<AppState>,
    Json(body): Json<EnqueueBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    // Resolve agent_id
    let agent_id = match body.agent_id {
        Some(ref id) if !id.is_empty() => id.clone(),
        _ => match conn.query_row(
            "SELECT default_agent_id FROM github_repos WHERE full_name = ?1",
            [&body.repo],
            |row| row.get::<_, Option<String>>(0),
        ) {
            Ok(Some(id)) if !id.is_empty() => id,
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "no agent_id provided and repo has no default_agent_id"})),
                );
            }
        },
    };

    // Find or create kanban card
    let card_id: Option<String> = conn
        .query_row(
            "SELECT id FROM kanban_cards WHERE github_issue_number = ?1 AND repo_id = ?2",
            rusqlite::params![body.issue_number, body.repo],
            |row| row.get(0),
        )
        .ok();

    let card_id = match card_id {
        Some(id) => id,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "kanban card not found for this issue"})),
            );
        }
    };

    let (card_status, card_repo_id, card_assigned_agent_id): (
        String,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT status, repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
            [&card_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap_or_default();

    // Complete stale active/pending runs that no longer have actionable entries.
    // A finished run must not silently absorb new work just because its status
    // was left behind as active/pending.
    conn.execute(
        "UPDATE auto_queue_runs
         SET status = 'completed',
             completed_at = COALESCE(completed_at, datetime('now'))
         WHERE id IN (
             SELECT r.id
             FROM auto_queue_runs r
             WHERE r.status IN ('active', 'pending')
               AND (r.repo = ?1 OR r.repo IS NULL)
               AND (r.agent_id = ?2 OR r.agent_id IS NULL)
               AND NOT EXISTS (
                   SELECT 1
                   FROM auto_queue_entries e
                   WHERE e.run_id = r.id
                     AND e.status IN ('pending', 'dispatched')
               )
         )",
        rusqlite::params![body.repo, agent_id],
    )
    .ok();

    // Find existing live active/pending run (do NOT create yet — preserves idempotent retry)
    let existing_run_id: Option<String> = conn
        .query_row(
            "SELECT r.id
             FROM auto_queue_runs r
             WHERE r.status IN ('active', 'pending')
               AND (r.repo = ?1 OR r.repo IS NULL)
               AND (r.agent_id = ?2 OR r.agent_id IS NULL)
               AND EXISTS (
                   SELECT 1
                   FROM auto_queue_entries e
                   WHERE e.run_id = r.id
                     AND e.status IN ('pending', 'dispatched')
               )
             ORDER BY r.created_at DESC
             LIMIT 1",
            rusqlite::params![body.repo, agent_id],
            |row| row.get(0),
        )
        .ok();

    // Check if already in queue (idempotent retry) — must run BEFORE status validation
    if let Some(ref rid) = existing_run_id {
        let already: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = ?1 AND kanban_card_id = ?2 AND status = 'pending'",
                rusqlite::params![rid, card_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;

        if already {
            return (
                StatusCode::OK,
                Json(
                    json!({"ok": true, "card_id": card_id, "agent_id": agent_id, "already_queued": true}),
                ),
            );
        }
    }

    // Never enqueue a card that already has an active dispatch. Previously the
    // caller force-transitioned such cards to ready first; with direct enqueue,
    // we must keep that guard here.
    let has_active_dispatch: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM task_dispatches \
             WHERE kanban_card_id = ?1 AND status IN ('pending', 'dispatched')",
            [&card_id],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if has_active_dispatch {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "card already has an active dispatch; cannot enqueue duplicate work",
                "card_id": card_id,
                "status": card_status,
            })),
        );
    }

    // Accept only prepared staging states directly so PMD does not need
    // redundant state nudges before enqueueing work.
    crate::pipeline::ensure_loaded();
    let effective_pipeline = crate::pipeline::resolve_for_card(
        &conn,
        card_repo_id.as_deref(),
        card_assigned_agent_id.as_deref(),
    );
    let enqueueable_states = enqueueable_states_for(&effective_pipeline);
    if !enqueueable_states.iter().any(|s| s == &card_status) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("card status is '{}', only ready/requested/dispatchable states can be enqueued", card_status),
                "card_id": card_id,
                "status": card_status,
                "allowed_states": enqueueable_states,
            })),
        );
    }

    // Enqueue only appends to an already-live run. Creating a brand-new pending
    // run here is confusing because nothing dispatches until a separate activate.
    // Reject instead so callers explicitly generate/activate a queue first.
    let run_id = match existing_run_id {
        Some(id) => id,
        None => {
            let last_run: Option<(String, String)> = conn
                .query_row(
                    "SELECT id, status
                     FROM auto_queue_runs
                     WHERE (repo = ?1 OR repo IS NULL)
                       AND (agent_id = ?2 OR agent_id IS NULL)
                     ORDER BY created_at DESC
                     LIMIT 1",
                    rusqlite::params![body.repo, agent_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .ok();
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "no live active/pending auto-queue run available; completed runs cannot accept enqueue. Generate or activate a queue first",
                    "card_id": card_id,
                    "agent_id": agent_id,
                    "last_run_id": last_run.as_ref().map(|(id, _)| id.clone()),
                    "last_run_status": last_run.as_ref().map(|(_, status)| status.clone()),
                })),
            );
        }
    };

    let entry_id = uuid::Uuid::new_v4().to_string();
    let max_rank: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(priority_rank), -1) FROM auto_queue_entries WHERE run_id = ?1",
            [&run_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    conn.execute(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, priority_rank)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![entry_id, run_id, card_id, agent_id, max_rank + 1],
    )
    .ok();

    (
        StatusCode::OK,
        Json(json!({"ok": true, "card_id": card_id, "agent_id": agent_id})),
    )
}

// ── PM-assisted callback ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct OrderBody {
    /// Ordered list of GitHub issue numbers (or card IDs)
    pub order: Vec<serde_json::Value>,
    pub rationale: Option<String>,
    /// Alias for rationale (compatibility)
    pub reasoning: Option<String>,
}

/// POST /api/auto-queue/runs/:id/order
/// Callback from PMD: provides the ordered card list for a pending run.
pub async fn submit_order(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Json(body): Json<OrderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    // Verify run exists and is pending, get repo for filtering
    let run_info: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT status, repo FROM auto_queue_runs WHERE id = ?1",
            [&run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    match run_info.as_ref().map(|(s, _)| s.as_str()) {
        Some("pending") => {}
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "run not found or not pending"})),
            );
        }
    }
    let run_repo = run_info.as_ref().and_then(|(_, r)| r.clone());

    // Create entries from the ordered list
    let mut created = 0;
    for (rank, item) in body.order.iter().enumerate() {
        // Item can be issue number (i64) or card_id (string)
        // When matching by issue number, filter by repo to prevent cross-repo collisions
        let card_id: Option<String> = if let Some(num) = item.as_i64() {
            if let Some(ref repo) = run_repo {
                conn.query_row(
                    "SELECT id FROM kanban_cards WHERE github_issue_number = ?1 AND repo_id = ?2",
                    rusqlite::params![num, repo],
                    |row| row.get(0),
                )
                .ok()
            } else {
                conn.query_row(
                    "SELECT id FROM kanban_cards WHERE github_issue_number = ?1",
                    [num],
                    |row| row.get(0),
                )
                .ok()
            }
        } else if let Some(id) = item.as_str() {
            Some(id.to_string())
        } else {
            None
        };

        let Some(card_id) = card_id else { continue };

        // Only enqueue cards in dispatchable states (pipeline-driven)
        let card_status: String = conn
            .query_row(
                "SELECT COALESCE(status, '') FROM kanban_cards WHERE id = ?1",
                [&card_id],
                |row| row.get(0),
            )
            .unwrap_or_default();
        let dispatchable_check = crate::pipeline::try_get()
            .map(|p| p.dispatchable_states().iter().any(|s| *s == card_status))
            .unwrap_or(card_status == "ready");
        if !dispatchable_check {
            tracing::info!(
                "[auto-queue] Skipping card {card_id} (status={card_status}, not dispatchable)"
            );
            continue;
        }

        let agent_id: String = conn
            .query_row(
                "SELECT COALESCE(assigned_agent_id, '') FROM kanban_cards WHERE id = ?1",
                [&card_id],
                |row| row.get(0),
            )
            .unwrap_or_default();

        let entry_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, priority_rank)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![entry_id, run_id, card_id, agent_id, rank as i64],
        )
        .ok();
        created += 1;
    }

    // Only activate if at least one card was enqueued; otherwise leave as pending
    // to prevent the activate() fallback from filling the run with unintended cards
    let rationale = body
        .rationale
        .as_deref()
        .or(body.reasoning.as_deref())
        .unwrap_or("PMD 분석 완료");
    if created > 0 {
        conn.execute(
            "UPDATE auto_queue_runs SET status = 'active', ai_rationale = ?1 WHERE id = ?2",
            rusqlite::params![rationale, run_id],
        )
        .ok();
    } else {
        tracing::warn!(
            "[auto-queue] submit_order: no ready cards enqueued, run {run_id} stays pending"
        );
        conn.execute(
            "UPDATE auto_queue_runs SET status = 'completed', ai_rationale = ?1 WHERE id = ?2",
            rusqlite::params![
                format!("{rationale} (no ready cards — auto-completed)"),
                run_id
            ],
        )
        .ok();
    }

    // Queue created and activated — dispatch is a separate step via POST /api/auto-queue/activate
    // This allows PMD to review/adjust the order before dispatching begins.
    drop(conn);

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "created": created,
            "run_id": run_id,
            "message": "Queue active. Call POST /api/auto-queue/activate to start dispatching.",
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::{QueueEntryOrder, reorder_entry_ids};

    fn entry(id: &str, status: &str, agent_id: &str) -> QueueEntryOrder {
        QueueEntryOrder {
            id: id.to_string(),
            status: status.to_string(),
            agent_id: agent_id.to_string(),
        }
    }

    #[test]
    fn reorder_entry_ids_reorders_only_pending_entries_in_scope() {
        let entries = vec![
            entry("done-a", "done", "agent-a"),
            entry("a-1", "pending", "agent-a"),
            entry("b-1", "pending", "agent-b"),
            entry("a-2", "pending", "agent-a"),
            entry("done-b", "done", "agent-b"),
        ];

        let reordered = reorder_entry_ids(
            &entries,
            &["a-2".to_string(), "a-1".to_string()],
            Some("agent-a"),
        )
        .expect("agent reorder should succeed");

        assert_eq!(
            reordered,
            vec![
                "done-a".to_string(),
                "a-2".to_string(),
                "b-1".to_string(),
                "a-1".to_string(),
                "done-b".to_string(),
            ]
        );
    }

    #[test]
    fn reorder_entry_ids_filters_non_pending_ids_from_legacy_payloads() {
        let entries = vec![
            entry("done-a", "done", "agent-a"),
            entry("p-1", "pending", "agent-a"),
            entry("p-2", "pending", "agent-a"),
            entry("done-b", "done", "agent-a"),
        ];

        let reordered = reorder_entry_ids(
            &entries,
            &[
                "done-a".to_string(),
                "p-2".to_string(),
                "p-1".to_string(),
                "done-b".to_string(),
            ],
            None,
        )
        .expect("legacy payload should still reorder pending entries");

        assert_eq!(
            reordered,
            vec![
                "done-a".to_string(),
                "p-2".to_string(),
                "p-1".to_string(),
                "done-b".to_string(),
            ]
        );
    }
}
