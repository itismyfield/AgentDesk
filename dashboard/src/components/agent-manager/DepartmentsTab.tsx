import type { DragEvent } from "react";
import type { Agent, Department } from "../../types";
import { localeName } from "../../i18n";
import AgentAvatar from "../AgentAvatar";
import { SurfaceActionButton, SurfaceCard, SurfaceEmptyState, SurfaceNotice } from "../common/SurfacePrimitives";
import type { Translator } from "./types";

interface DepartmentsTabProps {
  tr: Translator;
  locale: string;
  agents: Agent[];
  departments: Department[];
  deptOrder: Department[];
  deptOrderDirty: boolean;
  reorderSaving: boolean;
  draggingDeptId: string | null;
  dragOverDeptId: string | null;
  dragOverPosition: "before" | "after" | null;
  onSaveOrder: () => void;
  onCancelOrder: () => void;
  onMoveDept: (index: number, direction: -1 | 1) => void;
  onEditDept: (department: Department) => void;
  onDragStart: (deptId: string, event: DragEvent<HTMLDivElement>) => void;
  onDragOver: (deptId: string, event: DragEvent<HTMLDivElement>) => void;
  onDrop: (deptId: string, event: DragEvent<HTMLDivElement>) => void;
  onDragEnd: () => void;
}

export default function DepartmentsTab({
  tr,
  locale,
  agents,
  departments,
  deptOrder,
  deptOrderDirty,
  reorderSaving,
  draggingDeptId,
  dragOverDeptId,
  dragOverPosition,
  onSaveOrder,
  onCancelOrder,
  onMoveDept,
  onEditDept,
  onDragStart,
  onDragOver,
  onDrop,
  onDragEnd,
}: DepartmentsTabProps) {
  return (
    <div data-testid="agents-departments-tab" className="space-y-4">
      {deptOrderDirty && (
        <SurfaceNotice
          tone="info"
          className="flex flex-col gap-3 sm:flex-row sm:items-center"
          action={(
            <div className="flex flex-wrap items-center gap-2">
              <SurfaceActionButton onClick={onSaveOrder} disabled={reorderSaving}>
                {reorderSaving ? tr("저장 중...", "Saving...") : tr("순번 저장", "Save Order")}
              </SurfaceActionButton>
              <SurfaceActionButton tone="neutral" onClick={onCancelOrder}>
                {tr("취소", "Cancel")}
              </SurfaceActionButton>
            </div>
          )}
        >
          <span className="text-sm" style={{ color: "var(--th-text-primary)" }}>
            {tr("순번이 변경되었습니다.", "Order has been changed.")}
          </span>
        </SurfaceNotice>
      )}

      <div className="space-y-2">
        {deptOrder.map((dept, index) => {
          const members = agents
            .filter((agent) => agent.department_id === dept.id)
            .sort((left, right) => {
              const leftWorking = left.status === "working" ? 0 : 1;
              const rightWorking = right.status === "working" ? 0 : 1;
              if (leftWorking !== rightWorking) return leftWorking - rightWorking;
              return left.name.localeCompare(right.name);
            });
          const agentCountForDept = members.length;
          const isDragging = draggingDeptId === dept.id;
          const isDragTarget = dragOverDeptId === dept.id && draggingDeptId !== dept.id;
          const showDropBefore = isDragTarget && dragOverPosition === "before";
          const showDropAfter = isDragTarget && dragOverPosition === "after";
          return (
            <SurfaceCard
              data-testid={`agents-department-card-${dept.id}`}
              key={dept.id}
              className={`group relative px-4 py-4 transition-all hover:shadow-md ${isDragging ? "opacity-60" : ""}`}
              style={{ cursor: "grab" }}
              onDragStart={(e) => onDragStart(dept.id, e)}
              onDragOver={(e) => onDragOver(dept.id, e)}
              onDrop={(e) => onDrop(dept.id, e)}
              onDragEnd={onDragEnd}
              // SurfaceCard renders a div, so drag attributes stay here.
              // eslint-disable-next-line react/no-unknown-property
              draggable
            >
              {showDropBefore && (
                <div className="pointer-events-none absolute left-2 right-2 top-0 h-0.5 rounded bg-blue-400" />
              )}
              {showDropAfter && (
                <div className="pointer-events-none absolute left-2 right-2 bottom-0 h-0.5 rounded bg-blue-400" />
              )}

              <div className="flex flex-col gap-4 lg:flex-row lg:items-start">
                <div className="flex items-start gap-3">
                  <div className="flex flex-col gap-0.5">
                    <SurfaceActionButton
                      onClick={() => onMoveDept(index, -1)}
                      disabled={index === 0}
                      tone="neutral"
                      compact
                      className="h-5 w-6 px-0 py-0"
                    >
                      ▲
                    </SurfaceActionButton>
                    <SurfaceActionButton
                      onClick={() => onMoveDept(index, 1)}
                      disabled={index === deptOrder.length - 1}
                      tone="neutral"
                      compact
                      className="h-5 w-6 px-0 py-0"
                    >
                      ▼
                    </SurfaceActionButton>
                  </div>

                  <div
                    className="flex h-9 w-9 items-center justify-center rounded-xl text-sm font-bold"
                    style={{ background: `${dept.color}22`, color: dept.color }}
                  >
                    {index + 1}
                  </div>

                  <span className="pt-1 text-2xl">{dept.icon}</span>

                  <div className="min-w-0 flex-1">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                        {localeName(locale, dept)}
                      </span>
                      <span className="inline-block h-3 w-3 rounded-full" style={{ background: dept.color }}></span>
                      <span
                        className="rounded-full px-2 py-0.5 text-xs"
                        style={{ background: `${dept.color}22`, color: dept.color }}
                      >
                        {agentCountForDept} {tr("명", "agents")}
                      </span>
                    </div>
                    {dept.description && (
                      <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                        {dept.description}
                      </div>
                    )}
                  </div>
                </div>

                <div className="min-w-0 flex-1">
                  <div className="flex flex-wrap items-center gap-2">
                    {members.slice(0, 4).map((member) => (
                      <div
                        key={member.id}
                        className="inline-flex min-w-0 items-center gap-2 rounded-full border px-2 py-1"
                        style={{
                          borderColor:
                            "color-mix(in srgb, var(--th-border) 68%, transparent)",
                          background:
                            "color-mix(in srgb, var(--th-bg-surface) 82%, var(--th-card-bg) 18%)",
                        }}
                      >
                        <AgentAvatar agent={member} size={24} rounded="xl" />
                        <span className="max-w-[7rem] truncate text-xs" style={{ color: "var(--th-text-primary)" }}>
                          {localeName(locale, member)}
                        </span>
                      </div>
                    ))}
                    {members.length > 4 ? (
                      <span
                        className="rounded-full border px-2 py-1 text-xs"
                        style={{
                          borderColor:
                            "color-mix(in srgb, var(--th-border) 68%, transparent)",
                          color: "var(--th-text-muted)",
                        }}
                      >
                        +{members.length - 4}
                      </span>
                    ) : null}
                    {members.length === 0 ? (
                      <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                        {tr("소속 에이전트 없음", "No agents assigned")}
                      </span>
                    ) : null}
                  </div>
                </div>

                <div className="flex flex-wrap items-center justify-between gap-3 border-t pt-3" style={{ borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)" }}>
                  <code className="rounded px-2 py-0.5 text-xs opacity-60" style={{ background: "var(--th-input-bg)" }}>
                    {dept.id}
                  </code>

                  <SurfaceActionButton
                    onClick={() => onEditDept(dept)}
                    tone="neutral"
                    className="opacity-100 lg:opacity-0 lg:group-hover:opacity-100"
                  >
                    {tr("편집", "Edit")}
                  </SurfaceActionButton>
                </div>
              </div>
            </SurfaceCard>
          );
        })}
      </div>

      {deptOrder.length === 0 && (
        <SurfaceEmptyState className="py-16 text-center">
          <div className="text-3xl mb-2">🏢</div>
          {tr("등록된 부서가 없습니다.", "No departments found.")}
        </SurfaceEmptyState>
      )}
    </div>
  );
}
