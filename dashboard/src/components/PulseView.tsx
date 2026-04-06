import { useMemo } from "react";
import type { Agent, AuditLogEntry, DashboardStats, KanbanCard } from "../types";

interface PulseViewProps {
  stats: DashboardStats | null;
  agents: Agent[];
  kanbanCards: KanbanCard[];
  auditLogs: AuditLogEntry[];
  isKo: boolean;
  onSelectAgent?: (agent: Agent) => void;
}

const STATUS_DOT: Record<string, string> = {
  working: "#34d399",
  idle: "#94a3b8",
  break: "#fbbf24",
  offline: "#64748b",
};

export default function PulseView({
  stats,
  agents,
  kanbanCards,
  auditLogs,
  isKo,
  onSelectAgent,
}: PulseViewProps) {
  const agentCounts = stats?.agents ?? { total: 0, working: 0, idle: 0, break: 0, offline: 0 };

  const kanbanSummary = useMemo(() => {
    const open = kanbanCards.filter((c) => !["done", "closed", "cancelled"].includes(c.status)).length;
    const review = kanbanCards.filter((c) => c.status === "review").length;
    const blocked = kanbanCards.filter((c) => c.status === "blocked").length;
    const inProgress = kanbanCards.filter((c) => c.status === "in_progress").length;
    return { open, review, blocked, inProgress };
  }, [kanbanCards]);

  const workingAgents = useMemo(
    () => agents.filter((a) => a.status === "working").sort((a, b) => (a.name_ko || a.name).localeCompare(b.name_ko || b.name)),
    [agents],
  );

  const recentLogs = useMemo(() => auditLogs.slice(0, 8), [auditLogs]);

  if (!stats) {
    return (
      <div className="flex items-center justify-center h-full" style={{ color: "var(--th-text-muted)" }}>
        <div className="text-center">
          <div className="text-3xl mb-3 opacity-30">📡</div>
          <div className="text-sm">{isKo ? "펄스 로딩 중..." : "Loading Pulse..."}</div>
        </div>
      </div>
    );
  }

  return (
    <div className="h-full overflow-y-auto px-4 py-4 pb-20 space-y-4">
      {/* Agent Status HUD */}
      <section>
        <SectionLabel label={isKo ? "에이전트 현황" : "Agent Status"} />
        <div className="grid grid-cols-4 gap-2">
          <HudPill label={isKo ? "작업" : "Work"} value={agentCounts.working} color="#34d399" />
          <HudPill label={isKo ? "대기" : "Idle"} value={agentCounts.idle} color="#94a3b8" />
          <HudPill label={isKo ? "휴식" : "Break"} value={agentCounts.break} color="#fbbf24" />
          <HudPill label={isKo ? "오프" : "Off"} value={agentCounts.offline} color="#64748b" />
        </div>
      </section>

      {/* Active Work */}
      {workingAgents.length > 0 && (
        <section>
          <SectionLabel label={isKo ? "진행 중인 작업" : "Active Work"} />
          <div className="space-y-1.5">
            {workingAgents.map((agent) => (
              <button
                key={agent.id}
                type="button"
                className="w-full flex items-center gap-2.5 rounded-xl px-3 py-2 text-left"
                style={{ background: "var(--th-card-bg)", border: "1px solid var(--th-card-border)" }}
                onClick={() => onSelectAgent?.(agent)}
              >
                <span className="text-base">{agent.avatar_emoji}</span>
                <div className="flex-1 min-w-0">
                  <div className="text-xs font-medium truncate" style={{ color: "var(--th-text-primary)" }}>
                    {agent.alias || agent.name_ko || agent.name}
                  </div>
                  {agent.session_info && (
                    <div className="text-xs truncate mt-0.5" style={{ color: "var(--th-text-muted)" }}>
                      {agent.session_info}
                    </div>
                  )}
                </div>
                <span className="w-2 h-2 rounded-full shrink-0" style={{ background: STATUS_DOT[agent.status] ?? STATUS_DOT.offline }} />
              </button>
            ))}
          </div>
        </section>
      )}

      {/* Kanban Pulse */}
      <section>
        <SectionLabel label={isKo ? "칸반 요약" : "Kanban Pulse"} />
        <div className="grid grid-cols-2 gap-2">
          <KanbanPill label={isKo ? "열린 카드" : "Open"} value={kanbanSummary.open} accent="#0ea5e9" />
          <KanbanPill label={isKo ? "진행 중" : "In Progress"} value={kanbanSummary.inProgress} accent="#f59e0b" />
          <KanbanPill label={isKo ? "검토 중" : "Review"} value={kanbanSummary.review} accent="#14b8a6" />
          <KanbanPill label={isKo ? "막힘" : "Blocked"} value={kanbanSummary.blocked} accent="#ef4444" />
        </div>
      </section>

      {/* Recent Activity */}
      {recentLogs.length > 0 && (
        <section>
          <SectionLabel label={isKo ? "최근 활동" : "Recent Activity"} />
          <div className="space-y-1">
            {recentLogs.map((log) => (
              <div
                key={log.id}
                className="flex items-start gap-2 rounded-lg px-3 py-2 text-xs"
                style={{ background: "var(--th-card-bg)", border: "1px solid var(--th-card-border)" }}
              >
                <span className="shrink-0 mt-0.5 opacity-60">📋</span>
                <div className="flex-1 min-w-0">
                  <div className="truncate" style={{ color: "var(--th-text-primary)" }}>{log.summary || log.action}</div>
                  <div className="mt-0.5" style={{ color: "var(--th-text-muted)" }}>
                    {formatTimeAgo(log.created_at, isKo)}
                  </div>
                </div>
              </div>
            ))}
          </div>
        </section>
      )}
    </div>
  );
}

// ── Small subcomponents ──

function SectionLabel({ label }: { label: string }) {
  return (
    <div className="text-xs font-semibold uppercase tracking-[0.2em] mb-2 px-1" style={{ color: "var(--th-text-muted)" }}>
      {label}
    </div>
  );
}

function HudPill({ label, value, color }: { label: string; value: number; color: string }) {
  return (
    <div
      className="rounded-xl px-2 py-2 text-center"
      style={{ background: `${color}18`, border: `1px solid ${color}30` }}
    >
      <div className="text-lg font-bold" style={{ color }}>{value}</div>
      <div className="text-xs mt-0.5" style={{ color: "var(--th-text-muted)" }}>{label}</div>
    </div>
  );
}

function KanbanPill({ label, value, accent }: { label: string; value: number; accent: string }) {
  return (
    <div
      className="flex items-center gap-2 rounded-xl px-3 py-2"
      style={{ background: "var(--th-card-bg)", border: "1px solid var(--th-card-border)" }}
    >
      <span className="w-2 h-2 rounded-full shrink-0" style={{ background: accent }} />
      <span className="text-xs flex-1" style={{ color: "var(--th-text-muted)" }}>{label}</span>
      <span className="text-sm font-semibold" style={{ color: "var(--th-text-primary)" }}>{value}</span>
    </div>
  );
}

function formatTimeAgo(ts: number, isKo: boolean): string {
  const diff = Math.floor((Date.now() - ts) / 1000);
  if (diff < 60) return isKo ? "방금 전" : "just now";
  if (diff < 3600) {
    const m = Math.floor(diff / 60);
    return isKo ? `${m}분 전` : `${m}m ago`;
  }
  if (diff < 86400) {
    const h = Math.floor(diff / 3600);
    return isKo ? `${h}시간 전` : `${h}h ago`;
  }
  const d = Math.floor(diff / 86400);
  return isKo ? `${d}일 전` : `${d}d ago`;
}
