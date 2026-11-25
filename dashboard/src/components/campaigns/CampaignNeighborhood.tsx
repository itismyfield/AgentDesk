import { useMemo, useRef } from "react";
import { Background, MarkerType, Position, ReactFlow, type Edge, type Node, type ReactFlowInstance } from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import type { CampaignNode } from "../../api/campaigns";
import { Cap, Chip, Num, RowButton } from "../common/KitPrimitives";
import { getGraphStatusTone } from "../common/graph/GraphChrome";
import { rawEdgeId } from "./campaignGraphRouting";
import { campaignTaskTitle, dependencyNeighborhood } from "./campaignModel";
import { LABELS, type Tr } from "./campaignPresentation";
import { ZoomTools } from "./RelationsCanvasChrome";
import { StatusMarker, nodeMarker } from "./StatusMarker";
import { taskLabel } from "./relationsEditorModel";

const FIT = { padding: 0.14, minZoom: 0.5, maxZoom: 1 } as const;

export default function CampaignNeighborhood({ nodes: allNodes, selectedId, visibleIds, onSelect, tr }: {
  nodes: CampaignNode[]; selectedId: string; visibleIds: Set<string>; onSelect: (id: string) => void; tr: Tr;
}) {
  const flow = useRef<ReactFlowInstance | null>(null);
  const context = useMemo(() => dependencyNeighborhood(allNodes, selectedId), [allNodes, selectedId]);
  const rows = Math.max(context.visibleUpstream.length, context.visibleDownstream.length, 1);
  const graphNodes: Node[] = context.visible.map((node) => {
    const upstreamIndex = context.visibleUpstream.findIndex((candidate) => candidate.id === node.id);
    const downstreamIndex = context.visibleDownstream.findIndex((candidate) => candidate.id === node.id);
    const selected = node.id === selectedId;
    const outside = context.externalDependencies.get(node.id)?.length ?? 0;
    const statusTone = getGraphStatusTone(node.status);
    const marker = nodeMarker(node.status, node.stage);
    const shadow = [
      `inset 3px 0 0 ${statusTone.accent}`,
      selected ? "0 0 0 3px var(--accent-soft)" : !statusTone.quiet ? "var(--shadow-sm)" : "",
    ].filter(Boolean).join(", ");
    return {
      id: node.id,
      position: { x: selected ? 280 : upstreamIndex >= 0 ? 0 : 560, y: selected ? (rows - 1) * 52 : Math.max(upstreamIndex, downstreamIndex) * 104 },
      sourcePosition: Position.Right,
      targetPosition: Position.Left,
      ariaLabel: `${node.title}, ${tr(...LABELS[node.status])}`,
      data: {
        label: (
          <div className="campaign-graph-node" title={`${node.title} · ${node.group || tr("미분류", "Ungrouped")}`}>
            <div className="flex min-w-0 items-center gap-1.5">
              <StatusMarker tone={marker.tone} ratio={marker.ratio} />
              <span className="shrink-0 font-mono text-[11px] font-medium text-[var(--fg-muted)] tabular-nums">{taskLabel(node)}</span>
              <span className="min-w-0 flex-1 truncate text-xs text-[var(--fg-muted)]">{node.group || tr("미분류", "Ungrouped")}</span>
              <Chip tone={statusTone.chipTone} className="h-[19px] px-1.5 text-[10.5px]">{tr(...LABELS[node.status])}</Chip>
            </div>
            <strong className={statusTone.quiet ? "text-[var(--fg-dim)]" : "text-[var(--fg)]"}>{campaignTaskTitle(node)}</strong>
            <span className="truncate text-xs leading-4 text-[var(--fg-muted)] tabular-nums">
              {[node.stage, `R${node.round}`, (outside > 0 || !visibleIds.has(node.id)) && (outside > 0 ? tr(`외부 선행 ${outside}`, `${outside} external dependencies`) : tr("필터 밖 작업", "Outside filters"))].filter(Boolean).join(" · ")}
            </span>
          </div>
        ),
      },
      style: {
        width: 224,
        padding: "8px 10px 8px 12px",
        borderWidth: 1,
        borderStyle: "solid",
        borderColor: selected ? "var(--accent)" : statusTone.border,
        borderRadius: 14,
        background: statusTone.quiet && !selected ? "var(--bg-1)" : "var(--th-card-bg)",
        color: "var(--fg)",
        opacity: statusTone.quiet && !selected ? 0.8 : 1,
        boxShadow: shadow || undefined,
      },
    };
  });
  const edges: Edge[] = context.edges.map((edge) => ({
    id: rawEdgeId(edge),
    source: edge[0],
    target: edge[1],
    data: { edge },
    type: "smoothstep",
    markerEnd: { type: MarkerType.ArrowClosed, color: "var(--info)", width: 14, height: 14 },
    style: { stroke: "var(--info)", strokeWidth: 1.75 },
  }));
  return <div className="campaign-neighborhood">
    <div className="campaign-row">
      <strong className="text-[13px] font-semibold text-[var(--fg)]">{tr("선행 → 선택 작업 → 후속", "Dependencies → selected → dependents")}</strong>
      <Cap><Num>{context.visible.length}/{context.upstream.length + context.downstream.length + 1}</Num></Cap>
    </div>
    <p className="campaign-muted m-0 text-xs">{tr("선택한 작업과 바로 이어진 작업이에요. 다른 그룹의 연결도 함께 보여요.", "Direct connections to the selected task, including other groups.")}</p>
    <div className="campaign-graph-scroll">
      <div className="campaign-graph relative" style={{ height: Math.max(300, rows * 104 + 64) }} aria-label={tr("선택 작업 의존 관계", "Selected task dependencies")}>
        <ReactFlow key={selectedId} nodes={graphNodes} edges={edges} proOptions={{ hideAttribution: true }}
          onInit={(instance) => { flow.current = instance; }} fitView fitViewOptions={FIT}
          minZoom={0.4} maxZoom={1.3} nodesDraggable={false} nodesConnectable={false} onNodeClick={(_, node) => onSelect(node.id)}>
          <Background gap={20} size={1.6} color="var(--line)" />
        </ReactFlow>
        <ZoomTools role="toolbar" aria-label={tr("보기 도구", "View tools")} flow={flow} fit={FIT} fitLabel={tr("화면 맞춤", "Fit to screen")} tr={tr} />
      </div>
    </div>
    {(context.omittedUpstream > 0 || context.omittedDownstream > 0) && <p role="status" className="m-0 text-xs text-[var(--fg-muted)]">{tr(`그래프 밖: 선행 ${context.omittedUpstream}개 · 후속 ${context.omittedDownstream}개. 아래 전체 연결에서 선택하세요.`, `Outside this graph: ${context.omittedUpstream} dependencies · ${context.omittedDownstream} dependents. Select from all connections below.`)}</p>}
    <details className="campaign-connections"><summary className="text-xs font-medium text-[var(--fg-dim)]">{tr("전체 연결", "All connections")} · {context.upstream.length + context.downstream.length}</summary><div>
      {context.upstream.map((node) => <RowButton key={`up:${node.id}`} onClick={() => onSelect(node.id)} className="gap-2 rounded-[var(--r-sm)] px-2.5 py-2 sm:px-3"><Cap className="shrink-0">← {tr("선행", "Dependency")}</Cap><strong className="min-w-0 truncate text-[13px]">{node.title}</strong><Cap className="shrink-0">{node.group || tr("미분류", "Ungrouped")}</Cap></RowButton>)}
      {context.downstream.map((node) => <RowButton key={`down:${node.id}`} onClick={() => onSelect(node.id)} className="gap-2 rounded-[var(--r-sm)] px-2.5 py-2 sm:px-3"><Cap className="shrink-0">→ {tr("후속", "Dependent")}</Cap><strong className="min-w-0 truncate text-[13px]">{node.title}</strong><Cap className="shrink-0">{node.group || tr("미분류", "Ungrouped")}</Cap></RowButton>)}
    </div></details>
  </div>;
}
