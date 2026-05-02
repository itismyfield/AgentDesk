// Automation Executor
// Reads candidate_approved:* observations and dispatches GitHub Issue + Kanban card
// creation prompts to the agent. Dedup via candidate_dispatched:* observations and
// checkpoint dispatched_signatures. A signature is marked dispatched only after
// durable candidate_dispatched kv_meta is observed.

const DISPATCHED_TTL_DAYS = 7;
const CHECKPOINT_VERSION = 1;

// --- Checkpoint helpers ---

function emptyCheckpoint() {
  return {
    version: CHECKPOINT_VERSION,
    dispatched_signatures: {},   // signature -> dispatched_at ISO string (TTL 7d)
    stats: {
      ticks: 0,
      dispatched: 0,
      skipped_already_dispatched: 0,
    },
  };
}

function loadCheckpoint(raw) {
  if (!raw || typeof raw !== "object" || raw.version !== CHECKPOINT_VERSION) {
    return emptyCheckpoint();
  }
  const cp = Object.assign(emptyCheckpoint(), raw);
  cp.dispatched_signatures = raw.dispatched_signatures || {};
  cp.stats = Object.assign(emptyCheckpoint().stats, raw.stats || {});
  return cp;
}

function nowIso(now) {
  return typeof now === "string" ? now : now.toISOString ? now.toISOString() : String(now);
}

function observationKey(obs) {
  if (typeof obs.key === "string") return obs.key;
  if (typeof obs.evidence_ref === "string") {
    if (obs.evidence_ref.startsWith("kv_meta:routine_observation:")) {
      return obs.evidence_ref.slice("kv_meta:".length);
    }
    if (obs.evidence_ref.startsWith("routine_observation:")) {
      return obs.evidence_ref;
    }
  }
  return "";
}

function observationSignature(obs, prefix) {
  const key = observationKey(obs);
  return key.startsWith(prefix) ? key.slice(prefix.length) : null;
}

function observationPayload(obs) {
  return obs.value && typeof obs.value === "object" ? obs.value : obs;
}

// --- Prune expired dispatched_signatures ---

function pruneDispatched(cp, nowStr) {
  const cutoffMs = DISPATCHED_TTL_DAYS * 24 * 3600 * 1000;
  const cutoff = new Date(nowStr).getTime() - cutoffMs;
  for (const [sig, dispatchedAt] of Object.entries(cp.dispatched_signatures)) {
    if (new Date(dispatchedAt).getTime() < cutoff) {
      delete cp.dispatched_signatures[sig];
    }
  }
}

// --- Build dispatch prompt ---

function buildDispatchPrompt(signature, candidate) {
  const lines = [
    "승인된 자동화 후보에 대한 GitHub Issue 및 Kanban 카드 생성을 요청합니다.",
    "",
    `**후보 ID (signature)**: \`${signature}\``,
    `**카테고리**: ${candidate.category || "routine-candidate"}`,
    `**점수**: ${candidate.score || 0}`,
    `**승인 시각**: ${candidate.approved_at || "(알 수 없음)"}`,
    `**제안된 자동화**: ${candidate.suggested_automation || "(없음)"}`,
    `**결과 요약**: ${candidate.outcome_summary || "(없음)"}`,
    "",
    "---",
    "## 작업 지침",
    "",
    "1. **GitHub Issue 생성**: `kunkunGames/agentdesk` 저장소에 이슈를 생성해주세요.",
    "   - 제목: `[자동화 후보] " + (candidate.category || "routine-candidate") + ": " + signature + "`",
    "   - 레이블: `automation-candidate`",
    "   - 본문: 위 후보 정보 포함",
    "",
    "2. **Kanban 카드 생성**: kanban-writer 스킬을 사용해 카드를 생성해주세요.",
    "",
    "3. **완료 후 kv_meta 기록**:",
    "```",
    `routine_observation:candidate_dispatched:${signature}`,
    "```",
    "값(JSON): `{\"signature\":\"" + signature + "\",\"dispatched_at\":\"<현재시각ISO>\",\"category\":\"" + (candidate.category || "routine-candidate") + "\"}`",
    "TTL: 7d (604800초)",
    "",
    "이 kv_meta 기록은 executor 중복 방지 및 recommender 재추천 억제에 사용됩니다.",
  ];
  return lines.join("\n");
}

// --- Main tick ---

agentdesk.routines.register({
  name: "Automation Executor",

  tick(ctx) {
    const nowStr = nowIso(ctx.now);
    const cp = loadCheckpoint(ctx.checkpoint);
    const observations = ctx.observations || [];

    pruneDispatched(cp, nowStr);

    // Find candidate_approved observations
    const approvedPrefix = "routine_observation:candidate_approved:";
    const dispatchedPrefix = "routine_observation:candidate_dispatched:";
    const approvedObs = observations.filter((obs) => observationSignature(obs, approvedPrefix));

    // Find already-dispatched signatures (from kv_meta observations + checkpoint)
    const dispatchedFromObs = new Set(
      observations
        .map((obs) => observationSignature(obs, dispatchedPrefix))
        .filter(Boolean)
    );
    for (const signature of dispatchedFromObs) {
      if (!cp.dispatched_signatures[signature]) {
        cp.dispatched_signatures[signature] = nowStr;
      }
    }

    cp.stats.ticks++;

    if (approvedObs.length === 0) {
      return {
        action: "complete",
        result: {
          status: "ok",
          summary: "승인된 후보 없음",
          approved_count: 0,
        },
        checkpoint: cp,
      };
    }

    // Filter to candidates not yet dispatched
    const toDispatch = [];
    for (const obs of approvedObs) {
      const signature = observationSignature(obs, approvedPrefix);
      const candidate = observationPayload(obs);

      if (dispatchedFromObs.has(signature) || cp.dispatched_signatures[signature]) {
        cp.stats.skipped_already_dispatched++;
        continue;
      }

      toDispatch.push({ signature, candidate });
    }

    if (toDispatch.length === 0) {
      return {
        action: "complete",
        result: {
          status: "ok",
          summary: `승인 후보 ${approvedObs.length}건 모두 이미 처리됨`,
          approved_count: approvedObs.length,
          skipped: cp.stats.skipped_already_dispatched,
        },
        checkpoint: cp,
      };
    }

    // Dispatch first pending candidate; remaining handled on next ticks
    const { signature, candidate } = toDispatch[0];
    cp.stats.dispatched++;

    const prompt = buildDispatchPrompt(signature, candidate);

    return {
      action: "agent",
      prompt,
      checkpoint: cp,
    };
  },
});
