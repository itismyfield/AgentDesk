const test = require("node:test");
const assert = require("node:assert/strict");
const { loadRoutine } = require("./support/routine-harness");

const ROUTINE_PATH = "routines/monitoring/automation-executor-v2.js";

const BASE_NOW = new Date("2026-05-12T10:00:00Z");

const MAX_ITERATIONS = 10;
const DISPATCH_RETRY_MS = 30 * 60 * 1000;
const MAX_DISPATCH_RETRIES = 3;

function makeReadyObs(cardId, overrides = {}) {
  return {
    source: "kanban_ready",
    pipeline_stage_id: overrides.pipeline_stage_id ?? "automation-candidate",
    card_id: cardId,
    summary: overrides.summary || `Test card ${cardId}`,
    metadata: {
      automation_candidate: {
        source: overrides.source || "test",
        dedupe_key: overrides.dedupe_key || `test:${cardId}`,
        ...(overrides.automation_candidate || {}),
      },
      program: {
        repo_dir: overrides.repo_dir || "/tmp/repo",
        description: overrides.description || "Fix something",
        allowed_write_paths: overrides.allowed_write_paths || ["src/"],
        metric_name: overrides.metric_name || "score",
        metric_target: overrides.metric_target ?? 0.9,
        current_iteration: overrides.current_iteration ?? 0,
        ...(overrides.program || {}),
      },
    },
  };
}

function makeDispatchedObs(cardId) {
  return {
    source: "kanban_dispatched",
    card_id: cardId,
  };
}

// --- No candidates ---

test("no ready observations → complete with no-candidates summary", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: [], automationInventory: [] });

  assert.equal(r.action, "complete");
  assert.ok(r.result.summary.includes("없음"), `summary should mention no candidates: ${r.result.summary}`);
  assert.equal(r.checkpoint.stats.ticks, 1);
  assert.equal(r.checkpoint.stats.dispatched, 0);
});

test("general kanban ready card without automation discriminator is skipped", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [makeReadyObs("card-general", {
    pipeline_stage_id: null,
    automation_candidate: null,
  })];
  delete obs[0].pipeline_stage_id;
  delete obs[0].metadata.automation_candidate;

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });

  assert.equal(r.action, "complete");
  assert.equal(r.checkpoint.stats.dispatched, 0);
  assert.equal(r.checkpoint.stats.skipped, 1);
});

test("automation candidate marker without complete program is skipped", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [makeReadyObs("card-incomplete-program")];
  delete obs[0].metadata.program.repo_dir;

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });

  assert.equal(r.action, "complete");
  assert.equal(r.checkpoint.stats.dispatched, 0);
  assert.equal(r.checkpoint.stats.skipped, 1);
});

// --- Single ready card dispatch ---

test("single ready card → agent action with correct prompt content", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const cardId = "card-abc-123";
  const obs = [makeReadyObs(cardId, {
    summary: "Improve login security",
    allowed_write_paths: ["src/auth/"],
    metric_name: "security_score",
    metric_target: 0.95,
    current_iteration: 0,
  })];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });

  assert.equal(r.action, "agent", "should emit agent action");
  // Card ID in prompt
  assert.ok(r.prompt.includes(cardId), "prompt must include card_id");
  // Branch name
  const expectedBranch = `automation/${cardId}/iter-1`;
  assert.ok(r.prompt.includes(expectedBranch), `prompt must include branch name: ${expectedBranch}`);
  // API endpoint
  assert.ok(r.prompt.includes(`/api/automation-candidates/${cardId}/iteration-result`),
    "prompt must include the iteration-result API endpoint");
  // allowed_write_paths
  assert.ok(r.prompt.includes("src/auth/"), "prompt must mention allowed_write_paths");
  // metric name
  assert.ok(r.prompt.includes("security_score"), "prompt must include metric_name");

  // Checkpoint: pending entry created
  assert.ok(r.checkpoint.pending[cardId], "pending entry should be created");
  assert.equal(r.checkpoint.pending[cardId].attempt_count, 1);
  assert.equal(r.checkpoint.stats.dispatched, 1);
});

test("prompt iteration number equals current_iteration + 1", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const cardId = "card-iter-test";
  // current_iteration = 2 → should dispatch iter 3
  const obs = [makeReadyObs(cardId, { current_iteration: 2 })];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });

  assert.equal(r.action, "agent");
  assert.ok(r.prompt.includes("iter-3"), "prompt branch should be iter-3");
  assert.ok(r.prompt.includes("3 /"), "prompt should show iteration 3");
});

test("previous iterations are included in prompt when automationInventory provided", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const cardId = "card-with-history";
  const obs = [makeReadyObs(cardId, { current_iteration: 1 })];
  const prevIterations = [
    { iteration: 1, status: "keep", metric_before: 0.7, metric_after: 0.8, description: "First attempt" },
  ];

  const r = tick({
    now: BASE_NOW,
    checkpoint: null,
    observations: obs,
    automationInventory: { [cardId]: prevIterations },
  });

  assert.equal(r.action, "agent");
  assert.ok(r.prompt.includes("First attempt"), "prompt should include previous iteration description");
  assert.ok(r.prompt.includes("0.7"), "prompt should include metric_before from previous iter");
});

test("previous iterations are read from API inventory response shape", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const cardId = "card-api-inventory";
  const obs = [makeReadyObs(cardId, { current_iteration: 1 })];
  const prevIterations = [
    { iteration: 1, status: "keep", metric_before: 5, metric_after: 3, description: "API shaped history" },
  ];

  const r = tick({
    now: BASE_NOW,
    checkpoint: null,
    observations: obs,
    automationInventory: { card_id: cardId, iterations: prevIterations },
  });

  assert.equal(r.action, "agent");
  assert.ok(r.prompt.includes("API shaped history"), "prompt should include API-shaped history");
  assert.ok(r.prompt.includes("5"), "prompt should include metric_before from API-shaped history");
});

test("array automation inventory is filtered by card before prompt injection", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const cardId = "card-array-inventory";
  const obs = [makeReadyObs(cardId, { current_iteration: 1 })];

  const r = tick({
    now: BASE_NOW,
    checkpoint: null,
    observations: obs,
    automationInventory: [
      { pattern_id: "routine-only", status: "active", description: "Unrelated routine inventory" },
      { card_id: "other-card", iteration: 1, status: "discard", description: "Other card history" },
      { card_id: cardId, iterations: [
        { iteration: 1, status: "keep", metric_before: 4, metric_after: 2, description: "Matching card history" },
      ] },
    ],
  });

  assert.equal(r.action, "agent");
  assert.ok(r.prompt.includes("Matching card history"), "prompt should include matching card history");
  assert.ok(!r.prompt.includes("Unrelated routine inventory"), "prompt must not include routine inventory rows");
  assert.ok(!r.prompt.includes("Other card history"), "prompt must not include another card history");
});

test("previous iterations are read from keyed API inventory response shape", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const cardId = "card-keyed-api-inventory";
  const obs = [makeReadyObs(cardId, { current_iteration: 1 })];
  const prevIterations = [
    { iteration: 1, status: "discard", metric_before: 5, metric_after: 5, description: "Keyed API history" },
  ];

  const r = tick({
    now: BASE_NOW,
    checkpoint: null,
    observations: obs,
    automationInventory: { [cardId]: { iterations: prevIterations } },
  });

  assert.equal(r.action, "agent");
  assert.ok(r.prompt.includes("Keyed API history"), "prompt should include keyed API-shaped history");
});

// --- kanban_dispatched suppression ---

test("card in kanban_dispatched observation → suppressed (complete)", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const cardId = "card-dispatched";
  const obs = [
    makeReadyObs(cardId),
    makeDispatchedObs(cardId),
  ];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });

  assert.equal(r.action, "complete", "card already dispatched should not produce agent action");
  assert.equal(r.checkpoint.stats.dispatched, 0);
  assert.equal(r.checkpoint.stats.skipped, 1);
});

test("kanban_dispatched by evidence_ref format is also suppressed", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const cardId = "card-evidence-ref";
  const obs = [
    makeReadyObs(cardId),
    // evidence_ref format instead of card_id field
    { source: "kanban_dispatched", evidence_ref: `kanban_cards:${cardId}` },
  ];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });

  assert.equal(r.action, "complete");
  assert.equal(r.checkpoint.stats.skipped, 1);
});

// --- Checkpoint.dispatched suppression ---

test("card already in checkpoint.dispatched → skipped", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const cardId = "card-in-checkpoint";
  const obs = [makeReadyObs(cardId)];
  const checkpoint = {
    version: 2,
    dispatched: {
      [cardId]: { dispatched_at: BASE_NOW.toISOString(), status: "ok", iteration: 1 },
    },
    pending: {},
    stats: { ticks: 0, dispatched: 0, skipped: 0, max_iterations_reached: 0 },
  };

  const r = tick({ now: BASE_NOW, checkpoint, observations: obs, automationInventory: [] });

  assert.equal(r.action, "complete", "checkpointed card should not be re-dispatched");
  assert.equal(r.checkpoint.stats.skipped, 1);
});

// --- MAX_ITERATIONS boundary ---

test("card at iteration > MAX_ITERATIONS → max_iterations_reached, skipped", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const cardId = "card-max-iter";
  // current_iteration = 10 → next would be iter 11 > MAX_ITERATIONS(10)
  const obs = [makeReadyObs(cardId, { current_iteration: MAX_ITERATIONS })];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });

  assert.equal(r.action, "complete");
  assert.equal(r.checkpoint.stats.max_iterations_reached, 1);
  assert.equal(r.checkpoint.dispatched[cardId].status, "max_iterations_reached");
  assert.equal(r.checkpoint.stats.dispatched, 0);
});

test("card at iteration == MAX_ITERATIONS (iter 10) is still dispatched", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const cardId = "card-at-limit";
  // current_iteration = 9 → next is iter 10 == MAX_ITERATIONS → still dispatch
  const obs = [makeReadyObs(cardId, { current_iteration: 9 })];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });

  assert.equal(r.action, "agent", "iter 10 should still be dispatched");
  assert.ok(r.prompt.includes("iter-10"), "prompt should show iter-10");
});

// --- Retry window ---

test("card retried within DISPATCH_RETRY_MS → skipped", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const cardId = "card-retry-window";
  const obs = [makeReadyObs(cardId)];
  // last_attempted_at is 10 min ago (< 30 min window)
  const recentlyAttempted = new Date(BASE_NOW.getTime() - 10 * 60 * 1000).toISOString();
  const checkpoint = {
    version: 2,
    dispatched: {},
    pending: {
      [cardId]: {
        first_attempted_at: recentlyAttempted,
        last_attempted_at: recentlyAttempted,
        attempt_count: 1,
        iteration: 1,
      },
    },
    stats: { ticks: 0, dispatched: 0, skipped: 0, max_iterations_reached: 0 },
  };

  const r = tick({ now: BASE_NOW, checkpoint, observations: obs, automationInventory: [] });

  assert.equal(r.action, "complete", "card within retry window should be skipped");
  assert.equal(r.checkpoint.stats.skipped, 1);
});

test("card attempted after DISPATCH_RETRY_MS → re-dispatched", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const cardId = "card-retry-expired";
  const obs = [makeReadyObs(cardId)];
  // last_attempted_at is 40 min ago (> 30 min window)
  const expiredAttempt = new Date(BASE_NOW.getTime() - 40 * 60 * 1000).toISOString();
  const checkpoint = {
    version: 2,
    dispatched: {},
    pending: {
      [cardId]: {
        first_attempted_at: expiredAttempt,
        last_attempted_at: expiredAttempt,
        attempt_count: 1,
        iteration: 1,
      },
    },
    stats: { ticks: 0, dispatched: 0, skipped: 0, max_iterations_reached: 0 },
  };

  const r = tick({ now: BASE_NOW, checkpoint, observations: obs, automationInventory: [] });

  assert.equal(r.action, "agent", "card past retry window should be re-dispatched");
  assert.equal(r.checkpoint.pending[cardId].attempt_count, 2);
});

// --- MAX_DISPATCH_RETRIES ---

test("card at MAX_DISPATCH_RETRIES limit → permanently skipped", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const cardId = "card-max-retries";
  const obs = [makeReadyObs(cardId)];
  // attempt_count == MAX_DISPATCH_RETRIES(3)
  const expiredAttempt = new Date(BASE_NOW.getTime() - 40 * 60 * 1000).toISOString();
  const checkpoint = {
    version: 2,
    dispatched: {},
    pending: {
      [cardId]: {
        first_attempted_at: expiredAttempt,
        last_attempted_at: expiredAttempt,
        attempt_count: MAX_DISPATCH_RETRIES,
        iteration: 1,
      },
    },
    stats: { ticks: 0, dispatched: 0, skipped: 0, max_iterations_reached: 0 },
  };

  const r = tick({ now: BASE_NOW, checkpoint, observations: obs, automationInventory: [] });

  assert.equal(r.action, "complete", "card at max retries should be permanently skipped");
  assert.equal(r.checkpoint.stats.dispatched, 0, "should not count as dispatched");
});

// --- Checkpoint version mismatch ---

test("stale checkpoint version is reset to empty", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [];
  const staleCheckpoint = {
    version: 1, // old version
    dispatched_signatures: { "some-sig": true },
    stats: { ticks: 999 },
  };

  const r = tick({ now: BASE_NOW, checkpoint: staleCheckpoint, observations: obs, automationInventory: [] });

  assert.equal(r.checkpoint.version, 2, "checkpoint version should be reset to 2");
  assert.equal(r.checkpoint.stats.ticks, 1, "stats should reset (stale checkpoint discarded)");
});

// --- Multiple ready cards: first wins ---

test("two ready cards → only first is dispatched per tick", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [
    makeReadyObs("card-first"),
    makeReadyObs("card-second"),
  ];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });

  assert.equal(r.action, "agent", "should dispatch first card");
  assert.ok(r.prompt.includes("card-first"), "first card should be dispatched");
  assert.ok(!r.prompt.includes("card-second"), "second card should not appear in this tick's prompt");
  assert.equal(r.checkpoint.stats.dispatched, 1);
});
