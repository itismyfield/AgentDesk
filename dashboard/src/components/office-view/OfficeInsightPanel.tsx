import { useState, useEffect } from "react";
import type { Notification } from "../NotificationCenter";
import type { Agent, AuditLogEntry, KanbanCard, RoundTableMeeting } from "../../types";
import { getAgentWarnings } from "../../agent-insights";
import { getFontFamilyForText } from "../../lib/fonts";
import AgentAvatar from "../AgentAvatar";
import {
  getProviderLevelColors,
  getProviderMeta,
} from "../../app/providerTheme";
import type {
  OfficeManualIntervention,
  OfficeSeatStatus,
} from "./officeAgentState";
import {
  SurfaceActionButton,
  SurfaceListItem,
  SurfaceMetricPill,
  SurfaceNotice,
  SurfaceSection,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";

interface OfficeInsightPanelProps {
  agents: Agent[];
  notifications: Notification[];
  auditLogs: AuditLogEntry[];
  kanbanCards?: KanbanCard[];
  onNavigateToKanban?: () => void;
  isKo: boolean;
  onSelectAgent?: (agent: Agent) => void;
  selectedAgent?: Agent | null;
  onClearSelectedAgent?: () => void;
  activeMeeting?: RoundTableMeeting | null;
  manualInterventionByAgent?: Map<string, OfficeManualIntervention>;
  primaryCardByAgent?: Map<string, KanbanCard>;
  seatStatusByAgent?: Map<string, OfficeSeatStatus>;
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
  kanbanCards,
  onNavigateToKanban,
  isKo,
  onSelectAgent,
  docked = false,
}: OfficeInsightPanelProps) {
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
  const sectionTitle = isKo ? "오피스 운영 신호" : "Office operations";
  const sectionDescription = isKo
    ? "리뷰, 완료, 열린 이슈, 경고 에이전트를 같은 표면에서 빠르게 확인합니다."
    : "Review, closed issues, open work, and warning agents from one surface.";
  const rootClassName = docked
    ? "relative z-20 flex w-full flex-col gap-3 pointer-events-auto"
    : "relative z-20 mb-3 flex flex-col gap-3 px-3 pt-3 pointer-events-auto sm:absolute sm:left-auto sm:right-3 sm:top-3 sm:mb-0 sm:w-[min(22rem,calc(100vw-1.5rem))] sm:px-0 sm:pt-0";
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
          eyebrow={isKo ? "확장" : "Extension"}
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
      </div>

      <div className="hidden sm:flex sm:flex-col sm:gap-3">
        <SurfaceSection
          eyebrow={isKo ? "확장" : "Extension"}
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
      </div>
    </div>
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
    <SurfaceSubsection
      title={isKo ? "문제 agent" : "Warning agents"}
      description={isKo ? "주의 신호가 있는 에이전트를 바로 열 수 있습니다." : "Open agents with active warning signals."}
      className="mt-3"
    >
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
    </SurfaceSubsection>
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
    <SurfaceSubsection
      title={isKo ? "오늘 완료" : "Closed today"}
      description={isKo ? "오늘 GitHub에서 닫힌 이슈를 빠르게 훑습니다." : "Quick scan of GitHub issues closed today."}
      className="mt-3"
      actions={<SurfaceActionButton tone="neutral" compact onClick={onClose}>✕</SurfaceActionButton>}
    >
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
    </SurfaceSubsection>
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
    .flatMap((rp) => {
      const buckets = rp.buckets
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
        });
      if (rp.unsupported && buckets.length === 0) {
        return [];
      }
      return [
        {
          provider: normalizeMiniRateLimitProviderLabel(rp.provider),
          stale: rp.stale,
          unsupported: Boolean(rp.unsupported),
          reason: typeof rp.reason === "string" ? rp.reason : null,
          buckets,
        },
      ];
    });
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

/* Exported so the home `m_rate_limit` KPI tile can mount the same gauge
   the office `오피스 운영신호` panel uses. The previous home tile rendered
   only a single max-utilization percentage with a sparkline, which the
   user reported as low-density compared to the per-provider/per-bucket
   bars on the office page.

   `density` lets the home tile request a more spacious layout so its
   visual height aligns with the sibling HomeMetricTile cards (오늘 토큰 /
   API 비용 / 진행 중) on the same row. Office signals keep the original
   compact layout. */
export function MiniRateLimitBar({
  isKo,
  density = "compact",
}: {
  isKo: boolean;
  density?: "compact" | "comfortable";
}) {
  const [providers, setProviders] = useState<RLProvider[]>([]);
  const isComfy = density === "comfortable";

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
    <div className={isComfy ? "mt-3 space-y-3" : "mt-2 space-y-1"}>
      {providers.map((p) => {
        const providerMeta = getProviderMeta(p.provider);
        const visible = p.buckets;
        return (
          <div key={p.provider} className="flex items-center gap-0">
            {/* Fixed-width left: provider + stale placeholder */}
            <div
              className="flex items-center gap-1 shrink-0"
              style={{ width: isComfy ? 108 : 96 }}
            >
              <span
                className={
                  isComfy
                    ? "text-sm font-bold uppercase truncate"
                    : "text-xs font-bold uppercase truncate"
                }
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
                {visible.map((b) => {
                  /* Use the level `bar` color (= pure provider accent for
                     normal, --warn / --err for warning / danger) instead of
                     the lightened `text` mix so the Claude row reads as the
                     same orange as the CLAUDE label, and warning / danger
                     states still pop. The mixed-with-fg variant we used
                     before drifted toward yellow on the dark surface. */
                  const accentText = getProviderLevelColors(p.provider, b.level).bar;
                  return (
                    <div key={b.id} className="flex items-center gap-1">
                      <span
                        className={
                          isComfy
                            ? "text-sm font-bold shrink-0 w-[18px]"
                            : "text-xs font-bold shrink-0 w-[14px]"
                        }
                        style={{ color: accentText }}
                      >
                        {b.label}
                      </span>
                      <div className="flex-1 min-w-0">
                        <div
                          className={
                            isComfy
                              ? "relative h-1.5 rounded-full overflow-hidden"
                              : "relative h-[3px] rounded-full overflow-hidden"
                          }
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
                        className={
                          isComfy
                            ? "text-sm font-mono font-bold shrink-0 w-[34px] text-right"
                            : "text-xs font-mono font-bold shrink-0 w-[28px] text-right"
                        }
                        style={{
                          color:
                            b.utilization === null ? "var(--th-text-muted)" : accentText,
                        }}
                      >
                        {b.utilization === null ? "N/A" : `${b.utilization}%`}
                      </span>
                    </div>
                  );
                })}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
