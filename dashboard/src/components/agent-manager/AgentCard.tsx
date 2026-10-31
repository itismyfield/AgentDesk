import type { Agent, Department } from "../../types";
import { localeName } from "../../i18n";
import { getCurrentTaskSummary } from "../../lib/agentHelpers";
import { getProviderMeta } from "../../app/providerTheme";
import AgentPersona from "../AgentPersona";
import { ActionMenu } from "../common/ActionMenu";
import { SurfaceActionButton, SurfaceCard } from "../common/SurfacePrimitives";
import type { Translator } from "./types";

type AgentCardViewMode = "grid" | "list";

interface AgentCardProps {
  agent: Agent;
  locale: string;
  tr: Translator;
  departments: Department[];
  onOpen: () => void;
  onEdit: () => void;
  onDelete: () => Promise<void> | void;
  saving: boolean;
  topSkills: string[];
  viewMode: AgentCardViewMode;
}

export default function AgentCard({
  agent,
  locale,
  tr,
  departments,
  onOpen,
  onEdit,
  onDelete,
  saving,
  topSkills,
  viewMode,
}: AgentCardProps) {
  const dept = departments.find((d) => d.id === agent.department_id);
  const primaryLabel = localeName(locale, agent);
  const secondaryLabel = locale === "en" ? agent.name_ko || agent.name : agent.name;
  const providerMeta = getProviderMeta(agent.cli_provider);
  const task = getCurrentTaskSummary(agent, tr);

  return (
    <SurfaceCard
      data-testid={`agents-card-${agent.id}`}
      onClick={onOpen}
      onKeyDown={event => { if (event.target === event.currentTarget && (event.key === "Enter" || event.key === " ")) { event.preventDefault(); onOpen(); } }}
      role="button"
      tabIndex={0}
      aria-label={tr(`${primaryLabel} 상세 열기`, `Open ${primaryLabel} details`)}
      className={`group min-w-0 cursor-pointer p-4 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--th-accent-primary)] ${viewMode === "list" ? "grid items-start gap-4 sm:grid-cols-[minmax(0,1fr)_minmax(0,1.2fr)_auto]" : "space-y-4"}`}
      draggable={false}
      onDragStart={event => event.preventDefault()}
    >
      <div className="flex min-w-0 items-start gap-3">
        <AgentPersona agent={agent} size={40} />
        <div className="min-w-0">
          <div className="text-sm font-semibold leading-6 text-th-text-heading [overflow-wrap:anywhere]">{primaryLabel}</div>
          {secondaryLabel && secondaryLabel !== primaryLabel ? (
            <div className="text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">{secondaryLabel}</div>
          ) : null}
          <div className="mt-1 flex flex-wrap gap-x-3 gap-y-1 text-xs leading-5 text-th-text-muted">
            <span>{providerMeta.label}</span>
            <span>{dept ? localeName(locale, dept) : tr("부서 없음", "No department")}</span>
            <span className="tabular-nums">{tr(`완료 ${agent.stats_tasks_done.toLocaleString()}건`, `${agent.stats_tasks_done.toLocaleString()} done`)}</span>
          </div>
        </div>
      </div>
      <div className="min-w-0">
        <div className="text-xs text-th-text-muted">{task.label}</div>
        <div className="mt-1 text-sm leading-6 text-th-text-primary [overflow-wrap:anywhere]">{task.value}</div>
        <div className="mt-2 flex flex-wrap gap-x-3 gap-y-1 text-xs leading-5 text-th-text-muted">
          {topSkills.length ? topSkills.map(skill => <span key={`${agent.id}-${skill}`} className="[overflow-wrap:anywhere]">{skill}</span>) : <span>{tr("최근 스킬 데이터 없음", "No recent skill data")}</span>}
        </div>
      </div>
      <div className="flex shrink-0 flex-wrap items-center justify-end gap-2" onClick={event => event.stopPropagation()}>
        <SurfaceActionButton onClick={onEdit} tone="neutral" compact>{tr("편집", "Edit")}</SurfaceActionButton>
        <ActionMenu
          label={tr(`${primaryLabel} 메뉴`, `${primaryLabel} menu`)}
          items={[
            {
              key: "delete",
              label: tr("삭제", "Delete"),
              danger: true,
              disabled: saving || agent.status === "working",
              onSelect: onDelete,
              confirm: {
                title: tr(`${primaryLabel}을(를) 삭제할까요?`, `Delete ${primaryLabel}?`),
                description: tr("에이전트 설정이 지워지고 되돌릴 수 없어요.", "The agent's settings are removed for good."),
                confirmLabel: tr("삭제", "Delete"),
                cancelLabel: tr("취소", "Cancel"),
              },
            },
          ]}
        />
      </div>
    </SurfaceCard>
  );
}
