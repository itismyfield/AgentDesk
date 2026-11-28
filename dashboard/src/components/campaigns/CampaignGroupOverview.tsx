import { useCallback, useMemo, useSyncExternalStore } from "react";
import type { Node } from "@xyflow/react";
import type { CampaignNode, CampaignNodeLive } from "../../api/campaigns";
import { GRAPH_DIM_OPACITY } from "../common/graph/GraphChrome";
import type { CampaignCriticalPath } from "./campaignCriticalPath";
import CampaignGroupInspector from "./CampaignGroupInspector";
import { NO_TIES, campaignRawEdges, routeEdges, type GraphViewSettings } from "./campaignGraphRouting";
import { layoutGroups, placeLines } from "./campaignGroupLayout";
import { campaignGroup, groupCampaignNodes } from "./campaignModel";
import { effectiveRelationCounts } from "./campaignOverviewModel";
import type { Tr } from "./campaignPresentation";
import { groupViewScope, loadCampaignView, readStoredRaw, subscribeCampaignView } from "./campaignViewStorage";
import type { GroupNodeData } from "./RelationsTaskNode";
import RelationsViewCanvas, { type ViewLine } from "./RelationsViewCanvas";
import { cycleComponents, findCycles } from "./relationsEditorModel";

/** The group key, not its sort position, names a group, so adding a group renames nothing. */
const groupNodeId = (key: string) => `campaign-group:${encodeURIComponent(key)}`;

export type GroupOverview = ReturnType<typeof buildCampaignGroupOverview>;

/**
 * Groups placed by dependency direction, the lines between them and the loops among them: a group projection can
 * hold a cycle even when the task graph is a DAG. Saved ties keep their lines and strand order.
 */
export function buildCampaignGroupOverview(
  nodes: CampaignNode[],
  settings: GraphViewSettings = NO_TIES,
  live?: Record<string, CampaignNodeLive>,
) {
  const displayOf = new Map(nodes.map((node) => [node.id, groupNodeId(campaignGroup(node))]));
  const routed = routeEdges({ edges: campaignRawEdges(nodes), displayOf }, settings);
  const lines = routed.display.sort((a, b) => a.source.localeCompare(b.source) || a.target.localeCompare(b.target));
  const grouped = groupCampaignNodes(nodes);
  const { placed: positions, below } = layoutGroups(grouped.map((group) => groupNodeId(group.key)), lines.map((line) => [line.source, line.target]));
  const { connections, ties } = placeLines(lines, routed.settings.ties, positions, below);
  const internal = new Map<string, number>();
  for (const [, target] of routed.internal) internal.set(displayOf.get(target)!, (internal.get(displayOf.get(target)!) ?? 0) + 1);
  const groups = grouped.map((group) => {
    const id = groupNodeId(group.key);
    return {
      ...group,
      progress: effectiveRelationCounts(group.nodes, live),
      id,
      position: positions.get(id)!,
      internalDependencies: internal.get(id) ?? 0,
    };
  });
  const projection = groups.map((group) => ({ id: group.id, dependencies: connections.filter((line) => line.target === group.id).map((line) => line.source) }));
  // One loop per looped part names it; every line inside a part lies on some loop, so all of them are drawn as one.
  return { groups, connections, internalDependencies: routed.internal.length, ties, cycles: findCycles(projection), components: cycleComponents(projection) };
}

/** One group loop as words: 순환 · A → B → A. */
export const groupCycleText = (overview: GroupOverview, path: string[], tr: Tr) => {
  const names = path.map((id) => { const key = overview.groups.find((group) => group.id === id)?.key ?? ""; return key || tr("미분류", "Ungrouped"); }).join(" → ");
  return tr(`순환 · ${names}`, `Cycle · ${names}`);
};

/**
 * Every group and the dependencies between them, with the chosen group (or the whole campaign) summed up beside it.
 * Ties saved for this graph are drawn and never changed here: ties are made in the relations editor, on tasks.
 */
export default function CampaignGroupOverview({ campaignId, nodes, live, chosen, onChoose, onOpenGroup, onOpenTask, canvasHeight, criticalPath = null, tr }: {
  campaignId: string; nodes: CampaignNode[]; live?: Record<string, CampaignNodeLive>; chosen: string | null; onChoose: (group: string | null) => void;
  onOpenGroup: (group: string) => void; onOpenTask: (id: string) => void; canvasHeight?: string; criticalPath?: CampaignCriticalPath | null; tr: Tr;
}) {
  const scope = useMemo(() => groupViewScope(campaignId), [campaignId]);
  const raw = useSyncExternalStore((listener) => subscribeCampaignView(scope, listener), () => readStoredRaw(scope));
  const view = useMemo(() => loadCampaignView(scope, raw).view, [scope, raw]);
  // A stored `showOriginal` is ignored: its switch is gone, so honouring it would hide these ties for good.
  const overview = useMemo(() => buildCampaignGroupOverview(nodes, view, live), [nodes, view, live]);
  const keyOf = (id: string) => overview.groups.find((group) => group.id === id)?.key;

  // Built once per graph and choice, so a live refresh that changes nothing hands React Flow the same cards and lines.
  const graphNodes = useMemo(() => {
    const connectedIds = new Set(overview.connections.flatMap((line) => [line.source, line.target]));
    return overview.groups.map((group): Node => {
      const { counts, percent, total } = group.progress;
      const finished = total > 0 && counts.completed + counts.skipped === total;
      const isolated = !connectedIds.has(group.id);
      const critCount = criticalPath ? group.nodes.filter((n) => criticalPath.criticalNodeIds.has(n.id)).length : 0;
      const isCrit = criticalPath ? criticalPath.criticalGroups.has(group.key) || critCount > 0 : false;
      const statusLabel = finished
        ? tr("완료", "Done")
        : counts.blocked
          ? tr(`막힘 ${counts.blocked}`, `${counts.blocked} blocked`)
          : counts.running
            ? tr(`진행 ${counts.running}`, `${counts.running} running`)
            : tr("대기", "Pending");
      const data: GroupNodeData = {
        title: group.key || tr("미분류", "Ungrouped"),
        code: `#${total}`,
        meta: `${percent}%`,
        statusLabel,
        tone: finished ? "done" : counts.blocked ? "warn" : counts.running ? "progress" : "open",
        ratio: percent / 100,
        summary: criticalPath
          ? [isCrit ? tr(`병목 ${critCount}개`, `${critCount} critical`) : tr("여유 경로", "Has slack"), tr(`진행 ${counts.running}`, `${counts.running} running`)].join(" · ")
          : tr(`진행 ${counts.running} · 그룹 안 연결 ${group.internalDependencies}`, `${counts.running} running · ${group.internalDependencies} inside`),
        blocked: counts.blocked ? tr(`막힌 작업 ${counts.blocked}개`, `${counts.blocked} blocked tasks`) : null,
        far: [`${percent}%`, counts.blocked > 0 && tr(`막힘 ${counts.blocked}`, `${counts.blocked} blocked`)].filter(Boolean).join(" · "),
        openLabel: tr("열기 →", "Open →"),
        onOpen: () => onOpenGroup(group.key),
        critical: isCrit,
        criticalCount: critCount,
        criticalLabel: isCrit ? tr(`병목 ${critCount}`, `${critCount} crit`) : undefined,
      };
      return {
        id: group.id,
        type: "relationsGroup",
        position: group.position,
        data,
        draggable: false,
        selected: group.key === chosen,
        style: criticalPath && !isCrit ? { opacity: GRAPH_DIM_OPACITY } : undefined,
        className: isolated
          ? "[&_.react-flow__handle]:!opacity-0 [&_.react-flow__handle]:!pointer-events-none [&>div>i]:!opacity-0 [&>div>i]:!pointer-events-none"
          : undefined,
        ariaLabel: tr(`${data.title}, 작업 ${total}개, 완료 ${percent}%, 진행 ${counts.running}, 막힘 ${counts.blocked}`, `${data.title}, ${total} tasks, ${percent}% completed, ${counts.running} running, ${counts.blocked} blocked`),
      };
    });
  }, [overview, chosen, onOpenGroup, criticalPath, tr]);
  const cycleText = useCallback((path: string[]) => groupCycleText(overview, path, tr), [overview, tr]);
  const lineLabel = useCallback((line: ViewLine) => tr(`그룹 간 의존성 ${line.count}개`, `${line.count} dependencies between groups`), [tr]);
  return <RelationsViewCanvas label={tr("그룹별 전체 관계", "All campaign group relationships")} nodes={graphNodes} lines={overview.connections} ties={overview.ties}
    cycles={overview.cycles} components={overview.components} cycleText={cycleText} lineLabel={lineLabel} height={canvasHeight}
    criticalEdgeKeys={criticalPath?.criticalEdgeKeys} showCriticalLegend={!!criticalPath}
    onSelect={(id) => onChoose(id === null ? null : keyOf(id) ?? null)} onOpen={(id) => { const key = keyOf(id); if (key !== undefined) onOpenGroup(key); }} tr={tr}
    inspector={<CampaignGroupInspector overview={overview} nodes={nodes} live={live} chosen={chosen} cycleText={cycleText} onChoose={onChoose} onOpenGroup={onOpenGroup} onOpenTask={onOpenTask} tr={tr} />} />;
}
