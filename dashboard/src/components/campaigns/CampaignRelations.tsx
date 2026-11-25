import { ArrowLeft, ArrowRight, TriangleAlert } from "lucide-react";
import { useId, useMemo, useRef, useState, type ReactNode } from "react";
import type { Campaign, CampaignNode, CampaignNodeLive, CampaignNodeStatus } from "../../api/campaigns";
import { Badge as KitBadge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { Select } from "../../kit/free/select-clean";
import { HandoffSwimlanes, type HandoffItem, type HandoffLane, type HandoffStatus } from "../../kit/pro/handoff-swimlanes";
import { Cap, KitCard, MeterBar, Num, RowButton, Well } from "../common/KitPrimitives";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import CampaignGroupOverview, { buildCampaignGroupOverview, groupCycleText } from "./CampaignGroupOverview";
import CampaignGroupTasks from "./CampaignGroupTasks";
import { campaignRawEdges, rawEdgeId } from "./campaignGraphRouting";
import CampaignNeighborhood from "./CampaignNeighborhood";
import { campaignGroup, campaignTaskNumber, campaignTaskTitle, groupCampaignNodes } from "./campaignModel";
import { cleanBlocker, legacyTask } from "./campaignOverviewModel";
import type { Tr } from "./campaignPresentation";
import { loadCampaignView, storedEditorScopes } from "./campaignViewStorage";
import { keptDraft, type EditScope } from "./relationsEditorModel";
import { TaskGroupHeading } from "./TaskListParts";

type RelationsMode = "graph" | "swimlanes";
type SwimlaneScope = "active" | "all";

const SWIMLANE_STATUS: Record<CampaignNodeStatus, HandoffStatus> = {
  pending: "queued",
  running: "active",
  blocked: "waiting",
  completed: "done",
  failed: "failed",
  skipped: "skipped",
};

const SWIMLANE_COLORS = ["#0190fe", "#10b981", "#f59e0b", "#06b6d4", "#ec4899", "#3b82f6"];
const isActiveNode = (node: CampaignNode) => node.status !== "completed" && node.status !== "skipped";

/** The cable ties saved for this campaign's editor scopes, one line each: how many lines, from which tasks, and on the
 * second line the tasks they lead to. Members gone from the campaign are left out. */
function TieLines({ campaign, tr }: { campaign: Campaign; tr: Tr }) {
  const present = new Set(campaignRawEdges(campaign.nodes).map(rawEdgeId));
  const title = (id: string) => { const node = campaign.nodes.find((value) => value.id === id); return node ? campaignTaskTitle(node) : id; };
  const ties = storedEditorScopes(campaign.id).flatMap((scope) => loadCampaignView(scope).view.ties)
    .map((tie) => ({ id: tie.id, edges: tie.strandOrder.filter((edge) => present.has(rawEdgeId(edge))) })).filter((tie) => tie.edges.length);
  if (!ties.length) return null;
  return <ul aria-label={tr("케이블타이", "Cable ties")} className="m-0 grid list-none gap-1.5 p-0">{ties.map(({ id, edges }) => {
    const from = [...new Set(edges.map(([dep]) => dep))], to = [...new Set(edges.map(([, next]) => next))];
    const more = from.length - 1;
    return <li key={id}><Well className="flex items-center gap-3 px-3 py-2.5">
      <i aria-hidden className="size-4 shrink-0 rounded-full border-2 border-[var(--fg-muted)]" />
      <span className="grid min-w-0 flex-1 gap-0.5">
        <strong className="flex min-w-0 gap-1 text-sm font-medium text-[var(--fg)]"><span className="truncate">{tr(`묶음 ${edges.length} · ${title(from[0])}`, `Bundle of ${edges.length} · ${title(from[0])}`)}</span>
          {more > 0 && <>{" "}<span className="shrink-0">{tr(`외 ${more}`, `and ${more} more`)}</span></>}</strong>
        <Cap className="flex min-w-0 items-center gap-1"><ArrowRight aria-hidden className="size-3 shrink-0" /><span className="sr-only">{tr("이어지는 작업", "Leads to")}</span><span className="truncate">{to.map(title).join(" · ")}</span></Cap>
      </span>
      <Cap className="shrink-0">{tr("보기만", "View only")}</Cap>
    </Well></li>;
  })}</ul>;
}

/** Phones: each group as a row with its task count, completion and blocked count, under any loop among the groups. */
function GroupList({ campaign, live, onOpen, tr, children }: {
  campaign: Campaign; live?: Record<string, CampaignNodeLive>; onOpen: (group: string) => void; tr: Tr; children: ReactNode;
}) {
  const overview = useMemo(() => buildCampaignGroupOverview(campaign.nodes, undefined, live), [campaign.nodes, live]);
  return <div className="grid min-w-0 gap-3 p-3">
    {overview.cycles.map((path) => <Well key={path.join(">")} className="grid gap-0.5 bg-[var(--err-soft)] px-3 py-2.5">
      <strong className="flex items-center gap-1.5 text-sm text-[var(--err)]"><TriangleAlert aria-hidden className="size-4 shrink-0" /><span className="min-w-0 [overflow-wrap:anywhere]">{groupCycleText(overview, path, tr)}</span></strong>
      <Cap className="text-[var(--fg-dim)]">{tr("그룹끼리 서로 선행으로 이어져 있어요.", "These groups depend on each other in a loop.")}</Cap>
    </Well>)}
    <ul aria-label={tr("그룹 목록", "Groups")} className="m-0 grid list-none gap-0.5 p-0">{overview.groups.map((group) => {
      const { counts, percent, total } = group.progress;
      return <li key={group.id}><RowButton data-group={group.key} onClick={() => onOpen(group.key)} className="gap-3 rounded-[var(--r-sm)] px-3 py-2 sm:px-3">
        <span className="grid min-w-0 flex-1 gap-1.5">
          <span className="flex min-w-0 items-baseline gap-2"><strong className="min-w-0 truncate text-sm font-medium text-[var(--fg)]">{group.key || tr("미분류", "Ungrouped")}</strong><Cap className="shrink-0"><Num>{tr(`작업 ${total}개`, `${total} tasks`)}</Num></Cap></span>
          <MeterBar ratio={percent / 100} tone="ok" />
        </span>
        <span className="grid min-w-10 shrink-0 justify-items-end gap-0.5 text-xs"><Num className="text-[var(--fg)]">{percent}%</Num>{counts.blocked > 0 && <span className="text-[var(--err)]">{tr(`막힘 ${counts.blocked}`, `${counts.blocked} blocked`)}</span>}</span>
      </RowButton></li>;
    })}</ul>
    {children}
  </div>;
}

/** 관계도: switch between the spatial group/task DAG and cross-group HandoffSwimlanes while keeping group context intact. */
export default function CampaignRelations({ campaign, live, selectedId, onOpen, onEdit, tr }: {
  campaign: Campaign; live?: Record<string, CampaignNodeLive>; selectedId: string | null; onOpen: (id: string | null) => void; onEdit?: (scope: EditScope) => void; tr: Tr;
}) {
  const [viewMode, setViewMode] = useState<RelationsMode>("graph");
  const [chosen, setChosen] = useState<string | null>(null);
  const [group, setGroup] = useState<string | null>(null);
  const [task, setTask] = useState<string | null>(null);
  const [swimlaneChoice, setSwimlaneChoice] = useState<{ campaignId: string; scope: SwimlaneScope } | null>(null);
  const listId = useId();
  const root = useRef<HTMLDivElement>(null);
  const selected = campaign.nodes.find((node) => node.id === selectedId);
  const byId = useMemo(() => new Map(campaign.nodes.map((node) => [node.id, node])), [campaign.nodes]);
  const all = useMemo(() => new Set(campaign.nodes.map((node) => node.id)), [campaign.nodes]);
  const grouped = useMemo(() => groupCampaignNodes(campaign.nodes), [campaign.nodes]);
  const members = group === null ? [] : campaign.nodes.filter((node) => campaignGroup(node) === group);
  const groups = grouped.length;
  const kept = keptDraft(campaign.id) !== null;
  const phone = useIsMobile();
  const activeCount = useMemo(() => campaign.nodes.filter(isActiveNode).length, [campaign.nodes]);
  const defaultSwimlaneScope: SwimlaneScope =
    campaign.nodes.length > 24 && activeCount > 0 && activeCount < campaign.nodes.length ? "active" : "all";
  const swimlaneScope: SwimlaneScope =
    swimlaneChoice?.campaignId === campaign.id
      ? swimlaneChoice.scope === "active" && activeCount === 0
        ? "all"
        : swimlaneChoice.scope
      : defaultSwimlaneScope;
  const activeOnly = swimlaneScope === "active";

  // Keep group spatial context when inside a group instead of replacing the entire canvas on task selection.
  const inGroup = viewMode === "graph" && !phone && group !== null;
  const activeTaskInGroup = inGroup
    ? (selected && campaignGroup(selected) === group ? selected.id : task)
    : null;
  const editScope = (): EditScope => inGroup ? { kind: "group", key: group } : selected ? { kind: "task", id: selected.id, depth: 1 }
    : { kind: "group", key: chosen ?? group ?? campaignGroup(campaign.nodes.find((node) => (node.dependencies ?? []).length) ?? campaign.nodes[0]) };

  const focus = (selector: string) => requestAnimationFrame(() => requestAnimationFrame(() => root.current?.querySelector<HTMLElement>(selector)?.focus()));
  const openGroup = (key: string, id: string | null = null) => {
    setGroup(key); setChosen(key); setTask(id);
    if (!id) focus(phone ? "[data-relations-list] [aria-expanded]" : "[data-relations-back]");
  };
  const goto = (id: string) => { const node = byId.get(id); if (node) openGroup(campaignGroup(node), id); };
  const back = () => { const left = group; setGroup(null); setTask(null); if (left !== null) focus(`.react-flow__node[data-id="campaign-group:${CSS.escape(encodeURIComponent(left))}"]`); };
  const pick = (value: string) => {
    const node = byId.get(value);
    if (inGroup && node) {
      const targetGroup = campaignGroup(node);
      if (targetGroup !== group) setGroup(targetGroup);
      setTask(value);
    }
    onOpen(value || null);
  };
  const name = (key: string) => key || tr("미분류", "Ungrouped");

  const swimlanes = useMemo(() => {
    const visibleNodes = activeOnly ? campaign.nodes.filter(isActiveNode) : campaign.nodes;
    const visibleIds = new Set(visibleNodes.map((node) => node.id));
    const lanes: HandoffLane[] = grouped.flatMap((entry, index) => {
      const activeInLane = entry.nodes.filter(isActiveNode).length;
      if (activeOnly && activeInLane === 0) return [];
      return [{
        id: `group:${entry.key}`,
        name: entry.key || tr("미분류", "Ungrouped"),
        kind: "agent" as const,
        role: activeOnly
          ? tr(`활성 ${activeInLane}/${entry.progress.total}개 · ${entry.progress.percent}%`, `${activeInLane}/${entry.progress.total} active · ${entry.progress.percent}%`)
          : tr(`작업 ${entry.progress.total}개 · ${entry.progress.percent}%`, `${entry.progress.total} tasks · ${entry.progress.percent}%`),
        color: SWIMLANE_COLORS[index % SWIMLANE_COLORS.length],
      }];
    });
    const tasks: HandoffItem[] = visibleNodes.map((node) => {
      const num = campaignTaskNumber(node);
      const taskView = legacyTask(node, live?.[node.id]);
      const isBlocked = taskView.attention && !taskView.gate && taskView.status !== "failed";
      return {
        id: node.id,
        laneId: `group:${campaignGroup(node)}`,
        title: num ? `${num} ${campaignTaskTitle(node)}` : node.title,
        status: isBlocked ? "waiting" : SWIMLANE_STATUS[taskView.status],
        kind: isBlocked ? "gate" : "task",
        predecessors: (node.dependencies ?? []).filter((dep) => visibleIds.has(dep)),
        duration: node.stage || undefined,
        detail: cleanBlocker(node.blocker) ?? (node.summary?.trim() || undefined),
      };
    });
    return {
      lanes,
      tasks,
      hiddenGroups: grouped.length - lanes.length,
      hiddenTasks: campaign.nodes.length - visibleNodes.length,
    };
  }, [activeOnly, grouped, campaign.nodes, live, tr]);

  const swimlaneLabels = useMemo(() => ({
    progress: (done: number, total: number) => tr(`${done}/${total} 완료`, `${done} of ${total} done`),
    waiting: tr("막힌 작업 확인 필요", "Blocked tasks need attention"),
    approve: tr("상세 열기", "Open details"),
    reject: tr("닫기", "Dismiss"),
    close: tr("닫기", "Close"),
    empty: tr("표시할 작업이 없어요.", "No tasks to display."),
    loading: tr("스윔레인을 불러오는 중", "Loading swimlanes"),
    after: (titles: string) => tr(`선행: ${titles}`, `After ${titles}`),
    status: {
      queued: tr("대기", "Queued"),
      active: tr("진행 중", "Running"),
      waiting: tr("막힘", "Blocked"),
      done: tr("완료", "Done"),
      failed: tr("실패", "Failed"),
      skipped: tr("건너뜀", "Skipped"),
    },
    kind: {
      task: tr("작업", "Task"),
      gate: tr("막힘 게이트", "Blocked gate"),
      start: tr("시작", "Start"),
      end: tr("완료", "End"),
    },
  }), [tr]);

  return <div ref={root} className="campaign-relations grid min-w-0 gap-3">
    <div className="min-w-0 overflow-hidden rounded-[var(--r-md)] border border-[var(--line-soft)] bg-[var(--bg-1)]">
      <header className="flex min-h-14 min-w-0 flex-wrap items-center gap-x-3 gap-y-2 border-b border-[var(--line-soft)] px-4 py-2">
        {inGroup ? <div className="flex min-w-0 items-center gap-1">
          <IconButton data-relations-back icon={<ArrowLeft aria-hidden />} label={tr("전체 그룹으로 돌아가기", "Back to all groups")} onClick={back} />
          <nav aria-label={tr("이동 줄", "Breadcrumb")} className="min-w-0"><ol className="m-0 flex min-w-0 list-none items-center gap-1 p-0 text-[15px]">
            <li><Button variant="ghost" onClick={back}>{tr("전체 그룹", "All groups")}</Button></li>
            <li aria-hidden className="text-[var(--fg-muted)]">›</li>
            <li aria-current="page" className="min-w-0 truncate font-semibold text-[var(--fg)]">{name(group)}</li>
          </ol></nav>
          <Cap className="shrink-0 pl-2"><Num>{tr(`작업 ${members.length}개`, `${members.length} tasks`)}</Num></Cap>
        </div> : viewMode === "graph" && selected ? <div className="flex flex-wrap items-center gap-1.5">
          <Button variant="ghost" icon={<ArrowLeft aria-hidden className="size-4" />} onClick={() => onOpen(null)}>{tr("전체 그룹 관계", "All group connections")}</Button>
          {!phone && <Button variant="secondary" size="sm" onClick={() => openGroup(campaignGroup(selected), selected.id)}>{tr("그룹 안에서 보기 →", "Open in group →")}</Button>}
        </div> : <div className="grid min-w-0 gap-0.5">
          <strong className="truncate text-[15px] text-[var(--fg)]">{viewMode === "swimlanes" ? tr("그룹 스윔레인", "Group swimlanes") : tr("그룹 관계", "Group connections")}</strong>
          <Cap><Num>{tr(`그룹 ${groups}개 · 작업 ${campaign.nodes.length}개`, `${groups} groups · ${campaign.nodes.length} tasks`)}</Num></Cap>
        </div>}
        <div className="ml-auto flex min-w-0 flex-wrap items-center justify-end gap-2 max-sm:w-full">
          {onEdit && kept && <KitBadge size="md" tone="warning">{tr("저장하지 않은 편집이 있어요", "Unsaved edits are waiting")}</KitBadge>}
          {viewMode === "swimlanes" && (
            <div className="w-60 shrink-0">
              <SegmentedControl<SwimlaneScope>
                variant="tray"
                fullWidth
                size="sm"
                aria-label={tr("스윔레인 작업 필터", "Swimlane task filter")}
                value={swimlaneScope}
                onChange={(next) => setSwimlaneChoice({ campaignId: campaign.id, scope: next })}
                options={[
                  { value: "active", label: tr(`진행·막힘 (${activeCount})`, `Active (${activeCount})`), disabled: activeCount === 0 },
                  { value: "all", label: tr(`전체 (${campaign.nodes.length})`, `All (${campaign.nodes.length})`) },
                ]}
              />
            </div>
          )}
          <div className="w-40 shrink-0">
            <SegmentedControl<RelationsMode>
              variant="tray"
              fullWidth
              size="sm"
              aria-label={tr("관계도 보기 방식", "Connections view mode")}
              value={viewMode}
              onChange={setViewMode}
              options={[
                { value: "graph", label: tr("그래프", "Graph") },
                { value: "swimlanes", label: tr("스윔레인", "Swimlanes") },
              ]}
            />
          </div>
          <div className="campaign-node-picker min-w-0 flex-1 sm:w-64 sm:flex-none">
            <Select aria-label={tr("연결을 볼 작업", "Task to inspect")} size="lg" placeholder={tr("작업을 선택하세요", "Select a task")} value={selectedId}
              options={campaign.nodes.map((node) => ({ value: node.id, label: node.title }))} onChange={pick} />
          </div>
          {onEdit && <Button variant="secondary" size="lg" onClick={() => onEdit(editScope())}>{kept ? tr("이어서 편집", "Continue editing") : tr("편집", "Edit")}</Button>}
        </div>
      </header>
      {viewMode === "swimlanes" ? (
        <div className="overflow-x-auto p-3 sm:p-4">
          {activeOnly && swimlanes.hiddenTasks > 0 && (
            <p role="status" data-testid="swimlanes-filter-caption" className="m-0 pb-2.5 text-xs text-[var(--fg-muted)]">
              <Num>
                {swimlanes.hiddenGroups > 0
                  ? tr(
                      `완료·건너뜀 그룹 ${swimlanes.hiddenGroups}개 · 작업 ${swimlanes.hiddenTasks}개를 숨겼어요.`,
                      `Hiding ${swimlanes.hiddenGroups} completed/skipped groups and ${swimlanes.hiddenTasks} tasks.`,
                    )
                  : tr(
                      `완료·건너뜀 작업 ${swimlanes.hiddenTasks}개를 숨겼어요.`,
                      `Hiding ${swimlanes.hiddenTasks} completed/skipped tasks.`,
                    )}
              </Num>
            </p>
          )}
          <HandoffSwimlanes
            title={tr("그룹 간 작업 인계 흐름", "Cross-group task handoffs")}
            lanes={swimlanes.lanes}
            tasks={swimlanes.tasks}
            labels={swimlaneLabels}
            onSelect={onOpen}
            onApprove={(id) => onOpen(id)}
            style={{ width: "100%", maxWidth: "none" }}
          />
        </div>
      ) : selected && !inGroup ? (
        <div className="p-4"><CampaignNeighborhood nodes={campaign.nodes} selectedId={selected.id} visibleIds={all} onSelect={onOpen} tr={tr} /></div>
      ) : phone ? (
        <GroupList campaign={campaign} live={live} onOpen={openGroup} tr={tr}><TieLines campaign={campaign} tr={tr} /></GroupList>
      ) : inGroup ? (
        <CampaignGroupTasks key={group} campaign={campaign} live={live} group={group} chosen={activeTaskInGroup} onChoose={setTask} onGoto={goto} onGotoGroup={(key) => openGroup(key)}
          onOpenTask={(id) => { setTask(id); onOpen(id); }} tr={tr} />
      ) : (
        <CampaignGroupOverview campaignId={campaign.id} nodes={campaign.nodes} live={live} chosen={chosen} onChoose={setChosen} onOpenGroup={openGroup} onOpenTask={goto} tr={tr} />
      )}
    </div>
    {phone && viewMode === "graph" && group !== null && <div data-relations-list><KitCard pad="none">
      <div className="grid min-w-0 gap-0.5 p-1.5">
        <TaskGroupHeading open controls={listId} label={name(group)} count={members.length} onToggle={() => setGroup(null)} />
        <div id={listId} className="grid min-w-0 gap-0.5">{members.map((node) => {
          const after = campaign.nodes.filter((other) => (other.dependencies ?? []).includes(node.id)).length, before = (node.dependencies ?? []).filter((id) => byId.has(id)).length;
          return <RowButton key={node.id} data-node-id={node.id} data-task-id={node.id} selected={node.id === selectedId} onClick={() => onOpen(node.id)}
            className="campaign-task-row grid gap-0.5 rounded-[var(--r-sm)] py-2 text-[13px]"><span className="min-w-0 truncate">{node.title}</span>
            <Cap><Num>{tr(`선행 ${before} · 후행 ${after}`, `${before} before · ${after} after`)}</Num></Cap></RowButton>;
        })}</div>
      </div>
    </KitCard></div>}
  </div>;
}
