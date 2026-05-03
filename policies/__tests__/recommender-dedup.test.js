const test = require("node:test");
const assert = require("node:assert/strict");
const { loadRoutine } = require("./support/routine-harness");

const ROUTINE_PATH = "routines/monitoring/automation-candidate-recommender.js";

function makeObs(signature, evidenceRef, opts = {}) {
  return {
    signature,
    evidence_ref: evidenceRef || `routine_runs:${signature}.js:run:failed`,
    category: "failure-pattern",
    summary: `Test observation for ${signature}`,
    occurrences: opts.occurrences || 1,
    weight: opts.weight || 1,
  };
}

function parseScoringLine(result) {
  const summary = result.result && result.result.scoring_summary;
  if (!summary) return { scored: 0, deduped: 0 };
  const m = summary.match(/scored=(\d+).*deduped=(\d+)/);
  return m ? { scored: parseInt(m[1], 10), deduped: parseInt(m[2], 10) } : { scored: 0, deduped: 0 };
}

function buildCheckpointWithEvidence(entries, nowBase) {
  const seen_evidence = {};
  for (let i = 0; i < entries; i++) {
    seen_evidence[`key-${i}`] = new Date(nowBase + i * 1000).toISOString();
  }
  return {
    version: 1,
    cursors: {},
    candidates: {},
    suppressions: {},
    seen_evidence,
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

test("same evidence_ref is deduped on consecutive ticks", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [makeObs("repeated-pattern", "routine_runs:heavy.js:run:failed")];

  const result1 = tick({ now: new Date("2026-05-02T10:00:00Z"), checkpoint: null, observations: obs, automationInventory: [] });
  const s1 = parseScoringLine(result1);
  assert.equal(s1.scored, 1, "first tick should score 1");
  assert.equal(s1.deduped, 0, "first tick should have 0 deduped");

  const result2 = tick({ now: new Date("2026-05-02T10:01:00Z"), checkpoint: result1.checkpoint, observations: obs, automationInventory: [] });
  const s2 = parseScoringLine(result2);
  assert.equal(s2.deduped, 1, "second tick with same evidence_ref should dedup 1");
  assert.equal(s2.scored, 0, "second tick should score 0 new");
});

test("dedup hit refreshes seen_evidence timestamp for LRU", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const evidenceRef = "routine_runs:lru.js:run:failed";
  const obs = [makeObs("lru-pattern", evidenceRef)];

  const result1 = tick({ now: new Date("2026-05-02T10:00:00Z"), checkpoint: null, observations: obs, automationInventory: [] });
  const result2 = tick({ now: new Date("2026-05-02T10:10:00Z"), checkpoint: result1.checkpoint, observations: obs, automationInventory: [] });

  assert.equal(parseScoringLine(result2).deduped, 1, "second tick should dedup");
  assert.equal(result2.checkpoint.seen_evidence[evidenceRef].seen_at, "2026-05-02T10:10:00.000Z");
});

test("rolling grouped evidence scores only newly increased occurrences", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const evidenceRef = "routine_runs:rolling.js:run:failed";

  const result1 = tick({
    now: new Date("2026-05-02T10:00:00Z"),
    checkpoint: null,
    observations: [makeObs("rolling-pattern", evidenceRef, { occurrences: 2 })],
    automationInventory: [],
  });
  assert.equal(parseScoringLine(result1).scored, 1, "first aggregate should score");

  const result2 = tick({
    now: new Date("2026-05-02T10:05:00Z"),
    checkpoint: result1.checkpoint,
    observations: [makeObs("rolling-pattern", evidenceRef, { occurrences: 5 })],
    automationInventory: [],
  });
  const s2 = parseScoringLine(result2);
  assert.equal(s2.scored, 1, "aggregate with new occurrences should score once");
  assert.equal(s2.deduped, 0, "aggregate with higher count should not be fully deduped");
  assert.equal(result2.checkpoint.candidates["rolling-pattern"].evidence_count, 5);
});

test("rolling grouped evidence tracks the latest observed count after a window drop", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const evidenceRef = "routine_runs:sliding.js:run:failed";

  const result1 = tick({
    now: new Date("2026-05-02T10:00:00Z"),
    checkpoint: null,
    observations: [makeObs("sliding-pattern", evidenceRef, { occurrences: 10 })],
    automationInventory: [],
  });
  assert.equal(parseScoringLine(result1).scored, 1, "first aggregate should score");

  const result2 = tick({
    now: new Date("2026-05-02T10:05:00Z"),
    checkpoint: result1.checkpoint,
    observations: [makeObs("sliding-pattern", evidenceRef, { occurrences: 4 })],
    automationInventory: [],
  });
  const s2 = parseScoringLine(result2);
  assert.equal(s2.scored, 0, "smaller rolling window count should not rescore");
  assert.equal(s2.deduped, 1, "smaller rolling window count should be treated as already seen");
  assert.equal(result2.checkpoint.seen_evidence[evidenceRef].occurrences, 4);

  const result3 = tick({
    now: new Date("2026-05-02T10:10:00Z"),
    checkpoint: result2.checkpoint,
    observations: [makeObs("sliding-pattern", evidenceRef, { occurrences: 6 })],
    automationInventory: [],
  });
  const s3 = parseScoringLine(result3);
  assert.equal(s3.scored, 1, "increase after the window drop should score the fresh delta");
  assert.equal(s3.deduped, 0, "increase after the window drop should not be pinned to the old peak");
  assert.equal(result3.checkpoint.candidates["sliding-pattern"].evidence_count, 12);
  assert.equal(result3.checkpoint.seen_evidence[evidenceRef].occurrences, 6);
});

test("evidence is re-scored after 25h TTL expires", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [makeObs("stale-pattern", "routine_runs:stale.js:run:failed")];

  const result1 = tick({ now: new Date("2026-05-01T10:00:00Z"), checkpoint: null, observations: obs, automationInventory: [] });
  assert.equal(parseScoringLine(result1).scored, 1, "first tick scores 1");

  // 26h later — TTL of 25h has fully expired
  const result2 = tick({ now: new Date("2026-05-02T12:00:00Z"), checkpoint: result1.checkpoint, observations: obs, automationInventory: [] });
  const s2 = parseScoringLine(result2);
  assert.equal(s2.deduped, 0, "after TTL expiry evidence should not be deduped");
  assert.equal(s2.scored, 1, "evidence is re-scored after TTL expiry");
});

test("seen_evidence LRU cap trims entries above 500", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);

  // 501 entries, all timestamped within the 25h TTL window
  const nowMs = new Date("2026-05-02T10:00:00Z").getTime();
  const cp = buildCheckpointWithEvidence(501, nowMs - 10 * 3600 * 1000); // entries 10h ago

  const result = tick({ now: new Date("2026-05-02T10:00:00Z"), checkpoint: cp, observations: [], automationInventory: [] });

  const seenCount = Object.keys(result.checkpoint.seen_evidence || {}).length;
  assert.ok(seenCount <= 500, `seen_evidence should be capped at 500, got ${seenCount}`);
});

test("composite key deduplication works when evidence_ref is absent", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [{
    signature: "no-ref-pattern",
    source: "kv_meta",
    category: "failure-pattern",
    summary: "Some kv meta observation without evidence_ref",
    occurrences: 1,
    weight: 1,
  }];

  const result1 = tick({ now: new Date("2026-05-02T10:00:00Z"), checkpoint: null, observations: obs, automationInventory: [] });
  assert.equal(parseScoringLine(result1).scored, 1, "first tick scores 1");

  const result2 = tick({ now: new Date("2026-05-02T10:01:00Z"), checkpoint: result1.checkpoint, observations: obs, automationInventory: [] });
  assert.equal(parseScoringLine(result2).deduped, 1, "composite key dedup works without evidence_ref");
});

test("multiple distinct evidence_refs are each scored independently", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [
    makeObs("pattern-a", "routine_runs:script-a.js:run:failed"),
    makeObs("pattern-b", "routine_runs:script-b.js:run:failed"),
    makeObs("pattern-c", "routine_runs:script-c.js:run:succeeded"),
  ];

  const result1 = tick({ now: new Date("2026-05-02T10:00:00Z"), checkpoint: null, observations: obs, automationInventory: [] });
  const s1 = parseScoringLine(result1);
  assert.equal(s1.scored, 3, "all 3 distinct observations scored on first tick");
  assert.equal(s1.deduped, 0);

  // Same observations next tick — all 3 deduped
  const result2 = tick({ now: new Date("2026-05-02T10:01:00Z"), checkpoint: result1.checkpoint, observations: obs, automationInventory: [] });
  const s2 = parseScoringLine(result2);
  assert.equal(s2.deduped, 3, "all 3 deduped on second tick");
  assert.equal(s2.scored, 0);
});
