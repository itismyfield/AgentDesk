const test = require("node:test");
const assert = require("node:assert/strict");
const { loadRoutine } = require("./support/routine-harness");

const ROUTINE_PATH = "routines/monitoring/automation-executor.js";

const BASE_NOW = new Date("2026-05-02T10:00:00Z");

function makeApprovedObs(signature, overrides = {}) {
  return {
    key: `routine_observation:candidate_approved:${signature}`,
    value: {
      signature,
      score: overrides.score !== undefined ? overrides.score : 85,
      category: overrides.category || "routine-candidate",
      approved_at: overrides.approved_at || BASE_NOW.toISOString(),
      suggested_automation: overrides.suggested_automation || "자동화 제안",
      outcome_summary: overrides.outcome_summary || "결과 요약",
    },
    summary: `candidate_approved for ${signature}`,
  };
}

function makeNormalizedApprovedObs(signature, overrides = {}) {
  const obs = makeApprovedObs(signature, overrides);
  return {
    evidence_ref: `kv_meta:${obs.key}`,
    value: obs.value,
    summary: obs.summary,
  };
}

function makeDispatchedObs(signature, overrides = {}) {
  return {
    key: `routine_observation:candidate_dispatched:${signature}`,
    value: {
      signature,
      dispatched_at: overrides.dispatched_at,
      timestamp: overrides.timestamp,
      category: overrides.category || "routine-candidate",
    },
    summary: `candidate_dispatched for ${signature}`,
  };
}

test("approved candidate triggers agent dispatch with GitHub Issue prompt", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [makeApprovedObs("dispatch-sig")];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });
  assert.equal(r.action, "agent", "should emit agent action for approved candidate");
  assert.ok(r.prompt.includes("dispatch-sig"), "prompt should include signature");
  assert.ok(r.prompt.includes("GitHub Issue"), "prompt should mention GitHub Issue");
  assert.ok(r.prompt.includes("routine_observation:candidate_dispatched:dispatch-sig"),
    "prompt should instruct to write candidate_dispatched kv_meta");
  assert.ok(!r.checkpoint.dispatched_signatures["dispatch-sig"],
    "signature should be checkpointed only after durable candidate_dispatched kv_meta is observed");
});

test("normalized approved observation triggers agent dispatch", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [makeNormalizedApprovedObs("normalized-dispatch-sig")];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });
  assert.equal(r.action, "agent", "normalized kv_meta observation should emit agent action");
  assert.ok(r.prompt.includes("normalized-dispatch-sig"));
});

test("already dispatched candidate is skipped", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [
    makeApprovedObs("skip-sig"),
    makeDispatchedObs("skip-sig"),
  ];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });
  assert.equal(r.action, "complete", "already dispatched candidate should not produce agent action");
  assert.equal(r.checkpoint.stats.skipped_already_dispatched, 1, "skipped counter should be 1");
  assert.ok(r.checkpoint.dispatched_signatures["skip-sig"],
    "durable dispatched observation should be mirrored into checkpoint");
});

test("durable dispatched observation preserves marker timestamp", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const dispatchedAt = new Date(BASE_NOW.getTime() - 6 * 24 * 3600_000).toISOString();
  const obs = [
    makeApprovedObs("old-dispatch-sig"),
    makeDispatchedObs("old-dispatch-sig", { dispatched_at: dispatchedAt }),
  ];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });
  assert.equal(r.action, "complete", "already dispatched candidate should not produce agent action");
  assert.equal(
    r.checkpoint.dispatched_signatures["old-dispatch-sig"],
    dispatchedAt,
    "checkpoint should mirror durable marker time instead of now"
  );
});

test("no approved candidates returns complete with empty summary", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: [], automationInventory: [] });
  assert.equal(r.action, "complete", "no candidates → complete");
  assert.equal(r.result.approved_count, 0, "approved_count should be 0");
});
