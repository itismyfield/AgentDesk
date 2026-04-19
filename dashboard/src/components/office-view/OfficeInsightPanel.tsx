import { useState, useEffect, type ReactNode } from "react";
import type { Notification } from "../NotificationCenter";
import type { Agent, AuditLogEntry, KanbanCard } from "../../types";
import { getAgentWarnings } from "../../agent-insights";
import { getFontFamilyForText } from "../../lib/fonts";
import AgentAvatar from "../AgentAvatar";
import {
  getProviderLevelColors,
  getProviderMeta,
} from "../../app/providerTheme";
import {
  getProviderLevelColors,
  getProviderMeta,
} from "../../app/providerTheme";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceListItem,
  SurfaceMetricPill,
  SurfaceNotice,
  SurfaceSection,
} from "../common/SurfacePrimitives";

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

function eventAccent(type: Notification["type"]): string {
  switch (type) {
    case "success":
      return "var(--ok)";
    case "warning":
      return "var(--warn)";
    case "error":
      return "var(--err)";
    default:
      return "var(--info)";
  }
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
  const sectionEyebrow = isKo ? "오피스 상황판" : "Office pulse";
  const sectionTitle = isKo ? "오피스 운영 신호" : "Office operations";
  const sectionDescription = isKo
    ? "리뷰, 완료, 열린 이슈, 경고 에이전트를 같은 표면에서 빠르게 확인합니다."
    : "Review, closed issues, open work, and warning agents from one surface.";
  const situationBody = (
    <>
      <div className="mt-4 grid grid-cols-3 gap-2">
        <StatChip
          label={isKo ? "검토필요" : "Review"}
          value={String(reviewCount)}
          tone="info"
          interactive
          onClick={onNavigateToKanban}
        />
        <StatChip
          label={isKo ? "오늘 완료" : "Closed"}
          value={String(ghClosedToday)}
          tone="success"
          interactive
          onClick={handleShowClosedIssues}
        />
        <StatChip
          label={isKo ? "열린이슈" : "Open"}
          value={String(openIssueCount)}
          tone="warn"
          interactive
          onClick={onNavigateToKanban}
        />
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
    </>
  );

  return (
    <div className={rootClassName}>
      <div className="sm:hidden">
        <SurfaceSection
          eyebrow={sectionEyebrow}
          title={sectionTitle}
          description={sectionDescription}
          className="rounded-[24px] p-4"
          style={{
            borderColor: "color-mix(in srgb, var(--th-accent-primary) 14%, var(--th-border) 86%)",
            background:
              "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, var(--th-accent-primary-soft) 5%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
          }}
          actions={(
            <div className="flex flex-wrap items-center gap-2">
              {warningCount > 0 && (
                <SurfaceActionButton
                  tone="warn"
                  compact
                  onClick={() => setShowWarnings((value) => !value)}
                >
                  {isKo ? `경고 ${warningCount}` : `${warningCount} warnings`}
                </SurfaceActionButton>
              )}
              <SurfaceActionButton
                tone={mobileExpanded ? "info" : "neutral"}
                compact
                onClick={() => setMobileExpanded((value) => !value)}
              >
                {mobileExpanded ? (isKo ? "접기" : "Hide") : (isKo ? "더보기" : "Details")}
              </SurfaceActionButton>
            </div>
          )}
        >
          {situationBody}
        </SurfaceSection>

        {mobileExpanded ? (
          <div className="mt-3 max-h-[38vh] space-y-3 overflow-y-auto pr-1">
            <InsightCard title={isKo ? "최근 이벤트" : "Recent Activity"} count={visibleNotifications.length}>
              {visibleNotifications.length === 0 ? (
                <SurfaceNotice compact>
                  {isKo ? "표시할 런타임 이벤트가 없습니다" : "No runtime events"}
                </SurfaceNotice>
              ) : (
                <div className="mt-2 space-y-2">
                  {visibleNotifications.map((item) => (
                    <EventRow
                      key={item.id}
                      title={item.message}
                      ts={item.ts}
                      isKo={isKo}
                      accent={eventAccent(item.type)}
                    />
                  ))}
                </div>
              )}
            </InsightCard>

            <InsightCard title={isKo ? "최근 변경" : "Recent Changes"} count={recentChanges.length}>
              {recentChanges.length === 0 ? (
                <SurfaceNotice compact>
                  {isKo ? "표시할 변경 로그가 없습니다" : "No recent changes"}
                </SurfaceNotice>
              ) : (
                <div className="mt-2 space-y-2">
                  {recentChanges.map((item) => (
                    <EventRow
                      key={item.id}
                      title={item.summary}
                      ts={item.created_at}
                      isKo={isKo}
                      accent="var(--warn)"
                      subtitle={`${item.entity_type}:${item.entity_id}`}
                    />
                  ))}
                </div>
              )}
            </InsightCard>
          </div>
        ) : null}
      </div>

      <div className="hidden sm:flex sm:flex-col sm:gap-3">
        <SurfaceSection
          eyebrow={sectionEyebrow}
          title={sectionTitle}
          description={sectionDescription}
          className="rounded-[24px] p-4"
          style={{
            borderColor: "color-mix(in srgb, var(--th-accent-primary) 14%, var(--th-border) 86%)",
            background:
              "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, var(--th-accent-primary-soft) 5%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
          }}
          actions={warningCount > 0 ? (
            <SurfaceActionButton
              tone="warn"
              compact
              onClick={() => setShowWarnings((value) => !value)}
            >
              {isKo ? `경고 ${warningCount}` : `${warningCount} warnings`}
            </SurfaceActionButton>
          ) : undefined}
        >
          {situationBody}
        </SurfaceSection>

        <InsightCard title={isKo ? "최근 이벤트" : "Recent Activity"} count={visibleNotifications.length}>
          {visibleNotifications.length === 0 ? (
            <SurfaceNotice compact>
              {isKo ? "표시할 런타임 이벤트가 없습니다" : "No runtime events"}
            </SurfaceNotice>
          ) : (
            <div className="mt-2 space-y-2">
              {visibleNotifications.map((item) => (
                <EventRow
                  key={item.id}
                  title={item.message}
                  ts={item.ts}
                  isKo={isKo}
                  accent={eventAccent(item.type)}
                />
              ))}
            </div>
          )}
        </InsightCard>

        <InsightCard title={isKo ? "최근 변경" : "Recent Changes"} count={recentChanges.length}>
          {recentChanges.length === 0 ? (
            <SurfaceNotice compact>
              {isKo ? "표시할 변경 로그가 없습니다" : "No recent changes"}
            </SurfaceNotice>
          ) : (
            <div className="mt-2 space-y-2">
              {recentChanges.map((item) => (
                <EventRow
                  key={item.id}
                  title={item.summary}
                  ts={item.created_at}
                  isKo={isKo}
                  accent="var(--warn)"
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
    <SurfaceCard
      className="rounded-[24px] p-4"
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 66%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
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
    </SurfaceCard>
  );
}

function StatChip({
  label,
  value,
  tone,
  interactive = false,
  onClick,
}: {
  label: string;
  value: string;
  tone: "info" | "success" | "warn";
  interactive?: boolean;
  onClick?: () => void;
}) {
  return (
    <button
      type="button"
      onClick={interactive ? onClick : undefined}
      className="min-w-0 w-full rounded-2xl text-left"
      disabled={!interactive}
      style={{ cursor: interactive ? "pointer" : "default" }}
    >
      <SurfaceMetricPill
        label={label}
        value={<span className="text-sm font-semibold">{value}</span>}
        tone={tone}
        className="h-full w-full"
        style={{ minWidth: 0 }}
      />
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
    <SurfaceCard className="mt-3 p-3">
      <div className="text-xs font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
        {isKo ? "문제 agent" : "Warning agents"}
      </div>
      {items.length === 0 ? (
        <SurfaceNotice className="mt-2" compact>
          {isKo ? "현재 경고가 없습니다" : "No warnings right now"}
        </SurfaceNotice>
      ) : (
        <div className="mt-2 space-y-2">
          {items.map(({ agent, warnings }) => (
            <button
              key={agent.id}
              type="button"
              onClick={() => onSelectAgent?.(agent)}
              className="w-full text-left"
            >
              <SurfaceListItem
                tone="warn"
                className="p-3"
                trailing={<span className="text-xs" style={{ color: "var(--th-accent-warn)" }}>⚠</span>}
              >
                <div className="flex items-center gap-2">
                  <AgentAvatar agent={agent} size={22} rounded="xl" />
                  <div
                    className="font-pixel min-w-0 truncate text-xs font-medium"
                    style={{
                      color: "var(--th-text)",
                      fontFamily: getFontFamilyForText(
                        agent.alias || agent.name_ko || agent.name,
                        "pixel",
                      ),
                    }}
                  >
                    {agent.alias || agent.name_ko || agent.name}
                  </div>
                </div>
                <div className="mt-0.5 text-xs" style={{ color: "var(--th-text-muted)" }}>
                  {(isKo ? warnings.map((warning) => warning.ko) : warnings.map((warning) => warning.en)).join(", ")}
                </div>
              </SurfaceListItem>
            </button>
          ))}
        </div>
      )}
    </SurfaceCard>
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
    <SurfaceListItem
      className="p-3"
      trailing={
        <div className="text-xs text-right" style={{ color: "var(--th-text-muted)" }}>
          {timeAgo(ts, isKo)}
        </div>
      }
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
        </div>
      </div>
    </SurfaceListItem>
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
    <SurfaceCard className="mt-3 p-3">
      <div className="flex items-center justify-between">
        <div className="text-xs font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
          {isKo ? `오늘 완료 (${issues.length})` : `Closed today (${issues.length})`}
        </div>
        <SurfaceActionButton tone="neutral" compact onClick={onClose}>✕</SurfaceActionButton>
      </div>
      {issues.length === 0 ? (
        <SurfaceNotice className="mt-2" compact>
          {isKo ? "오늘 완료된 이슈가 없습니다" : "No issues closed today"}
        </SurfaceNotice>
      ) : (
        <div className="mt-2 max-h-[30vh] space-y-1.5 overflow-y-auto pr-1">
          {issues.map((issue) => (
            <a
              key={`${issue.repo}-${issue.number}`}
              href={issue.url}
              target="_blank"
              rel="noopener noreferrer"
              className="block"
            >
              <SurfaceListItem
                tone="success"
                className="p-3"
                trailing={
                  <span className="text-xs shrink-0" style={{ color: "var(--th-text-muted)" }}>
                    #{issue.number}
                  </span>
                }
              >
                <div className="flex items-center gap-2">
                  <span className="inline-flex items-center rounded-full border px-2 py-1 text-[10px] font-semibold leading-none" style={{ borderColor: "color-mix(in srgb, var(--th-accent-primary) 30%, var(--th-border) 70%)", background: "var(--th-accent-primary-soft)", color: "var(--th-accent-primary)" }}>
                    {repoShort(issue.repo)}
                  </span>
                </div>
                <div className="mt-1 text-xs leading-snug" style={{ color: "var(--th-text)" }}>
                  {issue.title}
                </div>
              </SurfaceListItem>
            </a>
          ))}
        </div>
      )}
    </SurfaceCard>
  );
}

/* ── Mini Rate Limit Bar (inline in insight panel) ── */

interface RLBucket {
  id: string;
  label: string;
  utilization: number | null;
  level: "normal" | "warning" | "danger";
}

interface RLProvider {
  provider: string;
  buckets: RLBucket[];
  stale: boolean;
  unsupported: boolean;
  reason: string | null;
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
  unsupported?: boolean;
  reason?: string | null;
}

const RL_HIDDEN_PROVIDERS = new Set(["github"]);
const RL_HIDDEN_BUCKETS = new Set(["7d Sonnet"]);

export function normalizeMiniRateLimitProviderLabel(provider: string): string {
  const normalized = provider.trim().toLowerCase();
  switch (normalized) {
    case "claude":
      return "Claude";
    case "codex":
      return "Codex";
    case "gemini":
      return "Gemini";
    case "qwen":
      return "Qwen";
    default:
      return provider ? provider.charAt(0).toUpperCase() + provider.slice(1) : provider;
  }
}

export function transformRLProviders(raw: RawRLProvider[]): RLProvider[] {
  return raw
    .filter((rp) => !RL_HIDDEN_PROVIDERS.has(rp.provider.toLowerCase()))
    .map((rp) => ({
      provider: normalizeMiniRateLimitProviderLabel(rp.provider),
      stale: rp.stale,
      unsupported: Boolean(rp.unsupported),
      reason: typeof rp.reason === "string" ? rp.reason : null,
      buckets: rp.buckets
        .filter((b) => !RL_HIDDEN_BUCKETS.has(b.name))
        .map((b) => {
          const utilization =
            b.limit > 0 && b.used >= 0 && b.remaining >= 0
              ? Math.round((b.used / b.limit) * 100)
              : null;
          return {
            id: b.name,
            label: b.name,
            utilization,
            level: (
              utilization !== null && utilization >= 95
                ? "danger"
                : utilization !== null && utilization >= 80
                  ? "warning"
                  : "normal"
            ) as "normal" | "warning" | "danger",
          };
        }),
    }));
}

const RL_ICONS: Record<string, string> = {
  Claude: "🤖",
  Codex: "⚡",
  Gemini: "🔮",
  Qwen: "🧠",
  OpenCode: "🧩",
  Copilot: "🛩️",
  Antigravity: "🌀",
  API: "🔌",
};

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
        const providerMeta = getProviderMeta(p.provider);
        const visible = p.buckets;
        return (
          <div key={p.provider} className="flex items-center gap-0">
            {/* Fixed-width left: provider + stale placeholder */}
            <div className="flex items-center gap-1 shrink-0" style={{ width: 96 }}>
              <span
                className="text-xs font-bold uppercase truncate"
                style={{ color: providerMeta.color }}
              >
                {(RL_ICONS[p.provider] ?? "•")} {p.provider}
              </span>
              {p.stale ? (
                <span
                  className="rounded px-0.5 text-[7px] font-medium shrink-0"
                  style={{
                    color: "var(--warn)",
                    background:
                      "color-mix(in oklch, var(--warn) 14%, var(--bg-2) 86%)",
                    border:
                      "1px solid color-mix(in oklch, var(--warn) 28%, var(--line) 72%)",
                  }}
                >
                  {isKo ? "지연" : "STALE"}
                </span>
              ) : null}
            </div>
            {p.unsupported || visible.length === 0 ? (
              <div className="flex-1 min-w-0">
                <div className="flex items-center gap-2 overflow-hidden">
                  <span
                    className="rounded px-1.5 py-0.5 text-[9px] font-semibold shrink-0"
                    style={{
                      color: "var(--fg-dim)",
                      background:
                        "color-mix(in oklch, var(--fg-faint) 10%, var(--bg-2) 90%)",
                      border:
                        "1px solid color-mix(in oklch, var(--fg-faint) 20%, var(--line) 80%)",
                    }}
                  >
                    {p.unsupported ? "N/A" : (isKo ? "비어있음" : "EMPTY")}
                  </span>
                  <span className="truncate text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                    {p.unsupported
                      ? (p.reason ?? (isKo ? "한도 텔레메트리 미지원" : "Rate-limit telemetry unavailable"))
                      : (isKo ? "표시할 버킷 데이터 없음" : "No bucket data")}
                  </span>
                </div>
              </div>
            ) : (
              <div className="flex-1 grid grid-cols-2 gap-x-2">
                {visible.map((b) => (
                  <div key={b.id} className="flex items-center gap-1">
                    <span
                      className="text-xs font-bold shrink-0 w-[14px]"
                      style={{ color: getProviderLevelColors(p.provider, b.level).text }}
                    >
                      {b.label}
                    </span>
                    <div className="flex-1 min-w-0">
                      <div
                        className="relative h-[3px] rounded-full overflow-hidden"
                        style={{ background: "var(--line-soft)" }}
                      >
                        <div
                          className="absolute inset-y-0 left-0 rounded-full"
                          style={{
                            width: b.utilization === null ? "0%" : `${Math.min(b.utilization, 100)}%`,
                            background:
                              b.utilization === null
                                ? "transparent"
                                : getProviderLevelColors(p.provider, b.level).bar,
                          }}
                        />
                      </div>
                    </div>
                    <span
                      className="text-xs font-mono font-bold shrink-0 w-[28px] text-right"
                      style={{
                        color:
                          b.utilization === null
                            ? "var(--th-text-muted)"
                            : getProviderLevelColors(p.provider, b.level).text,
                      }}
                    >
                      {b.utilization === null ? "N/A" : `${b.utilization}%`}
                    </span>
                  </div>
                ))}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
