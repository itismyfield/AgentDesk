const test = require("node:test");
const assert = require("node:assert/strict");
const { loadRoutine } = require("./support/routine-harness");

const ROUTINE_PATH = "routines/local-worktree-gc.js";

function dispatch() {
  const { routine, tick } = loadRoutine(ROUTINE_PATH);
  const result = tick({
    now: new Date("2026-07-21T00:00:00Z"),
    checkpoint: null,
    observations: [],
    automationInventory: [],
  });
  return { routine, result };
}

test("local worktree routine is explicitly report-only", () => {
  const { routine, result } = dispatch();

  assert.equal(routine.name, "local-worktree-gc");
  assert.equal(routine.metadata.safety_mode, "report_only");
  assert.equal(result.action, "agent");
  assert.match(result.prompt, /report-only/);
  assert.match(result.prompt, /"destructive_actions":0/);
});

test("local worktree routine dispatches zero deletion commands", () => {
  const { result } = dispatch();
  const destructiveCommands = [
    /\brm\s+-rf\b/i,
    /git\s+worktree\s+remove/i,
    /git\s+worktree\s+prune/i,
    /git\s+branch\s+-D/i,
    /git\s+update-ref\s+-d/i,
    /--force\b/i,
  ];

  for (const command of destructiveCommands) {
    assert.doesNotMatch(result.prompt, command);
  }
});

test("local worktree routine preserves unknown active and registered candidates", () => {
  const { result } = dispatch();

  assert.match(
    result.prompt,
    /Unknown, active, locked, dirty, unmerged, and registered worktrees must all be preserved\./,
  );
  assert.match(result.prompt, /Inspection failure produces `unknown`; it never authorizes cleanup\./);
  assert.match(result.prompt, /positive_ownership_proof/);
  assert.match(result.prompt, /ownership_proof_absence_reason/);
  assert.match(result.prompt, /path naming, age, lock state, registration, ref, HEAD ancestry, cleanliness/);
});
