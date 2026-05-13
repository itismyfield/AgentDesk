// End-to-end pipeline simulation: Recommender → Detector → Executor
// Verifies the full automation candidate lifecycle across all three routines.

const test = require("node:test");
const assert = require("node:assert/strict");
const { loadRoutine } = require("./support/routine-harness");

const RECOMMENDER_PATH = "routines/monitoring/automation-candidate-recommender.js";
const DETECTOR_PATH = "routines/monitoring/automation-candidate-detector.js";
const EXECUTOR_PATH = "routines/monitoring/automation-executor.js";

const BASE_NOW = new Date("2026-05-02T10:00:00Z");
const SIGNATURE = "routine-candidate:my-repeated-script.js";

// --- Observation factories ---

function makeRunObs(signature, evidenceRef, opts = {}) {
  return {
    signature,
    evidence_ref: evidenceRef,
    category: opts.category || "routine-candidate",
    summary: `Repeated failure: ${signature}`,
    occurrences: opts.occurrences || 1,
    weight: opts.weight || 1,
    timestamp: opts.timestamp || BASE_NOW.toISOString(),
  };
}

// Simulates precomputed_observation_from_kv output for candidate_review marker
function makeCandidateReviewKvObs(signature, candidate, overrides = {}) {
  return {
    key: `routine_observation:candidate_review:${signature}`,
    evidence_ref: `kv_meta:routine_observation:candidate_review:${signature}`,
    value: {
      signature,
      score: candidate.score || 85,
      evidence_count: candidate.evidence_count || 8,
      category: candidate.category || "routine-candidate",
      suggested_automation: candidate.suggested_automation || "자동화 제안",
      outcome_summary: candidate.outcome_summary || "결과 요약",
      last_seen_at: overrides.last_seen_at || BASE_NOW.toISOString(),
    },
    source: "candidate_review",
    category: "routine-candidate",
    signature: `routine-candidate:${signature}`,
    summary: `candidate_review: ${signature}`,
    timestamp: BASE_NOW.toISOString(),
    occurrences: 1,
    weight: 1,
  };
}

// Simulates precomputed_observation_from_kv output for candidate_approved marker
function makeCandidateApprovedKvObs(signature, opts = {}) {
  const approvedAt = opts.approved_at || BASE_NOW.toISOString();
  return {
    key: `routine_observation:candidate_approved:${signature}`,
    evidence_ref: `kv_meta:routine_observation:candidate_approved:${signature}`,
    value: {
      signature,
      score: opts.score || 85,
      category: opts.category || "routine-candidate",
      approved_at: approvedAt,
      suggested_automation: opts.suggested_automation || "자동화 제안",
      outcome_summary: opts.outcome_summary || "결과 요약",
    },
    source: "candidate_approved",
    category: "routine-candidate",
    signature: `routine-candidate:${signature}`,
    summary: `candidate_approved: ${signature}`,
    timestamp: approvedAt,
    occurrences: 1,
    weight: 1,
  };
}

// Simulates precomputed_observation_from_kv output for candidate_dispatched marker
function makeCandidateDispatchedKvObs(signature, opts = {}) {
  const dispatchedAt = opts.dispatched_at || BASE_NOW.toISOString();
  return {
    key: `routine_observation:candidate_dispatched:${signature}`,
    evidence_ref: `kv_meta:routine_observation:candidate_dispatched:${signature}`,
    value: {
      signature,
      dispatched_at: dispatchedAt,
      category: opts.category || "routine-candidate",
    },
    source: "candidate_dispatched",
    category: "routine-candidate",
    signature: `routine-candidate:${signature}`,
    summary: `candidate_dispatched: ${signature}`,
    timestamp: dispatchedAt,
    occurrences: 1,
    weight: 1,
  };
}

// Build 8 distinct run observations for the same pattern to exceed SCORE_THRESHOLD=80
function makeRunObsSet(signature, count = 8) {
  return Array.from({ length: count }, (_, i) =>
    makeRunObs(signature, `routine_runs:${signature}:run:failed:sample-${i}`)
  );
}

// --- Phase 1: Recommender reaches escalation threshold ---

test("pipeline phase 1: recommender accumulates evidence and escalates candidate", () => {
  const { tick } = loadRoutine(RECOMMENDER_PATH);

  const obs = makeRunObsSet(SIGNATURE);
  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });

  assert.equal(r.action, "agent", "recommender should escalate when score >= 80 and evidence >= 5");
  assert.ok(r.prompt.includes(SIGNATURE), "prompt should reference the candidate signature");
  assert.ok(
    r.prompt.includes("<determine from your workspace context>"),
    "materialize draft should use the detector-recognized repo_dir placeholder"
  );
  assert.ok(
    !r.prompt.includes("<required: absolute repo path>"),
    "materialize draft must not include stale repo_dir placeholder"
  );
  assert.ok(r.checkpoint.candidates[SIGNATURE], "candidate should be in checkpoint");
  assert.ok(
    r.checkpoint.candidates[SIGNATURE].score >= 80,
    `candidate score should be >= 80, got ${r.checkpoint.candidates[SIGNATURE].score}`
  );
});

test("pipeline phase 1b: ROI-aware high-impact category escalates with three evidence points", () => {
  const { tick } = loadRoutine(RECOMMENDER_PATH);
  const signature = "session-pattern:maker";
  const obs = [
    makeRunObs(signature, "session_transcripts:maker", {
      category: "session-pattern",
      occurrences: 3,
      weight: 2,
    }),
  ];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });

  assert.equal(r.action, "agent", "session-pattern should use ROI-aware gate below the global evidence=5 threshold");
  const candidate = r.checkpoint.candidates[signature];
  assert.equal(candidate.evidence_count, 3);
  assert.equal(candidate.category, "session-pattern");
  assert.ok(candidate.score >= 60, `candidate score should satisfy ROI gate, got ${candidate.score}`);
  assert.ok(r.prompt.includes("gate=60/3"), "prompt should explain the category-specific gate");
});

// --- Phase 2: Detector passes quality gate for a candidate_review observation ---

test("pipeline phase 2: detector quality-gates candidate_review and emits agent", () => {
  const { tick } = loadRoutine(DETECTOR_PATH);

  const candidate = { score: 87, evidence_count: 8, category: "routine-candidate" };
  const reviewObs = [makeCandidateReviewKvObs(SIGNATURE, candidate)];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: reviewObs, automationInventory: [] });

  assert.equal(r.action, "agent", "detector should emit agent action for valid candidate_review");
  assert.ok(r.prompt.includes(SIGNATURE), "approval prompt should include the signature");
  assert.ok(
    r.prompt.includes(`routine_observation:candidate_approved:${SIGNATURE}`),
    "prompt should instruct agent to write candidate_approved kv_meta"
  );
  assert.equal(r.checkpoint.seen_candidates[SIGNATURE].status, "emitted");
  assert.ok(
    r.checkpoint.seen_candidates[SIGNATURE].status !== "approved" &&
    r.checkpoint.seen_candidates[SIGNATURE].status !== "dispatched",
    "candidate should not be prematurely approved/dispatched in checkpoint"
  );
});

// --- Phase 3: Executor dispatches approved candidate ---

test("pipeline phase 3: executor dispatches approved candidate and does not pre-mark dispatched", () => {
  const { tick } = loadRoutine(EXECUTOR_PATH);

  const approvedObs = [makeCandidateApprovedKvObs(SIGNATURE)];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: approvedObs, automationInventory: [] });

  assert.equal(r.action, "agent", "executor should emit agent dispatch action for approved candidate");
  assert.ok(r.prompt.includes(SIGNATURE), "dispatch prompt should include the signature");
  assert.ok(
    r.prompt.includes(`routine_observation:candidate_dispatched:${SIGNATURE}`),
    "prompt should instruct agent to write candidate_dispatched kv_meta"
  );
  assert.ok(
    !r.checkpoint.dispatched_signatures[SIGNATURE],
    "dispatched_signatures must NOT be pre-set before durable kv_meta is observed"
  );
});

// --- Phase 4: Executor skips re-dispatch after durable dispatched marker appears ---

test("pipeline phase 4: executor skips candidate once durable dispatched kv_meta is observed", () => {
  const { tick } = loadRoutine(EXECUTOR_PATH);

  const dispatchedAt = new Date(BASE_NOW.getTime() - 2 * 3600_000).toISOString();
  const obs = [
    makeCandidateApprovedKvObs(SIGNATURE),
    makeCandidateDispatchedKvObs(SIGNATURE, { dispatched_at: dispatchedAt }),
  ];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });

  assert.equal(r.action, "complete", "executor should complete when durable dispatched marker exists");
  assert.equal(
    r.checkpoint.dispatched_signatures[SIGNATURE],
    dispatchedAt,
    "checkpoint should preserve the actual dispatch time, not now"
  );
});

// --- Phase 5: Recommender suppresses candidate after dispatched marker appears ---

test("pipeline phase 5: recommender suppresses re-recommendation after dispatched kv_meta observed", () => {
  const { tick } = loadRoutine(RECOMMENDER_PATH);

  // Regular run observations plus a dispatched marker
  const runObs = makeRunObsSet(SIGNATURE);
  const dispatchedObs = makeCandidateDispatchedKvObs(SIGNATURE);
  const allObs = [...runObs, dispatchedObs];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: allObs, automationInventory: [] });

  // Should not escalate because SIGNATURE is now suppressed via dispatched marker
  assert.equal(r.action, "complete", "recommender should not escalate a dispatched candidate");
  assert.ok(
    !r.checkpoint.candidates[SIGNATURE] ||
    r.checkpoint.candidates[SIGNATURE].state !== "recommended",
    "dispatched candidate should be dropped or not in recommended state"
  );
  assert.ok(
    (r.result.suppression_summary || "").includes("dispatched") ||
    !r.checkpoint.candidates[SIGNATURE],
    "suppression summary should mention dispatched suppression or candidate removed"
  );
});

// --- Phase 6: Full sequential pipeline simulation ---

test("pipeline phase 6: full sequential recommender→detector→executor flow", () => {
  const recommender = loadRoutine(RECOMMENDER_PATH);
  const detector = loadRoutine(DETECTOR_PATH);
  const executor = loadRoutine(EXECUTOR_PATH);

  const t0 = BASE_NOW;

  // Step 1: Recommender sees enough evidence and escalates
  const runObs = makeRunObsSet(SIGNATURE);
  const r1 = recommender.tick({ now: t0, checkpoint: null, observations: runObs, automationInventory: [] });
  assert.equal(r1.action, "agent", "step 1: recommender escalates");
  const recommenderCp = r1.checkpoint;

  // Step 2: (Agent writes candidate_review kv_meta) → Detector sees it
  const t1 = new Date(t0.getTime() + 5 * 60_000);
  const candidate = recommenderCp.candidates[SIGNATURE] || {};
  const reviewObs = [makeCandidateReviewKvObs(SIGNATURE, { score: candidate.score || 85, evidence_count: candidate.evidence_count || 8 })];
  const r2 = detector.tick({ now: t1, checkpoint: null, observations: reviewObs, automationInventory: [] });
  assert.equal(r2.action, "agent", "step 2: detector emits approval request");
  const detectorCp = r2.checkpoint;

  // Step 3: (Agent writes candidate_approved kv_meta) → Executor sees it
  const t2 = new Date(t1.getTime() + 5 * 60_000);
  const approvedObs = [makeCandidateApprovedKvObs(SIGNATURE, { score: candidate.score || 85 })];
  const r3 = executor.tick({ now: t2, checkpoint: null, observations: approvedObs, automationInventory: [] });
  assert.equal(r3.action, "agent", "step 3: executor dispatches");
  assert.ok(!r3.checkpoint.dispatched_signatures[SIGNATURE], "dispatched not pre-set");

  // Step 4: (Agent writes candidate_dispatched kv_meta) → Executor now skips on next tick
  const t3 = new Date(t2.getTime() + 5 * 60_000);
  const dispatchedAt = t2.toISOString();
  const dispatchedObs = [makeCandidateDispatchedKvObs(SIGNATURE, { dispatched_at: dispatchedAt })];
  const obsWithDispatched = [...approvedObs, ...dispatchedObs];
  const r4 = executor.tick({ now: t3, checkpoint: r3.checkpoint, observations: obsWithDispatched, automationInventory: [] });
  assert.equal(r4.action, "complete", "step 4: executor skips already dispatched candidate");
  assert.equal(r4.checkpoint.dispatched_signatures[SIGNATURE], dispatchedAt);

  // Step 5: Recommender on next cycle sees dispatched marker → suppresses candidate
  const t4 = new Date(t3.getTime() + 5 * 60_000);
  const allObs = [...runObs, ...dispatchedObs];
  const r5 = recommender.tick({ now: t4, checkpoint: recommenderCp, observations: allObs, automationInventory: [] });
  assert.equal(r5.action, "complete", "step 5: recommender suppresses dispatched candidate");

  // Step 6: Detector on next cycle — if candidate_review marker expires or is missing, no action
  const t5 = new Date(t4.getTime() + 5 * 60_000);
  const r6 = detector.tick({ now: t5, checkpoint: detectorCp, observations: dispatchedObs, automationInventory: [] });
  assert.equal(r6.action, "complete", "step 6: detector has no pending reviews");
});

// --- Normalized observation variants (simulating Rust provider output with both key+evidence_ref) ---

test("pipeline: normalized kv_meta obs (key+evidence_ref) works across all three routines", () => {
  const detector = loadRoutine(DETECTOR_PATH);
  const executor = loadRoutine(EXECUTOR_PATH);

  const reviewObs = [makeCandidateReviewKvObs(SIGNATURE, { score: 90, evidence_count: 10 })];
  const r1 = detector.tick({ now: BASE_NOW, checkpoint: null, observations: reviewObs, automationInventory: [] });
  assert.equal(r1.action, "agent", "detector handles kv obs with both key and evidence_ref");

  const approvedObs = [makeCandidateApprovedKvObs(SIGNATURE)];
  const r2 = executor.tick({ now: BASE_NOW, checkpoint: null, observations: approvedObs, automationInventory: [] });
  assert.equal(r2.action, "agent", "executor handles kv obs with both key and evidence_ref");
});
