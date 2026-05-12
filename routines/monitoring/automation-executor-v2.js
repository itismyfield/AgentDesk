// Automation Executor v2
//
// Consumes kanban_ready observations (pipeline_stage_id='automation-candidate', status='ready')
// and drives an autoresearch-style iteration loop per card:
//
//   ready → requested → in_progress → [metric-regression re-queue OR review → done]
//
// Re-queue on metric regression: current card → "review", new child card → "ready"
// (intermediate cards go to "review", NOT "done", to avoid premature kanban_dispatched suppression)
//
// Loop ends when MAX_ITERATIONS is reached or final gate triggers.
// LLM submits iteration results via POST /api/automation-candidates/{card_id}/iteration-result.
// Rust computes keep/discard verdict deterministically.

const MAX_ITERATIONS = 10;
const DISPATCH_RETRY_MS = 30 * 60 * 1000;
const MAX_DISPATCH_RETRIES = 3;
const DISPATCHED_WINDOW_DAYS = 7;
const CHECKPOINT_VERSION = 2;
const AUTOMATION_CANDIDATE_CONTRACT = Object.freeze({
  pipelineStageId: "automation-candidate",
  programKey: "program",
  requiredProgramFields: Object.freeze([
    "repo_dir",
    "allowed_write_paths",
    "metric_name",
    "metric_target",
  ]),
});

// --- Checkpoint helpers ---

function emptyCheckpoint() {
  return {
    version: CHECKPOINT_VERSION,
    // card_id -> { dispatched_at, iteration, status }
    dispatched: {},
    // card_id -> { attempt_count, last_attempted_at, first_attempted_at }
    pending: {},
    stats: { ticks: 0, dispatched: 0, skipped: 0, max_iterations_reached: 0 },
  };
}

function loadCheckpoint(raw) {
  if (!raw || raw.version !== CHECKPOINT_VERSION) return emptyCheckpoint();
  const cp = Object.assign(emptyCheckpoint(), raw);
  cp.dispatched = raw.dispatched || {};
  cp.pending    = raw.pending || {};
  cp.stats      = Object.assign(emptyCheckpoint().stats, raw.stats || {});
  return cp;
}

function nowIso(now) {
  return typeof now === "string" ? now : now.toISOString ? now.toISOString() : String(now);
}

function isRecent(iso, nowStr, maxMs) {
  const t = new Date(iso || "").getTime();
  return Number.isFinite(t) && new Date(nowStr).getTime() - t < maxMs;
}

function pruneDispatched(cp, nowStr) {
  const cutoff = new Date(nowStr).getTime() - DISPATCHED_WINDOW_DAYS * 86400 * 1000;
  for (const [id, entry] of Object.entries(cp.dispatched)) {
    if (new Date(entry.dispatched_at || 0).getTime() < cutoff) delete cp.dispatched[id];
  }
  for (const [id, entry] of Object.entries(cp.pending)) {
    if (new Date(entry.first_attempted_at || 0).getTime() < cutoff) delete cp.pending[id];
  }
}

function hasCompleteProgramContract(program) {
  const [repoDirField, allowedPathsField, metricNameField, metricTargetField] =
    AUTOMATION_CANDIDATE_CONTRACT.requiredProgramFields;
  return typeof program[repoDirField] === "string"
    && program[repoDirField].trim().length > 0
    && Array.isArray(program[allowedPathsField])
    && program[allowedPathsField].length > 0
    && program[allowedPathsField].every((path) => typeof path === "string" && path.trim().length > 0)
    && typeof program[metricNameField] === "string"
    && program[metricNameField].trim().length > 0
    && program[metricTargetField] != null;
}

function isAutomationCandidateCard(obs) {
  const program = (obs.metadata || {})[AUTOMATION_CANDIDATE_CONTRACT.programKey] || {};
  return obs.pipeline_stage_id === AUTOMATION_CANDIDATE_CONTRACT.pipelineStageId
    && hasCompleteProgramContract(program);
}

function candidateIterationBudget(program) {
  const value = Number(program.iteration_budget || MAX_ITERATIONS);
  if (!Number.isFinite(value)) return MAX_ITERATIONS;
  return Math.max(1, Math.min(MAX_ITERATIONS, Math.floor(value)));
}

function previousIterationsFor(inventory, cardId) {
  if (!inventory) return [];
  if (Array.isArray(inventory)) return inventory;
  if (Array.isArray(inventory[cardId])) return inventory[cardId];
  if (inventory[cardId] && Array.isArray(inventory[cardId].iterations)) {
    return inventory[cardId].iterations;
  }
  if (inventory.card_id === cardId && Array.isArray(inventory.iterations)) {
    return inventory.iterations;
  }
  return [];
}

// --- Build executor prompt (autoresearch-style: program contract + previous findings) ---

function buildIterationPrompt(cardId, card, iteration, previousIterations) {
  const program = (card.metadata && card.metadata.program) || {};
  const allowedPaths = (program.allowed_write_paths || []).join(", ") || "(not specified)";
  const metricName   = program.metric_name || "improvement_score";
  const metricDirection = program.metric_direction || program.direction || "lower_is_better";
  const metricTarget = program.metric_target != null ? String(program.metric_target) : "(not specified)";
  const iterBudget   = candidateIterationBudget(program);
  const description  = program.description || card.title || "(no description)";

  const prevSummary = previousIterations.length === 0
    ? "(이전 반복 없음 — 첫 번째 시도입니다)"
    : previousIterations.map((r, i) =>
        `  반복 ${r.iteration}: ${r.status} | ${metricName}: ${r.metric_before} → ${r.metric_after} | ${r.description || ""}`
      ).join("\n");

  return [
    "## 자동화 후보 반복 실행 요청 (Automation Executor v2)",
    "",
    `**카드 ID**: \`${cardId}\``,
    `**제목**: ${card.title || "(no title)"}`,
    `**반복 번호**: ${iteration} / ${iterBudget}`,
    "",
    "### Program Contract",
    `- **목표**: ${description}`,
    `- **수정 허용 경로**: \`${allowedPaths}\``,
    `- **지표명**: ${metricName}`,
    `- **지표 방향**: ${metricDirection}`,
    `- **목표값**: ${metricTarget}`,
    "",
    "### 이전 반복 결과",
    prevSummary,
    "",
    "### 실행 지침",
    "",
    "1. **먼저 아래 API로 격리된 git worktree를 준비**하고, 응답의 `path`에서만 작업:",
    "",
    "```",
    `POST /api/automation-candidates/${cardId}/prepare-worktree`,
    "Content-Type: application/json",
    "",
    JSON.stringify({ iteration }, null, 2),
    "```",
    "",
    "2. **allowed_write_paths 내에서만** 코드 수정",
    "3. 지표 측정 (변경 전/후)",
    "4. **반드시 아래 API 호출로 결과 제출** (다른 방법으로 상태 변경 금지):",
    "",
    "```",
    `POST /api/automation-candidates/${cardId}/iteration-result`,
    "Content-Type: application/json",
    "",
    JSON.stringify({
      iteration,
      branch: `automation/${cardId}/iter-${iteration}`,
      commit_hash: "<커밋 해시>",
      metric_before: "<변경 전 수치>",
      metric_after:  "<변경 후 수치>",
      is_simplification: false,
      status: "keep",
      description: "<변경 요약>",
      allowed_write_paths_used: program.allowed_write_paths || [],
      run_seconds: "<소요 초>",
      crash_trace: null,
    }, null, 2),
    "```",
    "",
    "**주의**: `lower_is_better`에서는 `metric_after < metric_before`, `higher_is_better`에서는 `metric_after > metric_before`인 경우에만 Rust가 `keep` 판정합니다.",
    "**주의**: allowed_write_paths 외 경로 수정 시 API가 403을 반환합니다.",
    "**주의**: 이 API 호출 없이 카드 상태를 직접 변경하지 마세요.",
  ].join("\n");
}

// --- Main tick ---

agentdesk.routines.register({
  name: "Automation Executor v2",

  tick(ctx) {
    const nowStr = nowIso(ctx.now);
    const cp = loadCheckpoint(ctx.checkpoint);
    const observations = ctx.observations || [];

    pruneDispatched(cp, nowStr);

    // Collect kanban_ready candidates (source 8 in store.rs)
    const readyCards = observations
      .filter((o) => o.source === "kanban_ready")
      .filter((o) => {
        const valid = isAutomationCandidateCard(o);
        if (!valid) cp.stats.skipped++;
        return valid;
      })
      .map((o) => ({ cardId: o.card_id || o.evidence_ref?.replace("kanban_cards:", ""), obs: o }))
      .filter((c) => c.cardId);

    // Collect kanban_dispatched to suppress already-completed cards (source 9)
    const dispatchedCardIds = new Set(
      observations
        .filter((o) => o.source === "kanban_dispatched")
        .map((o) => o.card_id || o.evidence_ref?.replace("kanban_cards:", ""))
        .filter(Boolean)
    );

    cp.stats.ticks++;

    if (readyCards.length === 0) {
      return {
        action: "complete",
        result: { status: "ok", summary: "실행 대기 중인 자동화 후보 없음" },
        checkpoint: cp,
      };
    }

    // Filter candidates
    const toProcess = [];
    for (const { cardId, obs } of readyCards) {
      if (cp.dispatched[cardId] || dispatchedCardIds.has(cardId)) {
        cp.stats.skipped++;
        continue;
      }
      const meta = obs.metadata || {};
      const program = meta.program || {};
      const maxIterations = candidateIterationBudget(program);
      const iteration = (program.current_iteration || 0) + 1;
      const pending = cp.pending[cardId];
      const pendingIteration = Number.isFinite(Number(pending?.iteration))
        ? Number(pending.iteration)
        : iteration;
      if (pending && pendingIteration !== iteration) {
        delete cp.pending[cardId];
      }
      const activePending = pending && pendingIteration === iteration ? pending : null;

      if (activePending && (activePending.attempt_count || 0) >= MAX_DISPATCH_RETRIES) continue;
      if (activePending && isRecent(activePending.last_attempted_at, nowStr, DISPATCH_RETRY_MS)) {
        cp.stats.skipped++;
        continue;
      }

      if (iteration > maxIterations) {
        cp.stats.max_iterations_reached++;
        cp.dispatched[cardId] = { dispatched_at: nowStr, status: "max_iterations_reached", iteration };
        continue;
      }

      toProcess.push({ cardId, obs, iteration, program });
    }

    if (toProcess.length === 0) {
      return {
        action: "complete",
        result: {
          status: "ok",
          summary: `ready 후보 ${readyCards.length}건 모두 처리됨 또는 대기 중`,
          skipped: cp.stats.skipped,
        },
        checkpoint: cp,
      };
    }

    // Process first candidate
    const { cardId, obs, iteration } = toProcess[0];
    const card = {
      title: obs.summary || "",
      metadata: obs.metadata || {},
    };

    // Read previous iteration results from ctx.automationInventory if available
    const previousIterations = previousIterationsFor(ctx.automationInventory, cardId);

    const prevPending = cp.pending[cardId];
    cp.pending[cardId] = {
      first_attempted_at: prevPending?.first_attempted_at || nowStr,
      last_attempted_at: nowStr,
      attempt_count: (prevPending?.attempt_count || 0) + 1,
      iteration,
    };
    cp.stats.dispatched++;

    return {
      action: "agent",
      prompt: buildIterationPrompt(cardId, card, iteration, previousIterations),
      checkpoint: cp,
    };
  },
});
