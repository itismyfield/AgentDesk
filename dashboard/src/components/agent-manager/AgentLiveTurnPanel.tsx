import { useEffect, useRef, useState } from "react";
import { formatElapsedCompact } from "../../agent-insights";
import * as api from "../../api";
import type { AgentTurnStatus, AgentTurnToolEvent } from "../../api";

interface AgentLiveTurnPanelProps {
  agentId: string;
  isKo: boolean;
  tr: (ko: string, en: string) => string;
}

const REFRESH_INTERVAL_MS = 1000;
const SCROLL_STICKY_THRESHOLD_PX = 28;

const TOOL_STATUS_STYLE: Record<
  AgentTurnToolEvent["status"],
  { bg: string; text: string; ko: string; en: string }
> = {
  running: {
    bg: "rgba(59,130,246,0.16)",
    text: "#93c5fd",
    ko: "실행중",
    en: "Running",
  },
  success: {
    bg: "rgba(34,197,94,0.18)",
    text: "#86efac",
    ko: "완료",
    en: "Done",
  },
  error: {
    bg: "rgba(239,68,68,0.18)",
    text: "#fca5a5",
    ko: "실패",
    en: "Error",
  },
  info: {
    bg: "rgba(168,85,247,0.18)",
    text: "#d8b4fe",
    ko: "생각중",
    en: "Thinking",
  },
};

function parseStartedAt(value: string | null): number | null {
  if (!value) return null;
  const normalized = value.includes("T") ? value : value.replace(" ", "T");
  const parsed = new Date(normalized);
  return Number.isNaN(parsed.getTime()) ? null : parsed.getTime();
}

function isNearBottom(element: HTMLDivElement): boolean {
  const remaining =
    element.scrollHeight - element.scrollTop - element.clientHeight;
  return remaining <= SCROLL_STICKY_THRESHOLD_PX;
}

function shouldRefreshFromEvent(event: Event, agentId: string): boolean {
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

function eventTitle(
  event: AgentTurnToolEvent,
  tr: AgentLiveTurnPanelProps["tr"],
): string {
  if (event.kind === "thinking") return tr("생각중", "Thinking");
  return event.tool_name || tr("도구", "Tool");
}

function eventSummary(event: AgentTurnToolEvent): string {
  return event.summary.trim() || event.line.trim();
}

export default function AgentLiveTurnPanel({
  agentId,
  isKo,
  tr,
}: AgentLiveTurnPanelProps) {
  const [turn, setTurn] = useState<AgentTurnStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [stopping, setStopping] = useState(false);
  const [followTail, setFollowTail] = useState(true);
  const [nowMs, setNowMs] = useState(() => Date.now());
  const [refreshNonce, setRefreshNonce] = useState(0);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const requestSeqRef = useRef(0);
  const followTailRef = useRef(true);

  useEffect(() => {
    followTailRef.current = followTail;
  }, [followTail]);

  useEffect(() => {
    let cancelled = false;

    async function refresh(silent = false) {
      const requestId = requestSeqRef.current + 1;
      requestSeqRef.current = requestId;
      if (!silent) setLoading(true);

      try {
        const next = await api.getAgentTurn(agentId);
        if (!cancelled && requestSeqRef.current === requestId) {
          setTurn(next);
        }
      } catch (error) {
        if (!cancelled) {
          console.error("Agent live turn fetch failed:", error);
        }
      } finally {
        if (!cancelled && requestSeqRef.current === requestId) {
          setLoading(false);
        }
      }
    }

    const handleWs = (event: Event) => {
      if (!shouldRefreshFromEvent(event, agentId)) return;
      void refresh(true);
    };

    void refresh();
    const timer = window.setInterval(() => {
      setNowMs(Date.now());
      void refresh(true);
    }, REFRESH_INTERVAL_MS);

    window.addEventListener("pcd-ws-event", handleWs as EventListener);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
      window.removeEventListener("pcd-ws-event", handleWs as EventListener);
    };
  }, [agentId, refreshNonce]);

  useEffect(() => {
    const element = scrollRef.current;
    if (!element || !followTailRef.current) return;
    element.scrollTop = element.scrollHeight;
  }, [turn?.updated_at, turn?.tool_events, turn?.recent_output]);

  const handleStop = async () => {
    if (stopping) return;
    setStopping(true);
    try {
      await api.stopAgentTurn(agentId);
      setTurn((prev) => (prev ? { ...prev, status: "idle" } : prev));
      setRefreshNonce((prev) => prev + 1);
    } catch (error) {
      console.error("Agent live turn stop failed:", error);
    } finally {
      setStopping(false);
    }
  };

  const handleScroll = () => {
    const element = scrollRef.current;
    if (!element) return;
    setFollowTail(isNearBottom(element));
  };

  if (loading && !turn) return null;
  if (!turn || turn.status !== "working") return null;

  const startedAtMs = parseStartedAt(turn.started_at);
  const elapsedLabel =
    startedAtMs != null
      ? formatElapsedCompact(Math.max(0, nowMs - startedAtMs), isKo)
      : null;
  const toolCount =
    turn.tool_count ??
    turn.tool_events.filter((event) => event.kind === "tool").length;
  const hasFallbackOutput =
    turn.tool_events.length === 0 && Boolean(turn.recent_output?.trim());

  return (
    <div
      className="px-5 py-3"
      style={{ borderBottom: "1px solid var(--th-card-border)" }}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div
            className="text-xs font-semibold uppercase tracking-widest"
            style={{ color: "var(--th-text-muted)" }}
          >
            {tr("라이브 턴", "Live Turn")}
          </div>
          <div className="flex flex-wrap gap-2 mt-2">
            <span
              className="text-xs px-2 py-1 rounded-lg"
              style={{ background: "rgba(248,113,113,0.16)", color: "#fca5a5" }}
            >
              LIVE
            </span>
            {elapsedLabel && (
              <span
                className="text-xs px-2 py-1 rounded-lg"
                style={{
                  background: "rgba(56,189,248,0.16)",
                  color: "#67e8f9",
                }}
              >
                {tr("경과", "Elapsed")}: {elapsedLabel}
              </span>
            )}
            <span
              className="text-xs px-2 py-1 rounded-lg"
              style={{ background: "rgba(168,85,247,0.16)", color: "#d8b4fe" }}
            >
              {tr("도구", "Tools")}: {toolCount}
            </span>
            {turn.provider && (
              <span
                className="text-xs px-2 py-1 rounded-lg"
                style={{
                  background: "var(--th-bg-surface)",
                  color: "var(--th-text-muted)",
                }}
              >
                {turn.provider}
              </span>
            )}
            {turn.active_dispatch_id && (
              <span
                className="text-xs px-2 py-1 rounded-lg truncate max-w-full"
                style={{
                  background: "var(--th-bg-surface)",
                  color: "var(--th-text-muted)",
                }}
              >
                {turn.active_dispatch_id}
              </span>
            )}
          </div>
        </div>
        <button
          type="button"
          onClick={() => void handleStop()}
          disabled={stopping}
          className="px-3 py-1.5 rounded-lg text-xs font-semibold transition-all disabled:opacity-60"
          style={{
            background: "rgba(239,68,68,0.14)",
            border: "1px solid rgba(239,68,68,0.24)",
            color: "#fca5a5",
          }}
        >
          {stopping ? tr("중단 중...", "Stopping...") : "Stop"}
        </button>
      </div>

      <div
        className="mt-3 rounded-2xl border overflow-hidden"
        style={{
          borderColor: "var(--th-border-subtle)",
          background:
            "linear-gradient(180deg, rgba(15,23,42,0.35), rgba(15,23,42,0.08))",
        }}
      >
        <div
          className="flex items-center justify-between gap-3 px-3 py-2"
          style={{ borderBottom: "1px solid rgba(148,163,184,0.12)" }}
        >
          <div
            className="text-xs font-medium"
            style={{ color: "var(--th-text-secondary)" }}
          >
            {tr("실시간 도구 호출", "Live Tool Activity")}
          </div>
          {!followTail && (
            <button
              type="button"
              onClick={() => {
                const element = scrollRef.current;
                if (!element) return;
                element.scrollTop = element.scrollHeight;
                setFollowTail(true);
              }}
              className="text-[11px] px-2 py-1 rounded-lg"
              style={{ background: "rgba(59,130,246,0.16)", color: "#93c5fd" }}
            >
              {tr("최신으로", "Jump to latest")}
            </button>
          )}
        </div>

        <div
          ref={scrollRef}
          onScroll={handleScroll}
          className="max-h-64 overflow-y-auto px-3 py-3 space-y-2"
        >
          {turn.tool_events.map((event, idx) => {
            const statusStyle =
              TOOL_STATUS_STYLE[event.status] ?? TOOL_STATUS_STYLE.info;
            return (
              <div
                key={`${event.line}:${idx}`}
                className="rounded-xl px-3 py-2"
                style={{ background: "rgba(15,23,42,0.34)" }}
              >
                <div className="flex items-center justify-between gap-2">
                  <div
                    className="text-xs font-medium truncate"
                    style={{ color: "var(--th-text-primary)" }}
                  >
                    {eventTitle(event, tr)}
                  </div>
                  <span
                    className="text-[11px] px-1.5 py-0.5 rounded-lg shrink-0"
                    style={{
                      background: statusStyle.bg,
                      color: statusStyle.text,
                    }}
                  >
                    {tr(statusStyle.ko, statusStyle.en)}
                  </span>
                </div>
                <div
                  className="mt-1 text-xs break-words"
                  style={{ color: "var(--th-text-secondary)" }}
                >
                  {eventSummary(event)}
                </div>
              </div>
            );
          })}

          {turn.tool_events.length === 0 && !hasFallbackOutput && (
            <div
              className="text-xs py-2"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr(
                "첫 도구 호출을 기다리는 중입니다.",
                "Waiting for the first tool call.",
              )}
            </div>
          )}

          {hasFallbackOutput && (
            <div
              className="rounded-xl px-3 py-2"
              style={{ background: "rgba(15,23,42,0.34)" }}
            >
              <div
                className="text-[11px]"
                style={{ color: "var(--th-text-muted)" }}
              >
                {tr("최근 출력", "Recent Output")} · {turn.recent_output_source}
              </div>
              <div
                className="mt-1 text-xs whitespace-pre-wrap break-words"
                style={{
                  color: "var(--th-text-secondary)",
                  fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
                }}
              >
                {turn.recent_output}
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
