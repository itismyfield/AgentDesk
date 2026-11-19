import * as api from "../../api";
import { parseTranscriptDate } from "./turn-transcript-utils";

export type TranscriptSource =
  | { type: "agent"; id: string; refreshSeed?: string | number | null; limit?: number }
  | { type: "card"; id: string; refreshSeed?: string | number | null; limit?: number };

export type TranscriptTranslator = (ko: string, en: string) => string;

export type TranscriptTone = "assistant" | "thinking" | "tool" | "result" | "error";

const toneFrom = (token: string) => ({
  bar: `color-mix(in oklch, ${token} 18%, transparent)`,
  border: `color-mix(in oklch, ${token} 55%, transparent)`,
  text: `color-mix(in oklch, ${token} 72%, var(--th-text-primary))`,
  dot: token,
});

/** Theme-token colors per event tone, so bars and tiles follow light and dark. */
export const TONE_STYLE: Record<TranscriptTone, { bar: string; border: string; text: string; dot: string }> = {
  assistant: toneFrom("var(--ok)"),
  thinking: toneFrom("var(--accent-violet)"),
  tool: toneFrom("var(--info)"),
  result: toneFrom("var(--th-text-muted)"),
  error: toneFrom("var(--err)"),
};

export const TRANSCRIPT_TONES: TranscriptTone[] = ["assistant", "thinking", "tool", "result", "error"];

export function toneLabel(tone: TranscriptTone, tr: TranscriptTranslator): string {
  switch (tone) {
    case "assistant": return tr("에이전트", "Agent");
    case "thinking": return "Thinking";
    case "tool": return tr("도구", "Tool");
    case "result": return tr("결과", "Result");
    case "error": return tr("오류", "Error");
  }
}

export function eventTone(event: api.SessionTranscriptEvent): TranscriptTone {
  if (event.is_error || event.kind === "error") return "error";
  if (event.kind === "thinking") return "thinking";
  if (event.kind === "tool_use") return "tool";
  if (
    event.kind === "tool_result" ||
    event.kind === "result" ||
    event.kind === "task" ||
    event.kind === "system"
  ) {
    return "result";
  }
  return "assistant";
}

export function eventTitle(
  event: api.SessionTranscriptEvent,
  tr: TranscriptTranslator,
): string {
  switch (event.kind) {
    case "assistant":
      return tr("에이전트", "Agent");
    case "thinking":
      return tr("Thinking", "Thinking");
    case "tool_use":
      return event.tool_name || tr("도구", "Tool");
    case "tool_result":
      return event.tool_name
        ? tr(`${event.tool_name} 결과`, `${event.tool_name} result`)
        : tr("도구 결과", "Tool result");
    case "result":
      return tr("최종 결과", "Final result");
    case "error":
      return tr("오류", "Error");
    case "task":
      return tr("작업 알림", "Task update");
    case "system":
      return tr("시스템", "System");
    default:
      return tr("이벤트", "Event");
  }
}

export function eventBody(event: api.SessionTranscriptEvent): string {
  return event.content.trim() || event.summary?.trim() || "";
}

export function mergeTranscriptEvents(
  events: api.SessionTranscriptEvent[],
): api.SessionTranscriptEvent[] {
  const merged: api.SessionTranscriptEvent[] = [];

  for (const rawEvent of events) {
    const event: api.SessionTranscriptEvent = {
      ...rawEvent,
      tool_name: rawEvent.tool_name?.trim() || null,
      summary: rawEvent.summary?.trim() || null,
      content: rawEvent.content?.trim() || "",
      status: rawEvent.status?.trim() || null,
    };
    const mergeable =
      event.kind === "assistant" ||
      event.kind === "thinking" ||
      event.kind === "result" ||
      event.kind === "error";
    const prev = merged[merged.length - 1];
    if (
      prev &&
      mergeable &&
      prev.kind === event.kind &&
      prev.tool_name === event.tool_name &&
      prev.status === event.status &&
      prev.is_error === event.is_error
    ) {
      if (event.content) {
        prev.content = prev.content
          ? `${prev.content}\n\n${event.content}`
          : event.content;
      }
      if (!prev.summary && event.summary) {
        prev.summary = event.summary;
      }
      continue;
    }

    if (eventBody(event) || event.kind === "result" || event.kind === "error") {
      merged.push(event);
    }
  }

  return merged;
}

export function transcriptEvents(
  transcript: api.SessionTranscript | null,
): api.SessionTranscriptEvent[] {
  if (!transcript) return [];
  if (transcript.events.length > 0) {
    return mergeTranscriptEvents(transcript.events);
  }

  const fallback: api.SessionTranscriptEvent[] = [];
  const assistantMessage = transcript.assistant_message.trim();
  if (assistantMessage) {
    fallback.push({
      kind: "assistant",
      tool_name: null,
      summary: null,
      content: assistantMessage,
      status: "success",
      is_error: false,
    });
  }
  if (!assistantMessage && transcript.user_message.trim()) {
    fallback.push({
      kind: "result",
      tool_name: null,
      summary: "completed",
      content: "",
      status: "success",
      is_error: false,
    });
  }
  return fallback;
}

export function buildCopyText(
  transcript: api.SessionTranscript,
  events: api.SessionTranscriptEvent[],
  tr: TranscriptTranslator,
): string {
  const lines = [
    `${tr("턴", "Turn")}: ${transcript.turn_id}`,
    `${tr("프로바이더", "Provider")}: ${transcript.provider ?? "-"}`,
    `${tr("생성 시각", "Created")}: ${transcript.created_at}`,
  ];
  if (transcript.duration_ms != null) {
    lines.push(`${tr("소요 시간", "Duration")}: ${transcript.duration_ms}ms`);
  }
  if (transcript.dispatch_title) {
    lines.push(`${tr("Dispatch", "Dispatch")}: ${transcript.dispatch_title}`);
  }
  if (transcript.user_message.trim()) {
    lines.push(`\n[${tr("사용자 요청", "Prompt")}]\n${transcript.user_message.trim()}`);
  }

  for (const event of events) {
    const label = eventTitle(event, tr);
    const body = eventBody(event);
    const header = event.tool_name ? `[${label}] ${event.tool_name}` : `[${label}]`;
    lines.push(body ? `\n${header}\n${body}` : `\n${header}`);
  }

  return lines.join("\n");
}

export async function copyText(text: string): Promise<void> {
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(text);
    return;
  }

  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.style.position = "fixed";
  textarea.style.opacity = "0";
  document.body.appendChild(textarea);
  textarea.focus();
  textarea.select();
  document.execCommand("copy");
  document.body.removeChild(textarea);
}

export function shouldRefreshAgent(event: Event, agentId: string): boolean {
  const detail = (event as CustomEvent).detail as
    | { type?: string; payload?: Record<string, unknown> }
    | undefined;
  const type = detail?.type;
  const payload = detail?.payload;
  if (!type || !payload) return false;

  if (type === "agent_status") {
    return payload.id === agentId;
  }

  if (
    type === "dispatched_session_new" ||
    type === "dispatched_session_update"
  ) {
    return payload.linked_agent_id === agentId;
  }

  return false;
}

export function shouldRefreshCard(event: Event, cardId: string): boolean {
  const detail = (event as CustomEvent).detail as
    | { type?: string; payload?: Record<string, unknown> }
    | undefined;
  const type = detail?.type;
  const payload = detail?.payload;
  if (!type || !payload) return false;

  if (type === "kanban_card_new" || type === "kanban_card_update") {
    return payload.id === cardId;
  }

  if (type === "dispatch_new" || type === "dispatch_update") {
    return payload.kanban_card_id === cardId;
  }

  return false;
}

/** What the event body is: a tool call carries its input, an error its message, everything else output. */
export function eventBodyLabel(event: api.SessionTranscriptEvent, tr: TranscriptTranslator): string {
  if (event.kind === "tool_use") return tr("입력", "Input");
  if (eventTone(event) === "error") return tr("오류", "Error");
  return tr("출력", "Output");
}

export interface TurnSpan {
  transcript: api.SessionTranscript;
  /** Seconds from the earliest turn start. */
  start: number;
  /** Seconds; null when the server did not record a duration. */
  duration: number | null;
}

/**
 * Places turns on one time axis. A transcript is written when its turn ends, so the start is created_at minus the duration.
 * Turns without a readable time keep their list order at the axis start.
 */
export function turnSpans(transcripts: api.SessionTranscript[]): { spans: TurnSpan[]; total: number } {
  const timed = transcripts.map((transcript) => {
    const end = parseTranscriptDate(transcript.created_at)?.getTime() ?? null;
    const duration = transcript.duration_ms != null && transcript.duration_ms >= 0 ? transcript.duration_ms : null;
    return { transcript, end, duration, start: end == null ? null : end - (duration ?? 0) };
  });
  const starts = timed.flatMap((item) => (item.start == null ? [] : [item.start]));
  const ends = timed.flatMap((item) => (item.end == null ? [] : [item.end]));
  const origin = starts.length ? Math.min(...starts) : 0;
  const last = ends.length ? Math.max(...ends) : origin;
  return {
    spans: timed.map((item) => ({
      transcript: item.transcript,
      start: item.start == null ? 0 : (item.start - origin) / 1000,
      duration: item.duration == null ? null : item.duration / 1000,
    })),
    total: Math.max(0, (last - origin) / 1000),
  };
}

const AXIS_STEPS = [0.5, 1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200, 86400];

/** About six ticks: the first step at least a sixth of the span, the range rounded up past an 8% margin. */
export function timeAxis(totalSeconds: number): { range: number; ticks: number[] } {
  const span = Math.max(1, totalSeconds);
  const target = span / 6;
  const step = AXIS_STEPS.find((value) => value >= target) ?? Math.ceil(target / 86400) * 86400;
  const range = Math.ceil((1.08 * span) / step) * step;
  const ticks: number[] = [];
  for (let value = 0; value <= range + 1e-9; value += step) ticks.push(Math.round(value * 100) / 100);
  return { range, ticks };
}

/** Offset from the axis start: seconds under a minute, then minutes, hours and days; exact multiples drop the lower unit. */
export function formatAxisOffset(seconds: number, isKo: boolean): string {
  const unit = (ko: string, en: string) => (isKo ? ko : en);
  if (seconds < 60) return `${Number(seconds.toFixed(1))}${unit("초", "s")}`;
  if (seconds < 3600) {
    const m = Math.floor(seconds / 60);
    const s = Math.round(seconds - m * 60);
    return s ? `${m}${unit("분", "m")} ${s}${unit("초", "s")}` : `${m}${unit("분", "m")}`;
  }
  if (seconds < 86400) {
    const h = Math.floor(seconds / 3600);
    const m = Math.round((seconds - h * 3600) / 60);
    return m ? `${h}${unit("시간", "h")} ${m}${unit("분", "m")}` : `${h}${unit("시간", "h")}`;
  }
  const d = Math.floor(seconds / 86400);
  const h = Math.round((seconds - d * 86400) / 3600);
  return h ? `${d}${unit("일", "d")} ${h}${unit("시간", "h")}` : `${d}${unit("일", "d")}`;
}

/** Event numbers for the order axis: the first, the last and up to three evenly spaced in between. */
export function orderTicks(count: number): number[] {
  if (count <= 0) return [];
  if (count <= 5) return Array.from({ length: count }, (_, index) => index + 1);
  const ticks = new Set<number>([1, count]);
  for (let part = 1; part < 4; part += 1) ticks.add(Math.round(1 + ((count - 1) * part) / 4));
  return Array.from(ticks).sort((a, b) => a - b);
}
