import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { aggregateText, run } from "../timeout-shadow-gate.mjs";

function record(section, overrides = {}) {
  return JSON.stringify({
    target: "agentdesk::timeout_shadow",
    card_id: "card-1",
    section,
    js_decision: "retry",
    reducer_decision: "retry",
    agree: true,
    ...overrides
  });
}

function shadow(section, overrides) {
  return `[timeout_shadow] ${record(section, overrides)}`;
}

test("aggregates current and rotated logs in deterministic section order", () => {
  const directory = mkdtempSync(join(tmpdir(), "timeout-shadow-gate-"));
  try {
    const current = join(directory, "dcserver.stdout.log");
    const rotated = join(directory, "dcserver.stdout.log.1");
    writeFileSync(current, `2026-07-28T00:00:00Z INFO ${shadow("_section_A")}\n`);
    writeFileSync(rotated, `prefix ${shadow("_section_J", { reducer_decision: "incomparable", agree: false, incomparable: true })}\n`);

    const result = run([current, rotated], "");
    assert.equal(result.exitCode, 0);
    assert.equal(result.output, JSON.stringify({
      _section_A: { total: 1, comparable: 1, agreement: 1, divergence: 0, error: 0 },
      _section_J: { total: 1, incomparable: 1, ratio: 1, error: 0 },
      _unclassified: { malformed: 0 }
    }));
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("reads stdin and filters only records carrying an out-of-range timestamp", () => {
  const input = [
    `2026-07-26T00:00:00Z INFO ${shadow("_section_A")}`,
    shadow("_section_A")
  ].join("\n");
  const result = run(["--stdin", "--since", "2026-07-27T00:00:00Z"], input);
  assert.equal(result.exitCode, 0);
  assert.deepEqual(JSON.parse(result.output)._section_A, {
    total: 1, comparable: 1, agreement: 1, divergence: 0, error: 0
  });
});

test("counts malformed shadow records but ignores unrelated log noise", () => {
  const report = aggregateText([
    "INFO ordinary log with { bad json",
    '[timeout_shadow] {"target":"agentdesk::timeout_shadow","section":"_section_A",'
  ]);
  assert.deepEqual(report._section_A, {
    total: 0, comparable: 0, agreement: 0, divergence: 0, error: 1
  });
  assert.equal(report._unclassified.malformed, 0);
});

test("reports A divergence separately from reducer errors", () => {
  const report = aggregateText([
    shadow("_section_A", { agree: false, reducer_decision: "exhaust" }),
    shadow("_section_A", { agree: false, reducer_decision: "error", error: "preview unavailable" })
  ]);
  assert.deepEqual(report._section_A, {
    total: 2, comparable: 1, agreement: 0, divergence: 1, error: 1
  });
});

test("reports J incomparable ratio and preserves zero-sample null ratio", () => {
  const report = aggregateText([
    shadow("_section_J", { reducer_decision: "incomparable", agree: false, incomparable: true }),
    shadow("_section_J", { reducer_decision: "retry", agree: true })
  ]);
  assert.deepEqual(report._section_J, { total: 2, incomparable: 1, ratio: 0.5, error: 0 });
  assert.equal(aggregateText([])._section_J.ratio, null);
});

test("positive sample thresholds fail on zero samples instead of passing as clean", () => {
  const result = run(["--min-a-samples", "1", "--min-j-samples", "1"], "");
  assert.equal(result.exitCode, 1);
  assert.match(result.failures.join(" "), /_section_A comparable samples 0 < 1/);
  assert.match(result.failures.join(" "), /_section_J samples 0 < 1/);
});

test("enforces pass and fail threshold combinations", () => {
  const input = [
    shadow("_section_A"),
    shadow("_section_J", { reducer_decision: "incomparable", agree: false, incomparable: true })
  ].join("\n");
  assert.equal(run(["--min-a-samples", "1", "--min-j-samples", "1", "--max-divergence", "0", "--max-errors", "0"], input).exitCode, 0);

  const divergent = shadow("_section_A", { agree: false, reducer_decision: "exhaust" });
  assert.equal(run(["--max-divergence", "0"], divergent).exitCode, 1);
  const errored = shadow("_section_A", { agree: false, reducer_decision: "error", error: "boom" });
  assert.equal(run(["--max-errors", "0"], errored).exitCode, 1);
});
