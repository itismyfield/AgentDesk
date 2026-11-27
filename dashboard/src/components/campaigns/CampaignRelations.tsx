import { ArrowLeft, ArrowRight, Route, TriangleAlert } from "lucide-react";
import { useId, useMemo, useRef, useState, type ReactNode } from "react";
import type { Campaign, CampaignNode, CampaignNodeLive, CampaignNodeStatus } from "../../api/campaigns";
import { Badge as KitBadge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { Select } from "../../kit/free/select-clean";
import { HandoffSwimlanes, type HandoffItem, type HandoffLane, type HandoffStatus } from "../../kit/pro/handoff-swimlanes";
import {
  GanttTimeline,
  computeTaskWaves,
  type GanttGroup,
  type GanttTask,
  type GanttTeam,
  type GanttTimelineLabels,
} from "../../kit/pro/gantt-timeline";
import { Cap, Chip, KitCard, MeterBar, Num, RowButton, Well } from "../common/KitPrimitives";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import { compareCampaignCriticalPath, computeCampaignCriticalPath, type CriticalPathMode } from "./campaignCriticalPath";
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

type RelationsMode = "timeline" | "graph" | "swimlanes";
type SwimlaneScope = "active" | "all";

const GANTT_STATUS_TEAMS: GanttTeam[] = [
  { id: "done", name: "Done", tone: "green" },
  { id: "running", name: "Running", tone: "sky" },
  { id: "blocked", name: "Blocked", tone: "amber" },
  { id: "failed", name: "Failed", tone: "rose" },
  { id: "pending", name: "Pending", tone: "teal" },
];

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
const cssEscape = (id: string) =>
  typeof CSS !== "undefined" && typeof CSS.escape === "function" ? CSS.escape(id) : id.replace(/([^\w-])/g, "\\$1");

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
      const upstream = overview.connections.filter((line) => line.target === group.id).length;
      const downstream = overview.connections.filter((line) => line.source === group.id).length;
      return <li key={group.id}><RowButton data-group={group.key} onClick={() => onOpen(group.key)} className="gap-3 rounded-[var(--r-sm)] px-3 py-2 sm:px-3">
        <span className="grid min-w-0 flex-1 gap-1.5">
          <span className="flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-0.5"><strong className="min-w-0 truncate text-sm font-medium text-[var(--fg)]">{group.key || tr("미분류", "Ungrouped")}</strong><Cap className="shrink-0"><Num>{tr(`작업 ${total}개`, `${total} tasks`)}</Num></Cap><Cap className="shrink-0"><Num>{tr(`선행 그룹 ${upstream} · 다음 그룹 ${downstream}`, `${upstream} upstream · ${downstream} downstream`)}</Num></Cap></span>
          <MeterBar ratio={percent / 100} tone="ok" />
        </span>
        <span className="grid min-w-10 shrink-0 justify-items-end gap-0.5 text-xs"><Num className="text-[var(--fg)]">{percent}%</Num>{counts.blocked > 0 && <span className="text-[var(--err)]">{tr(`막힘 ${counts.blocked}`, `${counts.blocked} blocked`)}</span>}</span>
      </RowButton></li>;
    })}</ul>
    {children}
  </div>;
}

/** 관계도: switch between Wave GanttTimeline, spatial group/task DAG, and cross-group HandoffSwimlanes while keeping group context intact. */
export default function CampaignRelations({ campaign, live, selectedId, onOpen, onEdit, tr }: {
  campaign: Campaign; live?: Record<string, CampaignNodeLive>; selectedId: string | null; onOpen: (id: string | null) => void; onEdit?: (scope: EditScope) => void; tr: Tr;
}) {
  const phone = useIsMobile();
  const [modeChoice, setModeChoice] = useState<RelationsMode | null>(null);
  const viewMode: RelationsMode = modeChoice ?? (phone ? "timeline" : "graph");
  const setViewMode = (next: RelationsMode) => setModeChoice(next);
  const [chosen, setChosen] = useState<string | null>(null);
  const [group, setGroup] = useState<string | null>(null);
  const [task, setTask] = useState<string | null>(null);
  const [swimlaneChoice, setSwimlaneChoice] = useState<{ campaignId: string; scope: SwimlaneScope } | null>(null);
  const [phoneGraphLayout, setPhoneGraphLayout] = useState<"canvas" | "list">("canvas");
  const [criticalOpen, setCriticalOpen] = useState(false);
  const [criticalModeChoice, setCriticalModeChoice] = useState<{ campaignId: string; mode: CriticalPathMode } | null>(null);
  const [whatIfState, setWhatIfState] = useState<{ campaignId: string; deltas: Record<string, number> } | null>(null);
  const listId = useId();
  const root = useRef<HTMLDivElement>(null);
  const selected = campaign.nodes.find((node) => node.id === selectedId);
  const byId = useMemo(() => new Map(campaign.nodes.map((node) => [node.id, node])), [campaign.nodes]);
  const all = useMemo(() => new Set(campaign.nodes.map((node) => node.id)), [campaign.nodes]);
  const grouped = useMemo(() => groupCampaignNodes(campaign.nodes), [campaign.nodes]);
  const members = group === null ? [] : campaign.nodes.filter((node) => campaignGroup(node) === group);
  const groups = grouped.length;
  const kept = keptDraft(campaign.id) !== null;
  const phoneListMode = phone && phoneGraphLayout === "list";
  const activeCount = useMemo(() => campaign.nodes.filter(isActiveNode).length, [campaign.nodes]);
  const hasCompletedTasks = activeCount < campaign.nodes.length;
  const defaultCriticalMode: CriticalPathMode = activeCount > 0 && hasCompletedTasks ? "remaining" : "structural";
  const criticalMode: CriticalPathMode =
    criticalModeChoice?.campaignId === campaign.id ? criticalModeChoice.mode : defaultCriticalMode;
  const whatIfDeltas = useMemo(
    () => (whatIfState?.campaignId === campaign.id ? whatIfState.deltas : {}),
    [whatIfState, campaign.id],
  );
  const comparison = useMemo(
    () => compareCampaignCriticalPath(campaign.nodes, criticalMode, whatIfDeltas),
    [campaign.nodes, criticalMode, whatIfDeltas],
  );
  const criticalActive = criticalOpen || comparison.hasWhatIf;

  const adjustWhatIf = (targetId: string, step: number) => {
    setCriticalOpen(true);
    setWhatIfState((prev) => {
      const base = prev?.campaignId === campaign.id ? prev.deltas : {};
      const cur = base[targetId] ?? 0;
      const baseDur = comparison.baseline.nodes.get(targetId)?.baseDuration ?? 1;
      const nextDelta = Math.max(-baseDur, Math.min(6, cur + step));
      const next = { ...base };
      if (nextDelta === 0) delete next[targetId];
      else next[targetId] = nextDelta;
      return { campaignId: campaign.id, deltas: next };
    });
  };
  const resetWhatIf = () => setWhatIfState({ campaignId: campaign.id, deltas: {} });

  const defaultSwimlaneScope: SwimlaneScope =
    campaign.nodes.length > 24 && activeCount > 0 && activeCount < campaign.nodes.length ? "active" : "all";
  const swimlaneScope: SwimlaneScope =
    swimlaneChoice?.campaignId === campaign.id
      ? swimlaneChoice.scope === "active" && activeCount === 0
        ? "all"
        : swimlaneChoice.scope
      : defaultSwimlaneScope;
  const activeOnly = swimlaneScope === "active";

  const [simPick, setSimPick] = useState<{ campaignId: string; id: string } | null>(null);
  // Keep group spatial context when inside a group instead of replacing the entire canvas on task selection.
  const inGroup = viewMode === "graph" && !phoneListMode && group !== null;
  const activeTaskInGroup = inGroup
    ? (selected && campaignGroup(selected) === group ? selected.id : task)
    : null;
  const simTaskId =
    selected?.id ??
    activeTaskInGroup ??
    (simPick?.campaignId === campaign.id ? simPick.id : null);
  const simTask = simTaskId ? byId.get(simTaskId) : undefined;
  const simSched = simTask ? comparison.simulated.nodes.get(simTask.id) : undefined;
  const editScope = (): EditScope => inGroup ? { kind: "group", key: group } : selected ? { kind: "task", id: selected.id, depth: 1 }
    : { kind: "group", key: chosen ?? group ?? campaignGroup(campaign.nodes.find((node) => (node.dependencies ?? []).length) ?? campaign.nodes[0]) };

  const focus = (selector: string) => requestAnimationFrame(() => requestAnimationFrame(() => root.current?.querySelector<HTMLElement>(selector)?.focus()));
  const openGroup = (key: string, id: string | null = null) => {
    setGroup(key); setChosen(key); setTask(id);
    if (!id) focus(phoneListMode ? "[data-relations-list] [aria-expanded]" : "[data-relations-back]");
  };
  const goto = (id: string) => { const node = byId.get(id); if (node) openGroup(campaignGroup(node), id); };
  const back = () => { const left = group; setGroup(null); setTask(null); if (left !== null) focus(`.react-flow__node[data-id="campaign-group:${cssEscape(encodeURIComponent(left))}"]`); };
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

  const ganttData = useMemo(() => {
    const fullWaves = computeTaskWaves(
      campaign.nodes.map((n) => ({ id: n.id, dependsOn: n.dependencies ?? [] })),
    );
    // Keep completed Wave-1 columns monotonic on the Gantt chart when all tasks are shown so active dependents never trigger curW <= dep.end conflict.
    const ganttWaveSched = criticalActive
      ? criticalMode === "remaining" && !activeOnly && hasCompletedTasks
        ? computeCampaignCriticalPath(campaign.nodes, { mode: "structural", whatIfDeltas })
        : comparison.simulated
      : null;
    const visibleNodes = activeOnly ? campaign.nodes.filter(isActiveNode) : campaign.nodes;
    const visibleIds = new Set(visibleNodes.map((n) => n.id));
    const ganttGroups: GanttGroup[] = [];
    const defaultCollapsed: string[] = [];

    for (const entry of grouped) {
      const membersInScope = entry.nodes.filter((n) => visibleIds.has(n.id));
      if (membersInScope.length === 0) continue;
      const gid = entry.key || "__ungrouped__";
      ganttGroups.push({
        id: gid,
        name: entry.key || tr("미분류", "Ungrouped"),
      });
      const allDone = membersInScope.every((n) => !isActiveNode(n));
      if (allDone && grouped.length > 1) defaultCollapsed.push(gid);
    }

    const ganttTasks: GanttTask[] = visibleNodes.map((node) => {
      const num = campaignTaskNumber(node);
      const taskView = legacyTask(node, live?.[node.id]);
      const sched = comparison.simulated.nodes.get(node.id);
      const waveNode = ganttWaveSched?.nodes.get(node.id);
      const w = waveNode
        ? { start: waveNode.waveStart, end: waveNode.waveEnd }
        : (fullWaves.waves.get(node.id) ?? { start: 1, end: 1 });
      const gid = campaignGroup(node) || "__ungrouped__";
      const isCyclic = fullWaves.cyclicIds.has(node.id);
      const isBlocked = taskView.attention && !taskView.gate && taskView.status !== "failed";
      const team =
        taskView.status === "completed" || taskView.status === "skipped"
          ? "done"
          : taskView.status === "failed"
            ? "failed"
            : isBlocked || Boolean(taskView.gate)
              ? "blocked"
              : taskView.status === "running"
                ? "running"
                : "pending";
      const progress =
        taskView.status === "completed" || taskView.status === "skipped"
          ? 1
          : taskView.status === "running"
            ? Math.max(0.25, Math.min(0.95, taskView.marker.ratio || 0.5))
            : isBlocked || Boolean(taskView.gate)
              ? Math.max(0.15, taskView.marker.ratio || 0.2)
              : taskView.status === "failed"
                ? 0.15
                : 0;
      const statusOverride: GanttTask["statusOverride"] = isCyclic || taskView.status === "failed"
        ? "conflict"
        : taskView.status === "completed" || taskView.status === "skipped"
          ? "done"
          : isBlocked || Boolean(taskView.gate)
            ? "behind"
            : taskView.status === "running"
              ? "on-track"
              : "not-started";
      const baseSubtitle = taskView.step ? tr(...taskView.step) : (num ?? undefined);
      const critSubtitle = criticalOpen && sched && !sched.cyclic
        ? [
            sched.critical
              ? tr("병목 · 여유 0", "Critical")
              : sched.duration > 0
                ? tr(`여유 +${sched.slack}`, `Slack +${sched.slack}`)
                : baseSubtitle,
            sched.delta ? (sched.delta > 0 ? `+${sched.delta}` : String(sched.delta)) : null,
          ]
            .filter(Boolean)
            .join(" · ")
        : baseSubtitle;
      const dependsOn = (node.dependencies ?? []).filter((dep) => {
        if (!visibleIds.has(dep)) return false;
        if (isCyclic || !ganttWaveSched) return true;
        return (ganttWaveSched.nodes.get(dep)?.waveEnd ?? 0) < w.start;
      });
      return {
        id: node.id,
        name: num ? `${num} ${campaignTaskTitle(node)}` : node.title,
        group: gid,
        waveStart: w.start,
        waveEnd: w.end,
        progress,
        team,
        statusOverride,
        assignee: node.assignee ?? undefined,
        assigneeLabel: node.assignee ?? undefined,
        subtitle: critSubtitle || undefined,
        dependsOn,
      };
    });

    return {
      groups: ganttGroups,
      tasks: ganttTasks,
      defaultCollapsed,
      hiddenGroups: grouped.length - ganttGroups.length,
      hiddenTasks: campaign.nodes.length - visibleNodes.length,
    };
  }, [activeOnly, campaign.nodes, comparison.simulated, criticalActive, criticalMode, criticalOpen, grouped, hasCompletedTasks, live, tr, whatIfDeltas]);

  const criticalTimelineCss = useMemo(() => {
    if (!criticalOpen || comparison.simulated.criticalNodeIds.size === 0) return "";
    const critIds = [...comparison.simulated.criticalNodeIds].map((id) => `[data-task-id="${cssEscape(id)}"]`).join(", ");
    const critRows = [...comparison.simulated.criticalNodeIds].map((id) => `:has([data-task-id="${cssEscape(id)}"])`).join(", ");
    return [
      `[data-critical-timeline="on"] button[data-task-id], [data-critical-timeline="on"] tr[data-task-id] { opacity: 0.44; transition: opacity 160ms ease, box-shadow 160ms ease; }`,
      `[data-critical-timeline="on"] .aui-gantt-timeline-row:has([data-task-id]):not(${critRows}) .aui-gantt-timeline-task-cell { opacity: 0.48; transition: opacity 160ms ease; }`,
      `[data-critical-timeline="on"] :is(${critIds}) { opacity: 1; }`,
      `[data-critical-timeline="on"] button:is(${critIds}) { box-shadow: 0 0 0 1.5px var(--accent); font-weight: 600; }`,
      `[data-critical-timeline="on"] tr:is(${critIds}) td:first-child { box-shadow: inset 3px 0 0 var(--accent); font-weight: 600; }`,
      `@media (prefers-reduced-motion: reduce) { [data-critical-timeline="on"] [data-task-id], [data-critical-timeline="on"] .aui-gantt-timeline-task-cell { transition: none !important; } }`,
    ].join("\n");
  }, [criticalOpen, comparison.simulated.criticalNodeIds]);

  const ganttLabels = useMemo<Partial<GanttTimelineLabels>>(() => ({
    filterPlaceholder: tr("작업·이슈 검색", "Filter tasks"),
    filterAria: tr("타임라인 작업 검색", "Filter timeline tasks"),
    viewTimeline: tr("타임라인", "Timeline"),
    viewList: tr("목록", "List"),
    expandAll: tr("그룹 모두 펼치기", "Expand groups"),
    collapseAll: tr("그룹 모두 접기", "Collapse groups"),
    densityCompact: tr("좁게", "Compact"),
    densityNormal: tr("보통", "Normal"),
    densityWide: tr("넓게", "Wide"),
    loading: tr("타임라인을 불러오는 중…", "Loading timeline…"),
    empty: tr("표시할 작업이 없어요.", "No tasks to display."),
    colTask: tr("작업", "Task"),
    colWave: tr("선행 차수", "Wave"),
    colProgress: tr("진행도", "Progress"),
    colStatus: tr("상태", "Status"),
    tasksHeader: (visibleCount, laneCount) =>
      tr(`그룹 ${laneCount} · 작업 ${visibleCount}`, `${laneCount} groups · ${visibleCount} tasks`),
    waveHeader: (wave) => tr(`${wave}차 (Wave ${wave})`, `Wave ${wave}`),
    waveShort: (wave) => tr(`${wave}차`, `W${wave}`),
    waveSpan: (start, end) =>
      start === end ? tr(`${start}차`, `Wave ${start}`) : tr(`${start}–${end}차`, `Wave ${start}–${end}`),
    waveTasksCount: (count) => tr(`작업 ${count}개`, `${count} tasks`),
    statusDone: tr("완료", "Done"),
    statusReached: tr("도달", "Reached"),
    statusOnTrack: tr("진행 중", "Running"),
    statusBehind: tr("막힘·대기", "Blocked"),
    statusConflict: tr("실패·충돌", "Failed / cycle"),
    statusNotStarted: tr("시작 전", "Pending"),
    dependencyConflict: tr("실패 또는 순환 의존", "Failed or cyclic dependency"),
  }), [tr]);

  const stepperBtnClass =
    "!m-0 inline-flex min-h-7 max-[899px]:!min-h-11 [@media(pointer:coarse)]:!min-h-11 max-[899px]:min-w-11 [@media(pointer:coarse)]:min-w-11 cursor-pointer items-center justify-center !rounded-md !border-0 !bg-transparent px-2 text-xs font-medium text-[var(--fg)] tabular-nums transition-colors hover:!bg-[var(--bg-3)] focus-visible:outline-2 focus-visible:outline-[var(--accent)] disabled:cursor-default disabled:opacity-40";

  return <div ref={root} className="campaign-relations grid min-w-0 gap-3">
    <div className="min-w-0 overflow-hidden rounded-[var(--r-md)] border border-[var(--line-soft)] bg-[var(--bg-1)]">
      <header className="flex min-h-14 min-w-0 flex-wrap items-center gap-x-3 gap-y-2 border-b border-[var(--line-soft)] px-4 py-3">
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
          <Button variant="secondary" size="sm" onClick={() => { setPhoneGraphLayout("canvas"); openGroup(campaignGroup(selected), selected.id); }}>{tr("그룹 안에서 보기 →", "Open in group →")}</Button>
        </div> : <div className="grid min-w-0 gap-0.5">
          <strong className="truncate text-[15px] text-[var(--fg)]">
            {viewMode === "timeline"
              ? tr("선행 차수 타임라인", "Wave timeline")
              : viewMode === "swimlanes"
                ? tr("그룹 스윔레인", "Group swimlanes")
                : tr("그룹 관계", "Group connections")}
          </strong>
          <Cap><Num>{tr(`그룹 ${groups}개 · 작업 ${campaign.nodes.length}개`, `${groups} groups · ${campaign.nodes.length} tasks`)}</Num></Cap>
        </div>}
        <div className="ml-auto flex min-w-0 flex-wrap items-center justify-end gap-2 max-sm:w-full">
          {onEdit && kept && <KitBadge size="md" tone="warning">{tr("저장하지 않은 편집이 있어요", "Unsaved edits are waiting")}</KitBadge>}
          <Button
            variant={criticalOpen ? "primary" : "secondary"}
            size="sm"
            data-testid="toggle-critical-path"
            aria-pressed={criticalOpen}
            icon={<Route aria-hidden className="size-3.5" />}
            className="max-[899px]:!min-h-11 [@media(pointer:coarse)]:!min-h-11"
            onClick={() => setCriticalOpen((v) => !v)}
          >
            {tr("크리티컬 패스", "Critical path")}
          </Button>
          {(viewMode === "swimlanes" || viewMode === "timeline") && (
            <div className="min-w-0 max-w-full flex-1 sm:w-[17.5rem] sm:flex-none">
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
          <div className="w-full sm:w-64 sm:shrink-0">
            <SegmentedControl<RelationsMode>
              variant="tray"
              fullWidth
              size="sm"
              aria-label={tr("관계도 보기 방식", "Connections view mode")}
              value={viewMode}
              onChange={setViewMode}
              options={[
                { value: "timeline", label: tr("타임라인", "Timeline") },
                { value: "graph", label: tr("그래프", "Graph") },
                { value: "swimlanes", label: tr("스윔레인", "Swimlanes") },
              ]}
            />
          </div>
          {phone && viewMode === "graph" && !selected && !inGroup && (
            <div className="w-36 shrink-0">
              <SegmentedControl<"canvas" | "list">
                variant="tray"
                fullWidth
                size="sm"
                aria-label={tr("모바일 그래프 표시", "Mobile graph layout")}
                value={phoneGraphLayout}
                onChange={setPhoneGraphLayout}
                options={[
                  { value: "canvas", label: tr("캔버스", "Canvas") },
                  { value: "list", label: tr("목록", "List") },
                ]}
              />
            </div>
          )}
          <div className="campaign-node-picker min-w-0 flex-1 sm:w-64 sm:flex-none">
            <Select aria-label={tr("연결을 볼 작업", "Task to inspect")} size="sm" placeholder={tr("작업을 선택하세요", "Select a task")} value={selectedId}
              options={campaign.nodes.map((node) => ({ value: node.id, label: node.title }))} onChange={pick} />
          </div>
          {onEdit && <Button variant="secondary" size="sm" className="max-[899px]:!min-h-11 [@media(pointer:coarse)]:!min-h-11" onClick={() => onEdit(editScope())}>{kept ? tr("이어서 편집", "Continue editing") : tr("편집", "Edit")}</Button>}
        </div>
      </header>
      {criticalActive && (
        <div
          data-testid="critical-path-bar"
          className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 border-b border-[var(--line-soft)] bg-[var(--bg-2)] px-4 py-2.5 text-xs"
        >
          <div className="flex min-w-0 flex-wrap items-center gap-2">
            {hasCompletedTasks && (
              <SegmentedControl<CriticalPathMode>
                variant="tray"
                size="sm"
                aria-label={tr("크리티컬 패스 기준", "Critical path mode")}
                value={criticalMode}
                onChange={(next) => setCriticalModeChoice({ campaignId: campaign.id, mode: next })}
                options={[
                  { value: "remaining", label: tr("잔여 경로", "Remaining") },
                  { value: "structural", label: tr("전체 구조", "Full DAG") },
                ]}
              />
            )}
            <Num className="font-medium text-[var(--fg)] [word-break:keep-all]">
              {tr(
                `최장 ${comparison.simulated.totalDuration}단계 · 핵심 작업 ${comparison.simulated.criticalNodeIds.size}개`,
                `${comparison.simulated.totalDuration} steps · ${comparison.simulated.criticalNodeIds.size} critical`,
              )}
            </Num>
            {comparison.hasWhatIf && (
              <>
                <Chip
                  data-testid="whatif-delta-chip"
                  className="tabular-nums"
                  tone={comparison.durationDelta > 0 ? "warn" : comparison.durationDelta < 0 ? "ok" : "info"}
                >
                  {comparison.durationDelta > 0
                    ? tr(`전체 +${comparison.durationDelta}단계 지연`, `+${comparison.durationDelta} steps delayed`)
                    : comparison.durationDelta < 0
                      ? tr(`전체 ${comparison.durationDelta}단계 단축`, `${comparison.durationDelta} steps saved`)
                      : tr("여유 단계 내 흡수 (지연 0)", "Absorbed by slack (0 delay)")}
                </Chip>
                {comparison.pathShifted && (
                  <Chip tone="accent" data-testid="whatif-shifted-chip">
                    {tr("병목 경로 이동됨", "Critical path shifted")}
                  </Chip>
                )}
                <Button
                  variant="ghost"
                  size="sm"
                  data-testid="whatif-reset"
                  className="max-[899px]:!min-h-11 [@media(pointer:coarse)]:!min-h-11"
                  onClick={resetWhatIf}
                >
                  {tr("초기화", "Reset")}
                </Button>
              </>
            )}
          </div>
          <div className="flex min-w-0 flex-wrap items-center gap-2">
            <div className="w-44 min-w-0 sm:w-52">
              <Select
                size="sm"
                aria-label={tr("What-if 대상 작업", "What-if target task")}
                placeholder={tr("시뮬레이션 작업 선택", "Choose task to simulate")}
                value={simTask?.id ?? null}
                options={campaign.nodes.map((n) => ({
                  value: n.id,
                  label: `${campaignTaskNumber(n) ?? n.id} ${campaignTaskTitle(n)}`,
                }))}
                onChange={(nextId) => setSimPick({ campaignId: campaign.id, id: nextId })}
              />
            </div>
            {simTask && simSched ? (
              <>
                <Chip tone={simSched.critical ? "accent" : "neutral"} className="tabular-nums">
                  {simSched.critical
                    ? tr("병목 · 여유 0", "Critical · 0 slack")
                    : tr(`여유 +${simSched.slack}단계`, `Slack +${simSched.slack}`)}
                </Chip>
                <div
                  role="group"
                  aria-label={tr("What-if 단계 조절", "What-if step adjustment")}
                  className="inline-flex items-center gap-0.5 rounded-lg border border-[var(--line)] bg-[var(--th-card-bg)] p-0.5"
                >
                  <button
                    type="button"
                    data-testid="whatif-minus"
                    disabled={simSched.duration <= 0}
                    className={stepperBtnClass}
                    onClick={() => adjustWhatIf(simTask.id, -1)}
                  >
                    −1
                  </button>
                  <Num className="px-2 text-xs font-medium whitespace-nowrap text-[var(--fg)]">
                    {tr(
                      `소요 ${simSched.duration}단계${simSched.delta ? ` (${simSched.delta > 0 ? `+${simSched.delta}` : simSched.delta})` : ""}`,
                      `${simSched.duration} step${simSched.duration === 1 ? "" : "s"}${simSched.delta ? ` (${simSched.delta > 0 ? `+${simSched.delta}` : simSched.delta})` : ""}`,
                    )}
                  </Num>
                  <button
                    type="button"
                    data-testid="whatif-plus-1"
                    disabled={simSched.delta >= 6}
                    className={stepperBtnClass}
                    onClick={() => adjustWhatIf(simTask.id, 1)}
                  >
                    +1
                  </button>
                  <button
                    type="button"
                    data-testid="whatif-plus-2"
                    disabled={simSched.delta >= 6}
                    className={stepperBtnClass}
                    onClick={() => adjustWhatIf(simTask.id, 2)}
                  >
                    +2
                  </button>
                </div>
              </>
            ) : (
              <Cap className="[word-break:keep-all]">
                {tr("작업을 고르면 지연·단축 What-if 파급을 실험할 수 있어요.", "Select a task to simulate delay or speedup impact.")}
              </Cap>
            )}
          </div>
        </div>
      )}
      {viewMode === "timeline" ? (
        <div className="p-3 sm:p-4" data-critical-timeline={criticalOpen ? "on" : undefined}>
          {criticalTimelineCss && <style>{criticalTimelineCss}</style>}
          {activeOnly && ganttData.hiddenTasks > 0 && (
            <p role="status" data-testid="timeline-filter-caption" className="m-0 pb-2.5 text-xs text-[var(--fg-muted)]">
              <Num>
                {ganttData.hiddenGroups > 0
                  ? tr(
                      `완료·건너뜀 그룹 ${ganttData.hiddenGroups}개 · 작업 ${ganttData.hiddenTasks}개를 숨겼어요. 선행 차수(Wave)는 전체 캠페인 기준을 유지해요.`,
                      `Hiding ${ganttData.hiddenGroups} completed/skipped groups and ${ganttData.hiddenTasks} tasks. Wave numbers preserve the full campaign DAG.`,
                    )
                  : tr(
                      `완료·건너뜀 작업 ${ganttData.hiddenTasks}개를 숨겼어요. 선행 차수(Wave)는 전체 캠페인 기준을 유지해요.`,
                      `Hiding ${ganttData.hiddenTasks} completed/skipped tasks. Wave numbers preserve the full campaign DAG.`,
                    )}
              </Num>
            </p>
          )}
          <GanttTimeline
            key={`${campaign.id}:${swimlaneScope}`}
            embedded
            readOnly
            axisMode="wave"
            hideDetailsPanel
            title={campaign.title}
            groups={ganttData.groups}
            teams={GANTT_STATUS_TEAMS}
            tasks={ganttData.tasks}
            defaultCollapsed={ganttData.defaultCollapsed}
            selectedTaskId={selectedId}
            onSelectTask={onOpen}
            labels={ganttLabels}
          />
        </div>
      ) : viewMode === "swimlanes" ? (
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
      ) : phoneListMode ? (
        <GroupList campaign={campaign} live={live} onOpen={openGroup} tr={tr}><TieLines campaign={campaign} tr={tr} /></GroupList>
      ) : inGroup ? (
        <CampaignGroupTasks key={group} campaign={campaign} live={live} group={group} chosen={activeTaskInGroup} onChoose={setTask} onGoto={goto} onGotoGroup={(key) => openGroup(key)}
          onOpenTask={(id) => { setTask(id); onOpen(id); }} canvasHeight={phone ? "360px" : undefined} criticalPath={criticalOpen ? comparison.simulated : null} tr={tr} />
      ) : (
        <CampaignGroupOverview campaignId={campaign.id} nodes={campaign.nodes} live={live} chosen={chosen} onChoose={setChosen} onOpenGroup={openGroup} onOpenTask={goto} canvasHeight={phone ? "360px" : undefined} criticalPath={criticalOpen ? comparison.simulated : null} tr={tr} />
      )}
    </div>
    {phoneListMode && viewMode === "graph" && group !== null && <div data-relations-list><KitCard pad="none">
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
