import { useState, useEffect } from "react";
import type { Notification } from "../NotificationCenter";
import type { Agent, AuditLogEntry, KanbanCard, RoundTableMeeting } from "../../types";
import { getAgentWarnings } from "../../agent-insights";
import { getFontFamilyForText } from "../../lib/fonts";
import AgentAvatar from "../AgentAvatar";
import type {
  OfficeManualIntervention,
  OfficeSeatStatus,
} from "./officeAgentState";
import { MiniRateLimitBar } from "./MiniRateLimitBar";
import {
  SurfaceActionButton,
  SurfaceListItem,
  SurfaceMetricPill,
  SurfaceNotice,
  SurfaceSection,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";

export {
  MiniRateLimitBar,
  normalizeMiniRateLimitProviderLabel,
  transformRLProviders,
} from "./MiniRateLimitBar";

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
