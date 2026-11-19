import { useMemo, useSyncExternalStore } from "react";
import { Background, Controls, MarkerType, Position, ReactFlow, type Edge, type Node } from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import type { CampaignNode } from "../../api/campaigns";
import { NO_TIES, campaignRawEdges, routeEdges, type GraphViewSettings } from "./campaignGraphRouting";
import { campaignGroup, groupCampaignNodes } from "./campaignModel";
import { TIE_WIDTH, tieHeight, tieSlots } from "./campaignTieGeometry";
import { CampaignTieNode } from "./CampaignTieGraph";
import { groupViewScope, loadCampaignView, readStoredRaw, subscribeCampaignView } from "./campaignViewStorage";

type Props = {
  campaignId: string;
  nodes: CampaignNode[];
  onSelectGroup: (group: string) => void;
  tr: (ko: string, en: string) => string;
};

/** The group key, not its sort position, names a group, so adding a group renames nothing. */
const groupNodeId = (key: string) => `campaign-group:${encodeURIComponent(key)}`;
const tieNodeId = (id: string) => `campaign-tie:${id}`;
const GROUP_WIDTH = 244;
const GROUP_HEIGHT = 145;
const NODE_TYPES = { campaignTie: CampaignTieNode };

/** A group projection can contain cycles even when the underlying task graph is a DAG. */
export function buildCampaignGroupOverview(nodes: CampaignNode[], settings: GraphViewSettings = NO_TIES) {
  const groups = groupCampaignNodes(nodes).map((group, index) => ({
    ...group,
    id: groupNodeId(group.key),
    position: { x: (index % 3) * 310, y: Math.floor(index / 3) * 210 },
    internalDependencies: 0,
  }));
  const groupsById = new Map(groups.map((group) => [group.id, group]));
  const displayOf = new Map(nodes.map((node) => [node.id, groupNodeId(campaignGroup(node))]));
  const routed = routeEdges({ edges: campaignRawEdges(nodes), displayOf }, settings);
  for (const [, target] of routed.internal) groupsById.get(displayOf.get(target)!)!.internalDependencies++;
  return {
    groups,
    connections: routed.display
      .map((line) => ({ ...line, count: line.edges.length }))
      .sort((a, b) => a.source.localeCompare(b.source) || a.target.localeCompare(b.target)),
    internalDependencies: routed.internal.length,
    /** The stored ties without members that left the definition. */
    ties: routed.settings.ties,
  };
}

/**
 * Every group and the dependencies between them. Ties saved for this graph are drawn as they were left and never
 * changed here: ties are made in the relations editor, on tasks.
 */
export default function CampaignGroupOverview({ campaignId, nodes, onSelectGroup, tr }: Props) {
  const scope = useMemo(() => groupViewScope(campaignId), [campaignId]);
  const raw = useSyncExternalStore((listener) => subscribeCampaignView(scope, listener), () => readStoredRaw(scope));
  const view = useMemo(() => loadCampaignView(scope, raw).view, [scope, raw]);
  const overview = useMemo(() => buildCampaignGroupOverview(nodes, view), [nodes, view]);
  // A stored `showOriginal` is ignored: its switch is gone, so honouring it would hide these ties for good.
  const lines = overview.connections;
  const shownTies = useMemo(() => overview.ties.filter((tie) => lines.some((line) => line.via.includes(tie.id))), [overview.ties, lines]);
  const slots = useMemo(() => tieSlots(shownTies, lines), [shownTies, lines]);

  const graphNodes: Node[] = overview.groups.map((group) => {
    const label = group.key || tr("미분류", "Ungrouped");
    const { counts, percent, total } = group.progress;
    return {
      id: group.id,
      position: group.position,
      sourcePosition: Position.Right,
      targetPosition: Position.Left,
      ariaLabel: tr(`${label}, 작업 ${total}개, 완료 ${percent}%, 진행 ${counts.running}, 막힘 ${counts.blocked}`, `${label}, ${total} tasks, ${percent}% completed, ${counts.running} running, ${counts.blocked} blocked`),
      data: {
        group: group.key,
        label: <div style={{ display: "grid", gap: 8, textAlign: "left", fontSize: 12 }}>
          <strong title={label} style={{ overflow: "hidden", whiteSpace: "nowrap", textOverflow: "ellipsis", fontSize: 14 }}>{label}</strong>
          <div className="campaign-row"><span>{tr(`작업 ${total}개`, `${total} tasks`)}</span><span>{counts.completed}/{total} · {percent}%</span></div>
          <progress value={percent} max={100} aria-label={tr(`${label} 완료율`, `${label} completion`)} style={{ width: "100%", height: 6, accentColor: "var(--th-accent-info)" }} />
          <div className="campaign-row"><span>{tr(`진행 ${counts.running}`, `${counts.running} running`)}</span><span>{tr(`막힘 ${counts.blocked}`, `${counts.blocked} blocked`)}</span></div>
          <small className="campaign-muted">{tr(`그룹 내부 연결 ${group.internalDependencies}개`, `${group.internalDependencies} internal dependencies`)}</small>
        </div>,
      },
      style: { width: GROUP_WIDTH, minHeight: GROUP_HEIGHT, padding: 14, border: `1px solid ${counts.blocked ? "#d97706" : "var(--th-border)"}`, borderRadius: 10, background: "var(--th-card-bg)", color: "var(--th-text-primary)", cursor: "pointer" },
    };
  });
  shownTies.forEach((tie, index) => graphNodes.push({
    id: tieNodeId(tie.id), type: "campaignTie", position: tie.position, draggable: false, selectable: false,
    ariaLabel: tr(`타이 ${index + 1}, 원본 선 ${tie.strandOrder.length}개`, `Tie ${index + 1}, ${tie.strandOrder.length} original lines`),
    data: { slots: slots.get(tie.id)?.length ?? 0 },
    style: { width: TIE_WIDTH, height: tieHeight(slots.get(tie.id)?.length ?? 0) },
  }));
  const edges: Edge[] = lines.flatMap((line) => {
    const style = { stroke: "var(--th-text-muted)", strokeWidth: 1.5 };
    const chain = [line.source, ...line.via.map(tieNodeId), line.target];
    const slotOf = (index: number) => slots.get(line.via[index - 1])?.indexOf(line.id) ?? 0;
    return chain.slice(1).map((target, index) => {
      const last = index === chain.length - 2;
      return {
        id: chain.length > 2 ? `${line.id}#${index}` : line.id,
        source: chain[index],
        target,
        sourceHandle: index > 0 ? `out-${slotOf(index)}` : undefined,
        targetHandle: last ? undefined : `in-${slotOf(index + 1)}`,
        data: { edges: line.edges },
        type: chain.length > 2 ? "straight" : "smoothstep",
        style,
        ...(last ? {
          label: String(line.count),
          ariaLabel: tr(`그룹 간 의존성 ${line.count}개`, `${line.count} dependencies between groups`),
          markerEnd: { type: MarkerType.ArrowClosed, color: "var(--th-text-muted)" },
          labelStyle: { fill: "var(--th-text-primary)", fontWeight: 600, fontSize: 12 },
          labelBgStyle: { fill: "var(--th-card-bg)" },
          labelBgPadding: [6, 3] as [number, number],
          labelBgBorderRadius: 4,
        } : {}),
      };
    });
  });

  const crossDependencies = overview.connections.reduce((sum, connection) => sum + connection.count, 0);
  return <section className="campaign-neighborhood" aria-label={tr("그룹별 전체 관계", "All campaign group relationships")}>
    <div className="campaign-row"><strong>{tr("그룹별 전체 관계", "All campaign group relationships")}</strong><span>{tr(`그룹 ${overview.groups.length}개 · 작업 ${nodes.length}개`, `${overview.groups.length} groups · ${nodes.length} tasks`)}</span></div>
    <p className="campaign-muted">{tr("전체 작업의 그룹 관계입니다. 화살표는 선행 그룹 → 후속 그룹, 숫자는 연결된 의존성 수입니다. 그룹을 고르면 그 작업이 아래에 나와요.", "Groups across all tasks. Arrows point from dependency to dependent; numbers count the connecting dependencies. Select a group to list its tasks below.")}</p>
    {nodes.length === 0 ? <p>{tr("표시할 작업이 없습니다.", "No tasks to display.")}</p> : <>
      <div className="campaign-graph-scroll"><div className="campaign-graph [&_.react-flow\_\_controls]:shadow-none [&_.react-flow\_\_controls-button]:!bg-th-bg-surface [&_.react-flow\_\_controls-button]:!text-th-text-primary [&_.react-flow\_\_controls-button]:!border-th-border-subtle [&_.react-flow\_\_attribution]:!bg-th-bg-surface [&_.react-flow\_\_attribution_a]:!text-th-text-muted" style={{ height: Math.min(580, Math.max(300, Math.ceil(overview.groups.length / 3) * 210)) }}>
        <ReactFlow nodes={graphNodes} edges={edges} nodeTypes={NODE_TYPES} fitView fitViewOptions={{ padding: 0.18, minZoom: 0.65, maxZoom: 1 }} minZoom={0.65} maxZoom={1.4}
          nodesDraggable={false} nodesConnectable={false} panOnScroll zoomOnScroll={false}
          onNodeClick={(_, node) => { if (node.type !== "campaignTie") onSelectGroup(String(node.data.group)); }}>
          <Background /><Controls showInteractive={false} />
        </ReactFlow>
      </div></div>
      <p className="campaign-muted">{tr(`그룹 간 연결 ${crossDependencies}개 · 내부 연결 ${overview.internalDependencies}개는 각 그룹에 요약됩니다. 드래그·스크롤로 이동하고 확대 버튼으로 조절하세요.`, `${crossDependencies} cross-group dependencies · ${overview.internalDependencies} internal dependencies summarized in their groups. Drag or scroll to pan; use the zoom controls to adjust.`)}</p>
    </>}
  </section>;
}
