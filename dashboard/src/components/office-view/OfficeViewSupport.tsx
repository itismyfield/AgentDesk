import { useEffect, useState } from "react";
import type { Agent, KanbanCard } from "../../types";
import AgentAvatar from "../AgentAvatar";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceNotice,
} from "../common/SurfacePrimitives";
import type {
  OfficeManualIntervention,
  OfficeSeatStatus,
} from "./officeAgentState";

function sortOfficeAgentsByStatus(
  agents: Agent[],
  manualInterventionByAgent: Map<string, OfficeManualIntervention>,
  seatStatusByAgent: Map<string, OfficeSeatStatus>,
): Agent[] {
  return [...agents].sort((a, b) => {
    const leftManual = manualInterventionByAgent.has(a.id) ? 0 : 1;
    const rightManual = manualInterventionByAgent.has(b.id) ? 0 : 1;
    if (leftManual !== rightManual) return leftManual - rightManual;

    const order: Record<OfficeSeatStatus, number> = {
      review: 0,
      working: 1,
      idle: 2,
      offline: 3,
    };
    const leftStatus = seatStatusByAgent.get(a.id) ?? "idle";
    const rightStatus = seatStatusByAgent.get(b.id) ?? "idle";
    const statusDiff = (order[leftStatus] ?? 9) - (order[rightStatus] ?? 9);
    if (statusDiff !== 0) return statusDiff;
    return (a.alias || a.name_ko || a.name).localeCompare(b.alias || b.name_ko || b.name);
  });
}

export function OfficeDesktopAgentAccessList({
  agents,
  isKo,
  onSelectAgent,
  manualInterventionByAgent,
  primaryCardByAgent,
  seatStatusByAgent,
}: {
  agents: Agent[];
  isKo: boolean;
  onSelectAgent?: (agent: Agent) => void;
  manualInterventionByAgent: Map<string, OfficeManualIntervention>;
  primaryCardByAgent: Map<string, KanbanCard>;
  seatStatusByAgent: Map<string, OfficeSeatStatus>;
}) {
  const sorted = sortOfficeAgentsByStatus(agents, manualInterventionByAgent, seatStatusByAgent);
  if (sorted.length === 0) return null;

  return (
    <div
      className="border-t px-4 py-3"
      style={{ borderColor: "color-mix(in srgb, var(--th-border) 62%, transparent)" }}
    >
      <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
        <span
          className="text-[11px] font-semibold uppercase tracking-[0.14em]"
          style={{ color: "var(--th-text-muted)" }}
        >
          {isKo ? "에이전트 상태" : "Agent Status"}
        </span>
        <span className="text-[11px]" style={{ color: "var(--th-text-faint)" }}>
          {isKo ? `${sorted.length}명` : `${sorted.length} agents`}
        </span>
      </div>
      <div className="grid gap-2 md:grid-cols-2 xl:grid-cols-3">
        {sorted.map((agent) => {
          const status = seatStatusByAgent.get(agent.id) ?? "idle";
          const statusMeta = getSeatStatusMeta(status, isKo);
          const manualIntervention = manualInterventionByAgent.get(agent.id) ?? null;
          const primaryCard = primaryCardByAgent.get(agent.id) ?? null;
          const agentLabel = agent.alias || agent.name_ko || agent.name;
          const detail = manualIntervention?.reason
            ? previewManualReason(manualIntervention.reason)
            : previewCardTitle(primaryCard?.title ?? null);
          return (
            <button
              key={agent.id}
              type="button"
              onClick={() => onSelectAgent?.(agent)}
              className="min-w-0 rounded-2xl border px-3 py-2.5 text-left transition-colors focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2"
              style={{
                background: manualIntervention
                  ? "color-mix(in srgb, var(--th-badge-amber-bg) 48%, var(--th-card-bg) 52%)"
                  : "color-mix(in srgb, var(--th-bg-surface) 88%, transparent)",
                borderColor: manualIntervention
                  ? "color-mix(in srgb, var(--th-accent-warn) 28%, var(--th-border) 72%)"
                  : "color-mix(in srgb, var(--th-border) 68%, transparent)",
                color: "var(--th-text-primary)",
                outlineColor: "var(--th-focus-ring)",
              }}
            >
              <span className="flex min-w-0 items-center gap-2">
                <AgentAvatar agent={agent} agents={agents} size={24} rounded="xl" />
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-sm font-semibold">{agentLabel}</span>
                  <span className="mt-1 flex flex-wrap items-center gap-1.5">
                    <span
                      className="inline-flex rounded-full px-2 py-0.5 text-[10px] font-medium"
                      style={{
                        background: statusMeta.background,
                        color: statusMeta.textColor,
                      }}
                    >
                      {statusMeta.label}
                    </span>
                    {manualIntervention ? (
                      <span
                        className="inline-flex rounded-full px-2 py-0.5 text-[10px] font-medium"
                        style={{
                          background: "color-mix(in srgb, var(--th-badge-amber-bg) 72%, transparent)",
                          color: "var(--th-accent-warn)",
                        }}
                      >
                        {isKo ? "수동 개입" : "Manual"}
                      </span>
                    ) : null}
                  </span>
                </span>
              </span>
              {detail ? (
                <span
                  className="mt-2 block truncate text-[11px]"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {detail}
                </span>
              ) : null}
            </button>
          );
        })}
      </div>
    </div>
  );
}

// ── Mobile Office Lite: agent status cards ──

export function getSeatStatusMeta(
  status: OfficeSeatStatus,
  isKo: boolean,
): { label: string; accent: string; textColor: string; background: string; border: string } {
  switch (status) {
    case "working":
      return {
        label: isKo ? "작업 중" : "Working",
        accent: "var(--th-accent-primary)",
        textColor: "var(--th-text-primary)",
        background: "color-mix(in srgb, var(--th-badge-emerald-bg) 62%, var(--th-card-bg) 38%)",
        border: "color-mix(in srgb, var(--th-accent-primary) 22%, var(--th-border) 78%)",
      };
    case "review":
      return {
        label: isKo ? "검토 중" : "In review",
        accent: "var(--th-accent-warn)",
        textColor: "var(--th-accent-warn)",
        background: "color-mix(in srgb, var(--th-badge-amber-bg) 62%, var(--th-card-bg) 38%)",
        border: "color-mix(in srgb, var(--th-accent-warn) 22%, var(--th-border) 78%)",
      };
    case "offline":
      return {
        label: isKo ? "오프라인" : "Offline",
        accent: "var(--th-text-muted)",
        textColor: "var(--th-text-muted)",
        background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)",
        border: "color-mix(in srgb, var(--th-border) 72%, transparent)",
      };
    case "idle":
    default:
      return {
        label: isKo ? "대기" : "Idle",
        accent: "var(--th-text-muted)",
        textColor: "var(--th-text-muted)",
        background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)",
        border: "color-mix(in srgb, var(--th-border) 72%, transparent)",
      };
  }
}

function previewManualReason(reason: string | null | undefined): string {
  if (!reason) return "";
  return reason.length > 72 ? `${reason.slice(0, 72)}…` : reason;
}

function previewCardTitle(title: string | null | undefined): string {
  if (!title) return "";
  return title.length > 52 ? `${title.slice(0, 52)}…` : title;
}

export function MobileAgentStatusGrid({
  agents,
  isKo,
  onSelectAgent,
  manualInterventionByAgent,
  primaryCardByAgent,
  seatStatusByAgent,
}: {
  agents: Agent[];
  isKo: boolean;
  onSelectAgent?: (agent: Agent) => void;
  manualInterventionByAgent: Map<string, OfficeManualIntervention>;
  primaryCardByAgent: Map<string, KanbanCard>;
  seatStatusByAgent: Map<string, OfficeSeatStatus>;
}) {
  const sorted = sortOfficeAgentsByStatus(agents, manualInterventionByAgent, seatStatusByAgent);

  const [expandedWarningAgentId, setExpandedWarningAgentId] = useState<string | null>(null);
  const manualCount = sorted.reduce(
    (count, agent) => count + (manualInterventionByAgent.has(agent.id) ? 1 : 0),
    0,
  );

  useEffect(() => {
    if (!expandedWarningAgentId) return;
    if (!manualInterventionByAgent.has(expandedWarningAgentId)) {
      setExpandedWarningAgentId(null);
    }
  }, [expandedWarningAgentId, manualInterventionByAgent]);

  return (
    /* User feedback: drop the gray SurfaceSubsection wrapper that
       previously wrapped this grid (title "에이전트 현황" + description
       + bordered card background). On mobile we now render the agent
       count + warning chip inline above the grid and let the grid sit
       directly on the page background. The redundant subtitle
       ("수동 개입, 좌석 상태, ... 카드로 빠르게 확인합니다.") is dropped
       since the cards themselves are the explanation. */
    <div className="mt-3 px-3 pb-6">
      <div className="flex flex-wrap items-center justify-between gap-2 px-1 pb-3">
        <span
          className="text-[11px] font-semibold uppercase tracking-[0.14em]"
          style={{ color: "var(--th-text-muted)" }}
        >
          {isKo ? `${sorted.length}명` : `${sorted.length} agents`}
        </span>
        {manualCount > 0 && (
          <span
            className="inline-flex items-center rounded-full border px-2.5 py-1 text-[10px] font-semibold uppercase tracking-[0.14em]"
            style={{
              borderColor: "color-mix(in srgb, var(--th-accent-warn) 24%, var(--th-border) 76%)",
              background: "color-mix(in srgb, var(--th-badge-amber-bg) 60%, var(--th-card-bg) 40%)",
              color: "var(--th-accent-warn)",
            }}
          >
            {isKo ? `경고 ${manualCount}` : `Warnings ${manualCount}`}
          </span>
        )}
      </div>
      {manualCount > 0 && (
        <SurfaceNotice tone="warn" compact className="mb-3">
          <div className="text-[11px] leading-5">
            {isKo
              ? `수동 개입이 필요한 에이전트 ${manualCount}명이 상단으로 정렬되어 있습니다.`
              : `${manualCount} agents with manual intervention are pinned to the top.`}
          </div>
        </SurfaceNotice>
      )}
        <div className="mt-4 grid grid-cols-1 gap-2.5 min-[520px]:grid-cols-2">
        {sorted.map((agent) => {
          const status = seatStatusByAgent.get(agent.id) ?? "idle";
          const statusMeta = getSeatStatusMeta(status, isKo);
          const manualIntervention = manualInterventionByAgent.get(agent.id) ?? null;
          const primaryCard = primaryCardByAgent.get(agent.id) ?? null;
          const agentLabel = agent.alias || agent.name_ko || agent.name;
          const sessionLabel =
            agent.session_info && agent.session_info !== statusMeta.label ? agent.session_info : null;
          const preview = manualIntervention?.reason
            ? previewManualReason(manualIntervention.reason)
            : previewCardTitle(primaryCard?.title ?? null);
          const isWarningExpanded = expandedWarningAgentId === agent.id;

          return (
            <SurfaceCard
              key={agent.id}
              className="rounded-[26px] px-3.5 py-3.5 text-left"
              style={{
                background: manualIntervention
                  ? "linear-gradient(180deg, color-mix(in srgb, var(--th-badge-amber-bg) 54%, var(--th-card-bg) 46%) 0%, color-mix(in srgb, var(--th-card-bg) 90%, transparent) 100%)"
                  : "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 95%, transparent) 100%)",
                borderColor: manualIntervention
                  ? "color-mix(in srgb, var(--th-accent-warn) 26%, var(--th-border) 74%)"
                  : "color-mix(in srgb, var(--th-border) 68%, transparent)",
              }}
            >
              <button type="button" onClick={() => onSelectAgent?.(agent)} className="w-full text-left">
                <div className="flex items-start justify-between gap-2">
                  <div className="flex min-w-0 items-start gap-2.5">
                    <span
                      className="inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-2xl border"
                      style={{
                        borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                        background: "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)",
                      }}
                    >
                      <AgentAvatar agent={agent} agents={agents} size={28} rounded="2xl" />
                    </span>
                    <div className="min-w-0">
                      <div className="truncate text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                        {agentLabel}
                      </div>
                      <div className="mt-1 flex flex-wrap items-center gap-1.5">
                        <span
                          className="inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[10px] font-medium"
                          style={{
                            color: statusMeta.textColor,
                            background: statusMeta.background,
                            border: `1px solid ${statusMeta.border}`,
                          }}
                        >
                          <span
                            className="h-1.5 w-1.5 rounded-full"
                            style={{ background: statusMeta.accent }}
                          />
                          {statusMeta.label}
                        </span>
                        {sessionLabel && (
                          <span
                            className="inline-flex items-center rounded-full border px-2 py-0.5 text-[10px] font-medium"
                            style={{
                              borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                              background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
                              color: "var(--th-text-muted)",
                            }}
                          >
                            <span className="truncate">{sessionLabel}</span>
                          </span>
                        )}
                      </div>
                    </div>
                  </div>
                  {manualIntervention && (
                    <span
                      className="shrink-0 rounded-full px-2 py-0.5 text-[10px] font-semibold"
                      style={{
                        color: "var(--th-accent-warn)",
                        background: "color-mix(in srgb, var(--th-badge-amber-bg) 72%, var(--th-card-bg) 28%)",
                      }}
                    >
                      {isKo ? "수동 개입" : "Manual"}
                    </span>
                  )}
                </div>
                {preview && (
                  <div className="mt-3 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                    {preview}
                  </div>
                )}
                {agent.department_name_ko && (
                  <div className="mt-2">
                    <span
                      className="inline-flex max-w-full items-center rounded-full px-2 py-0.5 text-[10px] font-medium"
                      style={{
                        color: statusMeta.textColor,
                        background: statusMeta.background,
                        border: `1px solid ${statusMeta.border}`,
                      }}
                    >
                      <span className="truncate">
                        {isKo ? agent.department_name_ko : (agent.department_name || agent.department_name_ko)}
                      </span>
                    </span>
                  </div>
                )}
              </button>
              {manualIntervention && (
                <div className="mt-3">
                  <SurfaceNotice
                    tone="warn"
                    compact
                    className="items-start rounded-[20px]"
                    action={(
                      <SurfaceActionButton
                        tone="warn"
                        compact
                        className="shrink-0 rounded-full"
                        onClick={(event) => {
                          event.stopPropagation();
                          setExpandedWarningAgentId((current) => (current === agent.id ? null : agent.id));
                        }}
                        aria-expanded={isWarningExpanded}
                      >
                      {isWarningExpanded
                        ? (isKo ? "접기" : "Hide")
                        : (isKo ? "사유 보기" : "Show reason")}
                      </SurfaceActionButton>
                    )}
                  >
                    <div className="min-w-0">
                      <div className="text-[10px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-accent-warn)" }}>
                        {isKo ? "경고" : "Warning"}
                      </div>
                      <div className="mt-1 text-[11px] font-semibold leading-5" style={{ color: "var(--th-text)" }}>
                        {manualIntervention.title}
                      </div>
                    </div>
                  </SurfaceNotice>
                  {isWarningExpanded && (
                    <SurfaceCard
                      className="mt-2 rounded-[20px] px-3 py-3"
                      style={{
                        borderColor: "color-mix(in srgb, var(--th-accent-warn) 22%, var(--th-border) 78%)",
                        background: "color-mix(in srgb, var(--th-badge-amber-bg) 62%, var(--th-card-bg) 38%)",
                      }}
                    >
                      <div
                        className="break-words whitespace-pre-wrap text-[11px] leading-5"
                        style={{ color: "var(--th-text-muted)" }}
                      >
                        {manualIntervention.reason
                          ?? (isKo
                            ? "구체 사유는 상세 패널에서 확인할 수 있습니다."
                            : "Open the detail panel to inspect the full reason.")}
                      </div>
                    </SurfaceCard>
                  )}
                </div>
              )}
            </SurfaceCard>
          );
        })}
      </div>
    </div>
  );
}
