import { useRef } from "react";
import type { Agent, Department } from "../../types";
import { localeName } from "../../i18n";
import { getProviderMeta } from "../../app/providerTheme";
import { getCurrentTaskSummary } from "../../lib/agentHelpers";
import AgentPersona from "../AgentPersona";
import { ActionMenu } from "../common/ActionMenu";
import { SurfaceActionButton, SurfaceCard } from "../common/SurfacePrimitives";
import type { Translator } from "./types";

interface AgentsGridViewProps {
  agents: Agent[];
  departments: Department[];
  locale: string;
  onDeleteAgent: (agentId: string) => void;
  onDuplicateAgent: (agent: Agent) => void;
  onEditAgent: (agent: Agent) => void;
  onEditDepartment: (department: Department) => void;
  onOpenAgent: (agent: Agent) => void;
  saving: boolean;
  selectedAgent: Agent | null;
  selectedAgentId: string | null;
  setSelectedAgentId: (id: string | null) => void;
  topSkillsByAgent: Map<string, string[]>;
  tr: Translator;
}

// Second line only when it adds something beyond the primary name.
function agentSecondaryLine(agent: Agent, locale: string): string | null {
  if (agent.alias?.trim()) return `aka ${agent.alias.trim()}`;
  const other = locale === "en" ? agent.name_ko || agent.name : agent.name;
  return other && other !== localeName(locale, agent) ? other : null;
}

function statusLabel(status: Agent["status"], tr: Translator): string {
  if (status === "working") return tr("작업 중", "Working");
  if (status === "offline") return tr("오프라인", "Offline");
  if (status === "break") return tr("휴식", "Break");
  return tr("대기", "Idle");
}

export function AgentsGridView({
  agents: sortedAgents,
  departments,
  locale,
  onDeleteAgent,
  onDuplicateAgent,
  onEditAgent,
  onEditDepartment,
  onOpenAgent,
  saving,
  selectedAgent,
  selectedAgentId,
  setSelectedAgentId,
  topSkillsByAgent,
  tr,
}: AgentsGridViewProps) {
  const selectionPanelRef = useRef<HTMLDivElement>(null);

  return (
<div
  data-testid="agents-view-grid"
  className={selectedAgent ? "grid min-w-0 items-start gap-4 xl:grid-cols-[minmax(0,1.3fr)_minmax(320px,0.9fr)]" : "grid min-w-0 gap-3"}
>
  <div className={`grid min-w-0 grid-cols-1 gap-3 ${selectedAgent ? "" : "sm:grid-cols-2 2xl:grid-cols-3"}`}>
    {sortedAgents.map((agent) => {
      const department = departments.find((candidate) => candidate.id === agent.department_id);
      const providerMeta = getProviderMeta(agent.cli_provider);
      const secondary = agentSecondaryLine(agent, locale);
      const isSelected = agent.id === selectedAgentId;

      return (
        <button
          key={agent.id}
          type="button"
          data-testid={`agents-grid-card-${agent.id}`}
          onClick={() => {
            setSelectedAgentId(agent.id);
            requestAnimationFrame(() => {
              if (window.matchMedia("(max-width: 1279px)").matches) {
                selectionPanelRef.current?.scrollIntoView({ block: "start" });
              }
            });
          }}
          aria-pressed={isSelected}
          className={`min-w-0 rounded-lg border p-4 text-left transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--th-accent-primary)] ${isSelected ? "border-[var(--th-accent-primary)] bg-surface-subtle" : "border-th-border-subtle bg-th-bg-surface hover:bg-surface-subtle"}`}
        >
          <div className="flex items-start gap-3">
            <AgentPersona agent={agent} size={40} />

            <div className="min-w-0 flex-1">
              <div className="text-sm font-semibold leading-6 [overflow-wrap:anywhere]" style={{ color: "var(--th-text-heading)" }}>
                {localeName(locale, agent)}
              </div>
              {secondary ? (
                <div className="text-xs leading-5 [overflow-wrap:anywhere]" style={{ color: "var(--th-text-muted)" }}>
                  {secondary}
                </div>
              ) : null}
            </div>

            <span
              className="shrink-0 text-xs leading-6"
              style={{
                borderColor: "transparent",
                background: "transparent",
                color: "var(--th-text-secondary)",
              }}
            >
              {providerMeta.label}
            </span>
          </div>

          <div className="mt-3 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-th-text-muted">
            <span>{statusLabel(agent.status, tr)}</span>
            {department ? (
              <span
                className="text-xs leading-5"
                style={{
                  color: "var(--th-text-muted)",
                  borderColor: "transparent",
                  background: "transparent",
                }}
                onDoubleClick={(event) => {
                  event.preventDefault();
                  event.stopPropagation();
                  onEditDepartment(department);
                }}
                title={tr("더블클릭: 부서 편집", "Double-click: edit dept")}
              >
                {department.icon} {localeName(locale, department)}
              </span>
            ) : null}
          </div>
        </button>
      );
    })}
  </div>

  {selectedAgent ? (
    <div ref={selectionPanelRef} className="order-first min-w-0 scroll-mt-4 xl:order-none xl:sticky xl:top-0">
    <SurfaceCard data-testid="agents-selection-panel" className="min-w-0 p-4 sm:p-5">
      {(() => {
        const department = departments.find((candidate) => candidate.id === selectedAgent.department_id);
        const providerMeta = getProviderMeta(selectedAgent.cli_provider);
        const selectedSecondary = agentSecondaryLine(selectedAgent, locale);
        const selectedName = localeName(locale, selectedAgent);
        const task = getCurrentTaskSummary(selectedAgent, tr);
        const roleKey = selectedAgent.role_id || selectedAgent.id;
        const topSkills = topSkillsByAgent.get(roleKey) ?? [];

        return (
          <div className="space-y-5">
            <div className="flex items-start gap-3">
              <AgentPersona agent={selectedAgent} size={48} />
              <div className="min-w-0 flex-1">
                <div className="truncate text-base font-semibold" style={{ color: "var(--th-text-heading)" }}>
                  {selectedName}
                </div>
                <div className="text-xs leading-5 [overflow-wrap:anywhere]" style={{ color: "var(--th-text-muted)" }}>
                  {[selectedSecondary, providerMeta.label].filter(Boolean).join(" · ")}
                </div>
              </div>
              <SurfaceActionButton tone="neutral" compact onClick={() => setSelectedAgentId(null)} aria-label={tr("선택 닫기", "Close selection")}>×</SurfaceActionButton>
            </div>

            <div className="grid gap-2">
              {[
                {
                  label: tr("상태", "Status"),
                  value: statusLabel(selectedAgent.status, tr),
                },
                {
                  label: tr("부서", "Department"),
                  value: department ? [department.icon, localeName(locale, department)].filter(Boolean).join(" ") : tr("부서 없음", "No department"),
                },
                {
                  label: tr("완료한 작업", "Tasks done"),
                  value: selectedAgent.stats_tasks_done.toLocaleString(),
                },
              ].map((item) => (
                <div
                  key={item.label}
                  className="flex items-center justify-between gap-3 border-b border-th-border-subtle py-2"
                  style={{
                    borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                    background: "color-mix(in srgb, var(--th-bg-surface) 82%, transparent)",
                  }}
                >
                  <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                    {item.label}
                  </span>
                  <span className="text-xs text-right" style={{ color: "var(--th-text-primary)" }}>
                    {item.value}
                  </span>
                </div>
              ))}
            </div>

            <div
              className="border-t border-th-border-subtle py-4"
              style={{
                borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                background: "color-mix(in srgb, var(--th-bg-surface) 84%, transparent)",
              }}
            >
              <div
                className="text-[11px] font-semibold uppercase tracking-[0.16em]"
                style={{ color: "var(--th-text-muted)" }}
              >
                {task.label}
              </div>
              <div className="mt-2 text-sm leading-6 [overflow-wrap:anywhere]" style={{ color: "var(--th-text-primary)" }}>
                {task.value}
              </div>
            </div>

            <div className="space-y-2">
              <div className="text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
                {tr("최근 스킬", "Recent Skills")}
              </div>
              <div className="flex flex-wrap gap-2">
                {topSkills.length > 0 ? (
                  topSkills.map((skill) => (
                    <span
                      key={`${selectedAgent.id}-${skill}`}
                      className="text-xs leading-5"
                      style={{
                        borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
                        background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
                        color: "var(--th-text-secondary)",
                      }}
                    >
                      {skill}
                    </span>
                  ))
                ) : (
                  <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                    {tr("최근 스킬 데이터 없음", "No recent skill data")}
                  </span>
                )}
              </div>
            </div>

            <div className="flex flex-wrap items-center gap-2">
              <SurfaceActionButton onClick={() => onOpenAgent(selectedAgent)}>
                {tr("상세 보기", "Open details")}
              </SurfaceActionButton>
              <SurfaceActionButton onClick={() => onEditAgent(selectedAgent)} tone="neutral">
                {tr("편집", "Edit")}
              </SurfaceActionButton>
              <ActionMenu
                label={tr(`${selectedName} 메뉴`, `${selectedName} menu`)}
                items={[
                  { key: "duplicate", label: tr("복제", "Duplicate"), onSelect: () => onDuplicateAgent(selectedAgent) },
                  {
                    key: "delete",
                    label: tr("삭제", "Delete"),
                    danger: true,
                    disabled: saving || selectedAgent.status === "working",
                    onSelect: () => onDeleteAgent(selectedAgent.id),
                    confirm: {
                      title: tr(`${selectedName}을(를) 삭제할까요?`, `Delete ${selectedName}?`),
                      description: tr("에이전트 설정이 지워지고 되돌릴 수 없어요.", "The agent's settings are removed for good."),
                      confirmLabel: tr("삭제", "Delete"),
                      cancelLabel: tr("취소", "Cancel"),
                    },
                  },
                ]}
              />
            </div>
          </div>
        );
      })()}
    </SurfaceCard>
    </div>
  ) : (
    <p className="text-xs leading-5 text-th-text-muted">{tr("에이전트를 고르면 상세 정보와 편집 메뉴가 보여요.", "Select an agent to see details and actions.")}</p>
  )}
</div>
  );
}
