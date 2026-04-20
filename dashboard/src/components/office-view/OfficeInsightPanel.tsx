import { useState, useEffect, type ReactNode } from "react";
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
  SurfaceCard,
  SurfaceListItem,
  SurfaceMetricPill,
  SurfaceNotice,
  SurfaceSection,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";

const SURFACE_FIELD_STYLE = {
  background: "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)",
  borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
} as const;

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
  notifications,
  auditLogs,
  kanbanCards,
  onNavigateToKanban,
  isKo,
  onSelectAgent,
  selectedAgent = null,
  onClearSelectedAgent,
  activeMeeting = null,
  manualInterventionByAgent,
  primaryCardByAgent,
  seatStatusByAgent,
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
  const workingCount = Array.from(seatStatusByAgent?.values() ?? []).filter((status) => status === "working").length;
  const meetingCount = activeMeeting ? 1 : 0;
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
  const sectionEyebrow = isKo ? "오피스 상황판" : "Office pulse";
  const sectionTitle = isKo ? "오피스 운영 신호" : "Office operations";
  const sectionDescription = isKo
    ? "리뷰, 완료, 열린 이슈, 경고 에이전트를 같은 표면에서 빠르게 확인합니다."
    : "Review, closed issues, open work, and warning agents from one surface.";
  const selectedCard = selectedAgent ? primaryCardByAgent?.get(selectedAgent.id) ?? null : null;
  const selectedManual = selectedAgent ? manualInterventionByAgent?.get(selectedAgent.id) ?? null : null;
  const selectedSeatStatus = selectedAgent
    ? seatStatusByAgent?.get(selectedAgent.id) ?? inferSeatStatus(selectedAgent)
    : null;
  const rootClassName = docked
    ? "relative z-20 flex w-full flex-col gap-3 pointer-events-auto"
    : "relative z-20 mb-3 flex flex-col gap-3 px-3 pt-3 pointer-events-auto sm:absolute sm:left-auto sm:right-3 sm:top-3 sm:mb-0 sm:w-[min(22rem,calc(100vw-1.5rem))] sm:px-0 sm:pt-0";
  const heroSection = selectedAgent ? (
    <SelectedAgentCard
      agent={selectedAgent}
      seatStatus={selectedSeatStatus}
      selectedCard={selectedCard}
      selectedManual={selectedManual}
      activeMeeting={activeMeeting}
      isKo={isKo}
      onNavigateToKanban={onNavigateToKanban}
      onClear={onClearSelectedAgent}
    />
  ) : (
    <OfficeSummaryCard
      reviewCount={reviewCount}
      openIssueCount={openIssueCount}
      warningCount={warningCount}
      workingCount={workingCount}
      meetingCount={meetingCount}
      isKo={isKo}
    />
  );
  const providerHealthCard = <ProviderHealthCard isKo={isKo} />;
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
      {heroSection}
      {providerHealthCard}

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

function inferSeatStatus(agent: Agent): OfficeSeatStatus {
  if (agent.status === "offline") return "offline";
  if (agent.status === "working") return "working";
  return "idle";
}

function seatStatusLabel(status: OfficeSeatStatus | null, isKo: boolean): string {
  switch (status) {
    case "working":
      return isKo ? "작업 중" : "Working";
    case "review":
      return isKo ? "리뷰 중" : "Review";
    case "offline":
      return isKo ? "오프라인" : "Offline";
    case "idle":
    default:
      return isKo ? "대기" : "Idle";
  }
}

function statusTone(status: OfficeSeatStatus | null): "neutral" | "info" | "success" | "warn" {
  switch (status) {
    case "working":
      return "success";
    case "review":
      return "warn";
    case "offline":
      return "neutral";
    case "idle":
    default:
      return "info";
  }
}

function formatCompactNumber(value: number): string {
  if (value >= 1_000_000_000) return `${(value / 1_000_000_000).toFixed(1)}B`;
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(1)}K`;
  return String(value);
}

function SelectedAgentCard({
  agent,
  seatStatus,
  selectedCard,
  selectedManual,
  activeMeeting,
  isKo,
  onNavigateToKanban,
  onClear,
}: {
  agent: Agent;
  seatStatus: OfficeSeatStatus | null;
  selectedCard: KanbanCard | null;
  selectedManual: OfficeManualIntervention | null;
  activeMeeting: RoundTableMeeting | null;
  isKo: boolean;
  onNavigateToKanban?: () => void;
  onClear?: () => void;
}) {
  const provider = getProviderMeta(agent.cli_provider ?? null);
  const displayName = agent.alias || agent.name_ko || agent.name;
  const departmentName = agent.department_name_ko || agent.department_name || (isKo ? "미배정" : "Unassigned");
  const currentTask = selectedCard?.title || agent.session_info || (isKo ? "현재 작업 없음" : "No active task");

  return (
    <SurfaceSection
      eyebrow={isKo ? "선택된 에이전트" : "Selected agent"}
      title={displayName}
      description={`${departmentName} · ${agent.role_id ?? provider.label}`}
      className="rounded-[24px] p-4"
      actions={(
        <div className="flex items-center gap-2">
          <SurfaceMetricPill
            label={isKo ? "상태" : "Status"}
            value={seatStatusLabel(seatStatus, isKo)}
            tone={statusTone(seatStatus)}
          />
          <SurfaceActionButton tone="neutral" compact onClick={onClear}>
            {isKo ? "닫기" : "Close"}
          </SurfaceActionButton>
        </div>
      )}
      style={{
        borderColor: `color-mix(in srgb, ${provider.color} 22%, var(--th-border) 78%)`,
        background:
          `linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, ${provider.bg} 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 97%, transparent) 100%)`,
      }}
    >
      <div className="mt-2 flex items-start gap-3">
        <AgentAvatar agent={agent} size={44} rounded="2xl" />
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <span
              className="inline-flex items-center rounded-full border px-2.5 py-1 text-[11px] font-medium"
              style={{
                borderColor: provider.border,
                background: provider.bg,
                color: provider.color,
              }}
            >
              {provider.label}
            </span>
            <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
              {agent.stats_tokens ? `${formatCompactNumber(agent.stats_tokens)} tokens` : (isKo ? "토큰 집계 대기" : "Token metrics pending")}
            </span>
          </div>
          <div className="mt-3 rounded-2xl border px-3 py-3" style={SURFACE_FIELD_STYLE}>
            <div className="text-[11px] uppercase tracking-[0.18em]" style={{ color: "var(--th-text-faint)" }}>
              {isKo ? "지금 작업" : "Current work"}
            </div>
            <div className="mt-1 text-sm leading-6" style={{ color: "var(--th-text-primary)" }}>
              {currentTask}
            </div>
            {selectedCard?.github_issue_number ? (
              <div className="mt-2 text-xs" style={{ color: "var(--th-text-muted)" }}>
                #{selectedCard.github_issue_number} · {selectedCard.github_repo ?? "GitHub"}
              </div>
            ) : null}
          </div>
        </div>
      </div>

      <div className="mt-3 grid grid-cols-2 gap-2">
        <SurfaceMetricPill
          label={isKo ? "XP" : "XP"}
          value={formatCompactNumber(agent.stats_xp)}
          tone="accent"
        />
        <SurfaceMetricPill
          label={isKo ? "작업 수" : "Tasks"}
          value={String(agent.stats_tasks_done)}
          tone="info"
        />
      </div>

      {selectedManual ? (
        <SurfaceNotice className="mt-3" tone="warn">
          <div className="text-xs font-medium">
            {isKo ? "수동 개입 필요" : "Manual intervention required"}
          </div>
          <div className="mt-1 text-xs leading-relaxed">
            {selectedManual.reason || selectedManual.title}
          </div>
        </SurfaceNotice>
      ) : null}

      {activeMeeting ? (
        <SurfaceNotice className="mt-3" tone="info">
          <div className="text-xs font-medium">
            {isKo ? "회의 진행 중" : "Meeting in progress"}
          </div>
          <div className="mt-1 text-xs leading-relaxed">
            {activeMeeting.agenda}
          </div>
        </SurfaceNotice>
      ) : null}

      <div className="mt-3 flex flex-wrap gap-2">
        {selectedCard && onNavigateToKanban ? (
          <SurfaceActionButton compact onClick={onNavigateToKanban}>
            {isKo ? "칸반 열기" : "Open Kanban"}
          </SurfaceActionButton>
        ) : null}
      </div>
    </SurfaceSection>
  );
}

function OfficeSummaryCard({
  reviewCount,
  openIssueCount,
  warningCount,
  workingCount,
  meetingCount,
  isKo,
}: {
  reviewCount: number;
  openIssueCount: number;
  warningCount: number;
  workingCount: number;
  meetingCount: number;
  isKo: boolean;
}) {
  return (
    <SurfaceSection
      eyebrow={isKo ? "오피스" : "Office"}
      title={isKo ? "오피스 운영 신호" : "Office pulse"}
      description={isKo ? "선택된 에이전트가 없을 때 전체 상황을 먼저 보여줍니다." : "Show the room summary when no agent is selected."}
      className="rounded-[24px] p-4"
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-primary) 14%, var(--th-border) 86%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, var(--th-accent-primary-soft) 5%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      <div className="mt-1 grid grid-cols-2 gap-2">
        <SurfaceMetricPill label={isKo ? "작업 중" : "Working"} value={String(workingCount)} tone="success" />
        <SurfaceMetricPill label={isKo ? "리뷰" : "Review"} value={String(reviewCount)} tone="warn" />
        <SurfaceMetricPill label={isKo ? "열린 이슈" : "Open"} value={String(openIssueCount)} tone="info" />
        <SurfaceMetricPill label={isKo ? "경고" : "Warnings"} value={String(warningCount)} tone={warningCount > 0 ? "warn" : "neutral"} />
      </div>
      <div className="mt-3 rounded-2xl border px-3 py-3" style={SURFACE_FIELD_STYLE}>
        <div className="text-[11px] uppercase tracking-[0.18em]" style={{ color: "var(--th-text-faint)" }}>
          {isKo ? "회의 상태" : "Meeting status"}
        </div>
        <div className="mt-1 text-sm" style={{ color: "var(--th-text-primary)" }}>
          {meetingCount > 0 ? (isKo ? `${meetingCount}개 진행 중` : `${meetingCount} active`) : (isKo ? "진행 중인 회의 없음" : "No active meeting")}
        </div>
      </div>
    </SurfaceSection>
  );
}

function ProviderHealthCard({ isKo }: { isKo: boolean }) {
  return (
    <SurfaceSubsection
      title={isKo ? "프로바이더 상태" : "Provider health"}
      description={isKo ? "버킷 사용량과 stale 상태를 같은 톤으로 확인합니다." : "Check bucket utilization and stale telemetry in one place."}
      className="rounded-[24px]"
    >
      <MiniRateLimitBar isKo={isKo} />
    </SurfaceSubsection>
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
    <SurfaceSubsection
      title={title}
      className="rounded-[24px]"
      actions={(
        <span
          className="inline-flex items-center rounded-full border px-2.5 py-1 text-[11px] font-medium"
          style={{
            borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
            background: "color-mix(in srgb, var(--th-card-bg) 82%, transparent)",
            color: "var(--th-text-muted)",
          }}
        >
          {count}
        </span>
      )}
    >
      {children}
    </SurfaceSubsection>
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
