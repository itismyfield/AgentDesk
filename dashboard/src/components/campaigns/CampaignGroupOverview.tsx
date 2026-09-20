import { useMemo } from "react";
import { Background, Controls, MarkerType, Position, ReactFlow, type Edge, type Node } from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import type { CampaignNode } from "../../api/campaigns";
import { campaignGroup, groupCampaignNodes } from "./campaignModel";

type Props = {
  nodes: CampaignNode[];
  onSelectGroup: (group: string) => void;
  tr: (ko: string, en: string) => string;
};

/** A group projection can contain cycles even when the underlying task graph is a DAG. */
export function buildCampaignGroupOverview(nodes: CampaignNode[]) {
  const groups = groupCampaignNodes(nodes).map((group, index) => ({
    ...group,
    id: `campaign-group-${index}`,
    position: { x: (index % 3) * 310, y: Math.floor(index / 3) * 210 },
    internalDependencies: 0,
  }));
  const groupsByKey = new Map(groups.map((group) => [group.key, group]));
  const nodesById = new Map(nodes.map((node) => [node.id, node]));
  const connections = new Map<string, { source: string; target: string; count: number }>();
  for (const node of nodes) {
    const target = groupsByKey.get(campaignGroup(node))!;
    for (const dependency of node.dependencies) {
      const dependencyNode = nodesById.get(dependency);
      if (!dependencyNode) continue;
      const source = groupsByKey.get(campaignGroup(dependencyNode))!;
      if (source.id === target.id) {
        target.internalDependencies++;
        continue;
      }
      const key = `${source.id}:${target.id}`;
      const connection = connections.get(key) ?? { source: source.id, target: target.id, count: 0 };
      connection.count++;
      connections.set(key, connection);
    }
  }
  return {
    groups,
    connections: [...connections.values()].sort((a, b) => a.source.localeCompare(b.source) || a.target.localeCompare(b.target)),
    internalDependencies: groups.reduce((total, group) => total + group.internalDependencies, 0),
  };
}

export default function CampaignGroupOverview({ nodes, onSelectGroup, tr }: Props) {
  const overview = useMemo(() => buildCampaignGroupOverview(nodes), [nodes]);
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
      style: { width: 244, minHeight: 145, padding: 14, border: `1px solid ${counts.blocked ? "#d97706" : "var(--th-border)"}`, borderRadius: 10, background: "var(--th-card-bg)", color: "var(--th-text-primary)", cursor: "pointer" },
    };
  });
  const edges: Edge[] = overview.connections.map((connection) => ({
    id: `${connection.source}:${connection.target}`,
    source: connection.source,
    target: connection.target,
    type: "smoothstep",
    label: String(connection.count),
    ariaLabel: tr(`그룹 간 의존성 ${connection.count}개`, `${connection.count} dependencies between groups`),
    markerEnd: { type: MarkerType.ArrowClosed, color: "var(--th-text-muted)" },
    style: { stroke: "var(--th-text-muted)", strokeWidth: 1.5 },
    labelStyle: { fill: "var(--th-text-primary)", fontWeight: 600, fontSize: 12 },
    labelBgStyle: { fill: "var(--th-card-bg)" },
    labelBgPadding: [6, 3],
    labelBgBorderRadius: 4,
  }));
  const crossDependencies = overview.connections.reduce((total, connection) => total + connection.count, 0);
  return <section className="campaign-neighborhood" aria-label={tr("그룹별 전체 관계", "All campaign group relationships")}>
    <div className="campaign-row"><strong>{tr("그룹별 전체 관계", "All campaign group relationships")}</strong><span>{tr(`그룹 ${overview.groups.length}개 · 작업 ${nodes.length}개`, `${overview.groups.length} groups · ${nodes.length} tasks`)}</span></div>
    <p className="campaign-muted">{tr("전체 작업의 그룹 관계입니다. 화살표는 선행 그룹 → 후속 그룹, 숫자는 연결된 의존성 수입니다. 그룹을 선택하면 해당 작업 목록으로 이동합니다.", "Groups across all tasks. Arrows point from dependency to dependent; numbers count the connecting dependencies. Select a group to open its task list.")}</p>
    {nodes.length === 0 ? <p>{tr("표시할 작업이 없습니다.", "No tasks to display.")}</p> : <>
      <div className="campaign-graph-scroll"><div className="campaign-graph" style={{ height: Math.min(580, Math.max(300, Math.ceil(overview.groups.length / 3) * 210)) }}>
        <ReactFlow nodes={graphNodes} edges={edges} fitView fitViewOptions={{ padding: 0.18, minZoom: 0.65, maxZoom: 1 }} minZoom={0.65} maxZoom={1.4}
          nodesDraggable={false} nodesConnectable={false} panOnScroll zoomOnScroll={false}
          onNodeClick={(_, node) => onSelectGroup(String(node.data.group))}>
          <Background /><Controls showInteractive={false} />
        </ReactFlow>
      </div></div>
      <p className="campaign-muted">{tr(`그룹 간 연결 ${crossDependencies}개 · 내부 연결 ${overview.internalDependencies}개는 각 그룹에 요약됩니다. 드래그·스크롤로 이동하고 확대 버튼으로 조절하세요.`, `${crossDependencies} cross-group dependencies · ${overview.internalDependencies} internal dependencies summarized in their groups. Drag or scroll to pan; use the zoom controls to adjust.`)}</p>
    </>}
  </section>;
}
