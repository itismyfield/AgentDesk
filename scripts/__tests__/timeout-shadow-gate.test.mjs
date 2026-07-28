import assert from "node:assert/strict";
import * as fs from "node:fs";
import { linkSync, mkdtempSync, renameSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { Readable } from "node:stream";
import * as timeoutShadowGate from "../timeout-shadow-gate.mjs";

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
    `0000-02-29T00:00:00+01:000 INFO ${shadow("_section_A")}`,
    `0000-02-29T00:00:00+01 INFO ${shadow("_section_A")}`,
    `12026-02-29T00:00:00Z INFO ${shadow("_section_A")}`,
    `0000-02-29T00:00:00ZBAD INFO ${shadow("_section_A")}`
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
    shadow("_section_J", { reducer_decision: "incomparable", agree: false, incomparable: true })
  ]);
  assert.deepEqual(report._section_J, { total: 1, successful: 1, incomparable: 1, ratio: 1, error: 0 });
  assert.equal(aggregateText([])._section_J.ratio, null);
});

test("J comparable output fails closed under the current producer contract", () => {
  const result = run([
    "--stdin", "--min-a-samples", "1", "--min-j-samples", "0",
    "--max-divergence", "0", "--max-errors", "0"
  ], [
    shadow("_section_A", { js_decision: "retry", reducer_decision: "retry", agree: true }),
    shadow("_section_J", { js_decision: "retry", reducer_decision: "exhaust", agree: false })
  ].join("\n"));

  assert.deepEqual(JSON.parse(result.output)._section_J, {
    total: 1, successful: 0, incomparable: 0, ratio: null, error: 1
  });
  assert.equal(result.exitCode, 1);
  assert.match(result.failures.join(" "), /shadow errors 1 > 0/);
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

test("A derives agreement from decisions so forged agree cannot evade divergence gates", async () => {
  const forgedTrue = shadow("_section_A", { reducer_decision: "exhaust", agree: true });
  const forgedFalse = shadow("_section_A", { reducer_decision: "retry", agree: false });
  const result = await runFromReadable([
    "--stdin", "--min-a-samples", "2", "--min-j-samples", "0", "--max-divergence", "0", "--max-errors", "2"
  ], Readable.from([Buffer.from(`${forgedTrue}\n${forgedFalse}\n`)]));
  const a = JSON.parse(result.output)._section_A;
  assert.deepEqual(a, { total: 2, comparable: 2, agreement: 1, divergence: 1, error: 2 });
  assert.equal(result.exitCode, 1);
  assert.match(result.failures.join(" "), /_section_A divergence 1 > 0/);
});

test("A incomparable and agree diagnostics cannot hide decision-derived divergence", () => {
  const result = run([
    "--stdin", "--min-a-samples", "1", "--min-j-samples", "0", "--max-divergence", "0", "--max-errors", "1"
  ], shadow("_section_A", { reducer_decision: "exhaust", agree: false, incomparable: true }));
  assert.deepEqual(JSON.parse(result.output)._section_A, {
    total: 1, comparable: 1, agreement: 0, divergence: 1, error: 1
  });
  assert.equal(result.exitCode, 1);
});

test("J unknown and missing preview labels are errors, never successful evidence", () => {
  for (const reducerDecision of ["unknown", "missing", ""]) {
    const result = run(["--stdin", "--min-a-samples", "0"], shadow("_section_J", {
      reducer_decision: reducerDecision, agree: false
    }));
    const j = JSON.parse(result.output)._section_J;
    assert.equal(j.successful, 0);
    assert.equal(j.error, 1);
    assert.equal(result.exitCode, 1);
  }
  const forgedDiagnostic = run(["--stdin", "--min-a-samples", "0"], shadow("_section_J", {
    reducer_decision: "retry", agree: false
  }));
  assert.equal(JSON.parse(forgedDiagnostic.output)._section_J.successful, 0);
  assert.equal(forgedDiagnostic.exitCode, 1);
});

test("rejects decimal counts and invalid ISO-8601 calendar timestamps", () => {
  assert.throws(() => parseArgs(["--min-a-samples", "1.5"]), /non-negative integer/);
  assert.throws(() => parseArgs(["--max-errors", "01"]), /non-negative integer/);
  assert.throws(() => parseArgs(["--since", "2026-02-30T00:00:00Z"]), /valid ISO-8601/);
  assert.throws(() => parseArgs(["--until", "2026-07-28T24:00:00Z"]), /valid ISO-8601/);
  assert.throws(() => parseArgs(["--since", "2026-07-28 00:00:00Z"]), /ISO-8601 calendar/);
});

test("fails closed when opened-file metadata changes during verification", () => {
  const directory = mkdtempSync(join(tmpdir(), "timeout-shadow-gate-"));
  try {
    const log = join(directory, "dcserver.stdout.log.1");
    writeFileSync(log, `${shadow("_section_A")}\n`);
    let stats = 0;
    const io = {
      ...fs,
      fstatSync(descriptor, options) {
        const stat = fs.fstatSync(descriptor, { bigint: true });
        stats += 1;
        return stats % 2 === 0 ? { ...stat, mtimeNs: stat.mtimeNs + 1n } : stat;
      }
    };
    assert.throws(() => aggregateFile(log, {}, io), /snapshot/);
    assert.equal(stats, 4);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("content recheck rejects same-inode rewrite even with restored metadata", () => {
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
      statSync() { return { ...baseline }; },
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
    assert.throws(() => aggregateFile(log, {}, io), /content changed|snapshot/);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("shrink during first scan retries then fails closed", () => {
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
    assert.throws(() => aggregateFile(log, {}, io), /snapshot|shrank|manifest/);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("caller-order-independent rotation during open cannot erase divergence", () => {
  const directory = mkdtempSync(join(tmpdir(), "timeout-shadow-gate-"));
  try {
    const current = join(directory, "dcserver.stdout.log");
    const rotated = join(directory, "dcserver.stdout.log.1");
    const archived = join(directory, "dcserver.stdout.log.2");
    writeFileSync(current, `${shadow("_section_A")}\n`);
    writeFileSync(rotated, `${shadow("_section_J")}\n`);
    const canonicalCurrent = fs.realpathSync(current);
    let rotatedOnce = false;
    const openedPaths = [];
    const io = {
      ...fs,
      openSync(path, flags) {
        openedPaths.push(path);
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
    assert.throws(() => aggregateFiles([rotated, current], {}, io), /manifest|opened inode|changed while opening/);
    assert.equal(openedPaths[0], canonicalCurrent);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("opened descriptors close on fstat failures while pre-open realpath failure opens none", () => {
  const directory = mkdtempSync(join(tmpdir(), "timeout-shadow-gate-"));
  try {
    const log = join(directory, "dcserver.stdout.log");
    writeFileSync(log, `${shadow("_section_A")}\n`);
    let closes = 0;
    const fstatIo = {
      ...fs,
      fstatSync() { throw new Error("fstatSync injected failure"); },
      closeSync(descriptor) {
        closes += 1;
        return fs.closeSync(descriptor);
      }
    };
    assert.throws(() => aggregateFile(log, {}, fstatIo), /injected failure/);
    assert.equal(closes, 2);

    let opens = 0;
    const realpathIo = {
      ...fs,
      realpathSync() { throw new Error("realpathSync injected failure"); },
      openSync(...args) { opens += 1; return fs.openSync(...args); }
    };
    assert.throws(() => aggregateFile(log, {}, realpathIo), /injected failure/);
    assert.equal(opens, 0);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("close errors attempt every original descriptor once without closing a reused fd", () => {
  const directory = mkdtempSync(join(tmpdir(), "timeout-shadow-gate-"));
  let replacementDescriptor = null;
  try {
    const first = join(directory, "a.log");
    const second = join(directory, "b.log");
    const replacement = join(directory, "replacement.log");
    writeFileSync(first, `${shadow("_section_A")}\n`);
    writeFileSync(second, `${shadow("_section_J")}\n`);
    writeFileSync(replacement, "replacement");
    const closeCalls = [];
    const io = {
      ...fs,
      closeSync(descriptor) {
        closeCalls.push(descriptor);
        if (replacementDescriptor === null) {
          fs.closeSync(descriptor);
          replacementDescriptor = fs.openSync(replacement, "r");
          throw new Error("first close injected failure");
        }
        return fs.closeSync(descriptor);
      }
    };
    assert.throws(() => aggregateFiles([first, second], {}, io), /first close injected failure/);
    assert.equal(new Set(closeCalls).size, 2);
    assert.equal(closeCalls.length, 2);
    assert.equal(replacementDescriptor, closeCalls[0]);
    assert.doesNotThrow(() => fs.fstatSync(replacementDescriptor));
  } finally {
    if (replacementDescriptor !== null) {
      try { fs.closeSync(replacementDescriptor); } catch (_) {}
    }
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
    let ctimeChanged = false;
    const io = {
      ...fs,
      fstatSync(descriptor, options) {
        const stat = fs.fstatSync(descriptor, options);
        if (!rotatedDuringRead) return stat;
        ctimeChanged = true;
        return { ...stat, ctimeNs: stat.ctimeNs + 1n };
      },
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
    assert.equal(ctimeChanged, true);
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

test("raw-byte scanner accepts exactly capped LF and CRLF text records", () => {
  const base = shadow("_section_A");
  const padding = " ".repeat(1024 * 1024 - Buffer.byteLength(base));
  assert.equal(aggregateText([`${base}${padding}\n`])._section_A.total, 1);
  assert.equal(aggregateText([`${base}${padding}\r\n`])._section_A.total, 1);
  assert.throws(() => aggregateText([Buffer.from(`${base}${padding}\r`)]), /log line exceeds 1048576 bytes/);
});

test("raw invalid UTF-8 is rejected identically by Buffer and Readable inputs", async () => {
  const invalidRecord = Buffer.concat([
    Buffer.from("[timeout_shadow] "),
    Buffer.alloc(400 * 1024, 0xff),
    Buffer.from("\n")
  ]);
  assert.throws(() => aggregateText([invalidRecord]), /invalid UTF-8 log line/);
  await assert.rejects(
    runFromReadable(["--stdin"], Readable.from([invalidRecord])),
    /invalid UTF-8 log line/
  );
  for (const section of ["_section_A", "_section_J"]) {
    const corrupted = Buffer.concat([
      Buffer.from(`[timeout_shadow] {"target":"agentdesk::timeout_shadow","section":"${section}","bad":"`),
      Buffer.from([0xff]),
      Buffer.from('"}\n')
    ]);
    await assert.rejects(runFromReadable(["--stdin"], Readable.from([corrupted])), /invalid UTF-8 log line/);
  }
});

test("file snapshots reject invalid UTF-8 corruption in A and J records", () => {
  const directory = mkdtempSync(join(tmpdir(), "timeout-shadow-gate-"));
  try {
    for (const section of ["_section_A", "_section_J"]) {
      const log = join(directory, `${section}.log`);
      writeFileSync(log, Buffer.concat([
        Buffer.from(`[timeout_shadow] {"target":"agentdesk::timeout_shadow","section":"${section}","bad":"`),
        Buffer.from([0xff]),
        Buffer.from('"}\n')
      ]));
      assert.throws(() => aggregateFile(log), /invalid UTF-8 log line/);
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("text and run library exports enforce the same cap above one MiB", () => {
  const oversized = "x".repeat(1024 * 1024 + 1);
  assert.throws(() => aggregateText([oversized]), /log line exceeds 1048576 bytes/);
  assert.throws(() => run(["--min-a-samples", "0", "--min-j-samples", "0"], oversized), /log line exceeds 1048576 bytes/);
});

test("CLI readable input never reaches a synchronous fd0 EAGAIN path", async () => {
  const readable = Readable.from([
    Buffer.from(`prefix ${shadow("_section_A")}\n`),
    Buffer.from(shadow("_section_J", { reducer_decision: "incomparable", agree: false, incomparable: true }))
  ]);
  const io = {
    ...fs,
    readSync() {
      const error = new Error("simulated nonblocking fd");
      error.code = "EAGAIN";
      throw error;
    }
  };
  assert.equal("runFromStdin" in timeoutShadowGate, false);
  const result = await runFromReadable(["--stdin"], readable, io);
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
