async fn sync_run_group_metadata_with_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<(), String> {
    let thread_group_count = sqlx::query_scalar::<_, i64>(
        "SELECT GREATEST(
                COALESCE(COUNT(DISTINCT COALESCE(thread_group, 0)), 0),
                1
            )::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1",
    )
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|err| format!("count thread groups for run {run_id}: {err}"))?;

    sqlx::query(
        "UPDATE auto_queue_runs
         SET thread_group_count = $1,
             max_concurrent_threads = $1
         WHERE id = $2",
    )
    .bind(thread_group_count)
    .bind(run_id)
    .execute(&mut **tx)
    .await
    .map_err(|err| format!("sync run group metadata for {run_id}: {err}"))?;
    Ok(())
}

async fn enqueue_entries_into_existing_run_with_pg(
    pool: &sqlx::PgPool,
    run_id: &str,
    requested_entries: &[GenerateEntryBody],
    cards_by_issue: &HashMap<i64, ResolvedDispatchCard>,
) -> Result<Vec<AddedRunEntry>, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|err| format!("begin enqueue transaction: {err}"))?;

    let existing_live_cards: HashSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT kanban_card_id
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(run_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(|err| format!("query existing queued cards: {err}"))?
    .into_iter()
    .collect();

    let mut next_rank_by_group = HashMap::new();
    for row in sqlx::query(
        "SELECT COALESCE(thread_group, 0)::BIGINT AS thread_group,
                (COALESCE(MAX(priority_rank), -1) + 1)::BIGINT AS next_priority_rank
         FROM auto_queue_entries
         WHERE run_id = $1
         GROUP BY COALESCE(thread_group, 0)",
    )
    .bind(run_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(|err| format!("query group ranks: {err}"))?
    {
        let thread_group: i64 = row
            .try_get("thread_group")
            .map_err(|err| format!("decode thread_group: {err}"))?;
        let next_priority_rank: i64 = row
            .try_get("next_priority_rank")
            .map_err(|err| format!("decode next_priority_rank: {err}"))?;
        next_rank_by_group.insert(thread_group, next_priority_rank);
    }

    let mut next_auto_group = sqlx::query_scalar::<_, i64>(
        "SELECT (COALESCE(MAX(COALESCE(thread_group, 0)), -1) + 1)::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1",
    )
    .bind(run_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|err| format!("query next thread group: {err}"))?;

    let mut existing_live_cards = existing_live_cards;
    let mut inserted = Vec::new();

    for entry in requested_entries {
        let Some(card) = cards_by_issue.get(&entry.issue_number) else {
            continue;
        };
        if existing_live_cards.contains(&card.card_id) {
            return Err(format!(
                "issue #{} is already queued in run {run_id}",
                entry.issue_number
            ));
        }

        let has_active_dispatch = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND status IN ('pending', 'dispatched')",
        )
        .bind(&card.card_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|err| format!("query active dispatches for {}: {err}", card.card_id))?;
        if has_active_dispatch > 0 {
            return Err(format!(
                "issue #{} already has an active dispatch and cannot be queued again",
                entry.issue_number
            ));
        }

        let thread_group = entry.thread_group.unwrap_or_else(|| {
            let chosen = next_auto_group;
            next_auto_group += 1;
            chosen
        });
        let priority_rank = *next_rank_by_group.entry(thread_group).or_insert(0);
        next_rank_by_group.insert(thread_group, priority_rank + 1);
        let entry_id = uuid::Uuid::new_v4().to_string();

        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, priority_rank, thread_group, batch_phase, reason
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8
             )",
        )
        .bind(&entry_id)
        .bind(run_id)
        .bind(&card.card_id)
        .bind(card.assigned_agent_id.as_deref().unwrap_or(""))
        .bind(priority_rank)
        .bind(thread_group)
        .bind(entry.batch_phase.unwrap_or(0))
        .bind(format!(
            "manual run entry add for issue #{}",
            entry.issue_number
        ))
        .execute(&mut *tx)
        .await
        .map_err(|err| format!("insert auto-queue entry: {err}"))?;

        existing_live_cards.insert(card.card_id.clone());
        inserted.push(AddedRunEntry {
            entry_id,
            thread_group,
            priority_rank,
        });
    }

    if !inserted.is_empty() {
        sync_run_group_metadata_with_pg_tx(&mut tx, run_id).await?;
    }

    tx.commit()
        .await
        .map_err(|err| format!("commit enqueue transaction: {err}"))?;
    Ok(inserted)
}

fn existing_live_run_conflict_response(
    run_id: &str,
    status: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": format!(
                "live auto-queue run already exists: run_id={run_id}, status={status}; pass force=true to cancel it before creating a new run"
            ),
            "existing_run_id": run_id,
            "existing_run_status": status,
        })),
    )
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

fn priority_sort_key(priority: &str) -> i32 {
    match priority {
        "urgent" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

fn planning_sort_key(card: &GenerateCandidate, idx: usize) -> (i32, usize) {
    (priority_sort_key(&card.priority), idx)
}

fn dependency_issue_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"#(\d+)").expect("dependency regex must compile"))
}

fn dependency_section_header_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)^\s*(?:#{1,6}\s*)?(dependencies?|dependency|depends on|선행 작업|선행작업|의존성)\s*:?\s*$",
        )
        .expect("dependency section regex must compile")
    })
}

fn dependency_inline_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)^\s*(?:[-*]\s*)?(?:#{1,6}\s*)?(dependencies?|dependency|depends on|선행 작업|선행작업|의존성)\s*:?\s+(.+)$",
        )
        .expect("dependency inline regex must compile")
    })
}

fn markdown_header_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^#{1,6}\s").expect("markdown header regex must compile"))
}

fn bare_dependency_list_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^\s*#\d+(?:[\s,]+#\d+)*\s*$")
            .expect("dependency bare-list regex must compile")
    })
}

fn insert_dependency_number(deps: &mut HashSet<i64>, self_issue_number: Option<i64>, num: i64) {
    if Some(num) != self_issue_number {
        deps.insert(num);
    }
}

fn collect_dependency_numbers_from_issue_refs(
    text: &str,
    deps: &mut HashSet<i64>,
    self_issue_number: Option<i64>,
) -> bool {
    let mut matched = false;
    for cap in dependency_issue_regex().captures_iter(text) {
        if let Ok(num) = cap[1].parse::<i64>() {
            matched = true;
            insert_dependency_number(deps, self_issue_number, num);
        }
    }
    matched
}

fn collect_dependency_numbers_from_json_value(
    value: &Value,
    deps: &mut HashSet<i64>,
    self_issue_number: Option<i64>,
) -> bool {
    match value {
        Value::Number(num) => num
            .as_i64()
            .map(|issue_number| {
                insert_dependency_number(deps, self_issue_number, issue_number);
                true
            })
            .unwrap_or(false),
        Value::String(raw) => {
            let trimmed = raw.trim();
            let mut matched =
                collect_dependency_numbers_from_issue_refs(trimmed, deps, self_issue_number);
            if let Ok(issue_number) = trimmed.trim_start_matches('#').parse::<i64>() {
                insert_dependency_number(deps, self_issue_number, issue_number);
                matched = true;
            }
            matched
        }
        Value::Array(items) => {
            let mut matched = false;
            for item in items {
                matched |=
                    collect_dependency_numbers_from_json_value(item, deps, self_issue_number);
            }
            matched
        }
        _ => false,
    }
}

fn extract_dependency_numbers_from_text(
    text: &str,
    source_label: &str,
    allow_bare_ref_list: bool,
    deps: &mut HashSet<i64>,
    signals: &mut HashSet<String>,
    self_issue_number: Option<i64>,
) {
    let trimmed = text.trim();
    if allow_bare_ref_list && bare_dependency_list_regex().is_match(trimmed) {
        if collect_dependency_numbers_from_issue_refs(trimmed, deps, self_issue_number) {
            signals.insert(format!("{source_label}:bare-list"));
        }
        return;
    }

    let mut active_section: Option<String> = None;
    for line in text.lines() {
        let trimmed_line = line.trim();
        if trimmed_line.is_empty() {
            continue;
        }

        if dependency_section_header_regex().is_match(trimmed_line) {
            active_section = Some(trimmed_line.to_string());
            continue;
        }

        if active_section.is_some() && markdown_header_regex().is_match(trimmed_line) {
            active_section = None;
        }

        if let Some(caps) = dependency_inline_regex().captures(trimmed_line) {
            let signal = format!("{source_label}:inline:{}", caps[1].trim().to_lowercase());
            if let Some(rest) = caps.get(2) {
                if collect_dependency_numbers_from_issue_refs(
                    rest.as_str(),
                    deps,
                    self_issue_number,
                ) {
                    signals.insert(signal);
                }
            }
            continue;
        }

        if let Some(section_label) = active_section.as_ref() {
            if collect_dependency_numbers_from_issue_refs(trimmed_line, deps, self_issue_number) {
                signals.insert(format!("{source_label}:section:{section_label}"));
            }
        }
    }
}

fn extract_dependency_parse_result(card: &GenerateCandidate) -> DependencyParseResult {
    let mut deps = HashSet::new();
    let mut signals = HashSet::new();

    if let Some(description) = card.description.as_deref() {
        extract_dependency_numbers_from_text(
            description,
            "description",
            false,
            &mut deps,
            &mut signals,
            card.github_issue_number,
        );
    }

    if let Some(metadata) = card.metadata.as_deref() {
        if let Ok(value) = serde_json::from_str::<Value>(metadata) {
            if let Some(object) = value.as_object() {
                for (key, field_value) in object {
                    if key.eq_ignore_ascii_case("depends_on")
                        || key.eq_ignore_ascii_case("dependencies")
                    {
                        if collect_dependency_numbers_from_json_value(
                            field_value,
                            &mut deps,
                            card.github_issue_number,
                        ) {
                            signals.insert(format!("metadata:json:{key}"));
                        }
                    }
                }
            }
        } else {
            extract_dependency_numbers_from_text(
                metadata,
                "metadata",
                true,
                &mut deps,
                &mut signals,
                card.github_issue_number,
            );
        }
    }

    let mut numbers: Vec<i64> = deps.into_iter().collect();
    numbers.sort_unstable();
    let mut signals: Vec<String> = signals.into_iter().collect();
    signals.sort();

    DependencyParseResult { numbers, signals }
}

fn extract_dependency_numbers(card: &GenerateCandidate) -> Vec<i64> {
    extract_dependency_parse_result(card).numbers
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

fn build_group_plan(cards: &[GenerateCandidate]) -> GroupPlan {
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

    let similarity_paths_per_card: Vec<HashSet<String>> =
        cards.iter().map(similarity_paths).collect();
    let dependency_numbers: Vec<Vec<i64>> = cards.iter().map(extract_dependency_numbers).collect();
    let path_backed_card_count = similarity_paths_per_card
        .iter()
        .filter(|paths| !paths.is_empty())
        .count();

    let n = cards.len();
    let mut dependency_adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut dependency_predecessors: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut similarity_conflicts: Vec<HashSet<usize>> = vec![HashSet::new(); n];
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
                    dependency_predecessors[idx].push(dep_idx);
                    union(&mut parent, dep_idx, idx);
                    dependency_edges += 1;
                }
            }
        }
    }

    let dependency_roots: Vec<usize> = (0..n).map(|idx| find(&mut parent, idx)).collect();

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
            similarity_edges += 1;
            if dependency_roots[left] != dependency_roots[right] {
                similarity_conflicts[left].insert(right);
                similarity_conflicts[right].insert(left);
            }
        }
    }

    let mut components: HashMap<usize, Vec<usize>> = HashMap::new();
    for idx in 0..n {
        let root = dependency_roots[idx];
        components.entry(root).or_default().push(idx);
    }

    let mut component_roots: Vec<usize> = components.keys().copied().collect();
    component_roots
        .sort_by_key(|root| components[root].iter().copied().min().unwrap_or(usize::MAX));

    let mut planned_entries = Vec::with_capacity(n);
    for (group_num, root) in component_roots.iter().enumerate() {
        let mut members = components[root].clone();
        members.sort_by_key(|idx| planning_sort_key(&cards[*idx], *idx));
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
            available.sort_by_key(|idx| planning_sort_key(&cards[*idx], *idx));
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
                batch_phase: 0,
                reason: entry_reason,
            });
        }
    }

    let mut global_in_degree: Vec<usize> = dependency_predecessors
        .iter()
        .map(|preds| preds.len())
        .collect();
    let mut ready: Vec<usize> = (0..n).filter(|idx| global_in_degree[*idx] == 0).collect();
    let mut dependency_order = Vec::with_capacity(n);
    let mut emitted = vec![false; n];

    while !ready.is_empty() {
        ready.sort_by_key(|idx| planning_sort_key(&cards[*idx], *idx));
        let current = ready.remove(0);
        if emitted[current] {
            continue;
        }
        emitted[current] = true;
        dependency_order.push(current);
        for &next in &dependency_adj[current] {
            if global_in_degree[next] > 0 {
                global_in_degree[next] -= 1;
                if global_in_degree[next] == 0 {
                    ready.push(next);
                }
            }
        }
    }

    if dependency_order.len() < n {
        let mut remaining: Vec<usize> = (0..n).filter(|idx| !emitted[*idx]).collect();
        remaining.sort_by_key(|idx| planning_sort_key(&cards[*idx], *idx));
        dependency_order.extend(remaining);
    }

    let mut batch_phase_by_idx = vec![0i64; n];
    let mut phase_assigned = vec![false; n];
    for idx in dependency_order {
        let earliest_phase = dependency_predecessors[idx]
            .iter()
            .copied()
            .filter(|pred| phase_assigned[*pred])
            .map(|pred| batch_phase_by_idx[pred] + 1)
            .max()
            .unwrap_or(0);
        let mut batch_phase = earliest_phase;
        while similarity_conflicts[idx]
            .iter()
            .copied()
            .filter(|other| phase_assigned[*other])
            .any(|other| batch_phase_by_idx[other] == batch_phase)
        {
            batch_phase += 1;
        }
        batch_phase_by_idx[idx] = batch_phase;
        phase_assigned[idx] = true;
    }

    for planned in &mut planned_entries {
        planned.batch_phase = batch_phase_by_idx[planned.card_idx];
    }

    let thread_group_count = component_roots.len() as i64;
    let recommended_parallel_threads = if thread_group_count <= 1 {
        1
    } else {
        thread_group_count.clamp(1, 4)
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

