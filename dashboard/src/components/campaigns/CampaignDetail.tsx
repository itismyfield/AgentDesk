import { X } from "lucide-react";
import { useState, type ReactNode } from "react";
import { IconButton } from "../../kit/free/icon-button-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { Drawer } from "../common/overlay/Drawer";
import { DrawerDockContext } from "../common/overlay/DrawerDock";
import { CAMPAIGN_SCOPE, type Tr } from "./campaignPresentation";
import { DetailDockPane, useDetailDock } from "./TaskListParts";
import { TaskScreen } from "./TaskScreen";

type View = "overview" | "relations" | "board";

/** The open campaign: title, chips and one status sentence, then 현황 (with 관계도 and 단계 보드 when given), and the task
 * opened from it in the pinned pane on wide screens or its own screen on phones. */
export default function CampaignDetail({ title, showTitle, chips, sentence, actions, notes, description, overview, relations, board, task, onCloseTask, tr }: {
  title: string; showTitle: boolean; chips?: ReactNode; sentence: string; actions?: ReactNode; notes?: ReactNode;
  /** In the header on wide screens; below everything on phones, which lead with status and tasks. */
  description?: ReactNode;
  overview: (controls: string | undefined) => ReactNode; relations?: ReactNode; board?: (controls: string | undefined) => ReactNode;
  task: { id: string; label: string; body: ReactNode } | null; onCloseTask: () => void; tr: Tr;
}) {
  const [view, setView] = useState<View>("overview");
  const dock = useDetailDock();
  const controls = dock.dock ? dock.id : undefined;
  // Closing returns focus to the row or card that opened the task, when it is still on screen.
  const close = () => {
    const opener = task && [...(dock.root.current?.querySelectorAll<HTMLElement>("[data-task-id]") ?? [])].find((element) => element.dataset.taskId === task.id);
    onCloseTask();
    opener?.focus({ preventScroll: true });
  };
  return <div ref={dock.root} className="campaign-detail grid min-w-0 content-start gap-4" style={task ? dock.rootStyle : undefined}>
    <header className="grid min-w-0 gap-1.5">
      {(showTitle || chips || actions) && <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-2">
        {showTitle && <h2 className="campaign-detail-title min-w-0 !text-2xl !font-semibold tracking-tight text-[var(--fg)] [overflow-wrap:anywhere]">{title}</h2>}
        {chips}
        {actions && <div className="campaign-actions ml-auto">{actions}</div>}
      </div>}
      <p className="campaign-detail-status !text-sm text-[var(--fg)] [word-break:keep-all]">{sentence}</p>
      {showTitle && description}
      {notes}
    </header>
    {(relations || board) && <div className="w-full sm:max-w-96">
      <SegmentedControl<View> kind="tabs" variant="tray" fullWidth size="sm" aria-label={tr("캠페인 보기", "Campaign view")} value={view} onChange={setView}
        options={[{ value: "overview", label: tr("현황", "Overview") }, ...relations ? [{ value: "relations" as const, label: tr("관계도", "Connections") }] : [],
          ...board ? [{ value: "board" as const, label: tr("단계 보드", "Stage board") }] : []]} />
    </div>}
    {relations && view === "relations" ? relations : board && view === "board" ? board(controls) : overview(controls)}
    {!showTitle && description}
    {task && <DetailDockPane dock={dock} label={tr("선택 작업", "Selected task")} empty={null} />}
    {task && !dock.dock && <TaskScreen campaign={title} label={task.label} onClose={onCloseTask} tr={tr}>{task.body}</TaskScreen>}
    <DrawerDockContext.Provider value={dock.dock}>
      {task && dock.dock && <Drawer open onClose={close} ariaLabel={tr("선택 작업", "Selected task")} width="min(560px, 100vw)">
        <div className={`${CAMPAIGN_SCOPE} campaign-inspector min-w-0 gap-3 text-sm [overflow-wrap:anywhere]`}>
          <div className="campaign-inspector-header flex items-center justify-between gap-3 border-b border-[var(--line-soft)] pb-2 text-xs text-[var(--fg-muted)]">
            <span className="tabular-nums">{task.label}</span>
            <IconButton icon={<X aria-hidden />} label={tr("상세 닫기", "Close details")} onClick={close} />
          </div>
          {task.body}
        </div>
      </Drawer>}
    </DrawerDockContext.Provider>
  </div>;
}
