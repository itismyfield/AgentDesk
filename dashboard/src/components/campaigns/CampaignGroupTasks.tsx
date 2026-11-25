import { useCallback, useEffect, useMemo, useSyncExternalStore, type HTMLAttributes } from "react";
import type { Node } from "@xyflow/react";
import type { Campaign, CampaignNode, CampaignNodeLive } from "../../api/campaigns";
import CampaignGroupInspector, { TaskInspector } from "./CampaignGroupInspector";
import { buildCampaignGroupOverview, groupCycleText } from "./CampaignGroupOverview";
import { NO_TIES, campaignRawEdges, routeEdges, type GraphViewSettings } from "./campaignGraphRouting";
import { LAYER, TASK_HEIGHT, layoutGroups, placeLines } from "./campaignGroupLayout";
import { CAMPAIGN_STAGES, campaignGroup, campaignStageStep, campaignTaskTitle } from "./campaignModel";
import { legacyTask } from "./campaignOverviewModel";
import { LABELS, type Tr } from "./campaignPresentation";
import { editorViewScope, loadCampaignView, readStoredRaw, subscribeCampaignView } from "./campaignViewStorage";
import type { TailNodeData, ViewTaskData } from "./RelationsTaskNode";
import RelationsViewCanvas, { type ViewLine } from "./RelationsViewCanvas";
import { cycleComponents, findCycles, scopeKey, taskLabel, taskName } from "./relationsEditorModel";

const tailId = (id: string) => `tail:${id}`;
/** A tail card's height; tails stacked in one edge column keep `TAIL_PITCH`, a card plus a gap. */
export const TAIL_HEIGHT = 76;
const TAIL_PITCH = TAIL_HEIGHT + 20;

/** One group's tasks laid out like the group graph, with tails for the outside tasks they depend on (left edge) or hand
 * on to (right edge); lines go through the ties saved for the group's editor scope, loops are looped parts with a member. */
export function buildGroupTasks(nodes: CampaignNode[], key: string, settings: GraphViewSettings = NO_TIES) {
  const members = nodes.filter((node) => campaignGroup(node) === key);
  const inside = new Set(members.map((node) => node.id));
  const byId = new Map(nodes.map((node) => [node.id, node]));
  const edges = campaignRawEdges(nodes).filter(([from, to]) => inside.has(from) || inside.has(to));
  const before = new Set(edges.flatMap(([from, to]) => inside.has(to) && !inside.has(from) ? [from] : []));
  const outside = [...new Set(edges.flatMap((edge) => edge.filter((id) => !inside.has(id))))];
  const displayOf = new Map<string, string>([...members.map((node) => [node.id, node.id] as const), ...outside.map((id) => [id, tailId(id)] as const)]);
  const routed = routeEdges({ edges, displayOf }, settings);
  const lines = routed.display.sort((a, b) => a.source.localeCompare(b.source) || a.target.localeCompare(b.target))
    .map((line) => ({ ...line, faint: line.source.startsWith("tail:") || line.target.startsWith("tail:") }));
  const { placed, below } = layoutGroups([...members.map((node) => node.id), ...outside.map(tailId)], lines.map((line) => [line.source, line.target]), TASK_HEIGHT);
  // Tails move to their own column at each edge of the linked tasks, stacked without overlapping.
  const ends = new Set(lines.flatMap((line) => [line.source, line.target]));
  const linked = members.filter((node) => ends.has(node.id)).map((node) => placed.get(node.id)!.x);
  const edge = { in: (linked.length ? Math.min(...linked) : 0) - LAYER, out: (linked.length ? Math.max(...linked) : 0) + LAYER };
  const tails = outside.map((id) => ({ id: tailId(id), node: byId.get(id)!, side: before.has(id) ? "in" as const : "out" as const }));
  for (const side of ["in", "out"] as const) {
    let next = -Infinity;
    for (const tail of tails.filter((value) => value.side === side).sort((a, b) => placed.get(a.id)!.y - placed.get(b.id)!.y)) {
      const y = Math.max(placed.get(tail.id)!.y, next);
      placed.set(tail.id, { x: edge[side], y });
      next = y + TAIL_PITCH;
    }
  }
  // Tails stacked past the linked cards push the shelf and the loop lanes down by as much.
  const bottom = Math.max(below, ...tails.map((tail) => placed.get(tail.id)!.y + TAIL_HEIGHT));
  for (const node of members) if (!ends.has(node.id)) { const at = placed.get(node.id)!; placed.set(node.id, { x: at.x, y: at.y + bottom - below }); }
  const { connections, ties } = placeLines(lines, routed.settings.ties, placed, bottom);
  // Each looped part (SCC) a member is in, in display ids, with its shortest loop; tasks the view does not show keep their own id.
  const paths = findCycles(nodes), display = (id: string) => displayOf.get(id) ?? id;
  const looped = cycleComponents(nodes).flatMap((part, index) => members.some((node) => part.has(node.id)) ? [{ part, path: paths[index] }] : []);
  return { members, tails, positions: placed, connections, ties, cycles: looped.map(({ path }) => path.map(display)),
    components: looped.map(({ part }) => new Set([...part].filter((id) => displayOf.has(id)).map(display))) };
}

/** The opened group's task graph, read only: task cards, faint tails that open their own group with that task chosen,
 * and the chosen task's summary (or the group's) beside it. */
export default function CampaignGroupTasks({ campaign, live, group, chosen, onChoose, onGoto, onGotoGroup, onOpenTask, tr }: {
  campaign: Campaign; live?: Record<string, CampaignNodeLive>; group: string; chosen: string | null; onChoose: (id: string | null) => void;
  /** Goes to a task's group with that task chosen. */
  onGoto: (id: string) => void; onGotoGroup: (group: string) => void; onOpenTask: (id: string) => void; tr: Tr;
}) {
  const scope = useMemo(() => editorViewScope(campaign.id, scopeKey({ kind: "group", key: group })), [campaign.id, group]);
  const raw = useSyncExternalStore((listener) => subscribeCampaignView(scope, listener), () => readStoredRaw(scope));
  const view = useMemo(() => loadCampaignView(scope, raw).view, [scope, raw]);
  const graph = useMemo(() => buildGroupTasks(campaign.nodes, group, view), [campaign.nodes, group, view]);
  const overview = useMemo(() => buildCampaignGroupOverview(campaign.nodes, undefined, live), [campaign.nodes, live]);
  const byId = useMemo(() => new Map(campaign.nodes.map((node) => [node.id, node])), [campaign.nodes]);
  const label = useCallback((key: string) => key || tr("미분류", "Ungrouped"), [tr]);
  const name = useCallback((id: string) => { const node = byId.get(id.replace(/^tail:/, "")); return node ? taskLabel(node) : id; }, [byId]);
  const cycleText = useCallback((path: string[]) => tr(`순환 · ${path.map(name).join(" → ")}`, `Cycle · ${path.map(name).join(" → ")}`), [name, tr]);
  const lineLabel = useCallback((line: ViewLine) => tr(`선행 ${name(line.source)} → ${name(line.target)}`, `Dependency ${name(line.source)} → ${name(line.target)}`), [name, tr]);
  // A task reached from outside (a tail, a search) gets the focus once its card is drawn, so the keyboard stays with it.
  useEffect(() => {
    if (chosen) requestAnimationFrame(() => requestAnimationFrame(() => document.querySelector<HTMLElement>(`.react-flow__node[data-id="${CSS.escape(chosen)}"]`)?.focus()));
  }, []); // eslint-disable-line react-hooks/exhaustive-deps -- only for the task this view opened on

  // Built once per graph and choice, so a live refresh that changes nothing hands React Flow the same cards and lines.
  const graphNodes = useMemo(() => [...graph.members.map((node): Node => {
    const deps = node.dependencies ?? [];
    const outside = deps.filter((dep) => byId.has(dep) && campaignGroup(byId.get(dep)!) !== group).length;
    const error = graph.components.some((part) => part.has(node.id));
    const taskView = legacyTask(node, live?.[node.id]);
    const step = campaignStageStep(node.stage);
    const stageInfo = step.index >= 0 ? tr(...CAMPAIGN_STAGES[step.index].label) : step.raw;
    const depInfo = deps.length > 0
      ? outside > 0
        ? tr(`선행 ${deps.length} (외부 ${outside})`, `${deps.length} deps (${outside} ext)`)
        : tr(`선행 ${deps.length}`, `${deps.length} deps`)
      : tr(`수용조건 ${(node.acceptance ?? []).length}`, `${(node.acceptance ?? []).length} acceptance`);
    const isBlocked = taskView.attention && !taskView.gate && taskView.status !== "failed";
    const data: ViewTaskData = {
      title: campaignTaskTitle(node),
      code: taskLabel(node),
      meta: label(group),
      status: taskView.status,
      statusLabel: tr(...LABELS[taskView.status]),
      ...taskView.marker,
      summary: [stageInfo, depInfo].filter(Boolean).join(" · "),
      note: error
        ? { tone: "err", text: tr("순환 오류", "Cycle") }
        : taskView.status === "failed"
          ? { tone: "err", text: tr("실패", "Failed") }
          : isBlocked
            ? { tone: "warn", text: tr("막힘", "Blocked") }
            : null,
      locked: false, error, picked: false,
    };
    return { id: node.id, type: "relationsTask", position: graph.positions.get(node.id)!, data, draggable: false, selected: node.id === chosen,
      domAttributes: { "data-task-id": node.id } as HTMLAttributes<HTMLDivElement>, ariaLabel: `${taskName(node)}, ${tr(...LABELS[taskView.status])}` };
  }), ...graph.tails.map((tail): Node => {
    const ends = { enters: graph.connections.some((line) => line.target === tail.id), leaves: graph.connections.some((line) => line.source === tail.id) };
    const data: TailNodeData = { title: taskName(tail.node), cover: taskLabel(tail.node), group: label(campaignGroup(tail.node)), side: tail.side, ends };
    return { id: tail.id, type: "relationsTail", position: graph.positions.get(tail.id)!, data, draggable: false, selected: false,
      ariaLabel: tr(`그룹 밖 작업 ${data.title}, ${data.group}`, `Task outside the group ${data.title}, ${data.group}`) };
  })], [graph, byId, live, group, chosen, label, tr]);
  // The detail hands the focus back to what held it as it opened, so the card takes it first and closing returns there.
  const openDetail = (id: string) => { document.querySelector<HTMLElement>(`.react-flow__node[data-task-id="${CSS.escape(id)}"]`)?.focus({ preventScroll: true }); onOpenTask(id); };
  const pick = (id: string | null) => id?.startsWith("tail:") ? onGoto(id.slice(5)) : onChoose(id);
  const task = chosen === null ? undefined : graph.members.find((node) => node.id === chosen);
  return <RelationsViewCanvas key={group} label={tr(`그룹 ${label(group)}의 작업 관계`, `Task relations in ${label(group)}`)} nodes={graphNodes} lines={graph.connections} ties={graph.ties}
    cycles={graph.cycles} components={graph.components} cycleText={cycleText} lineLabel={lineLabel}
    onSelect={pick} onOpen={(id) => id.startsWith("tail:") ? pick(id) : onOpenTask(id)} tr={tr}
    inspector={task ? <TaskInspector node={task} nodes={campaign.nodes} live={live?.[task.id]} cycles={graph.cycles.filter((_, index) => graph.components[index].has(task.id)).map(cycleText)}
      onChoose={(id) => byId.get(id) && campaignGroup(byId.get(id)!) === group ? onChoose(id) : onGoto(id)} onOpenTask={openDetail} tr={tr} />
      : <CampaignGroupInspector overview={overview} nodes={campaign.nodes} live={live} chosen={group} opened cycleText={(path) => groupCycleText(overview, path, tr)}
        onChoose={onGotoGroup} onOpenGroup={onGotoGroup} onOpenTask={onChoose} tr={tr} />} />;
}
