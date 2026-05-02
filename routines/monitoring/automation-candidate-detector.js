// Automation Candidate Detector
// Reads candidate_review:* observations (written by recommender escalation handler)
// and applies a quality gate. Passing candidates are forwarded to the agent for
// approval kv_meta write. Already approved/dispatched candidates are skipped.

const EVIDENCE_AGE_MAX_MS = 48 * 3600 * 1000;   // 48h — matches candidate_review TTL
const MIN_SCORE_THRESHOLD = 80;
const CHECKPOINT_VERSION = 1;
const SEEN_CANDIDATE_TTL_MS = 72 * 3600 * 1000;   // matches candidate_approved TTL

// --- Checkpoint helpers ---

function emptyCheckpoint() {
  return {
    version: CHECKPOINT_VERSION,
    seen_candidates: {},   // signature -> { first_seen_at, status: "emitted"|"skipped" }
    stats: {
      ticks: 0,
      approved_emitted: 0,
      skipped_already_approved: 0,
      skipped_quality_gate: 0,
    },
  };
}

function loadCheckpoint(raw) {
  if (!raw || typeof raw !== "object" || raw.version !== CHECKPOINT_VERSION) {
    return emptyCheckpoint();
  }
  const cp = Object.assign(emptyCheckpoint(), raw);
  cp.seen_candidates = raw.seen_candidates || {};
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

// --- Prune expired seen_candidates ---

function pruneSeen(cp, nowStr) {
  const cutoff = new Date(nowStr).getTime() - SEEN_CANDIDATE_TTL_MS;
  for (const [sig, entry] of Object.entries(cp.seen_candidates)) {
    if (new Date(entry.first_seen_at).getTime() < cutoff) {
      delete cp.seen_candidates[sig];
    }
  }
}

// --- Quality gate ---

function passesQualityGate(candidate, nowStr) {
  const score = typeof candidate.score === "number" ? candidate.score : 0;
  if (score < MIN_SCORE_THRESHOLD) return { pass: false, reason: `score=${score} < ${MIN_SCORE_THRESHOLD}` };

  if (candidate.evidence_age_ms != null) {
    if (candidate.evidence_age_ms > EVIDENCE_AGE_MAX_MS) {
      return { pass: false, reason: `evidence_age=${candidate.evidence_age_ms}ms > ${EVIDENCE_AGE_MAX_MS}ms` };
    }
  } else if (candidate.last_seen_at) {
    const ageMs = new Date(nowStr).getTime() - new Date(candidate.last_seen_at).getTime();
    if (ageMs > EVIDENCE_AGE_MAX_MS) {
      return { pass: false, reason: `evidence_age=${ageMs}ms > ${EVIDENCE_AGE_MAX_MS}ms` };
    }
  }

  return { pass: true, reason: null };
}

// --- Build approval prompt ---

function buildApprovalPrompt(signature, candidate) {
  const lines = [
    "자동화 후보 검토 요청입니다. 아래 후보를 검토하고 승인 여부를 결정해주세요.",
    "",
    `**후보 ID (signature)**: \`${signature}\``,
    `**카테고리**: ${candidate.category || "routine-candidate"}`,
    `**점수**: ${candidate.score}`,
    `**증거 수**: ${candidate.evidence_count || 0}`,
    `**제안된 자동화**: ${candidate.suggested_automation || "(없음)"}`,
    `**결과 요약**: ${candidate.outcome_summary || "(없음)"}`,
    "",
    "---",
    "## 승인 지침",
    "",
    "후보가 자동화 가치가 있다고 판단되면 다음 kv_meta를 기록해주세요:",
    "```",
    `routine_observation:candidate_approved:${signature}`,
    "```",
    "값(JSON): `{\"signature\":\"" + signature + "\",\"score\":" + candidate.score + ",\"approved_at\":\"<현재시각ISO>\",\"category\":\"" + (candidate.category || "routine-candidate") + "\"}`",
    "TTL: 72h",
    "",
    "승인하지 않는다면 kv_meta를 기록하지 않아도 됩니다.",
  ];
  return lines.join("\n");
}

// --- Main tick ---

agentdesk.routines.register({
  name: "Automation Candidate Detector",

  tick(ctx) {
    const nowStr = nowIso(ctx.now);
    const cp = loadCheckpoint(ctx.checkpoint);
    const observations = ctx.observations || [];

    pruneSeen(cp, nowStr);

    // Find candidate_review observations
    const reviewPrefix = "routine_observation:candidate_review:";
    const approvedPrefix = "routine_observation:candidate_approved:";
    const dispatchedPrefix = "routine_observation:candidate_dispatched:";
    const reviewObs = observations.filter((obs) => observationSignature(obs, reviewPrefix));

    // Find already-approved and dispatched signatures from observations
    const approvedSigs = new Set(
      observations
        .map((obs) => observationSignature(obs, approvedPrefix))
        .filter(Boolean)
    );
    const dispatchedSigs = new Set(
      observations
        .map((obs) => observationSignature(obs, dispatchedPrefix))
        .filter(Boolean)
    );

    cp.stats.ticks++;

    if (reviewObs.length === 0) {
      return {
        action: "complete",
        result: {
          status: "ok",
          summary: "검토할 후보 없음",
          review_count: 0,
        },
        checkpoint: cp,
      };
    }

    // Process each candidate_review observation
    const emitPrompts = [];
    for (const obs of reviewObs) {
      const signature = observationSignature(obs, reviewPrefix);
      const candidate = observationPayload(obs);

      // Skip already approved/dispatched
      if (approvedSigs.has(signature) || dispatchedSigs.has(signature)) {
        cp.stats.skipped_already_approved++;
        cp.seen_candidates[signature] = cp.seen_candidates[signature] || {
          first_seen_at: nowStr,
          status: "skipped_already_handled",
        };
        continue;
      }

      // Skip if we already emitted an approval prompt this cycle
      const seen = cp.seen_candidates[signature];
      if (seen && seen.status === "emitted") {
        cp.stats.skipped_already_approved++;
        continue;
      }

      // Quality gate
      const gate = passesQualityGate(candidate, nowStr);
      if (!gate.pass) {
        cp.stats.skipped_quality_gate++;
        cp.seen_candidates[signature] = { first_seen_at: nowStr, status: "rejected", reason: gate.reason };
        continue;
      }

      emitPrompts.push({ signature, candidate });
      cp.seen_candidates[signature] = { first_seen_at: nowStr, status: "emitted" };
      cp.stats.approved_emitted++;
    }

    if (emitPrompts.length === 0) {
      return {
        action: "complete",
        result: {
          status: "ok",
          summary: `검토 ${reviewObs.length}건 처리됨 (승인 요청 없음)`,
          review_count: reviewObs.length,
          skipped_approved: cp.stats.skipped_already_approved,
          skipped_quality_gate: cp.stats.skipped_quality_gate,
        },
        checkpoint: cp,
      };
    }

    // Emit one approval prompt (first candidate, others handled on next ticks)
    const { signature, candidate } = emitPrompts[0];
    const prompt = buildApprovalPrompt(signature, candidate);

    return {
      action: "agent",
      prompt,
      checkpoint: cp,
    };
  },
});
