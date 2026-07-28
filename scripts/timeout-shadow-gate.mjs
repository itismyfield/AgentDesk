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
import { StringDecoder } from "node:string_decoder";

const SHADOW_PREFIX = "[timeout_shadow] ";
const SHADOW_TARGET = "agentdesk::timeout_shadow";
const SECTIONS = new Set(["_section_A", "_section_J"]);
const FILE_READ_CHUNK_BYTES = 64 * 1024;
const STABLE_READ_ATTEMPTS = 2;

function emptySection() {
  return { total: 0, comparable: 0, agreement: 0, divergence: 0, error: 0 };
}

function emptyReport() {
  return {
    _section_A: emptySection(),
    _section_J: { total: 0, incomparable: 0, ratio: null, error: 0 },
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
  const daysInMonth = new Date(Date.UTC(year, month, 0)).getUTCDate();
  const zoneValid = zone === "Z" || (Number(zone.slice(1, 3)) <= 23 && Number(zone.slice(4, 6)) <= 59);
  if (month < 1 || month > 12 || day < 1 || day > daysInMonth || hour > 23 || minute > 59 || second > 59 || !zoneValid) {
    throw new Error(`${optionName} requires a valid ISO-8601 calendar timestamp`);
  }
  const parsed = Date.parse(value);
  if (Number.isNaN(parsed)) throw new Error(`${optionName} requires a valid ISO-8601 calendar timestamp`);
  return parsed;
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
    maxErrors: Number.POSITIVE_INFINITY
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
  const matches = prefix.match(/\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?/g);
  if (!matches || matches.length === 0) return null;
  const raw = matches[matches.length - 1];
  const normalized = /(?:Z|[+-]\d{2}:?\d{2})$/.test(raw)
    ? raw.replace(/([+-]\d{2})(\d{2})$/, "$1:$2")
    : raw.replace(" ", "T") + "Z";
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

function isIncomparableRecord(record) {
  return record.incomparable === true || record.reducer_decision === "incomparable";
}

function validateRecord(record) {
  if (!record || typeof record !== "object" || Array.isArray(record)) return "record is not an object";
  if (record.target !== SHADOW_TARGET) return "unexpected target";
  if (!SECTIONS.has(record.section)) return "unexpected section";
  if (typeof record.js_decision !== "string") return "missing js_decision";
  if (typeof record.reducer_decision !== "string") return "missing reducer_decision";
  if (typeof record.agree !== "boolean") return "missing agree";
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
    if (!isIncomparableRecord(record)) {
      section.comparable += 1;
      if (record.agree) section.agreement += 1;
      else section.divergence += 1;
    }
  } else if (isIncomparableRecord(record)) {
    section.incomparable += 1;
  }
}

function finalizeReport(report) {
  const j = report._section_J;
  j.ratio = j.total === 0 ? null : j.incomparable / j.total;
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
    for (const line of String(input).split(/\r?\n/)) processLine(report, line, effectiveOptions);
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
  return `${stat.dev}:${stat.ino}:${stat.size}:${stat.mtimeMs}`;
}

function forEachFileLine(file, onLine, io) {
  const descriptor = io.openSync(file, "r");
  let remainder = "";
  const buffer = Buffer.allocUnsafe(FILE_READ_CHUNK_BYTES);
  const decoder = new StringDecoder("utf8");
  try {
    for (;;) {
      const bytesRead = io.readSync(descriptor, buffer, 0, buffer.length, null);
      if (bytesRead === 0) break;
      const combined = remainder + decoder.write(buffer.subarray(0, bytesRead));
      const lines = combined.split("\n");
      remainder = lines.pop();
      for (const line of lines) onLine(line.endsWith("\r") ? line.slice(0, -1) : line);
    }
    remainder += decoder.end();
    if (remainder.length > 0) onLine(remainder.endsWith("\r") ? remainder.slice(0, -1) : remainder);
  } finally {
    io.closeSync(descriptor);
  }
}

function mergeReport(target, source) {
  target._section_A.total += source._section_A.total;
  target._section_A.comparable += source._section_A.comparable;
  target._section_A.agreement += source._section_A.agreement;
  target._section_A.divergence += source._section_A.divergence;
  target._section_A.error += source._section_A.error;
  target._section_J.total += source._section_J.total;
  target._section_J.incomparable += source._section_J.incomparable;
  target._section_J.error += source._section_J.error;
  target._unclassified.malformed += source._unclassified.malformed;
}

/**
 * Read a regular/rotated log with bounded memory.  A changed stat signature
 * means the file may have been renamed or appended during the scan; retry the
 * whole file once and otherwise reject it instead of mixing two snapshots.
 */
export function aggregateFile(file, options = {}, io = fs) {
  const effectiveOptions = { since: null, until: null, ...options };
  for (let attempt = 0; attempt < STABLE_READ_ATTEMPTS; attempt += 1) {
    const before = io.statSync(file);
    const report = emptyReport();
    forEachFileLine(file, (line) => processLine(report, line, effectiveOptions), io);
    const after = io.statSync(file);
    if (statSignature(before) === statSignature(after)) return finalizeReport(report);
  }
  throw new Error(`log changed while reading: ${file}`);
}

function uniqueFiles(files, io = fs) {
  const seenCanonical = new Set();
  const seenInodes = new Set();
  return files.map((file) => {
    const canonical = io.realpathSync(file);
    const stat = io.statSync(canonical);
    const inode = `${stat.dev}:${stat.ino}`;
    if (seenCanonical.has(canonical) || seenInodes.has(inode)) {
      throw new Error(`duplicate log input: ${file}`);
    }
    seenCanonical.add(canonical);
    seenInodes.add(inode);
    return canonical;
  });
}

export function thresholdFailures(report, options) {
  const failures = [];
  // A's meaningful evidence is comparable pairs.  J is intentionally
  // incomparable today, so all valid J records are useful evidence there.
  if (report._section_A.comparable < options.minASamples) {
    failures.push(`_section_A comparable samples ${report._section_A.comparable} < ${options.minASamples}`);
  }
  if (report._section_J.total < options.minJSamples) {
    failures.push(`_section_J samples ${report._section_J.total} < ${options.minJSamples}`);
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
    `  --min-j-samples <count>    Minimum _section_J records (default: 1)\n` +
    `  --max-divergence <count>   Maximum comparable _section_A disagreements\n` +
    `  --max-errors <count>       Maximum malformed/reducer-error records\n`;
}

export function run(argv, stdinText) {
  const options = parseArgs(argv);
  if (options.help) return { help: true, output: helpText(), exitCode: 0 };
  const report = emptyReport();
  for (const file of uniqueFiles(options.files)) mergeReport(report, aggregateFile(file, options));
  if (options.readStdin) {
    const stdinReport = aggregateText([stdinText], options);
    mergeReport(report, stdinReport);
  }
  finalizeReport(report);
  const failures = thresholdFailures(report, options);
  return { help: false, output: JSON.stringify(report), failures, exitCode: failures.length === 0 ? 0 : 1 };
}

if (import.meta.url === new URL(process.argv[1], "file:").href) {
  try {
    // Do not consume a terminal's stdin merely because file paths were
    // supplied.  Besides being surprising, that would make the gate hang in
    // an operator shell.  stdin is consumed only for the explicit/default
    // stdin modes parsed above.
    const cliOptions = parseArgs(process.argv.slice(2));
    const stdinText = !cliOptions.help && cliOptions.readStdin ? fs.readFileSync(0, "utf8") : "";
    const result = run(process.argv.slice(2), stdinText);
    if (result.help) process.stdout.write(result.output);
    else process.stdout.write(`${result.output}\n`);
    if (result.failures && result.failures.length > 0) process.stderr.write(`${result.failures.join("; ")}\n`);
    process.exitCode = result.exitCode;
  } catch (error) {
    process.stderr.write(`timeout-shadow-gate: ${error.message}\n`);
    process.exitCode = 2;
  }
}
