const test = require("node:test");
const assert = require("node:assert/strict");
const { loadRoutine } = require("./support/routine-harness");

const ROUTINE_PATH = "routines/monitoring/daily-log-digest.js";

test("daily log digest uses the monitoring agent-action frame", () => {
  const { routine, tick } = loadRoutine(ROUTINE_PATH);
  const now = new Date("2026-07-14T00:10:00Z");

  const result = tick({ now, checkpoint: null, observations: [], automationInventory: [] });

  assert.equal(routine.name, "Daily dcserver Log Digest");
  assert.equal(result.action, "agent");
  assert.match(result.prompt, /python3 "\$ROOT\/routines\/monitoring\/daily_log_digest\.py"/);
  assert.match(result.prompt, /Do not call `gh issue create` directly/);
  assert.match(result.prompt, /AGENTDESK_LOG_DIGEST_CREATE_ISSUE=confirmed/);
  assert.equal(result.checkpoint.last_dispatched_day, "2026-07-14");
});

test("daily checkpoint suppresses a second digest on the same day", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const first = tick({
    now: new Date("2026-07-14T00:10:00Z"),
    checkpoint: null,
    observations: [],
    automationInventory: [],
  });
  const duplicate = tick({
    now: new Date("2026-07-14T12:10:00Z"),
    checkpoint: first.checkpoint,
    observations: [],
    automationInventory: [],
  });

  assert.equal(duplicate.action, "complete");
  assert.equal(duplicate.result.status, "already_dispatched");
});

test("daily checkpoint dispatches again on the next day", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const result = tick({
    now: new Date("2026-07-15T00:10:00Z"),
    checkpoint: { version: 1, last_dispatched_day: "2026-07-14" },
    observations: [],
    automationInventory: [],
  });

  assert.equal(result.action, "agent");
  assert.equal(result.checkpoint.last_dispatched_day, "2026-07-15");
});

test("daily checkpoint day key follows the routine's default KST timezone", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const result = tick({
    now: new Date("2026-07-14T16:10:00Z"),
    checkpoint: { version: 1, last_dispatched_day: "2026-07-14" },
    observations: [],
    automationInventory: [],
  });

  assert.equal(result.action, "agent");
  assert.equal(result.checkpoint.last_dispatched_day, "2026-07-15");
});
