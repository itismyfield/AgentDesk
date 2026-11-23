import { useMemo, useRef, useSyncExternalStore, type CSSProperties } from "react";
import { Background, MarkerType, ReactFlow, type Edge, type Node, type NodeProps, type ReactFlowInstance } from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import type { CampaignNode } from "../../api/campaigns";
import CampaignGroupInspector from "./CampaignGroupInspector";
import { NO_TIES, campaignRawEdges, routeEdges, type GraphViewSettings, type RawEdgeKey } from "./campaignGraphRouting";
import { GROUP_PIN_Y, GROUP_WIDTH, LOOP_LANE, layoutGroups } from "./campaignGroupLayout";
import { campaignGroup, groupCampaignNodes } from "./campaignModel";
import type { Tr } from "./campaignPresentation";
import { groupViewScope, loadCampaignView, readStoredRaw, subscribeCampaignView } from "./campaignViewStorage";
import { RelationsLegend, RelationsLine, ZoomTools, useLegible, type LineData } from "./RelationsCanvasChrome";
import { RelationsGroupNode, RelationsTieNode, type GroupNodeData, type TieNodeData } from "./RelationsTaskNode";
import { findCycles } from "./relationsEditorModel";

/** The group key, not its sort position, names a group, so adding a group renames nothing. */
const groupNodeId = (key: string) => `campaign-group:${encodeURIComponent(key)}`;
const tieNodeId = (id: string) => `campaign-tie:${id}`;
/** A tie node is 44 units square; its lines meet at its centre. */
const TIE_HALF = 22;
/** A saved tie, only drawn, kept at its own size on screen when zoomed out; its lines meet at the centre it scales around. */
function FixedTieNode(props: NodeProps) {
  return <div style={{ transform: `scale(${useLegible()})` }}><RelationsTieNode {...props} /></div>;
}
const NODE_TYPES = { relationsGroup: RelationsGroupNode, relationsTie: FixedTieNode };
const EDGE_TYPES = { relationsLine: RelationsLine };
/** The canvas fills the window's height below the page header, never under 560px. */
const CANVAS_HEIGHT = "max(560px, calc(100dvh - 21rem))";
/** Fitting leaves the bottom strip to the legend and the zoom tools. */
const FIT = { padding: { x: "32px", top: "24px", bottom: "72px" }, minZoom: 0.1, maxZoom: 1 } as const;

export type GroupOverview = ReturnType<typeof buildCampaignGroupOverview>;

/**
 * Groups placed by dependency direction, the lines between them and the loops among them: a group projection can
 * hold a cycle even when the task graph is a DAG. Saved ties keep their lines and strand order; they sit where those
 * lines now run, since the positions they were saved with belonged to the old grid.
 */
export function buildCampaignGroupOverview(nodes: CampaignNode[], settings: GraphViewSettings = NO_TIES) {
  const displayOf = new Map(nodes.map((node) => [node.id, groupNodeId(campaignGroup(node))]));
  const routed = routeEdges({ edges: campaignRawEdges(nodes), displayOf }, settings);
  const lines = routed.display.sort((a, b) => a.source.localeCompare(b.source) || a.target.localeCompare(b.target));
  const grouped = groupCampaignNodes(nodes);
  const { placed: positions, below } = layoutGroups(grouped.map((group) => groupNodeId(group.key)), lines.map((line) => [line.source, line.target]));
  // An untied line that runs backwards loops under the linked groups, in its own lane.
  let lane = 0;
  const connections = lines.map((line) => ({ ...line, count: line.edges.length,
    loopY: !line.via.length && positions.get(line.target)!.x <= positions.get(line.source)!.x ? below + 28 + LOOP_LANE * lane++ : undefined }));
  const internal = new Map<string, number>();
  for (const [, target] of routed.internal) internal.set(displayOf.get(target)!, (internal.get(displayOf.get(target)!) ?? 0) + 1);
  const groups = grouped.map((group) => {
    const id = groupNodeId(group.key);
    return { ...group, id, position: positions.get(id)!, internalDependencies: internal.get(id) ?? 0 };
  });
  // A tie sits in the stretch every one of its lines crosses (between the last source and the first target), at their mean height.
  const ties = routed.settings.ties.flatMap((tie) => {
    const spans = connections.filter((line) => line.via.includes(tie.id)).map((line) => {
      const from = positions.get(line.source)!, to = positions.get(line.target)!;
      return { x0: from.x + GROUP_WIDTH, y0: from.y + GROUP_PIN_Y, x1: to.x, y1: to.y + GROUP_PIN_Y };
    });
    if (!spans.length) return [];
    const lo = Math.max(...spans.map((span) => span.x0)), hi = Math.min(...spans.map((span) => span.x1));
    const x = lo <= hi ? (lo + hi) / 2 : spans.reduce((sum, span) => sum + (span.x0 + span.x1) / 2, 0) / spans.length;
    const y = spans.reduce((sum, span) => sum + span.y0 + (span.y1 - span.y0) * Math.min(1, Math.max(0, (x - span.x0) / (span.x1 - span.x0 || 1))), 0) / spans.length;
    return [{ ...tie, position: { x: x - TIE_HALF, y: y - TIE_HALF } }];
  });
  const cycles = findCycles(groups.map((group) => ({ id: group.id, dependencies: connections.filter((line) => line.target === group.id).map((line) => line.source) })));
  return { groups, connections, internalDependencies: routed.internal.length, ties, cycles };
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
export default function CampaignGroupOverview({ campaignId, nodes, chosen, onChoose, onOpenGroup, onOpenTask, tr }: {
  campaignId: string; nodes: CampaignNode[]; chosen: string | null; onChoose: (group: string | null) => void;
  onOpenGroup: (group: string) => void; onOpenTask: (id: string) => void; tr: Tr;
}) {
  const scope = useMemo(() => groupViewScope(campaignId), [campaignId]);
  const raw = useSyncExternalStore((listener) => subscribeCampaignView(scope, listener), () => readStoredRaw(scope));
  const view = useMemo(() => loadCampaignView(scope, raw).view, [scope, raw]);
  // A stored `showOriginal` is ignored: its switch is gone, so honouring it would hide these ties for good.
  const overview = useMemo(() => buildCampaignGroupOverview(nodes, view), [nodes, view]);
  const flow = useRef<ReactFlowInstance | null>(null);
  const label = (key: string) => key || tr("미분류", "Ungrouped");

  const graphNodes: Node[] = overview.groups.map((group) => {
    const { counts, percent, total } = group.progress;
    const finished = total > 0 && counts.completed + counts.skipped === total;
    const data: GroupNodeData = {
      title: label(group.key), meta: tr(`작업 ${total}개 · 완료 ${percent}%`, `${total} tasks · ${percent}% done`),
      tone: finished ? "done" : counts.blocked ? "warn" : counts.running ? "progress" : "open", ratio: percent / 100, pins: [tr("선행", "Needs"), tr("다음", "Next")],
      summary: tr(`진행 ${counts.running} · 막힘 ${counts.blocked} · 그룹 안 연결 ${group.internalDependencies}`, `${counts.running} running · ${counts.blocked} blocked · ${group.internalDependencies} inside`),
      blocked: counts.blocked ? tr(`막힌 작업 ${counts.blocked}개`, `${counts.blocked} blocked tasks`) : null,
      far: [`${percent}%`, counts.blocked > 0 && tr(`막힘 ${counts.blocked}`, `${counts.blocked} blocked`)].filter(Boolean).join(" · "),
    };
    return { id: group.id, type: "relationsGroup", position: group.position, data, draggable: false, selected: group.key === chosen,
      ariaLabel: tr(`${data.title}, 작업 ${total}개, 완료 ${percent}%, 진행 ${counts.running}, 막힘 ${counts.blocked}`, `${data.title}, ${total} tasks, ${percent}% completed, ${counts.running} running, ${counts.blocked} blocked`) };
  });
  for (const tie of overview.ties) {
    const count = overview.connections.filter((line) => line.via.includes(tie.id)).reduce((sum, line) => sum + line.count, 0);
    graphNodes.push({ id: tieNodeId(tie.id), type: "relationsTie", position: tie.position, draggable: false, selectable: false, focusable: false,
      ariaLabel: tr(`케이블타이, 선 ${count}개`, `Cable tie, ${count} lines`), data: { count, menu: null, fixed: true } satisfies TieNodeData });
  }
  // A loop is labelled once, on its line that runs backwards across the columns when there is one.
  const looped = new Map<string, string[]>();
  for (const path of overview.cycles) path.slice(1).forEach((to, index) => looped.set(`${path[index]}->${to}`, path));
  const at = (id: string) => overview.groups.find((group) => group.id === id)!.position.x;
  const labelled = new Set(overview.cycles.map((path) => {
    const pairs = path.slice(1).map((to, index) => `${path[index]}->${to}`);
    return pairs.find((pair) => { const [from, to] = pair.split("->"); return at(to) <= at(from); }) ?? pairs[0];
  }));
  const edges: Edge[] = overview.connections.flatMap((line) => {
    const pair = `${line.source}->${line.target}`, path = looped.get(pair);
    const named = path && labelled.delete(pair) ? groupCycleText(overview, path, tr) : undefined;
    const chain = [line.source, ...line.via.map(tieNodeId), line.target];
    return chain.slice(1).map((target, index) => {
      const last = index === chain.length - 2;
      const data: LineData & { edges: RawEdgeKey[] } = { error: !!path, edges: line.edges, loopY: line.loopY, ...last ? { count: line.count, cycle: named } : {} };
      return { id: chain.length > 2 ? `${line.id}#${index}` : line.id, source: chain[index], target, type: "relationsLine", data, selectable: false, focusable: false,
        ...last ? { ariaLabel: tr(`그룹 간 의존성 ${line.count}개`, `${line.count} dependencies between groups`), markerEnd: { type: MarkerType.ArrowClosed, color: path ? "var(--err)" : "var(--fg-faint)", width: 12, height: 12 } } : {} };
    });
  });

  return <section aria-label={tr("그룹별 전체 관계", "All campaign group relationships")}
    className="grid min-w-0 min-[1280px]:h-[var(--relations-height)] min-[1280px]:grid-cols-[minmax(0,1fr)_18rem] min-[1280px]:grid-rows-[minmax(0,1fr)]"
    style={{ "--relations-height": CANVAS_HEIGHT } as CSSProperties}>
    <div data-relations-canvas className="relative h-[var(--relations-height)] min-w-0 bg-[var(--bg)]">
      <ReactFlow nodes={graphNodes} edges={edges} nodeTypes={NODE_TYPES} edgeTypes={EDGE_TYPES} proOptions={{ hideAttribution: true }}
        onInit={(instance) => { flow.current = instance; }} fitView fitViewOptions={FIT}
        minZoom={0.1} maxZoom={1.5} nodesDraggable={false} nodesConnectable={false} edgesFocusable={false} panOnScroll zoomOnScroll={false} zoomOnDoubleClick={false}
        deleteKeyCode={null} selectionKeyCode={null} multiSelectionKeyCode={null}
        // Selection arrives as a change for a click and for Enter on a focused group; Escape or the pane lets it go.
        onNodesChange={(changes) => {
          const picked = changes.find((change) => change.type === "select" && change.selected);
          if (picked && "id" in picked) onChoose(overview.groups.find((group) => group.id === picked.id)?.key ?? null);
          else if (changes.some((change) => change.type === "select")) onChoose(null);
        }}
        onNodeDoubleClick={(_, node) => { const group = overview.groups.find((value) => value.id === node.id); if (group) onOpenGroup(group.key); }}
        onPaneClick={() => onChoose(null)}>
        <Background gap={20} size={1.6} color="var(--line)" />
      </ReactFlow>
      <RelationsLegend className="max-w-[calc(100%-12rem)]" tr={tr} />
      <ZoomTools role="toolbar" aria-label={tr("보기 도구", "View tools")} flow={flow} fit={FIT} fitLabel={tr("화면 맞춤", "Fit to screen")} tr={tr} />
    </div>
    <CampaignGroupInspector overview={overview} nodes={nodes} chosen={chosen} cycleText={(path) => groupCycleText(overview, path, tr)} onChoose={onChoose} onOpenGroup={onOpenGroup} onOpenTask={onOpenTask} tr={tr} />
  </section>;
}
