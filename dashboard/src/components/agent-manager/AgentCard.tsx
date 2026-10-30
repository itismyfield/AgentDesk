import type { Agent, Department } from "../../types";
import { localeName } from "../../i18n";
import { getCurrentTaskSummary } from "../../lib/agentHelpers";
import { getFontFamilyForText } from "../../lib/fonts";
import { getProviderMeta } from "../../app/providerTheme";
import AgentAvatar from "../AgentAvatar";
import { SurfaceActionButton, SurfaceCard } from "../common/SurfacePrimitives";
import { STATUS_DOT } from "./constants";
import { getAgentLevel, getAgentTitle } from "./agentProgress";
import type { Translator } from "./types";

type AgentCardViewMode = "grid" | "list";

interface AgentCardProps {
  agent: Agent;
  spriteMap: Map<string, number>;
  isKo: boolean;
  locale: string;
  tr: Translator;
  departments: Department[];
  onOpen: () => void;
  onEdit: () => void;
  confirmDeleteId: string | null;
  onDeleteClick: () => void;
  onDeleteConfirm: () => void;
  onDeleteCancel: () => void;
  saving: boolean;
  topSkills: string[];
  viewMode: AgentCardViewMode;
}

export default function AgentCard({
  agent,
  spriteMap,
  isKo,
  locale,
  tr,
  departments,
  onOpen,
  onEdit,
  confirmDeleteId,
  onDeleteClick,
  onDeleteConfirm,
  onDeleteCancel,
  saving,
  topSkills,
  viewMode,
}: AgentCardProps) {
  const isDeleting = confirmDeleteId === agent.id;
  const dept = departments.find((d) => d.id === agent.department_id);
  const primaryLabel = localeName(locale, agent);
  const providerMeta = getProviderMeta(agent.cli_provider);
  const levelInfo = getAgentLevel(agent.stats_xp);
  const levelTitle = getAgentTitle(agent.stats_xp, isKo);
  const task = getCurrentTaskSummary(agent, tr);
  const contentFont = getFontFamilyForText(task.value, "sans");

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
        <div className="relative shrink-0">
          <AgentAvatar agent={agent} spriteMap={spriteMap} size={40} rounded="xl" />
          <span className={`absolute -bottom-0.5 -right-0.5 h-3 w-3 rounded-full border-2 border-th-card-bg ${STATUS_DOT[agent.status] ?? STATUS_DOT.idle}`} aria-hidden="true" />
        </div>
        <div className="min-w-0">
          <div className="text-sm font-semibold leading-6 text-th-text-heading [overflow-wrap:anywhere]">{primaryLabel}</div>
          <div className="text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">{locale === "en" ? agent.name_ko || agent.name : agent.name}</div>
          <div className="mt-1 flex flex-wrap gap-x-3 gap-y-1 text-xs leading-5 text-th-text-muted">
            <span>{providerMeta.label}</span>
            <span>Lv.{levelInfo.level} {levelTitle}</span>
            <span>{dept ? localeName(locale, dept) : tr("미배정", "Unassigned")}</span>
          </div>
          <div className="mt-1 text-xs leading-5 tabular-nums text-th-text-muted">XP {agent.stats_xp.toLocaleString()} · {tr("완료", "Done")} {agent.stats_tasks_done.toLocaleString()}</div>
        </div>
      </div>
      <div className="min-w-0">
        <div className="text-xs text-th-text-muted">{task.label}</div>
        <div className="mt-1 text-sm leading-6 text-th-text-primary [overflow-wrap:anywhere]" style={{fontFamily:contentFont}}>{task.value}</div>
        <div className="mt-2 flex flex-wrap gap-x-3 gap-y-1 text-xs leading-5 text-th-text-muted">
          {topSkills.length ? topSkills.map(skill => <span key={`${agent.id}-${skill}`} className="[overflow-wrap:anywhere]">{skill}</span>) : <span>{tr("최근 스킬 데이터 없음", "No recent skill data")}</span>}
        </div>
      </div>
      <div className="flex shrink-0 flex-wrap items-center justify-end gap-2" onClick={event => event.stopPropagation()}>
        <SurfaceActionButton onClick={onEdit} tone="neutral" compact>{tr("편집", "Edit")}</SurfaceActionButton>
        {isDeleting ? <>
          <SurfaceActionButton onClick={onDeleteConfirm} disabled={saving || agent.status === "working"} tone="danger" compact>{tr("해고", "Fire")}</SurfaceActionButton>
          <SurfaceActionButton onClick={onDeleteCancel} tone="neutral" compact>{tr("취소", "No")}</SurfaceActionButton>
        </> : <SurfaceActionButton onClick={onDeleteClick} tone="neutral" compact>{tr("삭제", "Delete")}</SurfaceActionButton>}
      </div>
    </SurfaceCard>
  );
}
