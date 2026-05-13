// Memento Digest Writer
// Periodically asks the LLM agent to recall recurring patterns from Memento
// and write them as routine_observation:memento_digest:{topic} kv_meta entries.
// This bridges the Memento signal gap: patterns in Memento only become recommender
// signals if they are also written to kv_meta.

const DIGEST_INTERVAL_MS = 4 * 3600 * 1000;  // re-query each topic at most every 4h
const MAX_TOPICS_PER_TICK = 3;               // one agent prompt covers up to 3 topics
const CHECKPOINT_VERSION = 1;

const QUERY_TOPICS = [
  { topic: "routine-failures", category: "routine-candidate", query: "반복되는 루틴 실행 실패나 에러 패턴" },
  { topic: "agent-errors", category: "session-pattern", query: "에이전트 반복 오류, 같은 실수 반복" },
  { topic: "user-repeated-requests", category: "routine-candidate", query: "사용자가 반복적으로 요청하는 작업 패턴" },
  { topic: "kanban-blockers", category: "kanban-flow", query: "칸반 카드 정체나 반복 차단 패턴" },
  { topic: "dispatch-failures", category: "dispatch-retry", query: "디스패치 반복 실패나 재시도 패턴" },
  { topic: "memory-hygiene", category: "memento-hygiene", query: "Memento 기억 품질, 중복, 잘못된 scope, 오래된 메모리 패턴" },
  { topic: "automation-opportunities", category: "routine-candidate", query: "자동화 가치가 높은 반복 수동 작업" },
];

function emptyCheckpoint() {
  return {
    version: CHECKPOINT_VERSION,
    last_queried: {},  // topic -> ISO timestamp
    stats: { ticks: 0, digests_requested: 0 },
  };
}

function loadCheckpoint(raw) {
  if (!raw || typeof raw !== "object" || raw.version !== CHECKPOINT_VERSION) {
    return emptyCheckpoint();
  }
  const cp = Object.assign(emptyCheckpoint(), raw);
  cp.last_queried = raw.last_queried || {};
  cp.stats = Object.assign(emptyCheckpoint().stats, raw.stats || {});
  return cp;
}

function nowIso(now) {
  return typeof now === "string" ? now : now.toISOString ? now.toISOString() : String(now);
}

function isDue(lastQueriedIso, nowStr, intervalMs) {
  if (!lastQueriedIso) return true;
  const last = new Date(lastQueriedIso).getTime();
  return Number.isNaN(last) || new Date(nowStr).getTime() - last >= intervalMs;
}

agentdesk.routines.register({
  name: "Memento Digest Writer",

  tick(ctx) {
    const nowStr = nowIso(ctx.now);
    const cp = loadCheckpoint(ctx.checkpoint);
    cp.stats.ticks++;

    // Find topics due for re-query
    const dueTopics = QUERY_TOPICS.filter(({ topic }) =>
      isDue(cp.last_queried[topic], nowStr, DIGEST_INTERVAL_MS)
    ).slice(0, MAX_TOPICS_PER_TICK);

    if (dueTopics.length === 0) {
      return {
        action: "complete",
        result: { status: "ok", summary: "모든 주제가 최근 조회됨, 다음 tick 대기" },
        checkpoint: cp,
      };
    }

    const topicLines = dueTopics.map(({ topic, category, query }) => {
      const key = `routine_observation:memento_digest:${topic}`;
      return [
        `### 주제: ${topic}`,
        `- 추천 카테고리: \`${category}\``,
        `- Memento recall 쿼리: "${query}"`,
        `- kv_meta 키: \`${key}\``,
        `- TTL: 6시간 (21600초)`,
        `- 값(JSON 형식):`,
        `  \`{"topic":"${topic}","count":<패턴_발생횟수>,"category":"${category}","source":"memento_digest","signature":"${category}:${topic}","latest_examples":["<최근예시1>","<최근예시2>"]}\``,
        `- 패턴이 없거나 count < 2면 kv_meta를 쓰지 않습니다.`,
      ].join("\n");
    }).join("\n\n");

    const prompt = [
      "Memento에서 반복 패턴을 조회하고 kv_meta digest를 기록해주세요.",
      "",
      "## 지침",
      "각 주제에 대해 Memento recall 도구를 사용해 관련 파편을 조회한 뒤,",
      "같은 문제·패턴이 2회 이상 반복되는 경우에만 kv_meta에 기록합니다.",
      "",
      "## 조회 주제",
      topicLines,
      "",
      "## 완료 조건",
      "각 주제마다 kv_meta를 기록하거나 (패턴 없으면 생략) 완료로 간주합니다.",
    ].join("\n");

    // Mark topics as queried (optimistically — agent may not write if no patterns)
    for (const { topic } of dueTopics) {
      cp.last_queried[topic] = nowStr;
    }
    cp.stats.digests_requested += dueTopics.length;

    return {
      action: "agent",
      prompt,
      checkpoint: cp,
    };
  },
});
