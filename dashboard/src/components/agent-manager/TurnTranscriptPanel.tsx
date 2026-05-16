import { useEffect, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { formatElapsedCompact } from "../../agent-insights";
import * as api from "../../api";
import {
  formatTranscriptTimestamp,
  normalizeActiveEventIndex,
  transcriptSelectionLabel,
} from "./turn-transcript-utils";

import {
  TONE_STYLE,
  buildCopyText,
  copyText,
  eventBody,
  eventTitle,
  eventTone,
  shouldRefreshAgent,
  shouldRefreshCard,
  transcriptEvents,
  type TranscriptSource,
} from "./TurnTranscriptModel";

interface TurnTranscriptPanelProps {
  source: TranscriptSource;
  tr: (ko: string, en: string) => string;
  isKo: boolean;
  title?: string;
  embedded?: boolean;
}

export default function TurnTranscriptPanel({
  source,
  tr,
  isKo,
  title,
  embedded = false,
}: TurnTranscriptPanelProps) {
  const [transcripts, setTranscripts] = useState<api.SessionTranscript[]>([]);
  const [selectedTurnId, setSelectedTurnId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [copyState, setCopyState] = useState<"idle" | "done" | "error">("idle");
  const [refreshNonce, setRefreshNonce] = useState(0);
  const [activeEventIndex, setActiveEventIndex] = useState<number | null>(null);
  const [promptExpanded, setPromptExpanded] = useState(false);
  const requestSeqRef = useRef(0);

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
  const eventRailRef = useRef<HTMLDivElement | null>(null);
  const eventRailVirtualizer = useVirtualizer({
    count: events.length,
    getScrollElement: () => eventRailRef.current,
    estimateSize: () => 44,
    horizontal: true,
    overscan: 8,
  });
  const selectedEvent =
    activeEventIndex == null ? null : events[activeEventIndex] ?? null;
  const toolCount = events.filter((event) => event.kind === "tool_use").length;
  const thinkingCount = events.filter((event) => event.kind === "thinking").length;
  const errorCount = events.filter((event) => eventTone(event) === "error").length;

  useEffect(() => {
    setPromptExpanded(false);
    setActiveEventIndex(events.length > 0 ? 0 : null);
  }, [selectedTranscript?.turn_id]);

  useEffect(() => {
    setActiveEventIndex((prev) => normalizeActiveEventIndex(prev, events.length));
  }, [events.length]);

  useEffect(() => {
    if (activeEventIndex == null) return;
    eventRailVirtualizer.scrollToIndex(activeEventIndex, { align: "center" });
  }, [activeEventIndex, eventRailVirtualizer]);

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

  const handleSelectEvent = (index: number) => {
    setActiveEventIndex(index);
  };

  return (
    <div
      className={embedded ? "py-0" : "px-5 py-3"}
      style={embedded ? undefined : { borderBottom: "1px solid var(--th-card-border)" }}
    >
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        {!embedded && (
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
        )}

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
            <>
              <div className="mt-3 sm:hidden">
                <select
                  value={selectedTranscript?.turn_id ?? transcripts[0]?.turn_id ?? ""}
                  onChange={(event) => setSelectedTurnId(event.target.value)}
                  className="w-full rounded-xl px-3 py-2 text-sm outline-none"
                  style={{
                    background: "rgba(255,255,255,0.03)",
                    border: "1px solid rgba(148,163,184,0.16)",
                    color: "var(--th-text-primary)",
                  }}
                  aria-label={tr("턴 선택", "Select turn")}
                >
                  {transcripts.map((transcript) => (
                    <option key={transcript.turn_id} value={transcript.turn_id}>
                      {transcriptSelectionLabel(transcript, isKo)}
                    </option>
                  ))}
                </select>
              </div>

              <div className="mt-3 hidden flex-wrap gap-2 sm:flex">
                {transcripts.map((transcript) => {
                  const selected = transcript.turn_id === selectedTranscript?.turn_id;
                  return (
                    <button
                      key={transcript.turn_id}
                      type="button"
                      onClick={() => setSelectedTurnId(transcript.turn_id)}
                      className="min-w-0 flex-[1_1_12rem] rounded-xl border px-3 py-2 text-left"
                      style={{
                        borderColor: selected ? "rgba(59,130,246,0.45)" : "rgba(148,163,184,0.16)",
                        backgroundColor: selected ? "rgba(37,99,235,0.12)" : "rgba(255,255,255,0.03)",
                      }}
                    >
                      <div className="text-xs font-medium truncate" style={{ color: "var(--th-text-primary)" }}>
                        {transcriptSelectionLabel(transcript, isKo)}
                      </div>
                      <div className="text-[11px] truncate" style={{ color: "var(--th-text-muted)" }}>
                        {(transcript.provider ?? "unknown").toUpperCase()} · {formatTranscriptTimestamp(transcript.created_at, isKo)}
                      </div>
                    </button>
                  );
                })}
              </div>
            </>
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
                  <button
                    type="button"
                    onClick={() => setPromptExpanded((prev) => !prev)}
                    className="flex w-full items-start justify-between gap-3 text-left"
                    aria-expanded={promptExpanded}
                  >
                    <div className="min-w-0">
                      <div
                        className="text-xs font-semibold uppercase tracking-widest"
                        style={{ color: "var(--th-text-muted)" }}
                      >
                        {tr("사용자 요청", "Prompt")}
                      </div>
                      {!promptExpanded && (
                        <div
                          className="mt-1 truncate text-xs"
                          style={{ color: "var(--th-text-muted)" }}
                        >
                          {selectedTranscript.user_message.trim().replace(/\s+/g, " ")}
                        </div>
                      )}
                    </div>
                    <span
                      className="shrink-0 rounded-full px-2.5 py-1 text-xs font-medium"
                      style={{
                        background: "rgba(148,163,184,0.16)",
                        color: "var(--th-text-secondary)",
                      }}
                    >
                      {promptExpanded ? tr("접기", "Collapse") : tr("펼치기", "Expand")}
                    </span>
                  </button>
                  {promptExpanded && (
                    <div
                      className="mt-3 border-t pt-3 text-sm whitespace-pre-wrap"
                      style={{
                        borderColor: "rgba(148,163,184,0.16)",
                        color: "var(--th-text-primary)",
                      }}
                    >
                      {selectedTranscript.user_message.trim()}
                    </div>
                  )}
                </div>
              )}

              <div className="mt-4">
                <div className="flex items-center justify-between gap-2">
                  <div
                    className="text-xs font-semibold uppercase tracking-widest"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {tr("타임라인", "Timeline")}
                  </div>
                  {selectedEvent && (
                    <span className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                      {activeEventIndex! + 1}/{events.length}
                    </span>
                  )}
                </div>
                {events.length === 0 ? (
                  <div className="mt-2 text-sm" style={{ color: "var(--th-text-muted)" }}>
                    {tr("이 턴에는 구조화 이벤트가 없습니다.", "No structured events for this turn.")}
                  </div>
                ) : (
                  <div
                    ref={eventRailRef}
                    className="mt-2 overflow-x-auto overscroll-x-contain pb-1"
                    data-testid="turn-transcript-event-rail"
                  >
                    <div
                      className="relative h-10"
                      style={{ width: `${eventRailVirtualizer.getTotalSize()}px` }}
                    >
                      {eventRailVirtualizer.getVirtualItems().map((virtualItem) => {
                        const index = virtualItem.index;
                        const event = events[index];
                        const tone = eventTone(event);
                        return (
                          <button
                            key={`${selectedTranscript.turn_id}-${index}`}
                            type="button"
                            onClick={() => handleSelectEvent(index)}
                            className="absolute top-0 h-10 rounded-lg border transition-transform hover:-translate-y-0.5"
                            style={{
                              width: 36,
                              transform: `translateX(${virtualItem.start}px)`,
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
                            aria-pressed={activeEventIndex === index}
                          />
                        );
                      })}
                    </div>
                  </div>
                )}
              </div>

              {selectedEvent && (() => {
                const tone = eventTone(selectedEvent);
                const style = TONE_STYLE[tone];
                const body = eventBody(selectedEvent);

                return (
                  <div className="mt-4 max-h-[26rem] overflow-y-auto pr-1">
                    <div
                      className="rounded-2xl border p-4"
                      style={{
                        borderColor: style.border,
                        backgroundColor: "rgba(255,255,255,0.05)",
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
                              {eventTitle(selectedEvent, tr)}
                            </span>
                            <span className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                              #{activeEventIndex! + 1}
                            </span>
                            {selectedEvent.status && (
                              <span className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                                {selectedEvent.status}
                              </span>
                            )}
                          </div>
                          {(selectedEvent.summary || selectedEvent.tool_name) && (
                            <div className="mt-2 text-sm" style={{ color: style.text }}>
                              {selectedEvent.summary || selectedEvent.tool_name}
                            </div>
                          )}
                        </div>
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
                  </div>
                );
              })()}
            </>
          )}
        </>
      )}
    </div>
  );
}
