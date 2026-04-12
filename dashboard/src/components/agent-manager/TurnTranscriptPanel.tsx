import { useEffect, useRef, useState } from "react";
import { formatElapsedCompact } from "../../agent-insights";
import * as api from "../../api";

type TranscriptSource =
  | { type: "agent"; id: string; refreshSeed?: string | number | null; limit?: number }
  | { type: "card"; id: string; refreshSeed?: string | number | null; limit?: number };

interface TurnTranscriptPanelProps {
  source: TranscriptSource;
  tr: (ko: string, en: string) => string;
  isKo: boolean;
  title?: string;
}

type TranscriptTone = "assistant" | "thinking" | "tool" | "result" | "error";

const TONE_STYLE: Record<
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

function parseDate(value: string | null | undefined): Date | null {
  if (!value) return null;
  const normalized = value.includes("T") ? value : value.replace(" ", "T");
  const parsed = new Date(normalized);
  return Number.isNaN(parsed.getTime()) ? null : parsed;
}

function formatTimestamp(value: string, isKo: boolean): string {
  const parsed = parseDate(value);
  if (!parsed) return value;
  return parsed.toLocaleString(isKo ? "ko-KR" : "en-US", {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function eventTone(event: api.SessionTranscriptEvent): TranscriptTone {
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

function eventTitle(
  event: api.SessionTranscriptEvent,
  tr: TurnTranscriptPanelProps["tr"],
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

function eventBody(event: api.SessionTranscriptEvent): string {
  return event.content.trim() || event.summary?.trim() || "";
}

function mergeTranscriptEvents(
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

function transcriptEvents(
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

function buildCopyText(
  transcript: api.SessionTranscript,
  events: api.SessionTranscriptEvent[],
  tr: TurnTranscriptPanelProps["tr"],
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

async function copyText(text: string): Promise<void> {
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

function shouldRefreshAgent(event: Event, agentId: string): boolean {
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

function shouldRefreshCard(event: Event, cardId: string): boolean {
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

export default function TurnTranscriptPanel({
  source,
  tr,
  isKo,
  title,
}: TurnTranscriptPanelProps) {
  const [transcripts, setTranscripts] = useState<api.SessionTranscript[]>([]);
  const [selectedTurnId, setSelectedTurnId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [copyState, setCopyState] = useState<"idle" | "done" | "error">("idle");
  const [refreshNonce, setRefreshNonce] = useState(0);
  const [activeEventIndex, setActiveEventIndex] = useState<number | null>(null);
  const requestSeqRef = useRef(0);
  const eventRefs = useRef<Record<number, HTMLDivElement | null>>({});

  useEffect(() => {
    let cancelled = false;

    async function refresh(silent = false) {
      const requestId = requestSeqRef.current + 1;
      requestSeqRef.current = requestId;
      if (!silent) setLoading(true);
      setError(null);

      try {
        const limit = source.limit ?? (source.type === "agent" ? 8 : 10);
        const next =
          source.type === "agent"
            ? await api.getAgentTranscripts(source.id, limit)
            : await api.getCardTranscripts(source.id, limit);
        if (!cancelled && requestSeqRef.current === requestId) {
          setTranscripts(next);
        }
      } catch (fetchError) {
        if (!cancelled && requestSeqRef.current === requestId) {
          setError(
            fetchError instanceof Error
              ? fetchError.message
              : tr("턴 로그를 불러오지 못했습니다.", "Failed to load transcripts."),
          );
        }
      } finally {
        if (!cancelled && requestSeqRef.current === requestId) {
          setLoading(false);
        }
      }
    }

    const handleWs = (event: Event) => {
      const shouldRefresh =
        source.type === "agent"
          ? shouldRefreshAgent(event, source.id)
          : shouldRefreshCard(event, source.id);
      if (shouldRefresh) {
        void refresh(true);
      }
    };

    void refresh();
    window.addEventListener("pcd-ws-event", handleWs as EventListener);
    return () => {
      cancelled = true;
      window.removeEventListener("pcd-ws-event", handleWs as EventListener);
    };
  }, [refreshNonce, source.id, source.limit, source.refreshSeed, source.type, tr]);

  useEffect(() => {
    if (transcripts.length === 0) {
      setSelectedTurnId(null);
      return;
    }
    if (!selectedTurnId || !transcripts.some((item) => item.turn_id === selectedTurnId)) {
      setSelectedTurnId(transcripts[0].turn_id);
    }
  }, [selectedTurnId, transcripts]);

  const selectedTranscript =
    transcripts.find((item) => item.turn_id === selectedTurnId) ?? transcripts[0] ?? null;
  const events = transcriptEvents(selectedTranscript);
  const toolCount = events.filter((event) => event.kind === "tool_use").length;
  const thinkingCount = events.filter((event) => event.kind === "thinking").length;
  const errorCount = events.filter((event) => eventTone(event) === "error").length;

  const handleCopyAll = async () => {
    if (!selectedTranscript) return;
    try {
      await copyText(buildCopyText(selectedTranscript, events, tr));
      setCopyState("done");
    } catch (copyError) {
      console.error("Transcript copy failed:", copyError);
      setCopyState("error");
    }
    window.setTimeout(() => setCopyState("idle"), 1800);
  };

  const handleJumpToEvent = (index: number) => {
    setActiveEventIndex(index);
    eventRefs.current[index]?.scrollIntoView({
      behavior: "smooth",
      block: "center",
    });
  };

  return (
    <div
      className="px-5 py-3"
      style={{ borderBottom: "1px solid var(--th-card-border)" }}
    >
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div className="min-w-0">
          <div
            className="text-xs font-semibold uppercase tracking-widest"
            style={{ color: "var(--th-text-muted)" }}
          >
            {title ?? tr("턴 트랜스크립트", "Turn Transcript")}
          </div>
          <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
            {source.type === "agent"
              ? tr("완료된 턴을 다시 살펴봅니다.", "Replay completed turns.")
              : tr("이 카드에 연결된 턴 로그를 확인합니다.", "Review turn logs linked to this card.")}
          </div>
        </div>

        <div className="flex gap-2">
          <button
            type="button"
            onClick={() => setRefreshNonce((prev) => prev + 1)}
            className="rounded-lg px-3 py-2 text-xs font-medium"
            style={{
              background: "rgba(59,130,246,0.16)",
              color: "#93c5fd",
            }}
          >
            {tr("새로고침", "Refresh")}
          </button>
          <button
            type="button"
            onClick={() => void handleCopyAll()}
            disabled={!selectedTranscript}
            className="rounded-lg px-3 py-2 text-xs font-medium disabled:opacity-40"
            style={{
              background:
                copyState === "done"
                  ? "rgba(34,197,94,0.16)"
                  : copyState === "error"
                    ? "rgba(239,68,68,0.16)"
                    : "rgba(148,163,184,0.16)",
              color:
                copyState === "done"
                  ? "#86efac"
                  : copyState === "error"
                    ? "#fca5a5"
                    : "var(--th-text-secondary)",
            }}
          >
            {copyState === "done"
              ? tr("복사됨", "Copied")
              : copyState === "error"
                ? tr("복사 실패", "Copy failed")
                : tr("Copy all", "Copy all")}
          </button>
        </div>
      </div>

      {loading ? (
        <div className="mt-3 text-sm" style={{ color: "var(--th-text-muted)" }}>
          {tr("불러오는 중...", "Loading...")}
        </div>
      ) : error ? (
        <div className="mt-3 text-sm" style={{ color: "#fca5a5" }}>
          {error}
        </div>
      ) : transcripts.length === 0 ? (
        <div className="mt-3 text-sm" style={{ color: "var(--th-text-muted)" }}>
          {tr("표시할 턴 로그가 없습니다.", "No transcript available yet.")}
        </div>
      ) : (
        <>
          {transcripts.length > 1 && (
            <div className="mt-3 flex gap-2 overflow-x-auto pb-1">
              {transcripts.map((transcript) => {
                const selected = transcript.turn_id === selectedTranscript?.turn_id;
                return (
                  <button
                    key={transcript.turn_id}
                    type="button"
                    onClick={() => setSelectedTurnId(transcript.turn_id)}
                    className="shrink-0 rounded-xl border px-3 py-2 text-left"
                    style={{
                      borderColor: selected ? "rgba(59,130,246,0.45)" : "rgba(148,163,184,0.16)",
                      backgroundColor: selected ? "rgba(37,99,235,0.12)" : "rgba(255,255,255,0.03)",
                    }}
                  >
                    <div className="text-xs font-medium" style={{ color: "var(--th-text-primary)" }}>
                      {transcript.dispatch_title || formatTimestamp(transcript.created_at, isKo)}
                    </div>
                    <div className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                      {(transcript.provider ?? "unknown").toUpperCase()} · {formatTimestamp(transcript.created_at, isKo)}
                    </div>
                  </button>
                );
              })}
            </div>
          )}

          {selectedTranscript && (
            <>
              <div className="mt-3 flex flex-wrap gap-2">
                <span
                  className="rounded-full px-2.5 py-1 text-xs"
                  style={{
                    background: "rgba(59,130,246,0.16)",
                    color: "#93c5fd",
                  }}
                >
                  {(selectedTranscript.provider ?? "unknown").toUpperCase()}
                </span>
                {selectedTranscript.duration_ms != null && (
                  <span
                    className="rounded-full px-2.5 py-1 text-xs"
                    style={{
                      background: "rgba(34,197,94,0.16)",
                      color: "#86efac",
                    }}
                  >
                    {tr("소요", "Duration")}{" "}
                    {formatElapsedCompact(selectedTranscript.duration_ms, isKo)}
                  </span>
                )}
                <span
                  className="rounded-full px-2.5 py-1 text-xs"
                  style={{
                    background: "rgba(148,163,184,0.16)",
                    color: "var(--th-text-secondary)",
                  }}
                >
                  {events.length} {tr("이벤트", "events")}
                </span>
                <span
                  className="rounded-full px-2.5 py-1 text-xs"
                  style={{
                    background: "rgba(59,130,246,0.12)",
                    color: "#bfdbfe",
                  }}
                >
                  {toolCount} {tr("도구", "tools")}
                </span>
                <span
                  className="rounded-full px-2.5 py-1 text-xs"
                  style={{
                    background: "rgba(168,85,247,0.12)",
                    color: "#e9d5ff",
                  }}
                >
                  {thinkingCount} Thinking
                </span>
                <span
                  className="rounded-full px-2.5 py-1 text-xs"
                  style={{
                    background: "rgba(239,68,68,0.12)",
                    color: "#fecaca",
                  }}
                >
                  {errorCount} {tr("오류", "errors")}
                </span>
              </div>

              {selectedTranscript.user_message.trim() && (
                <div
                  className="mt-4 rounded-2xl border p-4"
                  style={{
                    borderColor: "rgba(148,163,184,0.18)",
                    background: "rgba(255,255,255,0.03)",
                  }}
                >
                  <div
                    className="text-xs font-semibold uppercase tracking-widest"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {tr("사용자 요청", "Prompt")}
                  </div>
                  <div
                    className="mt-2 text-sm whitespace-pre-wrap"
                    style={{ color: "var(--th-text-primary)" }}
                  >
                    {selectedTranscript.user_message.trim()}
                  </div>
                </div>
              )}

              <div className="mt-4">
                <div
                  className="text-xs font-semibold uppercase tracking-widest"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {tr("타임라인", "Timeline")}
                </div>
                {events.length === 0 ? (
                  <div className="mt-2 text-sm" style={{ color: "var(--th-text-muted)" }}>
                    {tr("이 턴에는 구조화 이벤트가 없습니다.", "No structured events for this turn.")}
                  </div>
                ) : (
                  <div className="mt-2 flex gap-1 overflow-x-auto pb-1">
                    {events.map((event, index) => {
                      const tone = eventTone(event);
                      return (
                        <button
                          key={`${selectedTranscript.turn_id}-${index}`}
                          type="button"
                          onClick={() => handleJumpToEvent(index)}
                          className="h-10 rounded-lg border transition-transform hover:-translate-y-0.5"
                          style={{
                            minWidth: "28px",
                            flex: "1 0 28px",
                            backgroundColor: TONE_STYLE[tone].bar,
                            borderColor:
                              activeEventIndex === index
                                ? "#ffffff"
                                : "rgba(255,255,255,0.14)",
                            boxShadow:
                              activeEventIndex === index
                                ? "0 0 0 2px rgba(255,255,255,0.18)"
                                : "none",
                          }}
                          title={`${index + 1}. ${eventTitle(event, tr)}`}
                          aria-label={`${index + 1}. ${eventTitle(event, tr)}`}
                        />
                      );
                    })}
                  </div>
                )}
              </div>

              <div className="mt-4 max-h-[26rem] space-y-3 overflow-y-auto pr-1">
                {events.map((event, index) => {
                  const tone = eventTone(event);
                  const style = TONE_STYLE[tone];
                  const body = eventBody(event);
                  return (
                    <div
                      key={`${selectedTranscript.turn_id}-event-${index}`}
                      ref={(node) => {
                        eventRefs.current[index] = node;
                      }}
                      className="rounded-2xl border p-4"
                      style={{
                        borderColor: style.border,
                        backgroundColor:
                          activeEventIndex === index
                            ? "rgba(255,255,255,0.06)"
                            : "rgba(255,255,255,0.03)",
                      }}
                    >
                      <div className="flex flex-wrap items-start justify-between gap-2">
                        <div>
                          <div className="flex flex-wrap items-center gap-2">
                            <span
                              className="rounded-full px-2 py-0.5 text-[11px] font-semibold"
                              style={{
                                backgroundColor: style.chipBg,
                                color: style.chipText,
                              }}
                            >
                              {eventTitle(event, tr)}
                            </span>
                            <span className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                              #{index + 1}
                            </span>
                            {event.status && (
                              <span className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                                {event.status}
                              </span>
                            )}
                          </div>
                          {(event.summary || event.tool_name) && (
                            <div className="mt-2 text-sm" style={{ color: style.text }}>
                              {event.summary || event.tool_name}
                            </div>
                          )}
                        </div>
                        <button
                          type="button"
                          onClick={() => handleJumpToEvent(index)}
                          className="text-[11px]"
                          style={{ color: "var(--th-text-muted)" }}
                        >
                          {tr("포커스", "Focus")}
                        </button>
                      </div>
                      {body && (
                        <div
                          className="mt-3 text-sm whitespace-pre-wrap break-words"
                          style={{ color: "var(--th-text-primary)" }}
                        >
                          {body}
                        </div>
                      )}
                    </div>
                  );
                })}
              </div>
            </>
          )}
        </>
      )}
    </div>
  );
}
