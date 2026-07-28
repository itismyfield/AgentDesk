#!/usr/bin/env node
/**
 * Aggregate the preview-only timeout reducer shadow logs used by #3950.
 *
 * The producer intentionally emits one JSON record per line prefixed with
 * `[timeout_shadow] `.  dcserver/tracing may put arbitrary text in front of
 * that prefix, so this reader finds the prefix instead of assuming a log
 * format.  It never interprets ordinary log lines as shadow evidence.
 */
import fs from "node:fs";
import process from "node:process";
import { createHash } from "node:crypto";
import { fileURLToPath, pathToFileURL } from "node:url";
import { TextDecoder } from "node:util";

const SHADOW_PREFIX = "[timeout_shadow] ";
const SHADOW_TARGET = "agentdesk::timeout_shadow";
const SECTIONS = new Set(["_section_A", "_section_J"]);
const COMPARABLE_A_DECISIONS = new Set(["retry", "exhaust"]);
const EXPECTED_J_REDUCER_DECISION = "incomparable";
const STRICT_UTF8_DECODER = new TextDecoder("utf-8", { fatal: true });
const FILE_READ_CHUNK_BYTES = 64 * 1024;
const STABLE_READ_ATTEMPTS = 2;
// A shadow record is one log line.  This cap prevents a malformed stdin/log
// stream from growing an unterminated line without bound.
export const MAX_RECORD_LINE_BYTES = 1024 * 1024;

function emptySection() {
  return { total: 0, comparable: 0, agreement: 0, divergence: 0, error: 0 };
}

function emptyReport() {
  return {
    _section_A: emptySection(),
    _section_J: { total: 0, successful: 0, incomparable: 0, ratio: null, error: 0 },
    // A malformed line without a readable `section` cannot honestly be
    // attributed to A or J.  Keep it visible and include it in max-errors.
    _unclassified: { malformed: 0 }
  };
}

function parseNumber(name, value) {
  if (typeof value !== "string" || !/^(?:0|[1-9]\d*)$/.test(value)) {
    throw new Error(`${name} requires a non-negative integer`);
  }
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed)) throw new Error(`${name} requires a safe non-negative integer`);
  return parsed;
}

function parseTimestamp(value, optionName) {
  if (typeof value !== "string") throw new Error(`${optionName} requires an ISO-8601 calendar timestamp`);
  const match = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,9}))?(Z|[+-]\d{2}:\d{2})$/.exec(value);
  if (!match) throw new Error(`${optionName} requires an ISO-8601 calendar timestamp`);
  const [, yearText, monthText, dayText, hourText, minuteText, secondText, , zone] = match;
  const year = Number(yearText);
  const month = Number(monthText);
  const day = Number(dayText);
  const hour = Number(hourText);
  const minute = Number(minuteText);
  const second = Number(secondText);
  const leapYear = year % 4 === 0 && (year % 100 !== 0 || year % 400 === 0);
  const daysInMonth = [31, leapYear ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][month - 1];
  const zoneValid = zone === "Z" || (Number(zone.slice(1, 3)) <= 23 && Number(zone.slice(4, 6)) <= 59);
  if (month < 1 || month > 12 || day < 1 || day > daysInMonth || hour > 23 || minute > 59 || second > 59 || !zoneValid) {
    throw new Error(`${optionName} requires a valid ISO-8601 calendar timestamp`);
  }
  const fraction = (match[7] || "").padEnd(9, "0");
  const monthForMarch = month + (month > 2 ? -3 : 9);
  const adjustedYear = year - (month <= 2 ? 1 : 0);
  const era = Math.floor(adjustedYear / 400);
  const yearOfEra = adjustedYear - era * 400;
  const dayOfYear = Math.floor((153 * monthForMarch + 2) / 5) + day - 1;
  const dayOfEra = yearOfEra * 365 + Math.floor(yearOfEra / 4) - Math.floor(yearOfEra / 100) + dayOfYear;
  const daysSinceEpoch = era * 146097 + dayOfEra - 719468;
  const offsetSeconds = zone === "Z" ? 0 :
    (Number(zone.slice(1, 3)) * 60 + Number(zone.slice(4, 6))) * 60 * (zone.startsWith("+") ? 1 : -1);
  return (BigInt(daysSinceEpoch) * 86400n + BigInt(hour * 3600 + minute * 60 + second - offsetSeconds)) * 1000000000n + BigInt(fraction || "0");
}

export function parseArgs(argv) {
  const options = {
    files: [],
    readStdin: false,
    since: null,
    until: null,
    // A no-evidence report is never a deployment GO by default.  Operators
    // may explicitly lower these to zero when they only need an inventory.
    minASamples: 1,
    minJSamples: 1,
    maxDivergence: Number.POSITIVE_INFINITY,
    // Reducer/malformed errors are never clean shadow evidence by default.
    maxErrors: 0
  };

  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "--stdin" || argument === "-") {
      options.readStdin = true;
      continue;
    }
    if (argument === "--help" || argument === "-h") {
      options.help = true;
      continue;
    }

    const equals = argument.indexOf("=");
    const name = equals === -1 ? argument : argument.slice(0, equals);
    const inlineValue = equals === -1 ? undefined : argument.slice(equals + 1);
    const nextValue = () => {
      if (inlineValue !== undefined) return inlineValue;
      index += 1;
      return argv[index];
    };

    switch (name) {
      case "--since": options.since = parseTimestamp(nextValue(), name); break;
      case "--until": options.until = parseTimestamp(nextValue(), name); break;
      case "--min-a-samples": options.minASamples = parseNumber(name, nextValue()); break;
      case "--min-j-samples": options.minJSamples = parseNumber(name, nextValue()); break;
      case "--max-divergence": options.maxDivergence = parseNumber(name, nextValue()); break;
      case "--max-errors": options.maxErrors = parseNumber(name, nextValue()); break;
      default:
        if (argument.startsWith("-")) throw new Error(`unknown option: ${argument}`);
        options.files.push(argument);
    }
  }

  if (options.since !== null && options.until !== null && options.since > options.until) {
    throw new Error("--since must not be after --until");
  }
  if (options.files.length === 0) options.readStdin = true;
  return options;
}

function timestampFromPrefix(prefix) {
  // tracing's usual RFC 3339 timestamps and the common space-separated form.
  // The final timestamp before the payload is the only one belonging to the
  // log line; timestamps in the JSON itself are deliberately ignored.
  const matches = [...prefix.matchAll(/\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?/g)];
  if (matches.length === 0) return null;
  const match = matches[matches.length - 1];
  const raw = match[0];
  const before = match.index > 0 ? prefix[match.index - 1] : "";
  const after = prefix.slice(match.index + raw.length, match.index + raw.length + 1);
  // Only standalone timestamp tokens are eligible. This rejects a timestamp
  // embedded in 12026-... and suffixes such as ZBAD, plus partial offsets.
  if ((before && /[0-9A-Za-z_]/.test(before)) || (after && /[0-9A-Za-z_:+-]/.test(after))) return null;
  const normalizedBase = raw.replace(" ", "T");
  const normalized = /(?:Z|[+-]\d{2}:?\d{2})$/.test(normalizedBase)
    ? normalizedBase.replace(/([+-]\d{2})(\d{2})$/, "$1:$2")
    : normalizedBase + "Z";
  try {
    return parseTimestamp(normalized, "log timestamp");
  } catch {
    return null;
  }
}

function sectionHint(payload) {
  const match = /"section"\s*:\s*"(_section_[AJ])"/.exec(payload);
  return match ? match[1] : null;
}

function isErrorRecord(record) {
  return record.reducer_decision === "error" ||
    (typeof record.error === "string" && record.error.length > 0);
}

function validateRecord(record) {
  if (!record || typeof record !== "object" || Array.isArray(record)) return "record is not an object";
  if (record.target !== SHADOW_TARGET) return "unexpected target";
  if (!SECTIONS.has(record.section)) return "unexpected section";
  if (typeof record.js_decision !== "string") return "missing js_decision";
  if (typeof record.reducer_decision !== "string") return "missing reducer_decision";
  if (typeof record.agree !== "boolean") return "missing agree";
  if (Object.prototype.hasOwnProperty.call(record, "incomparable") && typeof record.incomparable !== "boolean") {
    return "invalid incomparable";
  }
  return null;
}

function addMalformed(report, payload) {
  const hintedSection = sectionHint(payload);
  if (hintedSection) report[hintedSection].error += 1;
  else report._unclassified.malformed += 1;
}

function addRecord(report, record) {
  const validationError = validateRecord(record);
  if (validationError) {
    const hintedSection = record && typeof record === "object" ? record.section : null;
    if (SECTIONS.has(hintedSection)) report[hintedSection].error += 1;
    else report._unclassified.malformed += 1;
    return;
  }

  const section = report[record.section];
  section.total += 1;
  if (isErrorRecord(record)) {
    section.error += 1;
    return;
  }
  if (record.section === "_section_A") {
    const reducerComparable = COMPARABLE_A_DECISIONS.has(record.reducer_decision);
    const derivedIncomparable = record.reducer_decision === "incomparable";
    if (!COMPARABLE_A_DECISIONS.has(record.js_decision) || (!reducerComparable && !derivedIncomparable)) {
      section.error += 1;
      return;
    }
    const derivedAgreement = reducerComparable && record.js_decision === record.reducer_decision;
    const diagnosticMismatch = (record.incomparable === true) !== derivedIncomparable ||
      record.agree !== derivedAgreement;
    if (diagnosticMismatch) section.error += 1;
    if (reducerComparable) {
      section.comparable += 1;
      if (derivedAgreement) section.agreement += 1;
      else section.divergence += 1;
    }
  } else {
    // _section_J currently previews with null status/state, so the reducer can
    // only produce incomparable or error.  A comparable label is not evidence
    // of agreement; it violates the producer contract and must fail closed.
    if (record.js_decision !== "retry" || record.reducer_decision !== EXPECTED_J_REDUCER_DECISION) {
      section.error += 1;
      return;
    }
    const diagnosticMismatch = record.incomparable !== true || record.agree !== false;
    if (diagnosticMismatch) section.error += 1;
    else {
      section.successful += 1;
      section.incomparable += 1;
    }
  }
}

function finalizeReport(report) {
  const j = report._section_J;
  j.ratio = j.successful === 0 ? null : j.incomparable / j.successful;
  return report;
}

function lineInRange(line, prefixIndex, options) {
  if (options.since === null && options.until === null) return true;
  const timestamp = timestampFromPrefix(line.slice(0, prefixIndex));
  // Filtering must fail closed.  A timestamp-less record cannot be placed in
  // the requested window, so treating it as evidence would defeat the gate.
  if (timestamp === null) return false;
  return (options.since === null || timestamp >= options.since) &&
    (options.until === null || timestamp <= options.until);
}

export function aggregateText(inputs, options = {}) {
  const effectiveOptions = { since: null, until: null, ...options };
  const report = emptyReport();
  for (const input of inputs) {
    const scanner = createLineScanner((line) => processLine(report, line, effectiveOptions));
    if (typeof input === "string") scanner.write(Buffer.from(input));
    else if (Buffer.isBuffer(input)) scanner.write(input);
    else throw new TypeError("aggregateText inputs must be strings or Buffers");
    scanner.end();
  }
  return finalizeReport(report);
}

function processLine(report, line, options) {
  const prefixIndex = line.indexOf(SHADOW_PREFIX);
  if (prefixIndex === -1 || !lineInRange(line, prefixIndex, options)) return;
  const payload = line.slice(prefixIndex + SHADOW_PREFIX.length).trim();
  try {
    addRecord(report, JSON.parse(payload));
  } catch {
    addMalformed(report, payload);
  }
}

function statSignature(stat) {
  // ctime changes for a pure rename even though this opened inode's bytes are
  // unchanged.  Preserve it on the snapshot for diagnostics, but let the
  // content double-hash plus identity/size/mtime decide acceptance.
  return `${stat.dev}:${stat.ino}:${stat.size}:${stat.mtimeNs}`;
}

function createLineScanner(onLine) {
  let segments = [];
  let rawBytes = 0;

  function append(segment) {
    if (segment.length === 0) return;
    // Allow one extra raw CR until the record terminator confirms CRLF.
    // Anything larger cannot become a valid <= 1 MiB record.
    if (rawBytes + segment.length > MAX_RECORD_LINE_BYTES + 1) {
      throw new Error(`log line exceeds ${MAX_RECORD_LINE_BYTES} bytes`);
    }
    // File reads reuse their 64 KiB buffer, so retain an owned bounded copy.
    segments.push(Buffer.from(segment));
    rawBytes += segment.length;
  }

  function finishLine(terminatedByLf) {
    const raw = Buffer.concat(segments, rawBytes);
    const contentLength = terminatedByLf && rawBytes > 0 && raw[rawBytes - 1] === 0x0d ? rawBytes - 1 : rawBytes;
    if (contentLength > MAX_RECORD_LINE_BYTES) {
      throw new Error(`log line exceeds ${MAX_RECORD_LINE_BYTES} bytes`);
    }
    let decoded;
    try {
      decoded = STRICT_UTF8_DECODER.decode(raw.subarray(0, contentLength));
    } catch {
      throw new Error("invalid UTF-8 log line");
    }
    onLine(decoded);
    segments = [];
    rawBytes = 0;
  }

  return {
    write(buffer) {
      let cursor = 0;
      for (;;) {
        const newline = buffer.indexOf(0x0a, cursor);
        if (newline === -1) {
          append(buffer.subarray(cursor));
          return;
        }
        append(buffer.subarray(cursor, newline));
        finishLine(true);
        cursor = newline + 1;
      }
    },
    end() {
      if (rawBytes > 0) finishLine(false);
    }
  };
}

function forEachDescriptorLine(descriptor, byteLimit, onLine, io, hash = null) {
  const buffer = Buffer.allocUnsafe(FILE_READ_CHUNK_BYTES);
  const scanner = createLineScanner(onLine);
  let remaining = byteLimit;
  let position = 0;
  for (;;) {
    const length = remaining === null ? buffer.length : Math.min(buffer.length, remaining);
    if (length === 0) break;
    const bytesRead = io.readSync(descriptor, buffer, 0, length, remaining === null ? null : position);
    if (bytesRead === 0) {
      if (remaining !== null) throw new Error("log shrank while reading snapshot");
      break;
    }
    const chunk = buffer.subarray(0, bytesRead);
    if (hash) hash.update(chunk);
    scanner.write(chunk);
    if (remaining !== null) {
      remaining -= bytesRead;
      position += bytesRead;
    }
  }
  scanner.end();
}

function hashDescriptor(descriptor, byteLimit, io) {
  const hash = createHash("sha256");
  const buffer = Buffer.allocUnsafe(FILE_READ_CHUNK_BYTES);
  let remaining = byteLimit;
  let position = 0;
  while (remaining > 0) {
    const bytesRead = io.readSync(descriptor, buffer, 0, Math.min(buffer.length, remaining), position);
    if (bytesRead === 0) throw new Error("log shrank while re-reading snapshot");
    hash.update(buffer.subarray(0, bytesRead));
    remaining -= bytesRead;
    position += bytesRead;
  }
  return hash.digest("hex");
}

function mergeReport(target, source) {
  target._section_A.total += source._section_A.total;
  target._section_A.comparable += source._section_A.comparable;
  target._section_A.agreement += source._section_A.agreement;
  target._section_A.divergence += source._section_A.divergence;
  target._section_A.error += source._section_A.error;
  target._section_J.total += source._section_J.total;
  target._section_J.successful += source._section_J.successful;
  target._section_J.incomparable += source._section_J.incomparable;
  target._section_J.error += source._section_J.error;
  target._unclassified.malformed += source._unclassified.malformed;
}

function rotationOrder(canonical) {
  const match = /^(.*)\.(\d+)$/.exec(canonical);
  return match ? { family: match[1], generation: BigInt(match[2]) } : { family: canonical, generation: 0n };
}

function canonicalizeFiles(files, io) {
  const seen = new Set();
  const entries = files.map((input) => {
    const canonical = io.realpathSync(input);
    if (seen.has(canonical)) throw new Error(`duplicate log input: ${input}`);
    seen.add(canonical);
    return { input, canonical, order: rotationOrder(canonical) };
  });
  entries.sort((left, right) => {
    if (left.order.family !== right.order.family) return left.order.family < right.order.family ? -1 : 1;
    if (left.order.generation !== right.order.generation) return left.order.generation < right.order.generation ? -1 : 1;
    return left.canonical < right.canonical ? -1 : left.canonical > right.canonical ? 1 : 0;
  });
  return entries;
}

function capturePathManifest(entries, io) {
  const manifest = [];
  const identities = new Set();
  for (const entry of entries) {
    const stat = io.statSync(entry.canonical, { bigint: true });
    const identity = `${stat.dev}:${stat.ino}`;
    if (identities.has(identity)) throw new Error(`duplicate log input (opened inode): ${entry.input}`);
    identities.add(identity);
    manifest.push({ ...entry, identity, signature: statSignature(stat), stat });
  }
  return manifest;
}

function manifestSignature(manifest) {
  return manifest.map((entry) => `${entry.canonical}=${entry.signature}`).join("\n");
}

function openFileSnapshots(manifest, snapshots, io) {
  const identities = new Set();
  for (const expected of manifest) {
    const descriptor = io.openSync(expected.canonical, "r");
    // Push before every later failure so the enclosing attempt closes this fd.
    const snapshot = { descriptor, file: expected.canonical };
    snapshots.push(snapshot);
    const stat = io.fstatSync(descriptor, { bigint: true });
    const identity = `${stat.dev}:${stat.ino}`;
    if (identities.has(identity)) throw new Error(`duplicate log input (opened inode): ${expected.input}`);
    if (identity !== expected.identity || statSignature(stat) !== expected.signature) {
      throw new Error(`log path changed while opening snapshot: ${expected.input}`);
    }
    if (stat.size > BigInt(Number.MAX_SAFE_INTEGER)) throw new Error(`log too large for bounded scan: ${expected.input}`);
    identities.add(identity);
    snapshot.stat = stat;
    snapshot.signature = statSignature(stat);
    snapshot.size = Number(stat.size);
  }
  return snapshots;
}

function closeSnapshots(snapshots, io) {
  let firstError = null;
  for (const snapshot of snapshots) {
    try {
      io.closeSync(snapshot.descriptor);
    } catch (error) {
      if (!firstError) firstError = error;
    }
  }
  return firstError;
}

/**
 * Open every input before reading any input.  Each descriptor is then read at
 * its captured size, so a rotation between reads cannot mix old and new path
 * contents.  Mutated opened files are retried once and then fail closed.
 */
export function aggregateFiles(files, options = {}, io = fs) {
  const effectiveOptions = { since: null, until: null, ...options };
  const entries = canonicalizeFiles(files, io);
  let lastError = null;
  let baselineManifest = null;
  let baselineContent = null;
  let coherenceLost = false;
  for (let attempt = 0; attempt < STABLE_READ_ATTEMPTS; attempt += 1) {
    const snapshots = [];
    const report = emptyReport();
    let attemptError = null;
    let stable = false;
    try {
      const manifest = capturePathManifest(entries, io);
      const currentManifest = manifestSignature(manifest);
      if (baselineManifest === null) baselineManifest = currentManifest;
      else if (baselineManifest !== currentManifest) throw new Error("log path manifest changed between snapshot attempts");
      openFileSnapshots(manifest, snapshots, io);
      if (manifestSignature(capturePathManifest(entries, io)) !== currentManifest) {
        throw new Error("log path manifest changed while opening snapshot");
      }
      const attemptContent = new Map();
      for (const snapshot of snapshots) {
        const hash = createHash("sha256");
        forEachDescriptorLine(snapshot.descriptor, snapshot.size, (line) => processLine(report, line, effectiveOptions), io, hash);
        snapshot.firstHash = hash.digest("hex");
        attemptContent.set(snapshot.file, snapshot.firstHash);
      }
      if (baselineContent === null) baselineContent = attemptContent;
      else {
        for (const [file, digest] of attemptContent) {
          if (baselineContent.get(file) !== digest) throw new Error(`log content changed between snapshot attempts: ${file}`);
        }
      }
      stable = true;
      for (const snapshot of snapshots) {
        const contentMatches = snapshot.firstHash === hashDescriptor(snapshot.descriptor, snapshot.size, io);
        const metadataMatches = statSignature(io.fstatSync(snapshot.descriptor, { bigint: true })) === snapshot.signature;
        if (!contentMatches || !metadataMatches) stable = false;
      }
      if (!stable) throw new Error("log changed while reading snapshot");
    } catch (error) {
      attemptError = error;
      coherenceLost = true;
    }
    const closeError = closeSnapshots(snapshots, io);
    if (!attemptError && closeError) attemptError = closeError;
    if (closeError) {
      lastError = attemptError;
      break;
    }
    if (stable && !attemptError && !coherenceLost) return finalizeReport(report);
    lastError = attemptError || new Error("log snapshot coherence could not be proven");
  }
  throw lastError || new Error("log changed while reading snapshot");
}

export function aggregateFile(file, options = {}, io = fs) {
  return aggregateFiles([file], options, io);
}

/**
 * Consume a Node Readable without touching its descriptor.  process.stdin is
 * sometimes nonblocking under node:test; Readable owns readiness/EAGAIN
 * handling, while this function keeps the existing bounded line scanner.
 */
async function aggregateReadable(readable, options) {
  const report = emptyReport();
  const scanner = createLineScanner((line) => processLine(report, line, options));
  for await (const chunk of readable) {
    scanner.write(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  scanner.end();
  return finalizeReport(report);
}

export function thresholdFailures(report, options) {
  const failures = [];
  // A's meaningful evidence is comparable pairs.  J is intentionally
  // incomparable today, so all valid J records are useful evidence there.
  if (report._section_A.comparable < options.minASamples) {
    failures.push(`_section_A comparable samples ${report._section_A.comparable} < ${options.minASamples}`);
  }
  if (report._section_J.successful < options.minJSamples) {
    failures.push(`_section_J successful samples ${report._section_J.successful} < ${options.minJSamples}`);
  }
  if (report._section_A.divergence > options.maxDivergence) {
    failures.push(`_section_A divergence ${report._section_A.divergence} > ${options.maxDivergence}`);
  }
  const errors = report._section_A.error + report._section_J.error + report._unclassified.malformed;
  if (errors > options.maxErrors) failures.push(`shadow errors ${errors} > ${options.maxErrors}`);
  return failures;
}

export function helpText() {
  return `Usage: node scripts/timeout-shadow-gate.mjs [options] [log-file ...]\n\n` +
    `Read timeout shadow records from files and/or --stdin, then print JSON.\n\n` +
    `Options:\n` +
    `  --stdin                    Read stdin in addition to log files\n` +
    `  --since <ISO-8601>         Include timestamped records at or after this time\n` +
    `  --until <ISO-8601>         Include timestamped records at or before this time\n` +
    `  --min-a-samples <count>    Minimum comparable _section_A records (default: 1)\n` +
    `  --min-j-samples <count>    Minimum successful _section_J records (default: 1)\n` +
    `  --max-divergence <count>   Maximum comparable _section_A disagreements\n` +
    `  --max-errors <count>       Maximum malformed/reducer-error records (default: 0)\n\n` +
    `Input records are streamed; each log line is limited to ${MAX_RECORD_LINE_BYTES} bytes.\n`;
}

export function run(argv, stdinText) {
  const options = parseArgs(argv);
  if (options.help) return { help: true, output: helpText(), exitCode: 0 };
  const report = aggregateFiles(options.files, options);
  if (options.readStdin) {
    const stdinReport = aggregateText([stdinText], options);
    mergeReport(report, stdinReport);
  }
  finalizeReport(report);
  const failures = thresholdFailures(report, options);
  return { help: false, output: JSON.stringify(report), failures, exitCode: failures.length === 0 ? 0 : 1 };
}

export async function runFromReadable(argv, readable = process.stdin, io = fs) {
  const options = parseArgs(argv);
  if (options.help) return { help: true, output: helpText(), exitCode: 0 };
  const report = aggregateFiles(options.files, options, io);
  if (options.readStdin) mergeReport(report, await aggregateReadable(readable, options));
  finalizeReport(report);
  const failures = thresholdFailures(report, options);
  return { help: false, output: JSON.stringify(report), failures, exitCode: failures.length === 0 ? 0 : 1 };
}

export function isMainModule(entry = process.argv[1], io = fs) {
  if (!entry) return false;
  try {
    return pathToFileURL(io.realpathSync(entry)).href ===
      pathToFileURL(io.realpathSync(fileURLToPath(import.meta.url))).href;
  } catch {
    return false;
  }
}

if (isMainModule()) {
  void runFromReadable(process.argv.slice(2)).then((result) => {
    if (result.help) process.stdout.write(result.output);
    else process.stdout.write(`${result.output}\n`);
    if (result.failures && result.failures.length > 0) process.stderr.write(`${result.failures.join("; ")}\n`);
    process.exitCode = result.exitCode;
  }).catch((error) => {
    process.stderr.write(`timeout-shadow-gate: ${error.message}\n`);
    process.exitCode = 2;
  });
}
