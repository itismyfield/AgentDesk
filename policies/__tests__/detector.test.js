const test = require("node:test");
const assert = require("node:assert/strict");
const { loadRoutine } = require("./support/routine-harness");

const ROUTINE_PATH = "routines/monitoring/automation-candidate-detector.js";

const BASE_NOW = new Date("2026-05-02T10:00:00Z");

function makeReviewObs(signature, overrides = {}) {
  return {
    key: `routine_observation:candidate_review:${signature}`,
    value: {
      signature,
      score: overrides.score !== undefined ? overrides.score : 85,
      evidence_count: overrides.evidence_count !== undefined ? overrides.evidence_count : 8,
      category: overrides.category || "routine-candidate",
      suggested_automation: overrides.suggested_automation || "자동화 제안",
      outcome_summary: overrides.outcome_summary || "결과 요약",
      last_seen_at: overrides.last_seen_at || BASE_NOW.toISOString(),
    },
    summary: `candidate_review for ${signature}`,
  };
}

function makeNormalizedReviewObs(signature, overrides = {}) {
  const obs = makeReviewObs(signature, overrides);
  return {
    evidence_ref: `kv_meta:${obs.key}`,
    value: obs.value,
    summary: obs.summary,
  };
}

function makeApprovedObs(signature) {
  return {
    key: `routine_observation:candidate_approved:${signature}`,
    summary: `candidate_approved for ${signature}`,
  };
}

function makeDispatchedObs(signature) {
  return {
    key: `routine_observation:candidate_dispatched:${signature}`,
    summary: `candidate_dispatched for ${signature}`,
  };
}

test("quality gate pass: emits agent action for valid candidate", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [makeReviewObs("valid-sig")];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });
  assert.equal(r.action, "agent", "should emit agent action for valid candidate");
  assert.ok(r.prompt.includes("valid-sig"), "prompt should include candidate signature");
  assert.ok(r.prompt.includes("routine_observation:candidate_approved:valid-sig"),
    "prompt should instruct to write candidate_approved kv_meta");
});

test("normalized provider candidate_review emits agent action", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [makeNormalizedReviewObs("normalized-sig")];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });
  assert.equal(r.action, "agent", "normalized kv_meta observation should emit agent action");
  assert.ok(r.prompt.includes("routine_observation:candidate_approved:normalized-sig"));
});

test("already approved candidate is skipped without re-emitting", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [
    makeReviewObs("approved-sig"),
    makeApprovedObs("approved-sig"),
  ];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });
  assert.equal(r.action, "complete", "should return complete when candidate already approved");
  assert.equal(r.result.review_count, 1, "should have processed 1 review obs");
});

test("evidence_age reject: candidate older than 48h is rejected at quality gate", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const oldTimestamp = new Date(BASE_NOW.getTime() - 49 * 3600_000).toISOString();
  const obs = [makeReviewObs("stale-sig", { last_seen_at: oldTimestamp })];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });
  assert.equal(r.action, "complete", "stale candidate should not produce agent action");
  assert.equal(r.checkpoint.stats.skipped_quality_gate, 1, "skipped_quality_gate should be 1");
});

test("previously emitted candidate is not re-emitted on second tick", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [makeReviewObs("dup-emit-sig")];

  // First tick: emits agent action
  const r1 = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });
  assert.equal(r1.action, "agent", "first tick should emit");

  // Second tick: same obs, same checkpoint — should not re-emit
  const nowT2 = new Date(BASE_NOW.getTime() + 60_000);
  const r2 = tick({ now: nowT2, checkpoint: r1.checkpoint, observations: obs, automationInventory: [] });
  assert.equal(r2.action, "complete", "second tick should not re-emit");
});

test("emitted candidate is retried when durable approval is still missing", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [makeReviewObs("retry-sig")];

  const r1 = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });
  assert.equal(r1.action, "agent", "first tick should emit");

  const nowT2 = new Date(BASE_NOW.getTime() + 61 * 60_000);
  const r2 = tick({ now: nowT2, checkpoint: r1.checkpoint, observations: obs, automationInventory: [] });
  assert.equal(r2.action, "agent", "stale emit should retry when no approval marker exists");
  assert.equal(r2.checkpoint.seen_candidates["retry-sig"].first_seen_at, BASE_NOW.toISOString());
  assert.equal(r2.checkpoint.seen_candidates["retry-sig"].last_emitted_at, nowT2.toISOString());
});

test("multiple review observations only mark the emitted candidate", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [
    makeReviewObs("first-sig"),
    makeReviewObs("second-sig"),
  ];

  const r1 = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });
  assert.equal(r1.action, "agent", "first tick should emit one agent action");
  assert.ok(r1.prompt.includes("first-sig"), "first candidate should be emitted first");
  assert.equal(r1.checkpoint.seen_candidates["first-sig"].status, "emitted");
  assert.equal(
    r1.checkpoint.seen_candidates["second-sig"],
    undefined,
    "non-emitted candidates must remain available for later ticks"
  );

  const r2 = tick({
    now: new Date(BASE_NOW.getTime() + 60_000),
    checkpoint: r1.checkpoint,
    observations: obs,
    automationInventory: [],
  });
  assert.equal(r2.action, "agent", "second tick should emit the remaining candidate");
  assert.ok(r2.prompt.includes("second-sig"), "second candidate should be emitted on the next tick");
  assert.equal(r2.checkpoint.seen_candidates["second-sig"].status, "emitted");
});
