import { useState, useEffect, type ReactNode } from "react";
import type { Notification } from "../NotificationCenter";
import type { Agent, AuditLogEntry, KanbanCard } from "../../types";
import { getAgentWarnings } from "../../agent-insights";

interface OfficeInsightPanelProps {
  agents: Agent[];
  notifications: Notification[];
  auditLogs: AuditLogEntry[];
  kanbanCards?: KanbanCard[];
  onNavigateToKanban?: () => void;
  isKo: boolean;
  onSelectAgent?: (agent: Agent) => void;
  docked?: boolean;
}

function timeAgo(ts: number, isKo: boolean): string {
  const diff = Date.now() - ts;
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return isKo ? "방금" : "just now";
  if (mins < 60) return isKo ? `${mins}분 전` : `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return isKo ? `${hrs}시간 전` : `${hrs}h ago`;
  const days = Math.floor(hrs / 24);
  return isKo ? `${days}일 전` : `${days}d ago`;
}

export default function OfficeInsightPanel({
  agents,
  notifications,
  auditLogs,
  kanbanCards,
  onNavigateToKanban,
  isKo,
  onSelectAgent,
  docked = false,
}: OfficeInsightPanelProps) {
  const [mobileExpanded, setMobileExpanded] = useState(false);
  const [showWarnings, setShowWarnings] = useState(false);
  const [ghClosedToday, setGhClosedToday] = useState(0);
  const [showClosedIssues, setShowClosedIssues] = useState(false);
  const [closedIssues, setClosedIssues] = useState<ClosedIssueItem[]>([]);
  const activeCards = kanbanCards ?? [];
  const reviewCount = activeCards.filter((c) => c.status === "review").length;
  const terminalStatuses = new Set(["done", "failed", "cancelled"]);
  const openIssueCount = activeCards.filter((c) => !terminalStatuses.has(c.status)).length;

  useEffect(() => {
    let mounted = true;
    const load = async () => {
      try {
        const res = await fetch("/api/stats", { credentials: "include" });
        if (!res.ok) return;
        const json = await res.json() as { github_closed_today?: number };
        if (mounted && typeof json.github_closed_today === "number") {
          setGhClosedToday(json.github_closed_today);
        }
      } catch { /* ignore */ }
    };
    load();
    const timer = setInterval(load, 60_000);
    return () => { mounted = false; clearInterval(timer); };
  }, []);

  const handleShowClosedIssues = async () => {
    if (showClosedIssues) { setShowClosedIssues(false); return; }
    try {
      const res = await fetch("/api/github-closed-today", { credentials: "include" });
      if (!res.ok) return;
      const json = await res.json() as { issues: ClosedIssueItem[] };
      setClosedIssues(json.issues);
      setShowClosedIssues(true);
    } catch { /* ignore */ }
  };
  const warningCount = agents.filter((agent) => getAgentWarnings(agent).length > 0).length;
  const warningAgents = agents
    .map((agent) => ({ agent, warnings: getAgentWarnings(agent) }))
    .filter((entry) => entry.warnings.length > 0);
  const recentNotifications = notifications.slice(0, 4);
  const recentChanges = auditLogs.slice(0, 4);
  const fallbackNotifications: Notification[] =
    agents
      .filter((agent) => agent.status === "working")
      .slice(0, 4)
      .map((agent, idx) => ({
        id: `working-${agent.id}-${idx}`,
        message: `${agent.alias || agent.name_ko || agent.name}: ${agent.session_info || (isKo ? "작업 중" : "Working")}`,
        type: "info",
        ts: Date.now(),
      }));
  const changeFallbackNotifications: Notification[] =
    recentChanges.map((item, idx) => ({
      id: `change-${item.id}-${idx}`,
      message: item.summary,
      type: "info",
      ts: item.created_at,
    }));
  const visibleNotifications =
    recentNotifications.length > 0
      ? recentNotifications
      : fallbackNotifications.length > 0
        ? fallbackNotifications
        : changeFallbackNotifications;
  const rootClassName = docked
    ? "relative z-20 w-full pointer-events-auto"
    : "relative z-20 mb-3 px-3 pt-3 pointer-events-auto sm:absolute sm:left-auto sm:right-3 sm:top-3 sm:mb-0 sm:w-[min(22rem,calc(100vw-1.5rem))] sm:px-0 sm:pt-0";

  return (
    <div className={rootClassName}>
      <div className="sm:hidden">
        <section
          className="rounded-2xl p-3 shadow-xl"
          style={{
            background: "color-mix(in srgb, var(--th-card-bg) 86%, transparent)",
            border: "1px solid var(--th-card-border)",
            backdropFilter: "blur(18px)",
          }}
        >
          <div className="flex items-center justify-between gap-3">
            <div className="flex items-center gap-1.5">
              <div className="text-xs font-semibold uppercase tracking-[0.24em]" style={{ color: "var(--th-text-muted)" }}>
                {isKo ? "상황판" : "Situation"}
              </div>
              {warningCount > 0 && (
                <button
                  type="button"
                  onClick={() => setShowWarnings((v) => !v)}
                  className="rounded-full px-1.5 py-0.5 text-xs font-bold"
                  style={{ color: "#92400e", background: "rgba(251,191,36,0.25)", border: "1px solid rgba(251,191,36,0.4)" }}
                >
                  ⚠ {warningCount}
                </button>
              )}
            </div>
            <button
              type="button"
              onClick={() => setMobileExpanded((value) => !value)}
              className="rounded-full px-2 py-1 text-xs font-medium"
              style={{
                color: "var(--th-text)",
                background: "var(--th-bg-surface)",
                border: "1px solid var(--th-card-border)",
              }}
            >
              {mobileExpanded ? (isKo ? "접기" : "Hide") : (isKo ? "더보기" : "Details")}
            </button>
          </div>
          <div className="mt-2 grid grid-cols-3 gap-2">
            <StatChip label={isKo ? "검토필요" : "Review"} value={String(reviewCount)} color="#14b8a6" interactive onClick={onNavigateToKanban} />
            <StatChip label={isKo ? "오늘 완료" : "Closed"} value={String(ghClosedToday)} color="#34d399" interactive onClick={handleShowClosedIssues} />
            <StatChip label={isKo ? "열린이슈" : "Open"} value={String(openIssueCount)} color="#f59e0b" interactive onClick={onNavigateToKanban} />
          </div>

          <MiniRateLimitBar isKo={isKo} />

          {showClosedIssues ? (
            <ClosedIssueList issues={closedIssues} isKo={isKo} onClose={() => setShowClosedIssues(false)} />
          ) : null}

          {showWarnings && warningCount > 0 ? (
            <WarningList
              items={warningAgents}
              isKo={isKo}
              onSelectAgent={(agent) => {
                onSelectAgent?.(agent);
                setShowWarnings(false);
              }}
            />
          ) : null}

          {mobileExpanded ? (
            <div className="mt-3 max-h-[38vh] space-y-3 overflow-y-auto pr-1">
              <InsightCard title={isKo ? "최근 이벤트" : "Recent Activity"} count={visibleNotifications.length}>
                {visibleNotifications.length === 0 ? (
                  <div className="mt-2 text-xs" style={{ color: "var(--th-text-muted)" }}>
                    {isKo ? "표시할 런타임 이벤트가 없습니다" : "No runtime events"}
                  </div>
                ) : (
                  <div className="mt-2 space-y-2">
                    {visibleNotifications.map((item) => (
                      <EventRow
                        key={item.id}
                        title={item.message}
                        ts={item.ts}
                        isKo={isKo}
                        accent={
                          item.type === "success"
                            ? "#34d399"
                            : item.type === "warning"
                              ? "#fbbf24"
                              : item.type === "error"
                                ? "#f87171"
                                : "#60a5fa"
                        }
                      />
                    ))}
                  </div>
                )}
              </InsightCard>

              <InsightCard title={isKo ? "최근 변경" : "Recent Changes"} count={recentChanges.length}>
                {recentChanges.length === 0 ? (
                  <div className="mt-2 text-xs" style={{ color: "var(--th-text-muted)" }}>
                    {isKo ? "표시할 변경 로그가 없습니다" : "No recent changes"}
                  </div>
                ) : (
                  <div className="mt-2 space-y-2">
                    {recentChanges.map((item) => (
                      <EventRow
                        key={item.id}
                        title={item.summary}
                        ts={item.created_at}
                        isKo={isKo}
                        accent="#f59e0b"
                        subtitle={`${item.entity_type}:${item.entity_id}`}
                      />
                    ))}
                  </div>
                )}
              </InsightCard>
            </div>
          ) : null}
        </section>
      </div>

      <div className="hidden sm:flex sm:flex-col sm:gap-3">
        <section
          className="rounded-2xl p-3 shadow-xl"
          style={{
            background: "color-mix(in srgb, var(--th-card-bg) 86%, transparent)",
            border: "1px solid var(--th-card-border)",
            backdropFilter: "blur(18px)",
          }}
        >
          <div className="flex items-center gap-1.5">
            <div className="text-xs font-semibold uppercase tracking-[0.24em]" style={{ color: "var(--th-text-muted)" }}>
              {isKo ? "상황판" : "Situation"}
            </div>
            {warningCount > 0 && (
              <button
                type="button"
                onClick={() => setShowWarnings((v) => !v)}
                className="rounded-full px-1.5 py-0.5 text-xs font-bold"
                style={{ color: "#92400e", background: "rgba(251,191,36,0.25)", border: "1px solid rgba(251,191,36,0.4)" }}
              >
                ⚠ {warningCount}
              </button>
            )}
          </div>
          <div className="mt-2 grid grid-cols-3 gap-2">
            <StatChip label={isKo ? "검토필요" : "Review"} value={String(reviewCount)} color="#14b8a6" interactive onClick={onNavigateToKanban} />
            <StatChip label={isKo ? "오늘 완료" : "Closed"} value={String(ghClosedToday)} color="#34d399" interactive onClick={handleShowClosedIssues} />
            <StatChip label={isKo ? "열린이슈" : "Open"} value={String(openIssueCount)} color="#f59e0b" interactive onClick={onNavigateToKanban} />
          </div>
          {showClosedIssues ? (
            <ClosedIssueList issues={closedIssues} isKo={isKo} onClose={() => setShowClosedIssues(false)} />
          ) : null}
          {showWarnings && warningCount > 0 ? (
            <WarningList
              items={warningAgents}
              isKo={isKo}
              onSelectAgent={(agent) => {
                onSelectAgent?.(agent);
                setShowWarnings(false);
              }}
            />
          ) : null}
          <MiniRateLimitBar isKo={isKo} />
        </section>

        <InsightCard title={isKo ? "최근 이벤트" : "Recent Activity"} count={visibleNotifications.length}>
          {visibleNotifications.length === 0 ? (
            <div className="mt-2 text-xs" style={{ color: "var(--th-text-muted)" }}>
              {isKo ? "표시할 런타임 이벤트가 없습니다" : "No runtime events"}
            </div>
          ) : (
            <div className="mt-2 space-y-2">
              {visibleNotifications.map((item) => (
                <EventRow
                  key={item.id}
                  title={item.message}
                  ts={item.ts}
                  isKo={isKo}
                  accent={
                    item.type === "success"
                      ? "#34d399"
                      : item.type === "warning"
                        ? "#fbbf24"
                        : item.type === "error"
                          ? "#f87171"
                          : "#60a5fa"
                  }
                />
              ))}
            </div>
          )}
        </InsightCard>

        <InsightCard title={isKo ? "최근 변경" : "Recent Changes"} count={recentChanges.length}>
          {recentChanges.length === 0 ? (
            <div className="mt-2 text-xs" style={{ color: "var(--th-text-muted)" }}>
              {isKo ? "표시할 변경 로그가 없습니다" : "No recent changes"}
            </div>
          ) : (
            <div className="mt-2 space-y-2">
              {recentChanges.map((item) => (
                <EventRow
                  key={item.id}
                  title={item.summary}
                  ts={item.created_at}
                  isKo={isKo}
                  accent="#f59e0b"
                  subtitle={`${item.entity_type}:${item.entity_id}`}
                />
              ))}
            </div>
          )}
        </InsightCard>
      </div>
    </div>
  );
}

function InsightCard({
  title,
  count,
  children,
}: {
  title: string;
  count: number;
  children: ReactNode;
}) {
  return (
    <section
      className="rounded-2xl p-3 shadow-xl"
      style={{
        background: "color-mix(in srgb, var(--th-card-bg) 84%, transparent)",
        border: "1px solid var(--th-card-border)",
        backdropFilter: "blur(18px)",
      }}
    >
      <div className="flex items-center justify-between">
        <div className="text-xs font-semibold uppercase tracking-[0.24em]" style={{ color: "var(--th-text-muted)" }}>
          {title}
        </div>
        <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
          {count}
        </span>
      </div>
      {children}
    </section>
  );
}

function StatChip({
  label,
  value,
  color,
  interactive = false,
  onClick,
}: {
  label: string;
  value: string;
  color: string;
  interactive?: boolean;
  onClick?: () => void;
}) {
  return (
    <button
      type="button"
      onClick={interactive ? onClick : undefined}
      className="rounded-xl px-2 py-2 text-left"
      disabled={!interactive}
      style={{
        background: "var(--th-bg-surface)",
        border: "1px solid var(--th-card-border)",
        cursor: interactive ? "pointer" : "default",
        opacity: interactive ? 1 : 1,
      }}
    >
      <div className="text-xs uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
        {label}
      </div>
      <div className="mt-1 text-sm font-semibold" style={{ color }}>
        {value}
      </div>
    </button>
  );
}

function WarningList({
  items,
  isKo,
  onSelectAgent,
}: {
  items: Array<{ agent: Agent; warnings: ReturnType<typeof getAgentWarnings> }>;
  isKo: boolean;
  onSelectAgent?: (agent: Agent) => void;
}) {
  return (
    <div
      className="mt-3 rounded-xl border p-2"
      style={{
        background: "var(--th-bg-surface)",
        borderColor: "var(--th-card-border)",
      }}
    >
      <div className="text-xs font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
        {isKo ? "문제 agent" : "Warning agents"}
      </div>
      {items.length === 0 ? (
        <div className="mt-2 text-xs" style={{ color: "var(--th-text-muted)" }}>
          {isKo ? "현재 경고가 없습니다" : "No warnings right now"}
        </div>
      ) : (
        <div className="mt-2 space-y-2">
          {items.map(({ agent, warnings }) => (
            <button
              key={agent.id}
              type="button"
              onClick={() => onSelectAgent?.(agent)}
              className="w-full rounded-lg px-2 py-2 text-left"
              style={{ background: "rgba(148,163,184,0.08)" }}
            >
              <div className="text-xs font-medium" style={{ color: "var(--th-text)" }}>
                {agent.avatar_emoji} {agent.alias || agent.name_ko || agent.name}
              </div>
              <div className="mt-0.5 text-xs" style={{ color: "var(--th-text-muted)" }}>
                {(isKo ? warnings.map((warning) => warning.ko) : warnings.map((warning) => warning.en)).join(", ")}
              </div>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

function EventRow({
  title,
  subtitle,
  ts,
  isKo,
  accent,
}: {
  title: string;
  subtitle?: string;
  ts: number;
  isKo: boolean;
  accent: string;
}) {
  return (
    <div
      className="rounded-xl px-2.5 py-2"
      style={{ background: "var(--th-bg-surface)" }}
    >
      <div className="flex items-start gap-2">
        <span className="mt-1 h-2 w-2 shrink-0 rounded-full" style={{ background: accent }} />
        <div className="min-w-0 flex-1">
          <div className="text-xs leading-relaxed" style={{ color: "var(--th-text-primary)" }}>
            {title}
          </div>
          {subtitle ? (
            <div className="mt-0.5 text-xs" style={{ color: "var(--th-text-muted)" }}>
              {subtitle}
            </div>
          ) : null}
          <div className="mt-0.5 text-xs" style={{ color: "var(--th-text-muted)" }}>
            {timeAgo(ts, isKo)}
          </div>
        </div>
      </div>
    </div>
  );
}

/* ── Closed Issue types ── */

interface ClosedIssueItem {
  number: number;
  title: string;
  repo: string;
  url: string;
  closedAt: string;
  labels: string[];
}

function ClosedIssueList({ issues, isKo, onClose }: { issues: ClosedIssueItem[]; isKo: boolean; onClose: () => void }) {
  const repoShort = (repo: string) => repo.split("/").pop() || repo;
  return (
    <div
      className="mt-3 rounded-xl border p-2"
      style={{ background: "var(--th-bg-surface)", borderColor: "var(--th-card-border)" }}
    >
      <div className="flex items-center justify-between">
        <div className="text-xs font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
          {isKo ? `오늘 완료 (${issues.length})` : `Closed today (${issues.length})`}
        </div>
        <button
          type="button"
          onClick={onClose}
          className="text-xs px-1 rounded"
          style={{ color: "var(--th-text-muted)" }}
        >
          ✕
        </button>
      </div>
      {issues.length === 0 ? (
        <div className="mt-2 text-xs" style={{ color: "var(--th-text-muted)" }}>
          {isKo ? "오늘 완료된 이슈가 없습니다" : "No issues closed today"}
        </div>
      ) : (
        <div className="mt-2 max-h-[30vh] space-y-1.5 overflow-y-auto pr-1">
          {issues.map((issue) => (
            <a
              key={`${issue.repo}-${issue.number}`}
              href={issue.url}
              target="_blank"
              rel="noopener noreferrer"
              className="block rounded-lg px-2 py-1.5"
              style={{ background: "rgba(148,163,184,0.08)" }}
            >
              <div className="flex items-center gap-1.5">
                <span className="text-xs font-bold shrink-0 rounded px-1 py-0.5" style={{ color: "#34d399", background: "rgba(52,211,153,0.1)" }}>
                  {repoShort(issue.repo)}
                </span>
                <span className="text-xs shrink-0" style={{ color: "var(--th-text-muted)" }}>
                  #{issue.number}
                </span>
              </div>
              <div className="mt-0.5 text-xs leading-snug" style={{ color: "var(--th-text)" }}>
                {issue.title}
              </div>
            </a>
          ))}
        </div>
      )}
    </div>
  );
}

/* ── Mini Rate Limit Bar (inline in insight panel) ── */

interface RLBucket {
  id: string;
  label: string;
  utilization: number;
  level: "normal" | "warning" | "danger";
}

interface RLProvider {
  provider: string;
  buckets: RLBucket[];
  stale: boolean;
}

interface RawRLBucket {
  name: string;
  limit: number;
  used: number;
  remaining: number;
  reset: number;
}

interface RawRLProvider {
  provider: string;
  buckets: RawRLBucket[];
  stale: boolean;
}

const RL_HIDDEN_PROVIDERS = new Set(["github"]);
const RL_HIDDEN_BUCKETS = new Set(["7d Sonnet"]);

function transformRLProviders(raw: RawRLProvider[]): RLProvider[] {
  return raw
    .filter((rp) => !RL_HIDDEN_PROVIDERS.has(rp.provider.toLowerCase()))
    .map((rp) => ({
      provider: rp.provider.charAt(0).toUpperCase() + rp.provider.slice(1),
      stale: rp.stale,
      buckets: rp.buckets
        .filter((b) => !RL_HIDDEN_BUCKETS.has(b.name))
        .map((b) => {
          const utilization = b.limit > 0 ? Math.round((b.used / b.limit) * 100) : 0;
          return {
            id: b.name,
            label: b.name,
            utilization,
            level: (utilization >= 95 ? "danger" : utilization >= 80 ? "warning" : "normal") as "normal" | "warning" | "danger",
          };
        }),
    }));
}

const RL_COLORS: Record<string, { normal: string; warning: string; danger: string; accent: string }> = {
  Claude: { accent: "#f59e0b", normal: "#f59e0b", warning: "#ea580c", danger: "#ef4444" },
  Codex: { accent: "#34d399", normal: "#34d399", warning: "#fbbf24", danger: "#f87171" },
  Gemini: { accent: "#3b82f6", normal: "#3b82f6", warning: "#f59e0b", danger: "#ef4444" },
  OpenCode: { accent: "#a855f7", normal: "#a855f7", warning: "#f59e0b", danger: "#ef4444" },
  Copilot: { accent: "#10b981", normal: "#10b981", warning: "#f59e0b", danger: "#ef4444" },
  Antigravity: { accent: "#f472b6", normal: "#f472b6", warning: "#f59e0b", danger: "#ef4444" },
  API: { accent: "#94a3b8", normal: "#94a3b8", warning: "#f59e0b", danger: "#ef4444" },
};
const RL_ICONS: Record<string, string> = {
  Claude: "🤖",
  Codex: "⚡",
  Gemini: "🔮",
  OpenCode: "🧩",
  Copilot: "🛩️",
  Antigravity: "🌀",
  API: "🔌",
};

function barColor(provider: string, level: string) {
  const p = RL_COLORS[provider] || RL_COLORS.Codex;
  if (level === "danger") return p.danger;
  if (level === "warning") return p.warning;
  return p.normal;
}

function MiniRateLimitBar({ isKo }: { isKo: boolean }) {
  const [providers, setProviders] = useState<RLProvider[]>([]);

  useEffect(() => {
    let mounted = true;
    const load = async () => {
      try {
        const res = await fetch("/api/rate-limits", { credentials: "include" });
        if (!res.ok) return;
        const json = await res.json() as { providers: RawRLProvider[] };
        if (mounted) setProviders(transformRLProviders(json.providers ?? []));
      } catch { /* ignore */ }
    };
    load();
    const timer = setInterval(load, 30_000);
    return () => { mounted = false; clearInterval(timer); };
  }, []);

  if (providers.length === 0) return null;

  return (
    <div className="mt-2 space-y-1">
      {providers.map((p) => {
        const accent = (RL_COLORS[p.provider] || RL_COLORS.Codex).accent;
        const visible = p.buckets;
        return (
          <div key={p.provider} className="flex items-center gap-0">
            {/* Fixed-width left: provider + stale placeholder */}
            <div className="flex items-center gap-1 shrink-0" style={{ width: 96 }}>
              <span className="text-xs font-bold uppercase truncate" style={{ color: accent }}>
                {(RL_ICONS[p.provider] ?? "•")} {p.provider}
              </span>
              {p.stale ? (
                <span
                  className="rounded px-0.5 text-[7px] font-medium shrink-0"
                  style={{ color: "#fbbf24", background: "rgba(251,191,36,0.1)", border: "1px solid rgba(251,191,36,0.2)" }}
                >
                  {isKo ? "지연" : "STALE"}
                </span>
              ) : null}
            </div>
            {/* Buckets grid — fixed 2-column */}
            <div className="flex-1 grid grid-cols-2 gap-x-2">
              {visible.map((b) => (
                <div key={b.id} className="flex items-center gap-1">
                  <span className="text-xs font-bold shrink-0 w-[14px]" style={{ color: barColor(p.provider, b.level) }}>
                    {b.label}
                  </span>
                  <div className="flex-1 min-w-0">
                    <div className="relative h-[3px] rounded-full overflow-hidden" style={{ background: "rgba(255,255,255,0.08)" }}>
                      <div
                        className="absolute inset-y-0 left-0 rounded-full"
                        style={{ width: `${Math.min(b.utilization, 100)}%`, background: barColor(p.provider, b.level) }}
                      />
                    </div>
                  </div>
                  <span className="text-xs font-mono font-bold shrink-0 w-[22px] text-right" style={{ color: barColor(p.provider, b.level) }}>
                    {b.utilization}%
                  </span>
                </div>
              ))}
            </div>
          </div>
        );
      })}
    </div>
  );
}
