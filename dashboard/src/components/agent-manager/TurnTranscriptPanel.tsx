import { useEffect, useId, useRef, useState, type CSSProperties, type KeyboardEvent } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { Bot, Brain, CircleAlert, Copy, CornerDownRight, RefreshCw, Wrench, type LucideIcon } from "lucide-react";
import { formatElapsedCompact } from "../../agent-insights";
import * as api from "../../api";
import { Badge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { CopyWell, DetailHeading, KIT_TOUCH_BUTTON, PropList } from "./CardDetailParts";
import {
  formatTranscriptTimestamp,
  normalizeActiveEventIndex,
  transcriptSelectionLabel,
} from "./turn-transcript-utils";

import {
  TONE_STYLE,
  TRANSCRIPT_TONES,
  buildCopyText,
  copyText,
  eventBody,
  eventBodyLabel,
  eventTitle,
  eventTone,
  formatAxisOffset,
  orderTicks,
  shouldRefreshAgent,
  shouldRefreshCard,
  timeAxis,
  toneLabel,
  transcriptEvents,
  turnSpans,
  type TranscriptSource,
  type TranscriptTone,
} from "./TurnTranscriptModel";

interface TurnTranscriptPanelProps {
  source: TranscriptSource;
  tr: (ko: string, en: string) => string;
  isKo: boolean;
  title?: string;
  embedded?: boolean;
}

const TONE_ICON: Record<TranscriptTone, LucideIcon> = {
  assistant: Bot,
  thinking: Brain,
  tool: Wrench,
  result: CornerDownRight,
  error: CircleAlert,
};

/** Name column and track share one grid; narrow panels stack the name over the bar. */
const ROW_GRID = "grid grid-cols-[minmax(0,15rem)_minmax(0,1fr)] gap-x-3 @max-[40rem]/run:grid-cols-[9.5rem_minmax(0,1fr)] @max-[30rem]/run:grid-cols-1";

function ToneTile({ tone, size = 18 }: { tone: TranscriptTone; size?: 18 | 22 }) {
  const Icon = TONE_ICON[tone];
  return (
    <span aria-hidden="true" className="grid shrink-0 place-items-center rounded-[5px] bg-th-card-bg shadow-[0_0_0_0.5px_var(--th-border-subtle)]"
      style={{ width: size, height: size, color: TONE_STYLE[tone].text }}>
      <Icon size={size === 22 ? 13 : 11} />
    </span>
  );
}

const PROVIDER_CODE: Record<string, string> = { claude: "CL", codex: "CX", gemini: "GM", qwen: "QW", opencode: "OC", copilot: "CP", antigravity: "AG" };

function ProviderTile({ provider }: { provider: string | null }) {
  const key = (provider ?? "").toLowerCase().replace(/[^a-z]/g, "");
  return (
    <span aria-hidden="true" className="grid h-[22px] min-w-[22px] shrink-0 place-items-center rounded-md px-0.5 text-[10px] font-semibold uppercase tracking-tight text-th-text-primary"
      style={{ background: `color-mix(in oklch, var(--${key || "api"}, var(--th-text-muted)) 26%, var(--th-card-bg))` }}>
      {PROVIDER_CODE[key] ?? (key.slice(0, 2) || "?")}
    </span>
  );
}

/** Tick labels above a track; the first hugs the left edge and the last the right. */
function AxisRow({ ticks, position, label, testId }: { ticks: { at: number; text: string }[]; position: (value: number) => number; label?: string; testId?: string }) {
  return (
    <div aria-hidden="true" className={`${ROW_GRID} h-6 px-2.5`} data-testid={testId}>
      <span className="truncate pt-1 text-[11px] text-th-text-muted @max-[30rem]/run:hidden">{label}</span>
      <div className="relative min-w-0">
        {ticks.map((tick, index) => (
          <span key={`${tick.at}-${index}`}
            className={`absolute top-1 whitespace-nowrap text-[11px] leading-4 text-th-text-muted ${index === 0 ? "" : index === ticks.length - 1 ? "-translate-x-full" : "-translate-x-1/2"} ${index % 2 === 1 ? "@max-[30rem]/run:invisible" : ""}`}
            style={{ left: `${position(tick.at)}%` }}>
            {tick.text}
          </span>
        ))}
      </div>
    </div>
  );
}

function GridLines({ positions }: { positions: number[] }) {
  return (
    <>
      {positions.map((left, index) => (
        <i key={index} aria-hidden="true" className="pointer-events-none absolute inset-y-0 border-l border-dashed border-th-border-subtle opacity-60" style={{ left: `${left}%` }} />
      ))}
    </>
  );
}

/** A bar on the shared axis with its label beside it; near the right edge the label moves to the bar's left. */
function TrackBar({ left, width, tone, label, failed }: { left: number; width: number; tone: TranscriptTone; label: string; failed?: boolean }) {
  const end = left + width;
  const flip = end > 84;
  const style = TONE_STYLE[tone];
  const labelStyle: CSSProperties = flip ? { right: `calc(${100 - left}% + 4px)` } : { left: `calc(${end}% + 4px)` };
  return (
    <>
      <span className="absolute top-1/2 h-3.5 -translate-y-1/2 rounded motion-safe:transition-[left,width] motion-safe:duration-300"
        style={{ left: `${left}%`, width: `max(3px, ${width}%)`, background: style.bar, boxShadow: `inset 0 0 0 0.5px ${style.border}` }} />
      <span className={`absolute top-1/2 h-[18px] -translate-y-1/2 whitespace-nowrap rounded bg-th-card-bg px-1 text-xs leading-[18px] ${failed ? "text-[color-mix(in_oklch,var(--err)_72%,var(--th-text-primary))]" : "text-th-text-secondary"}`}
        style={labelStyle}>
        {label}
      </span>
    </>
  );
}

/** Walks row buttons with the arrow keys inside a list. */
function moveRowFocus(event: KeyboardEvent<HTMLElement>, selector: string) {
  if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
  const rows = Array.from(event.currentTarget.querySelectorAll<HTMLElement>(selector));
  const index = rows.indexOf(event.target as HTMLElement);
  if (index < 0) return;
  event.preventDefault();
  rows[Math.max(0, Math.min(rows.length - 1, index + (event.key === "ArrowDown" ? 1 : -1)))]?.focus();
}

/** Turn transcripts laid out like a run: turns on a shared time axis, the chosen turn's summary and steps, then the chosen step's input or output. */
export default function TurnTranscriptPanel({
  source,
  tr,
  isKo,
  title,
  embedded = false,
}: TurnTranscriptPanelProps) {
  const id = useId();
  const [transcripts, setTranscripts] = useState<api.SessionTranscript[]>([]);
  const [selectedTurnId, setSelectedTurnId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [copyState, setCopyState] = useState<"idle" | "done" | "error">("idle");
  const [refreshNonce, setRefreshNonce] = useState(0);
  const [activeEventIndex, setActiveEventIndex] = useState<number | null>(null);
  const requestSeqRef = useRef(0);
  const copyTimer = useRef(0);
  useEffect(() => () => window.clearTimeout(copyTimer.current), []);

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
  const stepListRef = useRef<HTMLDivElement | null>(null);
  const stepVirtualizer = useVirtualizer({
    count: events.length,
    getScrollElement: () => stepListRef.current,
    estimateSize: () => 44,
    overscan: 8,
  });
  const selectedEvent =
    activeEventIndex == null ? null : events[activeEventIndex] ?? null;
  const toneCounts = TRANSCRIPT_TONES.reduce<Record<TranscriptTone, number>>(
    (acc, tone) => ({ ...acc, [tone]: events.filter((event) => eventTone(event) === tone).length }),
    { assistant: 0, thinking: 0, tool: 0, result: 0, error: 0 },
  );
  const toolCount = events.filter((event) => event.kind === "tool_use").length;
  const errorCount = toneCounts.error;

  useEffect(() => {
    setActiveEventIndex(events.length > 0 ? 0 : null);
  }, [selectedTranscript?.turn_id]);

  useEffect(() => {
    setActiveEventIndex((prev) => normalizeActiveEventIndex(prev, events.length));
  }, [events.length]);

  const handleCopyAll = async () => {
    if (!selectedTranscript) return;
    try {
      await copyText(buildCopyText(selectedTranscript, events, tr));
      setCopyState("done");
    } catch (copyError) {
      console.error("Transcript copy failed:", copyError);
      setCopyState("error");
    }
    window.clearTimeout(copyTimer.current);
    copyTimer.current = window.setTimeout(() => setCopyState("idle"), 1800);
  };

  const { spans, total } = turnSpans(transcripts);
  const axis = timeAxis(total);
  const pct = (seconds: number) => (100 * seconds) / axis.range;
  const prompt = selectedTranscript?.user_message.trim() ?? "";
  const stepTicks = orderTicks(events.length);
  const stepPct = (order: number) => (100 * order) / Math.max(1, events.length);

  return (
    <section
      className={`@container/run flex min-w-0 flex-col gap-5 ${embedded ? "" : "px-4 py-4 sm:px-5"}`}
      aria-labelledby={embedded ? undefined : `${id}-title`}
      aria-label={embedded ? title ?? tr("턴 트랜스크립트", "Turn Transcript") : undefined}
      data-testid="turn-transcript-panel"
    >
      <header className="flex min-w-0 flex-wrap items-start justify-between gap-3">
        {!embedded && (
          <div className="min-w-0 flex-1 @max-[30rem]/run:basis-full">
            <h4 id={`${id}-title`} className="m-0 text-sm font-medium text-th-text-heading">
              {title ?? tr("턴 트랜스크립트", "Turn Transcript")}
            </h4>
            <p className="m-0 mt-0.5 text-xs text-th-text-muted [word-break:keep-all]">
              {source.type === "agent"
                ? tr("완료된 턴을 다시 살펴봅니다.", "Replay completed turns.")
                : tr("이 카드에 연결된 턴 로그를 확인합니다.", "Review turn logs linked to this card.")}
            </p>
          </div>
        )}
        <div className="ml-auto flex shrink-0 flex-wrap gap-2">
          <Button variant="secondary" size="lg" className={KIT_TOUCH_BUTTON} icon={<RefreshCw />} onClick={() => setRefreshNonce((prev) => prev + 1)}>
            {tr("새로고침", "Refresh")}
          </Button>
          <Button variant="secondary" size="lg" className={KIT_TOUCH_BUTTON} icon={<Copy />} disabled={!selectedTranscript} onClick={() => void handleCopyAll()}>
            {copyState === "done"
              ? tr("복사됨", "Copied")
              : copyState === "error"
                ? tr("복사 실패", "Copy failed")
                : tr("전체 복사", "Copy all")}
          </Button>
        </div>
      </header>

      {loading ? (
        <div role="status" aria-label={tr("불러오는 중...", "Loading...")} className="flex flex-col gap-2">
          <span className="text-sm text-th-text-muted">{tr("불러오는 중...", "Loading...")}</span>
          {[72, 54, 88].map((width) => (
            <span key={width} aria-hidden="true" className="h-3 rounded bg-surface-medium motion-safe:animate-pulse" style={{ width: `${width}%` }} />
          ))}
        </div>
      ) : error ? (
        <p role="alert" className="m-0 rounded-[10px] bg-[color-mix(in_oklch,var(--err)_14%,transparent)] px-3 py-2.5 text-sm text-[color-mix(in_oklch,var(--err)_72%,var(--th-text-primary))] [overflow-wrap:anywhere]">
          {error}
        </p>
      ) : transcripts.length === 0 ? (
        <p className="m-0 py-4 text-sm text-th-text-muted">
          {tr("표시할 턴 로그가 없습니다.", "No transcript available yet.")}
        </p>
      ) : (
        <>
          <section className="flex min-w-0 flex-col gap-1.5" aria-labelledby={`${id}-turns`}>
            <DetailHeading id={`${id}-turns`} aside={<span className="text-xs text-th-text-muted">{tr(`${transcripts.length}개 턴 · 공통 시간축`, `${transcripts.length} turns · shared time axis`)}</span>}>
              {tr("턴", "Turns")}
            </DetailHeading>
            <AxisRow ticks={axis.ticks.map((tick) => ({ at: tick, text: formatAxisOffset(tick, isKo) }))} position={pct} label={tr("시작 기준", "From first start")} testId="turn-transcript-time-axis" />
            <ul className="m-0 flex list-none flex-col gap-0.5 p-0" onKeyDown={(event) => moveRowFocus(event, "[data-turn]")} data-testid="turn-transcript-turns">
              {spans.map((span) => {
                const transcript = span.transcript;
                const selected = transcript.turn_id === selectedTranscript?.turn_id;
                const label = transcriptSelectionLabel(transcript, isKo);
                const hasError = transcriptEvents(transcript).some((event) => eventTone(event) === "error");
                const duration = span.duration == null ? tr("소요 미기록", "No duration") : formatElapsedCompact(span.duration * 1000, isKo);
                const meta = `${(transcript.provider ?? "unknown").toUpperCase()} · ${formatTranscriptTimestamp(transcript.created_at, isKo)}`;
                return (
                  <li key={transcript.turn_id} className="min-w-0">
                    <button type="button" data-turn={transcript.turn_id} aria-current={selected ? "true" : undefined}
                      aria-label={`${label}, ${meta}, ${duration}${hasError ? `, ${tr("오류 있음", "has errors")}` : ""}`}
                      onClick={() => setSelectedTurnId(transcript.turn_id)}
                      className={`${ROW_GRID} min-h-11 w-full items-center gap-y-1 rounded-lg px-2.5 py-1 text-left transition-colors hover:bg-surface-light focus-visible:-outline-offset-2 ${selected ? "bg-surface-medium" : ""}`}>
                      <span className="flex min-w-0 items-center gap-2 text-[13px] leading-5 text-th-text-primary">
                        <ProviderTile provider={transcript.provider} />
                        <span className="truncate" title={`${label} · ${meta}`}>{label}</span>
                      </span>
                      <span className="relative h-5 min-w-0">
                        <GridLines positions={axis.ticks.map(pct)} />
                        <TrackBar left={pct(span.start)} width={pct(span.duration ?? 0)} tone={hasError ? "error" : selected ? "tool" : "result"} label={duration} failed={hasError} />
                      </span>
                    </button>
                  </li>
                );
              })}
            </ul>
          </section>

          {selectedTranscript && (
            <section className="flex min-w-0 flex-col gap-4" aria-labelledby={`${id}-run`} data-testid="turn-transcript-run">
              <div className="flex min-w-0 flex-wrap items-center justify-between gap-3">
                <div className="flex min-w-0 flex-1 items-center gap-2.5">
                  <ProviderTile provider={selectedTranscript.provider} />
                  <div className="min-w-0">
                    <h5 id={`${id}-run`} className="m-0 truncate text-[13px] font-medium leading-5 text-th-text-heading @max-[30rem]/run:whitespace-normal @max-[30rem]/run:[overflow-wrap:anywhere]">
                      {transcriptSelectionLabel(selectedTranscript, isKo)}
                    </h5>
                    <p className="m-0 truncate text-xs leading-4 text-th-text-muted">
                      {(selectedTranscript.provider ?? "unknown").toUpperCase()} · {formatTranscriptTimestamp(selectedTranscript.created_at, isKo)}
                    </p>
                  </div>
                </div>
                <Badge size="md" tone={errorCount > 0 ? "danger" : "success"} dot>
                  {errorCount > 0 ? tr(`오류 ${errorCount}`, `${errorCount} errors`) : tr("완료", "Completed")}
                </Badge>
              </div>
              <dl className="m-0 grid grid-cols-4 gap-2.5 @max-[40rem]/run:grid-cols-2" data-testid="turn-transcript-summary">
                {([
                  [tr("소요", "Duration"), selectedTranscript.duration_ms != null ? formatElapsedCompact(selectedTranscript.duration_ms, isKo) : "—", false],
                  [tr("이벤트", "Events"), String(events.length), false],
                  [tr("도구", "Tools"), String(toolCount), false],
                  [tr("오류", "Errors"), String(errorCount), errorCount > 0],
                ] as const).map(([name, value, fail]) => (
                  <div key={name} className="flex min-w-0 flex-col gap-0.5 rounded-xl bg-surface-subtle px-3.5 py-3 shadow-[inset_0_0_0_0.5px_var(--th-border-subtle)]">
                    <dt className="truncate text-xs leading-4 text-th-text-muted">{name}</dt>
                    <dd className={`m-0 truncate text-[15px] font-medium leading-[22px] ${fail ? "text-[color-mix(in_oklch,var(--err)_72%,var(--th-text-primary))]" : "text-th-text-primary"}`}>{value}</dd>
                  </div>
                ))}
              </dl>

              {prompt && <CopyWell title={tr("사용자 요청", "Prompt")} value={prompt} tr={tr} testId="turn-transcript-prompt" />}

              <section className="flex min-w-0 flex-col gap-1.5" aria-labelledby={`${id}-steps`}>
                <DetailHeading id={`${id}-steps`} aside={
                  <ul aria-hidden="true" className="m-0 flex list-none flex-wrap justify-end gap-x-3 gap-y-1 p-0 text-xs text-th-text-secondary">
                    {TRANSCRIPT_TONES.map((tone) => (
                      <li key={tone} className="inline-flex items-center gap-1.5">
                        <i className="h-2 w-2.5 rounded-sm" style={{ background: TONE_STYLE[tone].bar, boxShadow: `inset 0 0 0 0.5px ${TONE_STYLE[tone].border}` }} />
                        {toneLabel(tone, tr)} {toneCounts[tone]}
                      </li>
                    ))}
                  </ul>
                }>
                  {tr("실행 단계", "Steps")}
                  {selectedEvent && <span className="ml-2 font-normal text-th-text-muted">{activeEventIndex! + 1}/{events.length}</span>}
                </DetailHeading>
                {events.length === 0 ? (
                  <p className="m-0 py-3 text-sm text-th-text-muted">
                    {tr("이 턴에는 구조화 이벤트가 없습니다.", "No structured events for this turn.")}
                  </p>
                ) : (
                  <div ref={stepListRef} className="max-h-[22rem] min-w-0 overflow-y-auto overscroll-contain" data-testid="turn-transcript-steps"
                    onKeyDown={(event) => moveRowFocus(event, "[data-step]")}>
                    <div className="sticky top-0 z-10 bg-th-card-bg">
                      <AxisRow ticks={stepTicks.map((order) => ({ at: order, text: `#${order}` }))} position={(order) => stepPct(order - 0.5)} label={tr("이벤트 순서", "Event order")} />
                    </div>
                    <ul className="relative m-0 list-none p-0" style={{ height: stepVirtualizer.getTotalSize() }}>
                      {stepVirtualizer.getVirtualItems().map((item) => {
                        const index = item.index;
                        const event = events[index];
                        const tone = eventTone(event);
                        const name = eventTitle(event, tr);
                        const preview = (event.summary || eventBody(event)).replace(/\s+/g, " ").slice(0, 160);
                        const active = activeEventIndex === index;
                        return (
                          <li key={`${selectedTranscript.turn_id}-${index}`} ref={stepVirtualizer.measureElement} data-index={index}
                            className="absolute inset-x-0 top-0 pb-0.5" style={{ transform: `translateY(${item.start}px)` }}>
                            <button type="button" data-step={index} aria-current={active ? "true" : undefined}
                              aria-label={`${index + 1}. ${name}${preview ? `, ${preview}` : ""}`}
                              onClick={() => setActiveEventIndex(index)}
                              className={`${ROW_GRID} min-h-11 w-full items-center gap-y-1 rounded-lg px-2.5 py-1 text-left transition-colors hover:bg-surface-light focus-visible:-outline-offset-2 ${active ? "bg-surface-medium" : ""}`}>
                              <span className="flex min-w-0 items-center gap-2 text-[13px] leading-5">
                                <ToneTile tone={tone} />
                                <span className="min-w-0 truncate">
                                  <span className="font-medium text-th-text-primary">{name}</span>
                                  {preview && <span className="ml-1.5 text-th-text-muted">{preview}</span>}
                                </span>
                              </span>
                              <span className="relative h-5 min-w-0">
                                <GridLines positions={stepTicks.map((order) => stepPct(order - 0.5))} />
                                <TrackBar left={stepPct(index)} width={stepPct(1)} tone={tone} label={tone === "error" ? `#${index + 1} · ${tr("오류", "Error")}` : `#${index + 1}`} failed={tone === "error"} />
                              </span>
                            </button>
                          </li>
                        );
                      })}
                    </ul>
                  </div>
                )}
              </section>

              {selectedEvent && (() => {
                const tone = eventTone(selectedEvent);
                const body = eventBody(selectedEvent);
                const name = eventTitle(selectedEvent, tr);
                return (
                  <section className="flex min-w-0 flex-col gap-4 border-t border-th-border-subtle pt-4" aria-labelledby={`${id}-step`} data-testid="turn-transcript-step-detail">
                    <div className="flex min-w-0 items-center gap-2.5">
                      <ToneTile tone={tone} size={22} />
                      <div className="min-w-0 flex-1">
                        <h5 id={`${id}-step`} className="m-0 truncate text-[13px] font-medium leading-5 text-th-text-heading">{name}</h5>
                        <p className="m-0 text-xs leading-4 text-th-text-muted">{toneLabel(tone, tr)} · #{activeEventIndex! + 1}</p>
                      </div>
                      <Badge tone={tone === "error" ? "danger" : "neutral"} dot>{tone === "error" ? tr("오류", "Error") : toneLabel(tone, tr)}</Badge>
                    </div>
                    <PropList items={[
                      [tr("순서", "Order"), `${activeEventIndex! + 1} / ${events.length}`],
                      [tr("도구", "Tool"), selectedEvent.tool_name],
                      [tr("상태", "Status"), selectedEvent.status],
                      [tr("요약", "Summary"), selectedEvent.summary && selectedEvent.summary !== body ? selectedEvent.summary : null],
                    ]} />
                    {body
                      ? <CopyWell title={eventBodyLabel(selectedEvent, tr)} value={body} tr={tr} testId="turn-transcript-step-body" />
                      : <p className="m-0 text-sm text-th-text-muted">{tr("이 단계에는 본문이 없습니다.", "This step has no body.")}</p>}
                  </section>
                );
              })()}
            </section>
          )}
        </>
      )}
    </section>
  );
}
