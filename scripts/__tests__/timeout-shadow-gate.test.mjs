import assert from "node:assert/strict";
import * as fs from "node:fs";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { aggregateFile, aggregateText, parseArgs, run } from "../timeout-shadow-gate.mjs";

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

test("time windows exclude both out-of-range and timestamp-less records", () => {
  const input = [
    `2026-07-26T00:00:00Z INFO ${shadow("_section_A")}`,
    shadow("_section_A")
  ].join("\n");
  const result = run(["--stdin", "--since", "2026-07-27T00:00:00Z", "--min-a-samples", "0", "--min-j-samples", "0"], input);
  assert.equal(result.exitCode, 0);
  assert.deepEqual(JSON.parse(result.output)._section_A, {
    total: 0, comparable: 0, agreement: 0, divergence: 0, error: 0
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

test("default positive sample thresholds fail on zero samples instead of passing as clean", () => {
  const result = run([], "");
  assert.equal(result.exitCode, 1);
  assert.match(result.failures.join(" "), /_section_A comparable samples 0 < 1/);
  assert.match(result.failures.join(" "), /_section_J samples 0 < 1/);
});

test("rejects decimal counts and invalid ISO-8601 calendar timestamps", () => {
  assert.throws(() => parseArgs(["--min-a-samples", "1.5"]), /non-negative integer/);
  assert.throws(() => parseArgs(["--max-errors", "01"]), /non-negative integer/);
  assert.throws(() => parseArgs(["--since", "2026-02-30T00:00:00Z"]), /valid ISO-8601/);
  assert.throws(() => parseArgs(["--until", "2026-07-28T24:00:00Z"]), /valid ISO-8601/);
  assert.throws(() => parseArgs(["--since", "2026-07-28 00:00:00Z"]), /ISO-8601 calendar/);
});

test("streams a stable rotated file and retries a changed snapshot without double counting", () => {
  const directory = mkdtempSync(join(tmpdir(), "timeout-shadow-gate-"));
  try {
    const log = join(directory, "dcserver.stdout.log.1");
    writeFileSync(log, `${shadow("_section_A")}\n`);
    const signatureMtimes = [1, 2, 3, 3];
    let stats = 0;
    const io = {
      ...fs,
      statSync(path) {
        const stat = fs.statSync(path);
        return { ...stat, mtimeMs: signatureMtimes[stats++] };
      }
    };
    const report = aggregateFile(log, {}, io);
    assert.equal(report._section_A.total, 1);
    assert.equal(stats, 4);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("rejects duplicate canonical log inputs", () => {
  const directory = mkdtempSync(join(tmpdir(), "timeout-shadow-gate-"));
  try {
    const log = join(directory, "dcserver.stdout.log");
    writeFileSync(log, `${shadow("_section_A")}\n`);
    assert.throws(() => run([log, log], ""), /duplicate log input/);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
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
