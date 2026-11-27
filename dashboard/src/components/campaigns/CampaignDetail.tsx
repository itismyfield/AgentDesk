import { X } from "lucide-react";
import { useState, type ReactNode } from "react";
import { IconButton } from "../../kit/free/icon-button-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { Drawer } from "../common/overlay/Drawer";
import { DrawerDockContext } from "../common/overlay/DrawerDock";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import { CAMPAIGN_SCOPE, type Tr } from "./campaignPresentation";
import type { CampaignDetailView } from "./serverCampaignModel";
import { DetailDockPane, useDetailDock } from "./TaskListParts";
import { TaskScreen } from "./TaskScreen";

/** The open campaign: title, chips and one status sentence, then 현황 (with 관계도, 단계 보드 and 파이프라인 when given), and the task
 * opened from it in the pinned pane on wide screens, a slide-over drawer on medium screens, or its own screen on phones. */
export default function CampaignDetail({ title, showTitle, chips, sentence, tracker, actions, notes, description, overview, relations, board, pipeline, view: viewProp, onViewChange, task, onCloseTask, leave, focus = false, tr }: {
  title: string; showTitle: boolean; chips?: ReactNode; sentence: string; tracker?: ReactNode; actions?: ReactNode; notes?: ReactNode;
  /** In the header on wide screens; below everything on phones, which lead with status and tasks. */
  description?: ReactNode;
  overview: (controls: string | undefined) => ReactNode; relations?: ReactNode; board?: (controls: string | undefined) => ReactNode; pipeline?: ReactNode;
  view?: CampaignDetailView; onViewChange?: (view: CampaignDetailView) => void;
  task: { id: string; label: string; body: ReactNode } | null; onCloseTask: () => void;
  /** Asks before the view changes, while an editor below holds unsaved changes. */
  leave?: (proceed: () => void) => void;
  /** The pipeline's editor has the window: only it stays, so nothing behind it takes the focus. */
  focus?: boolean; tr: Tr;
}) {
  const [innerView, setInnerView] = useState<CampaignDetailView>("overview");
  const requested = viewProp ?? innerView;
  const view: CampaignDetailView =
    (requested === "relations" && !relations) ||
    (requested === "board" && !board) ||
    (requested === "pipeline" && !pipeline)
      ? "overview"
      : requested;
  const changeView = (next: CampaignDetailView) => {
    if (viewProp === undefined) setInnerView(next);
    onViewChange?.(next);
  };
  const phone = useIsMobile();
  const dock = useDetailDock();
  const controls = dock.dock ? dock.id : undefined;
  // Closing returns focus to the row or card that opened the task, when it is still on screen.
  const close = () => {
    const opener = task && [...(dock.root.current?.querySelectorAll<HTMLElement>("[data-task-id]") ?? [])].find((element) => element.dataset.taskId === task.id);
    onCloseTask();
    opener?.focus({ preventScroll: true });
  };
  return <div ref={dock.root} className="campaign-detail grid min-w-0 content-start gap-4" style={task ? dock.rootStyle : undefined}>
    {!focus && <header className="grid min-w-0 gap-1.5">
      {showTitle || chips ? (
        <>
          <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-2">
            {showTitle && <h2 className="campaign-detail-title min-w-0 !text-2xl !font-semibold tracking-tight text-[var(--fg)] [overflow-wrap:anywhere]">{title}</h2>}
            {chips}
            {actions && <div className="campaign-actions ml-auto">{actions}</div>}
          </div>
          <p className="campaign-detail-status !text-sm text-[var(--fg)] [word-break:keep-all]">{sentence}</p>
        </>
      ) : (
        <div className="flex min-w-0 flex-wrap items-center justify-between gap-x-3 gap-y-2">
          <p className="campaign-detail-status m-0 min-w-[12rem] flex-1 !text-sm text-[var(--fg)] [word-break:keep-all]">{sentence}</p>
          {actions && <div className="campaign-actions shrink-0">{actions}</div>}
        </div>
      )}
      {tracker}
      {showTitle && description}
      {notes}
    </header>}
    {!focus && (relations || board || pipeline) && <div className="w-full sm:max-w-[30rem]">
      <SegmentedControl<CampaignDetailView> kind="tabs" variant="tray" fullWidth size="sm" aria-label={tr("캠페인 보기", "Campaign view")} value={view} onChange={(next) => leave ? leave(() => changeView(next)) : changeView(next)}
        options={[{ value: "overview", label: tr("현황", "Overview") }, ...relations ? [{ value: "relations" as const, label: tr("관계도", "Connections") }] : [],
          ...board ? [{ value: "board" as const, label: tr("단계 보드", "Stage board") }] : [], ...pipeline ? [{ value: "pipeline" as const, label: tr("파이프라인", "Pipeline") }] : []]} />
    </div>}
    {relations && view === "relations" ? relations : board && view === "board" ? board(controls) : pipeline && view === "pipeline" ? pipeline : overview(controls)}
    {!showTitle && !focus && description}
    {task && !focus && <DetailDockPane dock={dock} label={tr("선택 작업", "Selected task")} empty={null} />}
    {task && !focus && phone && <TaskScreen campaign={title} label={task.label} onClose={onCloseTask} tr={tr}>{task.body}</TaskScreen>}
    <DrawerDockContext.Provider value={dock.dock}>
      {task && !focus && !phone && <Drawer open onClose={close} ariaLabel={tr("선택 작업", "Selected task")} width="min(520px, 100vw)">
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
