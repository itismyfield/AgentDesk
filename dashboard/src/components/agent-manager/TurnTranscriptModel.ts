import * as api from "../../api";

export type TranscriptSource =
  | { type: "agent"; id: string; refreshSeed?: string | number | null; limit?: number }
  | { type: "card"; id: string; refreshSeed?: string | number | null; limit?: number };

export type TranscriptTranslator = (ko: string, en: string) => string;

type TranscriptTone = "assistant" | "thinking" | "tool" | "result" | "error";

export const TONE_STYLE: Record<
  TranscriptTone,
  { bar: string; chipBg: string; chipText: string; border: string; text: string }
> = {
  assistant: {
    bar: "#22c55e",
    chipBg: "rgba(34,197,94,0.16)",
    chipText: "#86efac",
    border: "rgba(34,197,94,0.25)",
    text: "#dcfce7",
  },
  thinking: {
    bar: "#a855f7",
    chipBg: "rgba(168,85,247,0.16)",
    chipText: "#d8b4fe",
    border: "rgba(168,85,247,0.24)",
    text: "#f3e8ff",
  },
  tool: {
    bar: "#3b82f6",
    chipBg: "rgba(59,130,246,0.16)",
    chipText: "#93c5fd",
    border: "rgba(59,130,246,0.24)",
    text: "#dbeafe",
  },
  result: {
    bar: "#64748b",
    chipBg: "rgba(100,116,139,0.2)",
    chipText: "#cbd5e1",
    border: "rgba(148,163,184,0.24)",
    text: "#e2e8f0",
  },
  error: {
    bar: "#ef4444",
    chipBg: "rgba(239,68,68,0.16)",
    chipText: "#fca5a5",
    border: "rgba(239,68,68,0.24)",
    text: "#fee2e2",
  },
};

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
