/**
 * Model-free observability classifiers adapted from Agent Observatory (MIT):
 * loop detection, stop-reason and quota reset parsing, reply invitation checks,
 * Claude prompt-cache TTL countdown, and in-memory transcript search.
 */
import type { SessionTranscript, SessionTranscriptEvent } from "../api/agentActivity";

export type ObsTranslator = (ko: string, en: string) => string;

export type LoopPattern = "same" | "contract" | "pingpong" | "rotation";

export interface ToolStepSample {
  name: string;
  input: string;
  output: string;
  status: "running" | "completed" | "failed";
  errorText: string;
  label: string;
}

export interface LoopFinding {
  pattern: LoopPattern;
  count: number;
  tool: string;
  evidence: string[];
  key: string;
}

export const LOOP_SAME = 4;
export const LOOP_CONTRACT = 3;
export const LOOP_WINDOW = 12;
export const LOOP_WINDOW_DISTINCT = 2;
export const LOOP_ROTATION_WINDOW = 16;
export const LOOP_ROTATION_DISTINCT = 4;
export const CLAUDE_CACHE_TTL_MS = 5 * 60_000;
export const CLAUDE_CACHE_EXPIRING_MS = 60_000;
export const CLAUDE_CACHE_RECENT_WINDOW_MS = 30 * 60_000;

const POLLING_TOOLS = new Set([
  "TaskOutput",
  "BashOutput",
  "write_stdin",
  "wait",
  "sleep",
  "collab:wait",
  "KillShell",
]);

const POLLING_COMMAND =
  /^\s*(?:sleep\s+\d+(?:\.\d+)?[smh]?|(?:\/usr\/bin\/|\/bin\/)?tail\s+(?:(?:-\d+|-n\s*\d+|-f)\s+)*[\w./@%+=,:-]+\.(?:log|out|txt))\s*$/i;

const CONTRACT_ERRORS: ReadonlyArray<{ category: string; tools: ReadonlySet<string>; pattern: RegExp }> = [
  {
    category: "missing",
    tools: new Set(["Edit", "Write", "Read", "NotebookEdit", "MultiEdit"]),
    pattern: /required parameter[\s\S]{0,80}\bmissing\b|missing required parameter/i,
  },
  {
    category: "stale",
    tools: new Set(["Edit", "MultiEdit", "NotebookEdit"]),
    pattern: /string to replace not found|old_string[\s\S]{0,60}not found|target[\s\S]{0,40}not found/i,
  },
  {
    category: "ambiguous",
    tools: new Set(["Edit", "MultiEdit", "NotebookEdit"]),
    pattern: /found \d+ matches of the string|multiple occurrences/i,
  },
  {
    category: "same",
    tools: new Set(["Edit", "MultiEdit", "NotebookEdit"]),
    pattern: /old_string and new_string are exactly the same|no changes to make/i,
  },
];

const SECRET_PATTERNS = [
  /\b(sk-[A-Za-z0-9_-]{12,})\b/g,
  /\b(gh[pousr]_[A-Za-z0-9]{16,})\b/g,
  /\b(xox[baprs]-[A-Za-z0-9-]{12,})\b/g,
  /\b(Bearer\s+)[A-Za-z0-9._~+/=-]{12,}/gi,
  /\b(AIza[0-9A-Za-z_-]{20,})\b/g,
];

function clip(value: string, max = 600): string {
  return value.length > max ? value.slice(0, max) : value;
}

function oneLine(value: string, max = 84): string {
  const normalized = redactSecrets(value).replace(/\s+/g, " ").trim();
  return normalized.length > max ? `${normalized.slice(0, max - 1)}…` : normalized;
}

export function redactSecrets(value: string): string {
  let out = value;
  for (const pattern of SECRET_PATTERNS) {
    out = out.replace(pattern, (_match, prefix) => (typeof prefix === "string" && /^Bearer\s+/i.test(prefix) ? `${prefix}[REDACTED]` : "[REDACTED]"));
  }
  return out;
}

export function shortHash(value: string): string {
  let hash = 0x811c9dc5;
  for (let i = 0; i < value.length; i += 1) {
    hash ^= value.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash.toString(36);
}

export function isPollingStep(step: Pick<ToolStepSample, "name" | "label">): boolean {
  if (POLLING_TOOLS.has(step.name)) return true;
  if (/^(?:Bash|bash|exec|exec_command|shell|execute)$/i.test(step.name) && POLLING_COMMAND.test(step.label)) {
    return true;
  }
  return false;
}

function contractErrorCategory(step: ToolStepSample): string | null {
  if (step.status !== "failed" && !/error|not found|missing|matches/i.test(step.errorText)) return null;
  if (!step.errorText) return null;
  for (const matcher of CONTRACT_ERRORS) {
    if (matcher.tools.has(step.name) && matcher.pattern.test(step.errorText)) {
      return matcher.category;
    }
  }
  return null;
}

export function stepsFromLiveTurnEvents(
  events: ReadonlyArray<{
    kind: string;
    status?: string | null;
    tool_name?: string | null;
    summary?: string | null;
    line?: string | null;
  }> | undefined,
): ToolStepSample[] {
  if (!events?.length) return [];
  const steps: ToolStepSample[] = [];
  for (const ev of events) {
    if (ev.kind !== "tool") continue;
    const name = ev.tool_name?.trim() || "Tool";
    const rawDetail = ev.summary?.trim() || ev.line?.trim() || name;
    const status: ToolStepSample["status"] =
      ev.status === "error" ? "failed" : ev.status === "running" ? "running" : "completed";
    const errorText = status === "failed" ? clip(`${ev.summary ?? ""} ${ev.line ?? ""}`.trim()) : "";
    steps.push({
      name,
      input: `${name}|${clip(rawDetail, 300)}`,
      output: `${status}|${errorText}`,
      status,
      errorText,
      label: oneLine(rawDetail),
    });
  }
  return steps;
}

export function stepsFromTranscriptEvents(events: readonly SessionTranscriptEvent[]): ToolStepSample[] {
  const steps: ToolStepSample[] = [];
  const pairedIndices = new Set<number>();
  for (let i = 0; i < events.length; i += 1) {
    const ev = events[i];
    if (ev.kind !== "tool_use") continue;
    let paired: SessionTranscriptEvent | null = null;
    for (let j = i + 1; j < events.length; j += 1) {
      const next = events[j];
      if (next.kind === "tool_use" && next.tool_name === ev.tool_name) break;
      if (next.kind === "tool_result" && !pairedIndices.has(j) && (!next.tool_name || !ev.tool_name || next.tool_name === ev.tool_name)) {
        paired = next;
        pairedIndices.add(j);
        break;
      }
    }
    const name = ev.tool_name?.trim() || paired?.tool_name?.trim() || "Tool";
    const inputBody = ev.content?.trim() || ev.summary?.trim() || "";
    const outputBody = paired ? paired.content?.trim() || paired.summary?.trim() || "" : "";
    const failed = Boolean(ev.is_error || paired?.is_error || ev.status === "error" || paired?.status === "error");
    const running = !paired && (ev.status === "running" || ev.status === "pending");
    const status: ToolStepSample["status"] = failed ? "failed" : running ? "running" : "completed";
    steps.push({
      name,
      input: `${name}|${clip(inputBody, 400)}`,
      output: `${status}|${clip(outputBody, 400)}`,
      status,
      errorText: failed ? clip(`${outputBody} ${inputBody}`.trim()) : "",
      label: oneLine(ev.summary?.trim() || inputBody || name),
    });
  }
  return steps;
}

export function detectTurnLoop(allSteps: readonly ToolStepSample[]): LoopFinding | null {
  const finished = allSteps.filter((step) => step.status !== "running" && !isPollingStep(step));
  if (finished.length < LOOP_CONTRACT) return null;

  const fullSig = (step: ToolStepSample) => `${step.input}\u0000${step.output}`;
  const buildEvidence = (slice: readonly ToolStepSample[]) =>
    slice.slice(-6).map((step) => (step.label && step.label !== step.name ? `${step.name}: ${step.label}` : step.name));
  const makeResult = (pattern: LoopPattern, count: number, slice: readonly ToolStepSample[], sig: string) => ({
    finding: {
      pattern,
      count,
      tool: slice[slice.length - 1]!.name,
      evidence: buildEvidence(slice),
      key: `${pattern}:${shortHash(sig)}`,
    },
    inputs: new Set(slice.map((step) => step.input)),
  });

  let sameRun = 1;
  while (
    sameRun < finished.length &&
    fullSig(finished[finished.length - 1 - sameRun]!) === fullSig(finished[finished.length - 1]!)
  ) {
    sameRun += 1;
  }

  let matched: ReturnType<typeof makeResult> | null = null;
  if (sameRun >= LOOP_SAME) {
    matched = makeResult("same", sameRun, finished.slice(-sameRun), fullSig(finished[finished.length - 1]!));
  } else {
    const tail = finished.slice(-LOOP_CONTRACT);
    const categories = tail.map(contractErrorCategory);
    if (
      tail.length === LOOP_CONTRACT &&
      categories[0] &&
      categories.every((cat) => cat === categories[0]) &&
      tail.every((step) => step.name === tail[0]!.name)
    ) {
      matched = makeResult("contract", LOOP_CONTRACT, tail, `${tail[0]!.name}:${categories[0]}`);
    } else if (finished.length >= LOOP_WINDOW) {
      const win12 = finished.slice(-LOOP_WINDOW);
      const distinct12 = [...new Set(win12.map((step) => step.input))].sort();
      if (distinct12.length <= LOOP_WINDOW_DISTINCT) {
        matched = makeResult("pingpong", LOOP_WINDOW, win12, distinct12.join("\u0001"));
      } else if (finished.length >= LOOP_ROTATION_WINDOW) {
        const win16 = finished.slice(-LOOP_ROTATION_WINDOW);
        const distinct16 = [...new Set(win16.map((step) => step.input))].sort();
        if (distinct16.length <= LOOP_ROTATION_DISTINCT) {
          matched = makeResult("rotation", LOOP_ROTATION_WINDOW, win16, distinct16.join("\u0001"));
        }
      }
    }
  }

  if (!matched) return null;
  const lastFinishedIdx = allSteps.lastIndexOf(finished[finished.length - 1]!);
  const newerRunning = allSteps
    .slice(lastFinishedIdx + 1)
    .some(
      (step) =>
        step.status === "running" &&
        !isPollingStep(step) &&
        !matched.inputs.has(step.input) &&
        !(matched.finding.pattern === "contract" && step.name === matched.finding.tool),
    );
  return newerRunning ? null : matched.finding;
}

export function formatLoopSummary(finding: LoopFinding, tr: ObsTranslator): string {
  switch (finding.pattern) {
    case "same":
      return tr(
        `${finding.tool} 동일 호출이 ${finding.count}회 연속 반복되고 있어요`,
        `${finding.tool} repeated identically ${finding.count} times in a row`,
      );
    case "contract":
      return tr(
        `${finding.tool} 동일 인자 오류가 ${finding.count}회 연속 발생했어요`,
        `${finding.tool} hit the same argument error ${finding.count} times`,
      );
    case "pingpong":
      return tr(
        `최근 ${finding.count}회 도구 호출이 2가지 패턴만 왕복하고 있어요`,
        `Last ${finding.count} tool calls are alternating between 2 patterns`,
      );
    case "rotation":
      return tr(
        `최근 ${finding.count}회 도구 호출이 소수 패턴만 순환하고 있어요`,
        `Last ${finding.count} tool calls are cycling through <=4 patterns`,
      );
  }
}

export type StopReasonKind = "quota" | "silent" | "unfinished";

export interface TurnStopDiagnosis {
  kind: StopReasonKind;
  resetAtMs: number | null;
  resetGuessed: boolean;
  resetKstLabel: string | null;
}

const THEN_RESET =
  String.raw`(?:\s*[.,:;!|·∙—–-])*\s*(?:(?:please\s+)?(?:try again|retry|wait)\b|(?:(?:your\s+)?(?:limit|quota|usage)\s+(?:will\s+)?)?(?:resets?|available again)\b)|\s*\|\s*\d{10,13}\b`;

const LIMIT_STRICT = [
  /^(you['’]ve|you have) (hit|reached|exceeded) your ((usage|rate|session|weekly|daily|monthly|5-hour|five-hour|opus|sonnet|haiku|fable|plan|spend|credit|extra)[ -])*limits?\b/i,
  /^you['’]re out of (usage |extra usage )?credits\b/i,
  new RegExp(String.raw`^(\d+-hour|five-hour|weekly|daily|session) limit (reached|exceeded|hit)(?:\s*[.!。]?\s*$|${THEN_RESET})`, "i"),
];

const LIMIT_LOOSE_REPLY = [
  new RegExp(String.raw`^(you['’]ve|you have) (hit|reached|exceeded) (your|the) [\p{L}\p{N}'’ -]{0,40}?\blimits?(?:${THEN_RESET})`, "iu"),
  new RegExp(String.raw`^(claude( ai| code)?|codex|gemini|grok|cursor)?\s*((usage|rate|session|weekly|daily|5-hour|five-hour)[ -])+limits? (reached|exceeded|hit)(?:${THEN_RESET})`, "i"),
  new RegExp(String.raw`^(rate|usage) limit(ed)?( (reached|exceeded|hit))?(?:${THEN_RESET})`, "i"),
];

const LIMIT_ERROR =
  /\b(rate[ _-]?limit(ed)?|usage[ _-]?limit|quota exceeded|insufficient_quota|too many requests|limit (reached|exceeded)|resource[ _-]?exhausted)\b|\b429\b|한도\s*(초과|도달|소진)|쿼터\s*(초과|소진)/i;

const WORK_SUMMARY_FALSE_POSITIVE =
  /\b(handled|fixed|added|implemented|updated|patched|tests?|caught)\b|구현|수정|추가|패치|처리했|해결했/i;

const NOT_QUOTA = /context (window|length)|max(imum)?[ _-]?(output[ _-]?)?tokens|prompt is too long|컨텍스트\s*(길이|초과|한도)/i;

const LOCAL_COMMAND = /^\s*\/[a-z][\w-]*/i;

const MONTHS = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const WEEKDAYS = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
const HALF_DAY_MS = 43_200_000;

function zoneOffsetMinutes(atMs: number, zone: string): number {
  const parts = new Intl.DateTimeFormat("en-US", {
    timeZone: zone,
    hourCycle: "h23",
    year: "numeric",
    month: "numeric",
    day: "numeric",
    hour: "numeric",
    minute: "numeric",
    second: "numeric",
  }).formatToParts(atMs);
  const val = (type: string) => Number(parts.find((p) => p.type === type)?.value ?? 0);
  const utcMs = Date.UTC(val("year"), val("month") - 1, val("day"), val("hour") % 24, val("minute"), val("second"));
  return Math.round((utcMs - Math.floor(atMs / 1000) * 1000) / 60_000);
}

function wallInstants(year: number, month: number, day: number, hour: number, minute: number, zone: string): number[] {
  const wall = Date.UTC(year, month, day, hour, minute);
  const offsets = new Set([-HALF_DAY_MS, 0, HALF_DAY_MS].map((shift) => zoneOffsetMinutes(wall + shift, zone)));
  const exact = [...offsets]
    .map((offset) => wall - offset * 60_000)
    .filter((at) => at + zoneOffsetMinutes(at, zone) * 60_000 === wall)
    .sort((a, b) => a - b);
  return exact.length ? exact : [wall - zoneOffsetMinutes(wall - HALF_DAY_MS, zone) * 60_000];
}

function validZone(zone: string | undefined): string | null {
  if (!zone || zone.length > 64 || !/^[A-Za-z][\w+-]*(\/[\w+-]+){0,2}$/.test(zone)) return null;
  try {
    new Intl.DateTimeFormat("en-US", { timeZone: zone });
    return zone;
  } catch {
    return null;
  }
}

function zonedDate(atMs: number, zone: string): { year: number; month: number; day: number } {
  const shifted = new Date(atMs + zoneOffsetMinutes(atMs, zone) * 60_000);
  return { year: shifted.getUTCFullYear(), month: shifted.getUTCMonth(), day: shifted.getUTCDate() };
}

export function formatKstTimestamp(epochMs: number | null | undefined, tr: ObsTranslator): string | null {
  if (epochMs == null || !Number.isFinite(epochMs) || epochMs <= 0) return null;
  const parts = new Intl.DateTimeFormat("en-CA", {
    timeZone: "Asia/Seoul",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  }).formatToParts(new Date(epochMs));
  const pick = (type: string) => parts.find((p) => p.type === type)?.value ?? "";
  const formatted = `${Number(pick("month"))}/${Number(pick("day"))} ${pick("hour")}:${pick("minute")} KST`;
  return tr(`${formatted} 복구`, `Resets ${formatted}`);
}

export function parseQuotaResetMs(
  rawText: string,
  nowMs = Date.now(),
  defaultZone = "Asia/Seoul",
): { atMs: number; guessed: boolean } | null {
  const plausible = (at: number | null, guessed = false) =>
    at !== null && Number.isFinite(at) && at >= nowMs - 60 * 60_000 && at <= nowMs + 8 * 86_400_000
      ? { atMs: Math.trunc(at), guessed }
      : null;

  const pipeEpoch = /\|\s*(\d{10,13})\b/.exec(rawText);
  if (pipeEpoch) {
    const num = Number(pipeEpoch[1]);
    const hit = plausible(num < 1e12 ? num * 1000 : num);
    if (hit) return hit;
  }

  const iso = /\b(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:Z|[+-]\d{2}:?\d{2}))\b/.exec(rawText);
  if (iso) {
    const hit = plausible(Date.parse(iso[1]!));
    if (hit) return hit;
  }

  const relative =
    /\b(?:try again|retry|resets?|available again|wait)\s+(?:in|after)\s+((?:\d+(?:\.\d+)?\s*(?:days?|d|hours?|hrs?|h|minutes?|mins?|m|seconds?|secs?|s)\b\s*(?:and\s*|,\s*)?)+)/i.exec(
      rawText,
    ) ?? /(\d+(?:\.\d+)?\s*(?:일|시간|분|초)(?:\s*\d+(?:\.\d+)?\s*(?:일|시간|분|초))*)\s*(?:뒤|후)\s*(?:초기화|복구|재시도)/.exec(rawText);
  if (relative) {
    let deltaMs = 0;
    for (const [, amount, unit] of relative[1]!.matchAll(
      /(\d+(?:\.\d+)?)\s*(days?|d|hours?|hrs?|h|minutes?|mins?|m|seconds?|secs?|s|일|시간|분|초)(?![a-z])/gi,
    )) {
      const u = unit!.toLowerCase();
      const mult =
        u.startsWith("d") || u === "일"
          ? 86_400_000
          : u.startsWith("h") || u === "시간"
            ? 3_600_000
            : u.startsWith("s") || u === "초"
              ? 1_000
              : 60_000;
      deltaMs += Number(amount) * mult;
    }
    const hit = deltaMs > 0 ? plausible(nowMs + deltaMs) : null;
    if (hit) return hit;
  }

  const clock =
    /\b(?:resets?|reset at|available|try again at|again at|until)\s+(?:on\s+|at\s+)?(?:(mon|tue|wed|thu|fri|sat|sun)[a-z]*\.?,?\s+)?(?:(jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\.?\s+(\d{1,2})(?:st|nd|rd|th)?,?\s+(?:(\d{4}),?\s+)?(?:at\s+)?)?(\d{1,2})(?::(\d{2}))?\s*(am|pm)?\b(?:\s*\(([^)]+)\))?/i.exec(
      rawText,
    );
  if (!clock) return null;
  const [, weekdayName, monthName, dayText, yearText, hourText, minuteText, meridiem, zoneText] = clock;
  if (!meridiem && !minuteText) return null;
  let hour = Number(hourText);
  const minute = Number(minuteText ?? 0);
  if (meridiem) {
    if (hour < 1 || hour > 12) return null;
    hour = (hour % 12) + (meridiem.toLowerCase() === "pm" ? 12 : 0);
  }
  if (hour > 23 || minute > 59) return null;

  const namedZone = validZone(zoneText?.trim());
  const guessed = !namedZone;
  const zone = namedZone ?? validZone(defaultZone) ?? "Asia/Seoul";
  const today = zonedDate(nowMs, zone);
  const upcoming = (instants: readonly number[]) =>
    instants.find((at) => at >= nowMs) ?? [...instants].reverse().find((at) => at >= nowMs - 60_000);

  if (monthName) {
    const month = MONTHS.indexOf(monthName.toLowerCase().slice(0, 3));
    const day = Number(dayText);
    const onYear = (yr: number) =>
      new Date(Date.UTC(yr, month, day)).getUTCDate() === day ? wallInstants(yr, month, day, hour, minute, zone) : [];
    if (yearText) {
      const instants = onYear(Number(yearText));
      return plausible(upcoming(instants) ?? instants.at(-1) ?? null, guessed);
    }
    const thisYear = onYear(today.year);
    const hit = upcoming(thisYear) ?? thisYear.filter((at) => at >= nowMs - 60 * 60_000).at(-1);
    return plausible(hit ?? upcoming(onYear(today.year + 1)) ?? null, guessed);
  }

  for (let offset = 0; offset <= 7; offset += 1) {
    const candidate = new Date(Date.UTC(today.year, today.month, today.day + offset));
    if (weekdayName && WEEKDAYS[candidate.getUTCDay()] !== weekdayName.toLowerCase().slice(0, 3)) continue;
    const at = upcoming(
      wallInstants(candidate.getUTCFullYear(), candidate.getUTCMonth(), candidate.getUTCDate(), hour, minute, zone),
    );
    if (at !== undefined) return plausible(at, guessed);
  }
  return null;
}

const LIMIT_IN_ERROR = LIMIT_STRICT.map(
  (re) => new RegExp(re.source.replace(/^\^/, "(?:^|[\\s:：>\\]】)])"), re.flags),
);

function detectQuotaNotice(
  raw: string | null | undefined,
  mode: "error" | "reply",
  nowMs: number,
  tr: ObsTranslator,
): TurnStopDiagnosis | null {
  const trimmed = raw?.trim() ?? "";
  if (!trimmed || trimmed.length > 800 || NOT_QUOTA.test(trimmed)) return null;
  const matched =
    mode === "error"
      ? LIMIT_ERROR.test(trimmed) || LIMIT_IN_ERROR.some((re) => re.test(trimmed))
      : LIMIT_STRICT.some((re) => re.test(trimmed)) ||
        (trimmed.length <= 320 && !WORK_SUMMARY_FALSE_POSITIVE.test(trimmed) && LIMIT_LOOSE_REPLY.some((re) => re.test(trimmed)));
  if (!matched) return null;
  const parsed = parseQuotaResetMs(trimmed, nowMs);
  return {
    kind: "quota",
    resetAtMs: parsed?.atMs ?? null,
    resetGuessed: parsed?.guessed ?? false,
    resetKstLabel: formatKstTimestamp(parsed?.atMs ?? null, tr),
  };
}

export function classifyTranscriptStop(
  transcript: Pick<SessionTranscript, "user_message" | "assistant_message" | "events" | "created_at">,
  tr: ObsTranslator,
  nowMs?: number,
): TurnStopDiagnosis | null {
  const refMs = nowMs ?? (Number.isFinite(Date.parse(transcript.created_at)) ? Date.parse(transcript.created_at) : Date.now());
  const events = transcript.events ?? [];

  for (let i = events.length - 1; i >= 0; i -= 1) {
    const ev = events[i]!;
    if (ev.kind === "error" || ev.is_error) {
      const quota = detectQuotaNotice(ev.content || ev.summary, "error", refMs, tr);
      if (quota) return quota;
    }
    if (ev.kind === "assistant" || ev.kind === "tool_use" || ev.kind === "tool_result") break;
  }

  const assistantTail =
    transcript.assistant_message?.trim() ||
    [...events]
      .reverse()
      .find((ev) => ev.kind === "assistant" && (ev.content?.trim() || ev.summary?.trim()))
      ?.content?.trim() ||
    "";

  if (assistantTail) {
    const lines = assistantTail.split("\n").map((l) => l.trim()).filter(Boolean);
    return (
      detectQuotaNotice(assistantTail, "reply", refMs, tr) ??
      (lines.length > 1 ? detectQuotaNotice(lines.at(-1), "reply", refMs, tr) : null)
    );
  }

  const hasPrompt = Boolean(transcript.user_message?.trim());
  if (!hasPrompt) return null;
  if (LOCAL_COMMAND.test(transcript.user_message)) return null;

  const meaningful = events.filter((ev) => ev.kind === "assistant" || ev.kind === "tool_use" || ev.kind === "tool_result" || ev.kind === "error");
  if (meaningful.length === 0) {
    return { kind: "silent", resetAtMs: null, resetGuessed: false, resetKstLabel: null };
  }

  const lastMeaningful = meaningful.at(-1);
  if (lastMeaningful && (lastMeaningful.kind === "tool_use" || lastMeaningful.kind === "tool_result") && !lastMeaningful.is_error) {
    return { kind: "unfinished", resetAtMs: null, resetGuessed: false, resetKstLabel: null };
  }
  return null;
}

export function classifyRawOutputStop(
  recentOutput: string | null | undefined,
  tr: ObsTranslator,
  nowMs = Date.now(),
): TurnStopDiagnosis | null {
  if (!recentOutput?.trim()) return null;
  const tailLines = recentOutput
    .trim()
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean)
    .slice(-6);
  for (let i = tailLines.length - 1; i >= 0; i -= 1) {
    const candidate = tailLines.slice(i).join(" ");
    const hit = detectQuotaNotice(candidate, "error", nowMs, tr);
    if (hit && (LIMIT_IN_ERROR.some((re) => re.test(tailLines[i]!)) || /You['’]ve hit your|limit reached|insufficient_quota|429\b.*resets/i.test(candidate))) {
      return hit;
    }
  }
  return null;
}

export function formatStopBadgeLabel(stop: TurnStopDiagnosis, tr: ObsTranslator): string {
  if (stop.kind === "quota") {
    return stop.resetKstLabel
      ? `${tr("한도 소진", "Quota hit")} · ${stop.resetKstLabel}`
      : tr("한도 소진", "Quota hit");
  }
  if (stop.kind === "silent") return tr("빈 턴", "Silent turn");
  return tr("미마무리 종료", "Unfinished turn");
}

export type ReplyNudgeKind = "go-ahead" | "keep-going" | "question";

export interface ReplyInvitation {
  asksUser: boolean;
  nudge: ReplyNudgeKind;
  quote: string;
}

const COURTESY_CLOSE = [
  /^(is there )?anything else\b.{0,60}$/i,
  /^(any (other |more |further )?questions)\??$/i,
  /^(i )?hope (this|that|it) helps\b/i,
  /^(please )?(just )?(let me know|tell me|feel free to)\b.{0,80}\b(any|anything|further|other|additional|questions?)\b(?!.*\bme to\b)/i,
  /^(추가로|더)?\s*(궁금한|필요한|수정할|고칠)\s*(점|부분|사항|것)이?\s*(있으시면|있다면|있으면)\s*(언제든)?\s*(말씀해|알려)\s*주세요[.!~]*$/,
  /^도움이\s*(되셨길|되었길|됐길)\s*바랍니다[.!~]*$/,
  /^언제든\s*(편하게)?\s*(질문해|말씀해)\s*주세요[.!~]*$/,
];

const EXPLICIT_REQUEST =
  /\b(please (confirm|choose|decide|provide|reply|share|approve|review)|let me know (which|whether|if you want me to)|your call|up to you)\b|(확인해?\s*주세요|선택해?\s*주세요|결정해?\s*주세요|알려\s*주세요|승인해?\s*주세요|어느\s*쪽|어떤\s*(방식|방법|옵션|안)으로)/i;

const GO_AHEAD_PROMPT =
  /\b(want me to|would you like me to|shall (i|we)|should (i|we)|can (i|we) (proceed|continue|go ahead|merge|deploy|start)|ready to (go|proceed|merge|deploy|ship))\b|(진행할까요|계속할까요|머지할까요|병합할까요|배포할까요|적용할까요|시작할까요|수정할까요|반영할까요)\s*[?？]?$/i;

const KEEP_GOING_STATEMENT =
  /^(?:next[,:\s]+|now[,:\s]+)?(?:i(?:'ll| will| am going to)|we(?:'ll| will)|let me)\s+(?:now\s+|next\s+|go ahead and\s+)?(?:fix|update|add|run|write|implement|refactor|patch|apply|create|migrate|deploy|push|commit|merge|test|verify|continue|proceed)\b|^(?:다음으로|이어서|이제)\s+.+(?:하겠습니다|진행하겠습니다)[.!]*$/i;

const NOT_IMMEDIATE_NEXT = /\b(wait(ing)?|once ci|after ci|when you|if you|tomorrow|later)\b|(후에|뒤에|통과하면|승인되면)/i;

function isCourtesySentence(sentence: string): boolean {
  const s = sentence.trim();
  if (!s) return true;
  return COURTESY_CLOSE.some((re) => re.test(s));
}

function extractClosingSentences(rawText: string): Array<{ masked: string; shown: string }> {
  const spans: Array<{ mask: string; original: string }> = [];
  const mark = (mask: string) => (original: string) => `\uE000${spans.push({ mask, original }) - 1}\uE001`;
  const cleaned = rawText
    .replace(/[\uE000\uE001]/g, "")
    .replace(/```[\s\S]*?(?:```|$)/g, "\n\n")
    .replace(/`[^`\n]*`/g, mark("CODE"))
    .replace(/\]\([^)\s]*\)/g, "]")
    .replace(/https?:\/\/\S+/g, mark("URL"))
    .replace(/「[^」\n]*」|“[^”\n]*”|"[^"\n]*"/g, mark("QUOTE"));

  const unmaskMask = (s: string) => s.replace(/\uE000(\d+)\uE001/g, (_, idx) => spans[Number(idx)]!.mask);
  const unmaskOrig = (s: string) => {
    let out = s;
    for (let depth = 0; depth < 4 && out.includes("\uE000"); depth += 1) {
      out = out.replace(/\uE000(\d+)\uE001/g, (_, idx) => spans[Number(idx)]!.original);
    }
    return out;
  };

  const blocks = cleaned
    .split(/\n\s*\n/)
    .map((block) =>
      block
        .split("\n")
        .filter((line) => line.trim() && !/^\s*(#{1,6}\s|\||[-*_]{3,}\s*$)/.test(line))
        .join("\n")
        .trim(),
    )
    .map((block) =>
      block
        .split(/(?<=[。！!？?;])|(?<=[.])\s+|\n/)
        .map((s) => s.trim())
        .filter(Boolean)
        .map((s) => ({ masked: unmaskMask(s), shown: unmaskOrig(s) }))
        .filter((item) => !isCourtesySentence(item.masked)),
    )
    .filter((arr) => arr.length > 0);

  let tail = blocks.at(-1) ?? [];
  if (blocks.length > 1 && tail.map((x) => x.masked).join("").length < 24) {
    tail = [...blocks.at(-2)!, ...tail];
  }
  return tail.slice(-4);
}

export function analyzeReplyInvitation(text: string | null | undefined): ReplyInvitation | null {
  if (!text?.trim()) return null;
  const tail = extractClosingSentences(text);
  if (tail.length === 0) return null;

  const normalizeShown = (raw: string) => {
    const clean = redactSecrets(raw)
      .replace(/\*\*|__|~~/g, "")
      .replace(/^\s*(?:[-+•]|\d+[.)])\s+/, "")
      .replace(/\s+/g, " ")
      .trim();
    return clean.length > 120 ? `${clean.slice(0, 119)}…` : clean;
  };

  const last = tail.at(-1)!;
  const plainLast = last.masked.replace(/[*_~]+/g, "").trim();

  if (GO_AHEAD_PROMPT.test(plainLast)) {
    return { asksUser: true, nudge: "go-ahead", quote: normalizeShown(last.shown) };
  }

  for (let i = tail.length - 1; i >= 0; i -= 1) {
    const candidate = tail[i]!;
    const plain = candidate.masked.replace(/[*_~]+/g, "").trim();
    if (GO_AHEAD_PROMPT.test(plain)) {
      return { asksUser: true, nudge: "go-ahead", quote: normalizeShown(candidate.shown) };
    }
    if (EXPLICIT_REQUEST.test(plain) || /[?？][)）」"”]*$/.test(plain)) {
      return { asksUser: true, nudge: "question", quote: normalizeShown(candidate.shown) };
    }
  }

  if (plainLast.length <= 180 && !NOT_IMMEDIATE_NEXT.test(plainLast) && KEEP_GOING_STATEMENT.test(plainLast)) {
    return { asksUser: false, nudge: "keep-going", quote: normalizeShown(last.shown) };
  }

  return null;
}

export type PromptCachePhase = "warm" | "expiring" | "cold";

export interface PromptCacheStatus {
  phase: PromptCachePhase;
  remainingMs: number;
  coldReason: "expired" | "compacted" | null;
}

const COMPACTION_SIGNAL = /\bcompact_boundary\b|conversation compacted|context compacted|^\s*\/compact\b/im;

export function computePromptCacheStatus({
  provider,
  lastActivityMs,
  isWorking,
  recentOutput,
  nowMs = Date.now(),
}: {
  provider: string | null | undefined;
  lastActivityMs: number | null | undefined;
  isWorking?: boolean;
  recentOutput?: string | null;
  nowMs?: number;
}): PromptCacheStatus | null {
  if ((provider ?? "").trim().toLowerCase() !== "claude") return null;
  if (recentOutput && COMPACTION_SIGNAL.test(recentOutput)) {
    return { phase: "cold", remainingMs: 0, coldReason: "compacted" };
  }
  if (lastActivityMs == null || !Number.isFinite(lastActivityMs) || lastActivityMs <= 0) return null;
  const elapsed = Math.max(0, nowMs - lastActivityMs);
  if (elapsed > CLAUDE_CACHE_RECENT_WINDOW_MS && !isWorking) return null;
  if (isWorking && elapsed < CLAUDE_CACHE_TTL_MS) {
    const rem = CLAUDE_CACHE_TTL_MS - elapsed;
    return {
      phase: rem <= CLAUDE_CACHE_EXPIRING_MS ? "expiring" : "warm",
      remainingMs: rem,
      coldReason: null,
    };
  }
  const remaining = CLAUDE_CACHE_TTL_MS - elapsed;
  if (remaining <= 0) {
    return { phase: "cold", remainingMs: 0, coldReason: "expired" };
  }
  return {
    phase: remaining <= CLAUDE_CACHE_EXPIRING_MS ? "expiring" : "warm",
    remainingMs: remaining,
    coldReason: null,
  };
}

export function formatPromptCacheLabel(status: PromptCacheStatus, tr: ObsTranslator): string {
  if (status.phase === "cold") {
    return status.coldReason === "compacted"
      ? tr("캐시 식음 (압축됨)", "Cache cold (compacted)")
      : tr("캐시 식음", "Cache cold");
  }
  if (status.phase === "expiring") {
    const secs = Math.max(1, Math.ceil(status.remainingMs / 1_000));
    return tr(`캐시 ${secs}초`, `Cache ${secs}s`);
  }
  const mins = Math.max(1, Math.ceil(status.remainingMs / 60_000));
  return tr(`캐시 ${mins}분`, `Cache ${mins}m`);
}

function normalizeSearchText(value: string): string {
  return value.normalize("NFKC").toLowerCase();
}

export function parseSearchTerms(query: string): string[] {
  const terms: string[] = [];
  const normalized = normalizeSearchText(query.trim());
  if (!normalized) return terms;
  for (const match of normalized.matchAll(/"([^"]+)"|(\S+)/g)) {
    const token = (match[1] ?? match[2] ?? "").trim();
    if (token) terms.push(token);
  }
  return terms;
}

export function searchTranscriptEvents(
  events: readonly SessionTranscriptEvent[],
  query: string,
): number[] {
  const terms = parseSearchTerms(query);
  if (terms.length === 0) return [];
  const matches: number[] = [];
  for (let i = 0; i < events.length; i += 1) {
    const ev = events[i]!;
    const haystack = normalizeSearchText(`${ev.kind} ${ev.tool_name ?? ""} ${ev.summary ?? ""} ${ev.content ?? ""}`);
    if (terms.every((term) => haystack.includes(term))) {
      matches.push(i);
    }
  }
  return matches;
}
