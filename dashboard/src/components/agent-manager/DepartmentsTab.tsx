import type { DragEvent } from "react";
import type { Agent, Department } from "../../types";
import { localeName } from "../../i18n";
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
    <div className="space-y-4">
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
          const agentCountForDept = agents.filter((agent) => agent.department_id === dept.id).length;
          const isDragging = draggingDeptId === dept.id;
          const isDragTarget = dragOverDeptId === dept.id && draggingDeptId !== dept.id;
          const showDropBefore = isDragTarget && dragOverPosition === "before";
          const showDropAfter = isDragTarget && dragOverPosition === "after";
          return (
            <SurfaceCard
              key={dept.id}
              className={`group relative flex items-center gap-3 px-4 py-3 transition-all hover:shadow-md ${isDragging ? "opacity-60" : ""}`}
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
                className="w-8 h-8 rounded-lg flex items-center justify-center text-sm font-bold"
                style={{ background: `${dept.color}22`, color: dept.color }}
              >
                {index + 1}
              </div>

              <span className="text-2xl">{dept.icon}</span>

              <div className="flex-1 min-w-0">
                <div className="flex items-center gap-2">
                  <span className="font-semibold text-sm" style={{ color: "var(--th-text-heading)" }}>
                    {localeName(locale, dept)}
                  </span>
                  <span className="w-3 h-3 rounded-full inline-block" style={{ background: dept.color }}></span>
                  <span
                    className="text-xs px-2 py-0.5 rounded-full"
                    style={{ background: `${dept.color}22`, color: dept.color }}
                  >
                    {agentCountForDept} {tr("명", "agents")}
                  </span>
                </div>
                {dept.description && (
                  <div className="text-xs mt-0.5 truncate" style={{ color: "var(--th-text-muted)" }}>
                    {dept.description}
                  </div>
                )}
              </div>

              <code className="text-xs px-2 py-0.5 rounded opacity-50" style={{ background: "var(--th-input-bg)" }}>
                {dept.id}
              </code>

              <SurfaceActionButton
                onClick={() => onEditDept(dept)}
                tone="neutral"
                className="opacity-0 group-hover:opacity-100"
              >
                {tr("편집", "Edit")}
              </SurfaceActionButton>
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
