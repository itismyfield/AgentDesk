import assert from "node:assert/strict";
import * as fs from "node:fs";
import { linkSync, mkdtempSync, renameSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { Readable } from "node:stream";

import { aggregateFile, aggregateFiles, aggregateText, parseArgs, run, runFromReadable } from "../timeout-shadow-gate.mjs";

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
      _section_J: { total: 1, successful: 1, incomparable: 1, ratio: 1, error: 0 },
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

test("time windows preserve microsecond boundaries and support zoned space timestamps", () => {
  const precise = run([
    "--stdin", "--since", "2026-07-28T00:00:00.000002Z", "--min-a-samples", "0", "--min-j-samples", "0"
  ], [
    `2026-07-28T00:00:00.000001Z INFO ${shadow("_section_A")}`,
    `2026-07-28T00:00:00.000002Z INFO ${shadow("_section_A")}`
  ].join("\n"));
  assert.equal(JSON.parse(precise.output)._section_A.total, 1);

  const zoned = run([
    "--stdin", "--since", "2026-07-28T00:00:00Z", "--min-a-samples", "0", "--min-j-samples", "0"
  ], [
    `2026-07-28 00:00:00Z INFO ${shadow("_section_A")}`,
    `2026-07-28 09:00:00+09:00 INFO ${shadow("_section_A")}`
  ].join("\n"));
  assert.equal(JSON.parse(zoned.output)._section_A.total, 2);
});

test("timestamp token parsing rejects partial offsets and accepts year zero leap day", () => {
  const options = ["--stdin", "--since", "0000-02-29T00:00:00Z", "--min-a-samples", "0", "--min-j-samples", "0"];
  const result = run(options, [
    `0000-02-29T00:00:00Z INFO ${shadow("_section_A")}`,
    `0000-02-29T00:00:00+09:000 INFO ${shadow("_section_A")}`,
    `0000-02-29T00:00:00+09 INFO ${shadow("_section_A")}`
  ].join("\n"));
  assert.equal(JSON.parse(result.output)._section_A.total, 1);
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
  assert.deepEqual(report._section_J, { total: 2, successful: 2, incomparable: 1, ratio: 0.5, error: 0 });
  assert.equal(aggregateText([])._section_J.ratio, null);
});

test("default positive sample thresholds fail on zero samples instead of passing as clean", () => {
  const result = run([], "");
  assert.equal(result.exitCode, 1);
  assert.match(result.failures.join(" "), /_section_A comparable samples 0 < 1/);
  assert.match(result.failures.join(" "), /_section_J successful samples 0 < 1/);
});

test("J reducer errors are not successful minimum evidence and fail closed by default", () => {
  const result = run(["--stdin", "--min-a-samples", "0"], shadow("_section_J", {
    reducer_decision: "error", agree: false, error: "preview failed"
  }));
  assert.equal(result.exitCode, 1);
  assert.match(result.failures.join(" "), /_section_J successful samples 0 < 1/);
  assert.match(result.failures.join(" "), /shadow errors 1 > 0/);
});

test("rejects decimal counts and invalid ISO-8601 calendar timestamps", () => {
  assert.throws(() => parseArgs(["--min-a-samples", "1.5"]), /non-negative integer/);
  assert.throws(() => parseArgs(["--max-errors", "01"]), /non-negative integer/);
  assert.throws(() => parseArgs(["--since", "2026-02-30T00:00:00Z"]), /valid ISO-8601/);
  assert.throws(() => parseArgs(["--until", "2026-07-28T24:00:00Z"]), /valid ISO-8601/);
  assert.throws(() => parseArgs(["--since", "2026-07-28 00:00:00Z"]), /ISO-8601 calendar/);
});

test("retries a changed opened-file snapshot without double counting", () => {
  const directory = mkdtempSync(join(tmpdir(), "timeout-shadow-gate-"));
  try {
    const log = join(directory, "dcserver.stdout.log.1");
    writeFileSync(log, `${shadow("_section_A")}\n`);
    const signatureMtimes = [1, 2, 3, 3];
    let stats = 0;
    const io = {
      ...fs,
      fstatSync(descriptor) {
        const stat = fs.fstatSync(descriptor, { bigint: true });
        const stamp = BigInt(signatureMtimes[stats++]);
        return { ...stat, mtimeNs: stamp, ctimeNs: stamp };
      }
    };
    const report = aggregateFile(log, {}, io);
    assert.equal(report._section_A.total, 1);
    assert.equal(stats, 4);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("content recheck rejects same-inode rewrite with restored metadata and keeps only the fresh retry aggregate", () => {
  const directory = mkdtempSync(join(tmpdir(), "timeout-shadow-gate-"));
  try {
    const log = join(directory, "dcserver.stdout.log");
    const oldRecord = `${shadow("_section_A")}\n`;
    const newRecord = `${shadow("_section_J")}\n`;
    assert.equal(Buffer.byteLength(oldRecord), Buffer.byteLength(newRecord));
    writeFileSync(log, oldRecord);
    const baseline = fs.statSync(log, { bigint: true });
    let rewritten = false;
    const io = {
      ...fs,
      fstatSync() { return { ...baseline }; },
      readSync(...args) {
        const bytes = fs.readSync(...args);
        if (!rewritten) {
          rewritten = true;
          writeFileSync(log, newRecord);
        }
        return bytes;
      }
    };
    const report = aggregateFile(log, {}, io);
    assert.equal(report._section_A.total, 0);
    assert.equal(report._section_J.total, 1);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("shrink during first scan retries with a fresh snapshot", () => {
  const directory = mkdtempSync(join(tmpdir(), "timeout-shadow-gate-"));
  try {
    const log = join(directory, "dcserver.stdout.log");
    writeFileSync(log, `${shadow("_section_A")}\n`);
    let shrunk = false;
    const io = {
      ...fs,
      readSync(...args) {
        const bytes = fs.readSync(...args);
        if (!shrunk) {
          shrunk = true;
          writeFileSync(log, "");
        }
        return bytes;
      }
    };
    assert.equal(aggregateFile(log, {}, io)._section_A.total, 0);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("opened-inode collision caused by rotation retries as one fresh all-file snapshot", () => {
  const directory = mkdtempSync(join(tmpdir(), "timeout-shadow-gate-"));
  try {
    const current = join(directory, "dcserver.stdout.log");
    const rotated = join(directory, "dcserver.stdout.log.1");
    const archived = join(directory, "dcserver.stdout.log.2");
    writeFileSync(current, `${shadow("_section_A")}\n`);
    writeFileSync(rotated, `${shadow("_section_J")}\n`);
    let rotatedOnce = false;
    const io = {
      ...fs,
      openSync(path, flags) {
        const descriptor = fs.openSync(path, flags);
        if (!rotatedOnce) {
          rotatedOnce = true;
          renameSync(rotated, archived);
          renameSync(current, rotated);
          writeFileSync(current, `${shadow("_section_J")}\n`);
        }
        return descriptor;
      }
    };
    const report = aggregateFiles([current, rotated], {}, io);
    assert.equal(report._section_A.total, 1);
    assert.equal(report._section_J.total, 1);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("opened descriptors close on fstat and realpath failures", () => {
  const directory = mkdtempSync(join(tmpdir(), "timeout-shadow-gate-"));
  try {
    const log = join(directory, "dcserver.stdout.log");
    writeFileSync(log, `${shadow("_section_A")}\n`);
    for (const failedMethod of ["fstatSync", "realpathSync"]) {
      let closes = 0;
      const io = {
        ...fs,
        [failedMethod]() { throw new Error(`${failedMethod} injected failure`); },
        closeSync(descriptor) {
          closes += 1;
          return fs.closeSync(descriptor);
        }
      };
      assert.throws(() => aggregateFile(log, {}, io), /injected failure/);
      assert.equal(closes, 2);
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("opens all rotated inputs before reading so a mid-scan rotation cannot drop or duplicate records", () => {
  const directory = mkdtempSync(join(tmpdir(), "timeout-shadow-gate-"));
  try {
    const current = join(directory, "dcserver.stdout.log");
    const rotated = join(directory, "dcserver.stdout.log.1");
    const archived = join(directory, "dcserver.stdout.log.2");
    writeFileSync(current, `${shadow("_section_A")}\n`);
    writeFileSync(rotated, `${shadow("_section_J", { reducer_decision: "incomparable", agree: false, incomparable: true })}\n`);
    let rotatedDuringRead = false;
    const io = {
      ...fs,
      readSync(...args) {
        const bytes = fs.readSync(...args);
        if (!rotatedDuringRead) {
          rotatedDuringRead = true;
          renameSync(rotated, archived);
          renameSync(current, rotated);
          writeFileSync(current, `${shadow("_section_A", { card_id: "new-current" })}\n`);
        }
        return bytes;
      }
    };
    const report = aggregateFiles([current, rotated], {}, io);
    assert.deepEqual(report._section_A, { total: 1, comparable: 1, agreement: 1, divergence: 0, error: 0 });
    assert.deepEqual(report._section_J, { total: 1, successful: 1, incomparable: 1, ratio: 1, error: 0 });
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

test("rejects duplicate opened inode inputs through hard links", () => {
  const directory = mkdtempSync(join(tmpdir(), "timeout-shadow-gate-"));
  try {
    const log = join(directory, "dcserver.stdout.log");
    const alias = join(directory, "same-inode.log");
    writeFileSync(log, `${shadow("_section_A")}\n`);
    linkSync(log, alias);
    assert.throws(() => run([log, alias], ""), /duplicate log input/);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("main guard executes invalid CLI through a symlink alias", () => {
  const directory = mkdtempSync(join(tmpdir(), "timeout-shadow-gate-"));
  try {
    const script = fileURLToPath(new URL("../timeout-shadow-gate.mjs", import.meta.url));
    const alias = join(directory, "timeout-shadow-gate-alias.mjs");
    symlinkSync(script, alias);
    const result = spawnSync(process.execPath, [alias, "--not-an-option"], { encoding: "utf8" });
    assert.equal(result.status, 2);
    assert.match(result.stderr, /unknown option/);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("streamed stdin rejects an unterminated line over the documented record cap", () => {
  const script = fileURLToPath(new URL("../timeout-shadow-gate.mjs", import.meta.url));
  const result = spawnSync(process.execPath, [script, "--min-a-samples", "0", "--min-j-samples", "0"], {
    encoding: "utf8",
    input: "x".repeat(1024 * 1024 + 1)
  });
  assert.equal(result.status, 2);
  assert.match(result.stderr, /log line exceeds 1048576 bytes/);
});

test("text and run library exports enforce the same bounded scanner cap", () => {
  const oversized = "x".repeat(1024 * 1024 + 1);
  assert.throws(() => aggregateText([oversized]), /log line exceeds 1048576 bytes/);
  assert.throws(() => run(["--min-a-samples", "0", "--min-j-samples", "0"], oversized), /log line exceeds 1048576 bytes/);
});

test("CLI readable input uses chunk iteration rather than a synchronous fd read", async () => {
  const readable = Readable.from([
    Buffer.from(`prefix ${shadow("_section_A")}\n`),
    Buffer.from(shadow("_section_J", { reducer_decision: "incomparable", agree: false, incomparable: true }))
  ]);
  const result = await runFromReadable(["--stdin"], readable);
  assert.equal(result.exitCode, 0);
  assert.deepEqual(JSON.parse(result.output)._section_J, { total: 1, successful: 1, incomparable: 1, ratio: 1, error: 0 });
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
