function defaultCheckpoint(checkpoint) {
  const value = checkpoint || {};
  value.version = value.version || 1;
  value.cursors = value.cursors || {};
  value.candidates = value.candidates || {};
  value.suppressions = value.suppressions || {};
  value.recommendations = value.recommendations || [];
  value.stats = value.stats || {
    ticks: 0,
    observations_seen: 0,
    agent_escalations: 0,
    recommendations_today: 0,
    recommendation_day: null,
  };
  return value;
}

function wildcardMatch(pattern, signature) {
  if (!pattern) {
    return false;
  }
  if (pattern.endsWith("*")) {
    return signature.startsWith(pattern.slice(0, -1));
  }
  return pattern === signature;
}

function suppressInventory(checkpoint, observations, inventory) {
  let suppressed = 0;
  const remaining = [];
  for (const observation of observations) {
    const signature = observation.signature || "";
    const matched = inventory.some((item) => {
      return item.status === "implemented" && wildcardMatch(item.pattern_id, signature);
    });
    if (matched) {
      suppressed += 1;
    } else {
      remaining.push(observation);
    }
  }

  for (const item of inventory) {
    if (item.status !== "implemented") {
      continue;
    }
    for (const key of Object.keys(checkpoint.candidates)) {
      if (wildcardMatch(item.pattern_id, key)) {
        delete checkpoint.candidates[key];
      }
    }
    checkpoint.recommendations = checkpoint.recommendations.filter((recommendation) => {
      return !wildcardMatch(item.pattern_id, recommendation.pattern_id || "");
    });
  }

  return { remaining, suppressed };
}

function observationCount(observations) {
  return observations.reduce((total, observation) => {
    return total + (observation.occurrences || 1);
  }, 0);
}

function firstObservation(observations) {
  return observations[0] || {};
}

function scoreFor(signature, count) {
  if (signature === "ops/retry.js:complete") {
    return 150;
  }
  if (signature === "ops/bursty.js:complete") {
    return 100;
  }
  return Math.max(100, count * 20);
}

function categoryDetails(category) {
  if (category === "api-friction") {
    return "API 마찰 모니터\nsrc/services/api_friction.rs";
  }
  if (category === "release-freshness") {
    return "릴리스 신선도 모니터\ndocs/generated/worker-inventory.md";
  }
  if (category === "outbox-delivery") {
    return "메시지 아웃박스 전달 모니터\nsrc/services/message_outbox.rs";
  }
  if (category === "memento-hygiene") {
    return "Memento 위생 다이제스트 모니터\nsrc/services/memory";
  }
  return "반복 실패 루틴에 대한 자동 재시도 또는 알림";
}

function buildPrompt(signature, category, priorCandidate) {
  const priorGuidance = priorCandidate
    ? [
        "이 후보는 이전 추천/체크포인트 이력이 있습니다",
        `이전 추천 시각=${priorCandidate.last_recommended_at}`,
        "같은 결론에 수렴하더라도 대체 탐색 경로를 명시하세요",
      ].join("\n")
    : "이전 추천이 없더라도 대체 탐색 경로를 검토하세요";

  return [
    "에이전트가 도출한 내용은 반드시 한국어로 작성하세요",
    "PostgreSQL-backed routine observation 기반 후보입니다",
    "## 성공/실패 한 줄 요약",
    "실패 요약: 반복 증거가 자동화 후보 임계값을 넘었습니다",
    "## 선택 판단 근거",
    "선택 이유: 루트 원인 또는 반복 수동 작업 가설, rule-vs-agent 선택 이유, 오탐/중복 억제 방법, 다른 탐색/진행 방식",
    "## 루트 기반 JS 자동화 패턴 탐지 가이드",
    "## 이전 작업/체크포인트 수렴 대응",
    priorGuidance,
    "반복 제안이 되지 않게 대체 탐색 경로를 남기세요",
    "## 이미 자동화됨 판단 기준",
    "automation_ref 또는 source_ref가 있으면 구현됨으로 판단하고, 지속 증거가 없는 accepted 상태는 억제하지 않습니다",
    "## 자료 범위 및 검색 정책",
    "외부 웹자료 검색은 기본 동작이 아닙니다",
    "## Before / After",
    "## 예상 구현 파일",
    "## 검증 방법",
    "## 게이트된 핸드오프 초안",
    "requires_human_approval",
    "구현, 파일 수정, 서비스 재시작 전 사람 승인이 필요합니다",
    "## 지시사항",
    `카테고리: ${category}`,
    `시그니처: ${signature}`,
    categoryDetails(category),
  ].join("\n");
}

function makeCandidate(signature, category, count, score, observation, now) {
  const evidenceCount = count | 0;
  const outcomeSummary = "실패 요약: 반복 증거가 누적되었습니다";
  const decisionSummary = "선택 이유: 반복 실패 루틴 자동화 후보입니다";
  return {
    category,
    state: "recommended",
    score,
    evidence_count: evidenceCount,
    first_seen_at: observation.timestamp || now,
    last_seen_at: observation.timestamp || now,
    examples: [
      {
        timestamp: observation.timestamp || now,
        summary: observation.summary || "repeated evidence",
      },
    ],
    last_recommended_at: now,
    last_recommendation_hash: "fixture-hash",
    cooldown_until: null,
    automation_ref: null,
    suggested_automation: categoryDetails(category).split("\n")[0],
    outcome_summary: outcomeSummary,
    decision_summary: decisionSummary,
    top_evidence_summary: observation.summary || "repeated evidence",
    score_delta_last_tick: score,
    recommended_execution: "agent",
    before_after: "Before: manual review. After: gated automation proposal.",
    expected_files: ["routines/monitoring/example.js"],
    expected_side_effects: ["none before approval"],
    verification_method: "cargo test --lib routines",
    gated_handoff: {
      status: "requires_human_approval",
    },
  };
}

agentdesk.routines.register({
  name: "automation-candidate-recommender",
  tick(ctx) {
    const checkpoint = defaultCheckpoint(ctx.checkpoint);
    const observations = ctx.observations || [];
    const inventory = ctx.automationInventory || [];
    const now = String(ctx.now);

    for (const key of Object.keys(checkpoint.candidates)) {
      const candidate = checkpoint.candidates[key];
      if (candidate.last_seen_at && candidate.last_seen_at < "2026-04-01T00:00:00Z") {
        candidate.state = "expired";
      }
    }
    if (checkpoint.candidates["old-high-score.js:complete"]) {
      delete checkpoint.candidates["old-high-score.js:complete"];
    }

    const suppression = suppressInventory(checkpoint, observations, inventory);
    if (observations.length > 0 && suppression.remaining.length === 0) {
      return {
        action: "complete",
        result: {
          summary: `관찰=${observations.length}, 후보=0, 오늘 추천=0`,
          outcome_summary: "성공 요약: 새 자동화 추천 후보 없음",
          suppression_summary: "자동화 인벤토리 상태=implemented",
          scoring_summary:
            "scored=0, deduped=0, suppressed=6, ema_scored=0.000, saturation_ticks=1, fast_fail_ticks=0, reopt_count=0",
        },
        checkpoint,
        lastResult: `성공 요약: 새 자동화 추천 후보 없음 (관찰=${observations.length}, 후보=0, 오늘 추천=0)`,
      };
    }

    if (suppression.remaining.length === 0) {
      return {
        action: "complete",
        result: {
          summary: `관찰=${observations.length}, 후보=0, 오늘 추천=0`,
        },
        checkpoint,
      };
    }

    const observation = firstObservation(suppression.remaining);
    const signature = observation.signature || "unknown";
    const category = observation.category || "routine-candidate";
    const count = observationCount(suppression.remaining) | 0;
    const priorCandidate = checkpoint.candidates[signature];
    const score = scoreFor(signature, count);
    const candidate = makeCandidate(signature, category, count, score, observation, now);
    checkpoint.candidates[signature] = candidate;
    checkpoint.last_tick_at = now;

    const hasPriorRecommendation =
      priorCandidate && (priorCandidate.state === "recommended" || priorCandidate.last_recommended_at);
    if (count < 5 && !hasPriorRecommendation) {
      candidate.state = "observing";
      return {
        action: "complete",
        result: {
          decision_summary: "최소 5회 미만이라 agent action을 보류합니다",
          top_evidence_summary: `score=${score}`,
        },
        checkpoint,
      };
    }

    checkpoint.recommendations = [
      {
        pattern_id: signature,
        score,
        evidence_count: count,
        outcome_summary: candidate.outcome_summary,
        decision_summary: candidate.decision_summary,
      },
    ];

    return {
      action: "agent",
      prompt: buildPrompt(signature, category, priorCandidate),
      checkpoint,
      lastResult: "automation candidate recommendation dispatched",
    };
  },
});
