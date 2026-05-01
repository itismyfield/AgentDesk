// Automation Candidate Recommender
// 매 tick마다 bounded observations를 checkpoint에 누적하고,
// 강한 근거(score >= 80)에서만 agent proposal을 생성한다.
// P0: read-only, no auto-implementation, proposal-only.

const SCORE_THRESHOLD = 80;
const DAILY_CAP = 3;
const COOLDOWN_HOURS = 6;
const MAX_EXAMPLES_PER_CANDIDATE = 3;
const PROMPT_CAP_BYTES = 12288;
const CHECKPOINT_CAP_BYTES = 65536;
const CHECKPOINT_VERSION = 1;
const CANDIDATE_TTL_DAYS = 30;
const KNOWN_CATEGORIES = new Set([
  "routine-candidate",
  "release-freshness",
  "outbox-delivery",
  "memento-hygiene",
  "api-friction",
]);

// --- Scoring weights ---
const WEIGHT_BASE = 10;
const WEIGHT_RECENCY_BONUS = 10;   // occurred in last 30 min
const WEIGHT_FIRST_SEEN = 5;       // new candidate bonus

// --- Checkpoint helpers ---

function emptyCheckpoint() {
  return {
    version: CHECKPOINT_VERSION,
    cursors: {},
    candidates: {},
    suppressions: {},
    recommendations: [],
    last_tick_at: null,
    stats: {
      ticks: 0,
      observations_seen: 0,
      agent_escalations: 0,
      recommendations_today: 0,
      recommendation_day: null,
    },
  };
}

function loadCheckpoint(raw) {
  if (!raw || typeof raw !== "object" || raw.version !== CHECKPOINT_VERSION) {
    return emptyCheckpoint();
  }
  const cp = Object.assign(emptyCheckpoint(), raw);
  cp.candidates = raw.candidates || {};
  cp.suppressions = raw.suppressions || {};
  cp.stats = Object.assign(emptyCheckpoint().stats, raw.stats || {});
  return cp;
}

function nowIso(now) {
  return typeof now === "string" ? now : now.toISOString ? now.toISOString() : String(now);
}

function dateOf(iso) {
  return iso ? iso.slice(0, 10) : null;
}

function addHours(isoStr, hours) {
  const ms = new Date(isoStr).getTime() + hours * 3600 * 1000;
  return new Date(ms).toISOString();
}

function simpleHash(str) {
  let h = 0;
  for (let i = 0; i < str.length; i++) {
    h = ((h << 5) - h + str.charCodeAt(i)) | 0;
  }
  return (h >>> 0).toString(16);
}

function normalizeCategory(value) {
  if (typeof value === "string" && KNOWN_CATEGORIES.has(value)) {
    return value;
  }
  return "routine-candidate";
}

function observationOccurrences(obs) {
  const value = Number(obs.occurrences || obs.count || 1);
  if (!Number.isFinite(value) || value < 1) {
    return 1;
  }
  return Math.min(50, Math.floor(value));
}

function utf8CharBytes(ch) {
  const codePoint = ch.codePointAt(0);
  if (codePoint <= 0x7f) return 1;
  if (codePoint <= 0x7ff) return 2;
  if (codePoint <= 0xffff) return 3;
  return 4;
}

function utf8ByteLength(value) {
  let bytes = 0;
  for (const ch of String(value || "")) {
    bytes += utf8CharBytes(ch);
  }
  return bytes;
}

function truncateUtf8(value, maxBytes) {
  if (maxBytes <= 0) return "";
  let bytes = 0;
  let out = "";
  for (const ch of String(value || "")) {
    const nextBytes = utf8CharBytes(ch);
    if (bytes + nextBytes > maxBytes) break;
    out += ch;
    bytes += nextBytes;
  }
  return out;
}

// --- Daily cap reset ---

function resetDailyCapIfNeeded(cp, nowStr) {
  const today = dateOf(nowStr);
  if (cp.stats.recommendation_day !== today) {
    cp.stats.recommendations_today = 0;
    cp.stats.recommendation_day = today;
  }
}

// --- Suppression / inventory filter ---

function hasDurableAcceptance(entry) {
  return Boolean(entry && (entry.automation_ref || entry.source_ref));
}

function buildSuppressedSet(cp, inventory) {
  const exact = new Set();
  const prefixes = [];

  function addPattern(patternId) {
    if (!patternId) return;
    if (patternId.endsWith(":*")) {
      prefixes.push(patternId.slice(0, -1));
    } else {
      exact.add(patternId);
    }
  }

  // From checkpoint suppressions
  for (const [patternId, entry] of Object.entries(cp.suppressions || {})) {
    const state = entry.state;
    if (
      (state === "accepted" && hasDurableAcceptance(entry)) ||
      state === "implemented" ||
      state === "suppressed" ||
      state === "rejected"
    ) {
      addPattern(patternId);
    }
  }

  // From automation inventory (exact pattern_id match only)
  for (const item of inventory || []) {
    if (
      item.pattern_id &&
      ((item.status === "accepted" && hasDurableAcceptance(item)) ||
        item.status === "implemented" ||
        item.status === "suppressed" ||
        item.status === "rejected")
    ) {
      addPattern(item.pattern_id);
    }
  }

  // Candidate-level accepted/implemented/suppressed/rejected states
  for (const [patternId, candidate] of Object.entries(cp.candidates || {})) {
    const s = candidate.state;
    if (
      (s === "accepted" && hasDurableAcceptance(candidate)) ||
      s === "implemented" ||
      s === "suppressed" ||
      s === "rejected"
    ) {
      addPattern(patternId);
    }
  }

  return {
    has(patternId) {
      return exact.has(patternId) || prefixes.some((prefix) => patternId.startsWith(prefix));
    },
  };
}

function dropSuppressedCandidates(cp, suppressedSet) {
  for (const [patternId, candidate] of Object.entries(cp.candidates || {})) {
    if (suppressedSet.has(patternId)) {
      const s = candidate.state;
      if (
        (s === "accepted" && hasDurableAcceptance(candidate)) ||
        s === "implemented" ||
        s === "suppressed" ||
        s === "rejected"
      ) {
        continue;
      }
      delete cp.candidates[patternId];
    }
  }

  if (Array.isArray(cp.recommendations)) {
    cp.recommendations = cp.recommendations.filter((item) => !suppressedSet.has(item.pattern_id));
  }
}

// --- Expired suppression cleanup ---

function pruneExpiredSuppressions(cp, nowStr) {
  for (const [patternId, entry] of Object.entries(cp.suppressions || {})) {
    if (entry.expires_at && entry.expires_at < nowStr) {
      delete cp.suppressions[patternId];
      if (cp.candidates[patternId]) {
        cp.candidates[patternId].state = "observing";
      }
    }
  }
}

function expireStaleCandidates(cp, nowStr) {
  const cutoff = new Date(new Date(nowStr).getTime() - CANDIDATE_TTL_DAYS * 24 * 3600 * 1000).toISOString();
  for (const candidate of Object.values(cp.candidates || {})) {
    if (
      candidate.last_seen_at &&
      candidate.last_seen_at < cutoff &&
      (candidate.state === "observing" || candidate.state === "recommended")
    ) {
      candidate.state = "expired";
    }
  }
}

// --- Score observations into candidates ---

function scoreObservations(cp, observations, suppressedSet, nowStr) {
  const thirtyMinAgo = new Date(new Date(nowStr).getTime() - 30 * 60 * 1000).toISOString();

  for (const obs of observations) {
    cp.stats.observations_seen++;

    const patternId = obs.signature;
    if (!patternId) continue;
    if (suppressedSet.has(patternId)) continue;

    const category = normalizeCategory(obs.category);
    let candidate = cp.candidates[patternId];
    if (!candidate) {
      candidate = {
        category,
        state: "observing",
        score: WEIGHT_FIRST_SEEN,
        evidence_count: 0,
        first_seen_at: obs.timestamp || nowStr,
        last_seen_at: null,
        examples: [],
        last_recommended_at: null,
        last_recommendation_hash: null,
        cooldown_until: null,
        automation_ref: null,
        has_error_evidence: false,
      };
      cp.candidates[patternId] = candidate;
    } else {
      candidate.category = normalizeCategory(candidate.category || category);
    }

    // Skip if already resolved
    if (candidate.state === "accepted" && !hasDurableAcceptance(candidate)) {
      candidate.state = "recommended";
    }
    if (
      (candidate.state === "accepted" && hasDurableAcceptance(candidate)) ||
      candidate.state === "implemented" ||
      candidate.state === "suppressed" ||
      candidate.state === "rejected"
    ) {
      continue;
    }

    const occurrences = observationOccurrences(obs);
    candidate.evidence_count += occurrences;
    candidate.last_seen_at = obs.timestamp || nowStr;

    // Score delta
    const weight = typeof obs.weight === "number" ? obs.weight : 1;
    const scoredOccurrences = Math.min(occurrences, 5);
    let delta = WEIGHT_BASE * weight * scoredOccurrences;
    if (weight === 2) {
      candidate.has_error_evidence = true;
    }

    // Recency bonus
    if (obs.timestamp && obs.timestamp >= thirtyMinAgo) {
      delta += WEIGHT_RECENCY_BONUS;
    }

    candidate.score = Math.min(100, candidate.score + delta);

    // Keep up to 3 examples
    if (candidate.examples.length < MAX_EXAMPLES_PER_CANDIDATE) {
      candidate.examples.push({
        summary: obs.summary,
        timestamp: obs.timestamp,
        evidence_ref: obs.evidence_ref,
        weight,
        occurrences,
      });
    }
  }
}

// --- Find best escalation candidate ---

function findEscalationCandidate(cp, nowStr) {
  if (cp.stats.recommendations_today >= DAILY_CAP) {
    return null;
  }

  let best = null;
  let bestScore = SCORE_THRESHOLD - 1;

  for (const [patternId, candidate] of Object.entries(cp.candidates || {})) {
    if (candidate.state !== "observing" && candidate.state !== "recommended") {
      continue;
    }
    if (candidate.score <= bestScore) {
      continue;
    }
    if (candidate.evidence_count < 5) {
      continue;
    }
    if (candidate.cooldown_until && candidate.cooldown_until > nowStr) {
      continue;
    }

    // Dedupe: same hash within cooldown
    const hash = simpleHash(patternId + ":" + candidate.evidence_count);
    if (candidate.last_recommendation_hash === hash) {
      continue;
    }

    bestScore = candidate.score;
    best = { patternId, candidate, hash };
  }

  return best;
}

// --- Mark candidate as recommended ---

function markRecommended(cp, escalation, nowStr) {
  const { patternId, candidate, hash } = escalation;
  const assessment = candidateAssessment(patternId, candidate);
  candidate.state = "recommended";
  candidate.last_recommended_at = nowStr;
  candidate.last_recommendation_hash = hash;
  candidate.cooldown_until = addHours(nowStr, COOLDOWN_HOURS);
  candidate.suggested_automation = assessment.suggestedAutomation;
  candidate.recommended_execution = assessment.recommendedExecution;
  candidate.outcome_summary = assessment.outcomeSummary;
  candidate.before_after = assessment.beforeAfter;
  candidate.expected_files = assessment.expectedFiles;
  candidate.expected_side_effects = assessment.expectedSideEffects;
  candidate.verification_method = assessment.verificationMethod;
  candidate.gated_handoff = assessment.gatedHandoff;
  cp.stats.recommendations_today++;
  cp.stats.agent_escalations++;
  cp.recommendations.push({
    pattern_id: patternId,
    recommended_at: nowStr,
    hash,
    score: candidate.score,
    evidence_count: candidate.evidence_count,
    outcome_summary: assessment.outcomeSummary,
  });
  // Keep recommendations list bounded
  if (cp.recommendations.length > 50) {
    cp.recommendations = cp.recommendations.slice(-50);
  }
}

// --- Agent prompt builder ---

function buildOutcomeSummary(patternId, candidate, isErrorPattern, category) {
  const latestExample = (candidate.examples || []).slice(-1)[0] || {};
  const latestSummary = String(latestExample.summary || patternId)
    .replace(/\s+/g, " ")
    .slice(0, 120);
  const count = candidate.evidence_count || observationOccurrences(latestExample) || 0;
  const categoryLabel = {
    "routine-candidate": "루틴 반복",
    "release-freshness": "릴리스 신선도",
    "outbox-delivery": "메시지 발송",
    "memento-hygiene": "메모리 위생",
    "api-friction": "API 마찰",
  }[category] || "루틴 후보";
  const prefix = isErrorPattern || category === "outbox-delivery" || category === "api-friction"
    ? "실패 요약"
    : "성공 요약";
  const action = prefix === "실패 요약"
    ? "자동 복구나 알림 후보입니다"
    : "수동 확인 없이 루틴화할 후보입니다";
  return `${prefix}: ${categoryLabel} 패턴이 ${count}회 반복되어 ${action}. 최근 근거: ${latestSummary}`;
}

function candidateAssessment(patternId, candidate) {
  const isErrorPattern = Boolean(candidate.has_error_evidence) ||
    (candidate.examples || []).some((example) => example.weight === 2);
  const category = normalizeCategory(candidate.category);
  const categoryProfiles = {
    "routine-candidate": {
      suggestedAutomation: isErrorPattern
        ? "반복 실패 루틴에 대한 자동 재시도 또는 알림"
        : "반복 패턴을 자동 처리하는 예약 루틴",
      before: "반복 루틴 근거는 수동 로그 확인 후에만 보입니다.",
      after: "제한된 루틴/규칙이 반복 패턴을 처리하거나 쿨다운을 두고 에스컬레이션합니다.",
      files: ["routines/monitoring/*.js", "src/services/routines/*"],
      sideEffects: "루틴 또는 규칙 경로가 추가될 수 있으므로 쿨다운, 중복 제거, Discord 노이즈를 검증해야 합니다.",
      verification: "대상 루틴 로더 테스트를 실행하고 체크포인트 후보 필드를 확인합니다.",
    },
    "release-freshness": {
      suggestedAutomation: "오래된 배포, 버전, 생성 인벤토리 신호를 감지하는 릴리스 신선도 모니터",
      before: "버전이나 생성 문서가 오래된 뒤에야 사람이 릴리스 드리프트를 발견합니다.",
      after: "신선도 점검이 오래된 릴리스 상태가 누적되기 전에 업데이트 경로를 제안합니다.",
      files: ["scripts/*release*", "src/cli/*", "docs/generated/worker-inventory.md"],
      sideEffects: "읽기 전용 신선도 점검이 추가될 수 있으며 자동 게시, 태깅, 배포는 피해야 합니다.",
      verification: "스크립트 검사와 릴리스 부작용이 없음을 증명하는 신선도 픽스처를 실행합니다.",
    },
    "outbox-delivery": {
      suggestedAutomation: "반복 전송 또는 큐 적재 실패를 감지하는 메시지 아웃박스 전달 모니터",
      before: "전달 실패 반복 패턴을 찾으려면 DB/로그를 사람이 직접 확인해야 합니다.",
      after: "반복 아웃박스 실패가 명확한 전달 수정 경로를 가진 제한된 제안으로 묶입니다.",
      files: ["src/services/message_outbox.rs", "src/services/routines/discord_log.rs", "src/services/discord/*"],
      sideEffects: "알림 재시도 또는 폴백 동작이 바뀔 수 있으므로 중복 제거와 전달 대상을 검증해야 합니다.",
      verification: "아웃박스/루틴 대상 테스트를 실행하고 전달 실패 픽스처를 확인합니다.",
    },
    "memento-hygiene": {
      suggestedAutomation: "반복되는 메모리 품질 또는 라우팅 문제를 요약하는 Memento 위생 다이제스트 모니터",
      before: "메모리 위생 문제는 원문 노트에 흩어져 있어 안전하게 조치하기 어렵습니다.",
      after: "토픽/횟수/최신 예시 다이제스트만 제한된 제안으로 변환합니다.",
      files: ["src/services/memory/*", "src/services/routines/store.rs", "routines/monitoring/*.js"],
      sideEffects: "이 루틴은 원문 메모리 본문을 읽거나 쓰면 안 되며 다이제스트 절단을 검증해야 합니다.",
      verification: "추천기 다이제스트 픽스처를 실행하고 프롬프트에 원문 메모리 본문이 없는지 확인합니다.",
    },
    "api-friction": {
      suggestedAutomation: "반복되는 문서 또는 엔드포인트 워크플로 붕괴를 감지하는 API 마찰 모니터",
      before: "API 마찰이 에이전트 응답에서 반복되지만 통합 개선 제안으로 이어지지 않습니다.",
      after: "반복 마찰 마커가 지문별로 묶이고 문서 및 검증 가이드가 함께 제안됩니다.",
      files: ["src/services/api_friction.rs", "src/server/routes/*", "docs/*"],
      sideEffects: "문서 또는 API 라우팅이 바뀔 수 있으며 DB 직접 우회가 도입되지 않았는지 검증해야 합니다.",
      verification: "API 마찰 파싱 테스트와 대상 루틴 추천기 픽스처를 실행합니다.",
    },
  };
  const profile = categoryProfiles[category] || categoryProfiles["routine-candidate"];
  const recommendedExecution = category === "routine-candidate" && candidate.score < 90
    ? "rule"
    : "agent";
  const title = patternId.replace(/\s+/g, " ").slice(0, 96);
  return {
    suggestedAutomation: profile.suggestedAutomation,
    recommendedExecution,
    outcomeSummary: buildOutcomeSummary(patternId, candidate, isErrorPattern, category),
    beforeAfter: {
      before: profile.before,
      after: profile.after,
    },
    expectedFiles: profile.files,
    expectedSideEffects: profile.sideEffects,
    verificationMethod: profile.verification,
    gatedHandoff: {
      status: "requires_human_approval",
      kanban_card_draft: {
        title: `[automation-candidate] ${title}`,
        category,
        acceptance: [
          "브랜치/카드 변경 전에 제안 승인이 완료되어야 합니다",
          "루틴은 제한적이고 멱등적으로 유지되어야 합니다",
          "검증 명령 또는 픽스처가 PR/카드에 기록되어야 합니다",
        ],
      },
      pr_draft: {
        title: `자동화 후보 구현: ${title}`,
        body_hint: "Before/After, 예상 파일, 부작용, 검증 근거를 포함합니다.",
      },
      side_effects: "사람이 게이트된 핸드오프를 명시적으로 승인하기 전까지는 없음",
    },
  };
}

function buildPrompt(escalation) {
  const { patternId, candidate } = escalation;
  const evidenceLines = (candidate.examples || [])
    .map((ex, i) => `${i + 1}. [${ex.timestamp || "?"}] ${ex.summary || ""} (occurrences=${ex.occurrences || 1})`)
    .join("\n");

  const {
    suggestedAutomation,
    recommendedExecution,
    beforeAfter,
    expectedFiles,
    expectedSideEffects,
    verificationMethod,
    outcomeSummary,
    gatedHandoff,
  } = candidateAssessment(patternId, candidate);
  const handoffAcceptance = (gatedHandoff.kanban_card_draft.acceptance || [])
    .map((item) => `- ${item}`)
    .join("\n");

  const raw = `# 자동화 후보 추천

패턴: ${patternId}
카테고리: ${normalizeCategory(candidate.category)}
점수: ${candidate.score}/100
근거: ${candidate.evidence_count}회 발생 (최초: ${candidate.first_seen_at || "?"}, 최신: ${candidate.last_seen_at || "?"})

## 근거 예시
${evidenceLines || "(기록 없음)"}

## 성공/실패 한 줄 요약
${outcomeSummary}

## 루트 기반 JS 자동화 패턴 탐지 가이드
- 이 제안은 runtime이 제공한 bounded observation, checkpoint, automation inventory만 근거로 판단합니다.
- 같은 증상이 아니라 같은 루트 원인 또는 같은 수동 작업이 반복되는지 pattern/category/count/first-last/example을 연결해 설명합니다.
- 단순 빈도만 보지 말고 최근성, 실패 weight, 운영 ROI, 부작용, 이미 구현/억제된 자동화와의 중복 가능성을 함께 평가합니다.
- 규칙으로 충분한 deterministic retry/check/threshold인지, 문맥 판단과 코드 변경 설계가 필요한 agent 주도 자동화인지 구분합니다.
- 근거가 부족하거나 오탐 가능성이 높으면 자동화 보류로 결론을 내립니다.

## Before / After
- Before: ${beforeAfter.before}
- After: ${beforeAfter.after}

## 예상 구현 파일
${expectedFiles.map((file) => `- ${file}`).join("\n")}

## 판단
- 제안 자동화: ${suggestedAutomation}
- 권장 실행 방식: ${recommendedExecution} (규칙 기반 vs 에이전트 주도)
- 예상 부작용: ${expectedSideEffects}

## 검증 방법
${verificationMethod}

## 게이트된 핸드오프 초안
- 상태: ${gatedHandoff.status}
- Kanban 제목: ${gatedHandoff.kanban_card_draft.title}
- PR 제목: ${gatedHandoff.pr_draft.title}
- 핸드오프 부작용: ${gatedHandoff.side_effects}
${handoffAcceptance}

## 지시사항
에이전트가 도출한 내용은 반드시 한국어로 작성합니다. 이 자동화를 구현할 가치가 있는지 평가하고 다음을 제공합니다:
1. 자동화 여부(예 / 아니오 / 보류), 신뢰도, 그리고 이유
2. 루트 원인 또는 반복 수동 작업 가설과 그 근거
3. 구현한다면 제안 구현 방식, rule-vs-agent 선택 이유, 영향 파일/루틴
4. 성공/실패에 대한 한 줄 요약
5. 예상 부작용, 오탐/중복 억제 방법, 자동화 동작 검증 방법

구현, 파일 수정, 서비스 재시작, memento 쓰기, PR/카드/이슈 생성은 금지합니다.
이 요청은 제안 전용입니다.`;

  if (utf8ByteLength(raw) <= PROMPT_CAP_BYTES) {
    return raw;
  }

  // Trim examples to fit cap
  const header = raw.split("## 근거 예시")[0];
  const footer = "\n\n## 지시사항\n" + raw.split("## 지시사항\n")[1];
  const budget = PROMPT_CAP_BYTES - utf8ByteLength(header) - utf8ByteLength(footer) - 20;
  const trimmedEvidence = truncateUtf8(evidenceLines, budget);
  return header + "## 근거 예시\n" + trimmedEvidence + footer;
}

// --- Checkpoint size guard ---

function guardCheckpointSize(cp) {
  const json = JSON.stringify(cp);
  if (json.length <= CHECKPOINT_CAP_BYTES) return cp;

  // Prune least-recently-observed candidates first.
  const entries = Object.entries(cp.candidates).sort(
    ([, a], [, b]) => String(a.last_seen_at || "").localeCompare(String(b.last_seen_at || ""))
  );
  let pruned = 0;
  for (const [patternId] of entries) {
    if (JSON.stringify(cp).length <= CHECKPOINT_CAP_BYTES) break;
    if (
      cp.candidates[patternId].state === "observing"
    ) {
      delete cp.candidates[patternId];
      pruned++;
    }
  }

  // Trim examples on remaining candidates
  for (const candidate of Object.values(cp.candidates)) {
    if (candidate.examples && candidate.examples.length > 1) {
      candidate.examples = candidate.examples.slice(-1);
    }
  }

  return cp;
}

// --- Main tick ---

agentdesk.routines.register({
  name: "Automation Candidate Recommender",

  tick(ctx) {
    const nowStr = nowIso(ctx.now);
    const cp = loadCheckpoint(ctx.checkpoint);
    const observations = ctx.observations || [];
    const inventory = ctx.automationInventory || [];

    resetDailyCapIfNeeded(cp, nowStr);
    pruneExpiredSuppressions(cp, nowStr);
    expireStaleCandidates(cp, nowStr);

    const suppressedSet = buildSuppressedSet(cp, inventory);
    dropSuppressedCandidates(cp, suppressedSet);
    scoreObservations(cp, observations, suppressedSet, nowStr);

    cp.stats.ticks++;
    cp.last_tick_at = nowStr;

    const escalation = findEscalationCandidate(cp, nowStr);

    if (!escalation) {
      const activeCandidates = Object.values(cp.candidates).filter(
        (c) => c.state === "observing" || c.state === "recommended"
      ).length;
      const summary = `관찰=${observations.length}, 후보=${activeCandidates}, 오늘 추천=${cp.stats.recommendations_today}`;
      const outcomeSummary = `성공 요약: 새 자동화 추천 후보 없음 (${summary})`;
      return {
        action: "complete",
        result: {
          status: "ok",
          summary,
          outcome_summary: outcomeSummary,
          observation_count: observations.length,
          active_candidate_count: activeCandidates,
          recommendations_today: cp.stats.recommendations_today,
        },
        checkpoint: guardCheckpointSize(cp),
        lastResult: outcomeSummary,
      };
    }

    const prompt = buildPrompt(escalation);
    markRecommended(cp, escalation, nowStr);

    return {
      action: "agent",
      prompt,
      checkpoint: guardCheckpointSize(cp),
    };
  },
});
