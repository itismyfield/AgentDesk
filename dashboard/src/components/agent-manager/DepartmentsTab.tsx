import { useEffect, useRef, useState, type DragEvent } from "react";
import { ArrowDown, ArrowUp, Building2, ChevronRight, GripVertical, Plus } from "lucide-react";
import type { Department } from "../../types";
import { localeName } from "../../i18n";
import { Button } from "../../kit/free/button-clean";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import AgentPersona from "../AgentPersona";
import { Cap, RowButton, SectionCard } from "../common/KitPrimitives";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import type { HomeAgent } from "../home/homeModel";
import { PERSONA } from "./AgentRow";
import type { Translator } from "./types";

interface DepartmentsTabProps {
  tr: Translator;
  locale: string;
  /** The listed agents with home's states, as the agents tab counts them. */
  members: HomeAgent[];
  deptOrder: Department[];
  deptOrderDirty: boolean;
  reorderSaving: boolean;
  reorderFailed: boolean;
  draggingDeptId: string | null;
  dragOverDeptId: string | null;
  dragOverPosition: "before" | "after" | null;
  onSaveOrder: () => void;
  onCancelOrder: () => void;
  onMoveDept: (index: number, direction: -1 | 1) => void;
  onEditDept: (department: Department) => void;
  onCreateDept: () => void;
  onDragStart: (deptId: string, event: DragEvent<HTMLElement>) => void;
  onDragOver: (deptId: string, event: DragEvent<HTMLElement>) => void;
  onDrop: (deptId: string, event: DragEvent<HTMLElement>) => void;
  onDragEnd: () => void;
}

/** Departments as one list: a row opens the edit window; arrows (and dragging on desktop) change the order until it is saved. */
export default function DepartmentsTab({
  tr,
  locale,
  members,
  deptOrder,
  deptOrderDirty,
  reorderSaving,
  reorderFailed,
  draggingDeptId,
  dragOverDeptId,
  dragOverPosition,
  onSaveOrder,
  onCancelOrder,
  onMoveDept,
  onEditDept,
  onCreateDept,
  onDragStart,
  onDragOver,
  onDrop,
  onDragEnd,
}: DepartmentsTabProps) {
  const isMobile = useIsMobile();
  const listRef = useRef<HTMLOListElement>(null);
  // The moved row's arrow keeps the focus; at either end it passes to the other arrow.
  const [moved, setMoved] = useState<{ id: string; direction: -1 | 1 } | null>(null);
  useEffect(() => {
    if (!moved) return;
    const arrows = listRef.current?.querySelectorAll<HTMLButtonElement>(`[data-dept-move="${CSS.escape(moved.id)}"]`);
    const [up, down] = Array.from(arrows ?? []);
    const wanted = moved.direction < 0 ? up : down;
    (wanted && !wanted.disabled ? wanted : moved.direction < 0 ? down : up)?.focus();
    setMoved(null);
  }, [deptOrder, moved]);

  const addButton = (
    <Button variant="secondary" size={isMobile ? "lg" : "sm"} icon={<Plus size={14} aria-hidden />} onClick={onCreateDept}>
      {tr("부서 추가", "Add department")}
    </Button>
  );

  return (
    <div data-testid="agents-departments-tab" className="flex min-w-0 flex-col gap-3">
      {deptOrderDirty ? (
        <div
          role="status"
          data-testid="agents-departments-order-bar"
          className="sticky top-0 z-10 flex min-w-0 flex-wrap items-center gap-2 rounded-(--r-md) border border-(--line) bg-(--bg-2) px-3 py-2 shadow-(--shadow-sm) sm:px-4"
        >
          <span className="min-w-0 flex-1 text-sm text-(--fg)">
            {reorderFailed
              ? tr("순서를 저장하지 못했어요. 다시 저장해 보세요.", "Could not save the order. Try again.")
              : tr("순서를 바꿨어요. 저장해야 다른 화면에도 반영돼요.", "The order changed. Save it to apply everywhere.")}
          </span>
          <Button variant="secondary" size={isMobile ? "lg" : "sm"} onClick={onCancelOrder} disabled={reorderSaving}>
            {tr("되돌리기", "Undo")}
          </Button>
          <Button variant="primary" size={isMobile ? "lg" : "sm"} onClick={onSaveOrder} loading={reorderSaving}>
            {tr("순서 저장", "Save order")}
          </Button>
        </div>
      ) : null}

      <SectionCard title={tr("부서", "Departments")} count={deptOrder.length} aside={<span className="ml-auto">{addButton}</span>} testId="agents-departments">
        {deptOrder.length === 0 ? (
          <div className="border-t border-(--line-soft) p-3 sm:p-4">
            <EmptyState
              icon={<Building2 size={18} />}
              title={tr("아직 부서가 없어요", "No departments yet")}
              text={tr("부서를 만들면 에이전트를 묶어 볼 수 있어요.", "Add a department to group agents.")}
            />
          </div>
        ) : (
          <ol ref={listRef} className="@container m-0 min-w-0 list-none border-t border-(--line-soft) p-0">
            {deptOrder.map((dept, index) => {
              const name = localeName(locale, dept);
              const people = members.filter((entry) => entry.agent.department_id === dept.id);
              const target = dragOverDeptId === dept.id && draggingDeptId !== dept.id ? dragOverPosition : null;
              return (
                <li
                  key={dept.id}
                  data-testid={`agents-department-row-${dept.id}`}
                  draggable={!isMobile}
                  onDragStart={(event) => onDragStart(dept.id, event)}
                  onDragOver={(event) => onDragOver(dept.id, event)}
                  onDrop={(event) => onDrop(dept.id, event)}
                  onDragEnd={onDragEnd}
                  className={`relative flex min-w-0 items-center border-t border-(--line-soft) first:border-t-0 ${draggingDeptId === dept.id ? "opacity-50" : ""}`}
                >
                  {target ? (
                    <span aria-hidden className={`pointer-events-none absolute inset-x-3 h-0.5 rounded-full bg-(--accent) ${target === "before" ? "-top-px" : "-bottom-px"}`} />
                  ) : null}
                  {isMobile ? null : <GripVertical aria-hidden size={16} className="ml-3 shrink-0 cursor-grab text-(--fg-faint)" />}
                  <RowButton
                    className="min-w-0 flex-1 sm:pl-2"
                    aria-label={tr(`${name} 편집, ${people.length}명`, `Edit ${name}, ${people.length} agents`)}
                    onClick={() => onEditDept(dept)}
                  >
                    <span aria-hidden className="size-2.5 shrink-0 rounded-full" style={{ background: dept.color || "var(--fg-faint)" }} />
                    <span className="min-w-0 flex-1">
                      <span className="block truncate text-sm leading-5 font-medium text-(--fg)">{name}</span>
                      <Cap className="block truncate font-mono">
                        {dept.id} · {tr(`순서 ${index + 1}`, `order ${index + 1}`)}
                      </Cap>
                    </span>
                    {dept.description ? (
                      <Cap className="hidden min-w-0 flex-1 truncate @3xl:block">{dept.description}</Cap>
                    ) : null}
                    {people.length > 0 ? (
                      <span aria-hidden className="hidden shrink-0 items-center @md:flex">
                        {people.slice(0, 4).map((entry) => (
                          <AgentPersona key={entry.agent.id} agent={entry.agent} state={PERSONA[entry.state]} size={24} className="-ml-1.5 ring-2 ring-(--bg-2) first:ml-0" />
                        ))}
                      </span>
                    ) : null}
                    <Cap className="shrink-0 text-right font-display tabular-nums whitespace-nowrap">
                      {people.length > 0 ? tr(`${people.length}명`, `${people.length}`) : tr("소속 없음", "No agents")}
                    </Cap>
                    <ChevronRight aria-hidden size={15} className="shrink-0 text-(--fg-faint)" />
                  </RowButton>
                  <span className="flex shrink-0 items-center gap-1 pr-3 sm:pr-4">
                    <IconButton
                      data-dept-move={dept.id}
                      icon={<ArrowUp size={15} />}
                      label={tr(`${name} 위로`, `Move ${name} up`)}
                      disabled={index === 0}
                      onClick={() => {
                        onMoveDept(index, -1);
                        setMoved({ id: dept.id, direction: -1 });
                      }}
                    />
                    <IconButton
                      data-dept-move={dept.id}
                      icon={<ArrowDown size={15} />}
                      label={tr(`${name} 아래로`, `Move ${name} down`)}
                      disabled={index === deptOrder.length - 1}
                      onClick={() => {
                        onMoveDept(index, 1);
                        setMoved({ id: dept.id, direction: 1 });
                      }}
                    />
                  </span>
                </li>
              );
            })}
          </ol>
        )}
        {deptOrder.length > 1 ? (
          <Cap className="block border-t border-(--line-soft) px-3 py-2.5 sm:px-4">
            {isMobile
              ? tr("줄을 누르면 편집 창이 열려요. 화살표로 순서를 바꾼 뒤 저장해요.", "Tap a row to edit it. Use the arrows to reorder, then save.")
              : tr("줄을 누르면 편집 창이 열려요. 끌거나 화살표로 순서를 바꾼 뒤 저장해요.", "Click a row to edit it. Drag or use the arrows to reorder, then save.")}
          </Cap>
        ) : null}
      </SectionCard>
    </div>
  );
}
