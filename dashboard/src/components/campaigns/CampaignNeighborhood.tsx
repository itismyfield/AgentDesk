import { useMemo } from "react";
import { Background, Controls, MarkerType, Position, ReactFlow } from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import type { CampaignNode } from "../../api/campaigns";
import { dependencyNeighborhood } from "./campaignModel";
import { COLORS, LABELS, type Tr } from "./campaignPresentation";

export default function CampaignNeighborhood({ nodes: allNodes, selectedId, visibleIds, onSelect, tr }: {
  nodes: CampaignNode[]; selectedId: string; visibleIds: Set<string>; onSelect: (id: string) => void; tr: Tr;
}) {
  const context = useMemo(() => dependencyNeighborhood(allNodes, selectedId), [allNodes, selectedId]);
  const rows = Math.max(context.visibleUpstream.length, context.visibleDownstream.length, 1);
  const graphNodes = context.visible.map((node) => {
    const upstreamIndex = context.visibleUpstream.findIndex((candidate) => candidate.id === node.id);
    const downstreamIndex = context.visibleDownstream.findIndex((candidate) => candidate.id === node.id);
    const selected = node.id === selectedId;
    const outside = context.externalDependencies.get(node.id)?.length ?? 0;
    return {
      id: node.id, position: { x: selected ? 210 : upstreamIndex >= 0 ? 0 : 420, y: selected ? (rows - 1) * 36 : Math.max(upstreamIndex, downstreamIndex) * 72 },
      sourcePosition: Position.Right, targetPosition: Position.Left,
      ariaLabel: `${node.title}, ${tr(...LABELS[node.status])}`,
      data: { label: <div className="campaign-graph-node" title={`${node.title} · ${node.group || tr("미분류", "Ungrouped")}`}><strong>{node.title}</strong><span>{tr(...LABELS[node.status])} · {node.stage} · R{node.round}</span>{(outside > 0 || !visibleIds.has(node.id)) && <small>{outside > 0 ? tr(`외부 선행 ${outside}`, `${outside} external dependencies`) : tr("필터 밖 작업", "Outside filters")}</small>}</div> },
      style: { width: 176, height: 60, border: `1px solid ${COLORS[node.status]}`, borderRadius: 7, background: "var(--th-card-bg)", color: "var(--th-text-primary)", boxShadow: selected ? "0 0 0 2px var(--th-accent-info)" : undefined },
    };
  });
  const edges = [
    ...context.visibleUpstream.map((node) => ({ source: node.id, target: selectedId })),
    ...context.visibleDownstream.map((node) => ({ source: selectedId, target: node.id })),
  ].map((edge) => ({ ...edge, id: `${edge.source}:${edge.target}`, type: "smoothstep", markerEnd: { type: MarkerType.ArrowClosed }, style: { stroke: "var(--th-text-muted)", strokeWidth: 1.5 } }));
  return <div className="campaign-neighborhood">
    <div className="campaign-row"><strong>{tr("선행 → 선택 작업 → 후속", "Dependencies → selected → dependents")}</strong><span>{context.visible.length}/{context.upstream.length + context.downstream.length + 1}</span></div>
    <p className="campaign-muted">{tr("선택한 작업과 직접 연결된 작업입니다. 다른 그룹·필터 밖의 연결도 함께 표시합니다.", "Direct connections to the selected task, including other groups and tasks outside your filters.")}</p>
    <div className="campaign-graph-scroll"><div className="campaign-graph" style={{ height: Math.max(260, rows * 72 + 40) }} aria-label={tr("선택 작업 의존 관계", "Selected task dependencies")}>
      <ReactFlow key={selectedId} nodes={graphNodes} edges={edges} fitView minZoom={0.65} maxZoom={1.2} nodesDraggable={false} nodesConnectable={false} onNodeClick={(_, node) => onSelect(node.id)}>
        <Background /><Controls showInteractive={false} />
      </ReactFlow>
    </div></div>
    {(context.omittedUpstream > 0 || context.omittedDownstream > 0) && <p role="status">{tr(`그래프 밖: 선행 ${context.omittedUpstream}개 · 후속 ${context.omittedDownstream}개. 아래 전체 연결에서 선택하세요.`, `Outside this graph: ${context.omittedUpstream} dependencies · ${context.omittedDownstream} dependents. Select from all connections below.`)}</p>}
    <details className="campaign-connections"><summary>{tr("전체 연결", "All connections")} · {context.upstream.length + context.downstream.length}</summary><div>
      {context.upstream.map((node) => <button key={`up:${node.id}`} onClick={() => onSelect(node.id)}><span>← {tr("선행", "Dependency")}</span><strong>{node.title}</strong><small>{node.group || tr("미분류", "Ungrouped")}</small></button>)}
      {context.downstream.map((node) => <button key={`down:${node.id}`} onClick={() => onSelect(node.id)}><span>→ {tr("후속", "Dependent")}</span><strong>{node.title}</strong><small>{node.group || tr("미분류", "Ungrouped")}</small></button>)}
    </div></details>
  </div>;
}
