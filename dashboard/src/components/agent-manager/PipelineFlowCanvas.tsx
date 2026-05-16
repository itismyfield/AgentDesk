import { memo, useCallback, useMemo, type CSSProperties } from "react";
import {
  Background,
  Controls,
  Handle,
  MarkerType,
  MiniMap,
  Position,
  ReactFlow,
  type Connection,
  type Edge,
  type EdgeMouseHandler,
  type Node,
  type NodeMouseHandler,
  type NodeProps,
  type NodeTypes,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";

import { fsmStateTone, transitionAccent } from "./pipeline-visual-editor-styles";
import {
  buildFsmEdgeBindingKey,
  inferFsmEventName,
  type FsmEdgeBinding,
  type GraphEdge,
  type GraphNode,
  type PipelineGraphLayout,
  type Selection,
} from "./pipeline-visual-editor-model";

type Tr = (ko: string, en: string) => string;

interface PipelineStateNodeData extends Record<string, unknown> {
  compactGraph: boolean;
  isFsmVariant: boolean;
  node: GraphNode;
  selected: boolean;
  tr: Tr;
}

type PipelineStateNode = Node<PipelineStateNodeData, "pipelineState">;
type PipelineEdge = Edge<{ graphEdge: GraphEdge }, "smoothstep">;

interface PipelineFlowCanvasProps {
  compactGraph: boolean;
  fsmEdgeBindings: Record<string, FsmEdgeBinding>;
  graph: PipelineGraphLayout;
  graphPanelNote: string;
  isFsmVariant: boolean;
  onConnectTransition: (fromId: string, toId: string) => void;
  onSelectionChange: (selection: Selection) => void;
  selection: Selection;
  tr: Tr;
  useScrollableMobileFsmCanvas: boolean;
}

const FLOW_SHELL_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
  background:
    "radial-gradient(circle at top left, color-mix(in srgb, var(--th-accent-primary-soft) 74%, transparent) 0%, transparent 42%), radial-gradient(circle at bottom right, color-mix(in srgb, var(--th-badge-sky-bg) 64%, transparent) 0%, transparent 34%), color-mix(in srgb, var(--th-card-bg) 95%, transparent)",
};

const FSM_FLOW_SHELL_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-border) 82%, transparent)",
  background: "#0e1014",
};

const MUTED_TEXT_STYLE: CSSProperties = {
  color: "var(--th-text-muted)",
};

const PipelineStateNodeView = memo(function PipelineStateNodeView({
  data,
}: NodeProps<PipelineStateNode>) {
  const { compactGraph, isFsmVariant, node, selected, tr } = data;
  const tone = fsmStateTone(node.id);
  const handleStyle: CSSProperties = {
    width: 9,
    height: 9,
    borderColor: isFsmVariant ? "transparent" : "var(--th-card-bg)",
    background: isFsmVariant ? "transparent" : "var(--th-accent-primary)",
    opacity: isFsmVariant ? 0 : 0.88,
  };
  const meta = isFsmVariant
    ? `n=${node.index + 1}`
    : [
        node.hookCount > 0 ? `${node.hookCount}h` : null,
        node.hasClock ? "clock" : null,
        node.hasTimeout ? "timeout" : null,
      ]
        .filter(Boolean)
        .join(" · ") || tr("속성 없음", "No extras");

  return (
    <div
      className="rounded-[18px] border px-3 py-2 shadow-sm"
      style={{
        width: node.width,
        minHeight: node.height,
        borderColor: isFsmVariant
          ? tone.stroke
          : selected
            ? "var(--th-accent-primary)"
            : node.terminal
              ? "color-mix(in srgb, var(--th-accent-primary) 52%, #16a34a 48%)"
              : "color-mix(in srgb, var(--th-border) 88%, transparent)",
        borderWidth: selected ? 2 : 1.5,
        background: isFsmVariant
          ? "#141821"
          : node.terminal
            ? "color-mix(in srgb, var(--th-badge-emerald-bg) 82%, var(--th-card-bg) 18%)"
            : "color-mix(in srgb, var(--th-card-bg) 94%, transparent)",
        boxShadow: isFsmVariant
          ? `0 0 0 1px rgba(255,255,255,0.02), 0 14px 30px ${tone.glow}`
          : selected
            ? "0 0 0 4px color-mix(in srgb, var(--th-accent-primary-soft) 70%, transparent)"
            : undefined,
        color: "var(--th-text-primary)",
      }}
    >
      <Handle type="target" position={Position.Left} style={handleStyle} />
      <Handle type="source" position={Position.Right} style={handleStyle} />
      {isFsmVariant && (
        <div
          className="-mx-3 -mt-2 mb-2 h-[3px] rounded-t-[18px]"
          style={{ background: tone.stroke }}
        />
      )}
      <div
        className="truncate whitespace-nowrap text-[11px]"
        title={node.id}
        style={{
          color: isFsmVariant
            ? "rgba(148, 163, 184, 0.78)"
            : selected
              ? "var(--th-accent-primary)"
              : "var(--th-text-muted)",
          fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
        }}
      >
        {node.id}
      </div>
      <div
        className="mt-1 truncate whitespace-nowrap text-sm font-semibold"
        title={node.label}
        style={{
          color: node.terminal && !isFsmVariant
            ? "color-mix(in srgb, var(--th-accent-primary) 58%, #166534 42%)"
            : "var(--th-text-primary)",
        }}
      >
        {node.label}
      </div>
      <div
        className={`${compactGraph && !isFsmVariant ? "mt-1 text-[10px]" : "mt-2 text-[11px]"} truncate whitespace-nowrap`}
        title={meta}
        style={{
          color: isFsmVariant ? "rgba(148, 163, 184, 0.74)" : "var(--th-text-muted)",
        }}
      >
        {meta}
      </div>
    </div>
  );
});

const nodeTypes = {
  pipelineState: PipelineStateNodeView,
} satisfies NodeTypes;

function edgeLabel(edge: GraphEdge, isFsmVariant: boolean, fsmEdgeBindings: Record<string, FsmEdgeBinding>, tr: Tr) {
  if (isFsmVariant) {
    const bindingKey = buildFsmEdgeBindingKey(edge.from, edge.to);
    return fsmEdgeBindings[bindingKey]?.event ?? inferFsmEventName(edge.from, edge.to);
  }
  if (edge.type === "free") {
    return tr("자동", "auto");
  }
  if (edge.type === "gated") {
    return edge.gates.length > 0
      ? tr(`조건${edge.gates.length}`, `cond${edge.gates.length}`)
      : tr("조건부", "cond");
  }
  return String(edge.type);
}

export default function PipelineFlowCanvas({
  compactGraph,
  fsmEdgeBindings,
  graph,
  graphPanelNote,
  isFsmVariant,
  onConnectTransition,
  onSelectionChange,
  selection,
  tr,
  useScrollableMobileFsmCanvas,
}: PipelineFlowCanvasProps) {
  const nodes = useMemo<PipelineStateNode[]>(
    () =>
      graph.nodes.map((node) => {
        const selected = selection?.kind === "state" && selection.stateId === node.id;
        return {
          id: node.id,
          type: "pipelineState",
          position: { x: node.x, y: node.y },
          sourcePosition: Position.Right,
          targetPosition: Position.Left,
          selected,
          focusable: true,
          ariaLabel: `${tr("상태", "State")} ${node.label}`,
          data: {
            compactGraph,
            isFsmVariant,
            node,
            selected,
            tr,
          },
        };
      }),
    [compactGraph, graph.nodes, isFsmVariant, selection, tr],
  );

  const edges = useMemo<PipelineEdge[]>(
    () =>
      graph.edges.map((edge) => {
        const selected = selection?.kind === "transition" && selection.index === edge.index;
        const accent = transitionAccent(edge.type);
        const stroke = isFsmVariant
          ? selected
            ? "var(--th-accent-primary)"
            : "rgba(148, 163, 184, 0.72)"
          : accent.stroke;
        return {
          id: edge.key,
          source: edge.from,
          target: edge.to,
          type: "smoothstep",
          data: { graphEdge: edge },
          selected,
          markerEnd: {
            type: MarkerType.ArrowClosed,
            color: stroke,
            width: 18,
            height: 18,
          },
          label: edgeLabel(edge, isFsmVariant, fsmEdgeBindings, tr),
          labelShowBg: true,
          labelBgPadding: [8, 4],
          labelBgBorderRadius: 10,
          labelBgStyle: {
            fill: isFsmVariant
              ? selected
                ? "color-mix(in srgb, var(--th-accent-primary-soft) 44%, #151922 56%)"
                : "#141821"
              : selected
                ? "color-mix(in srgb, var(--th-accent-primary-soft) 74%, var(--th-card-bg) 26%)"
                : "color-mix(in srgb, var(--th-card-bg) 94%, transparent)",
            stroke,
            strokeWidth: 1,
          },
          labelStyle: {
            fill: isFsmVariant ? stroke : "var(--th-text-primary)",
            fontSize: 11,
            fontWeight: 700,
            whiteSpace: "nowrap",
            fontFamily: isFsmVariant
              ? "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace"
              : undefined,
          },
          style: {
            stroke,
            strokeOpacity: selected ? 0.96 : isFsmVariant ? 0.82 : 0.68,
            strokeWidth: selected ? (isFsmVariant ? 2.4 : 3.2) : isFsmVariant ? 1.8 : 2.4,
          },
          interactionWidth: 22,
          focusable: true,
          ariaLabel: `${tr("전환", "Transition")} ${edge.from} ${tr("에서", "to")} ${edge.to}`,
        };
      }),
    [fsmEdgeBindings, graph.edges, isFsmVariant, selection, tr],
  );

  const handleNodeClick = useCallback<NodeMouseHandler<PipelineStateNode>>(
    (_event, node) => {
      onSelectionChange({ kind: "state", stateId: node.id });
    },
    [onSelectionChange],
  );

  const handleEdgeClick = useCallback<EdgeMouseHandler<PipelineEdge>>(
    (_event, edge) => {
      onSelectionChange({ kind: "transition", index: edge.data?.graphEdge.index ?? -1 });
    },
    [onSelectionChange],
  );

  const handleConnect = useCallback(
    (connection: Connection) => {
      if (isFsmVariant || !connection.source || !connection.target) {
        return;
      }
      onConnectTransition(connection.source, connection.target);
    },
    [isFsmVariant, onConnectTransition],
  );

  return (
    <div className="space-y-3">
      <div
        className="overflow-hidden rounded-[24px] border p-2 sm:p-3"
        style={isFsmVariant ? FSM_FLOW_SHELL_STYLE : FLOW_SHELL_STYLE}
        data-testid={useScrollableMobileFsmCanvas ? "fsm-canvas-scroll" : undefined}
      >
        <div
          className="rounded-[20px]"
          style={{
            height: isFsmVariant ? (useScrollableMobileFsmCanvas ? 360 : 420) : compactGraph ? 460 : 520,
            minHeight: isFsmVariant ? 340 : 420,
          }}
        >
          <ReactFlow
            nodes={nodes}
            edges={edges}
            nodeTypes={nodeTypes}
            fitView
            fitViewOptions={{ padding: isFsmVariant ? 0.18 : 0.16 }}
            maxZoom={1.6}
            minZoom={0.35}
            nodesDraggable={false}
            nodesConnectable={!isFsmVariant}
            elementsSelectable
            panOnDrag
            panOnScroll
            zoomOnPinch
            zoomOnScroll={false}
            preventScrolling={false}
            onNodeClick={handleNodeClick}
            onEdgeClick={handleEdgeClick}
            onConnect={handleConnect}
            proOptions={{ hideAttribution: true }}
            colorMode="dark"
          >
            <Background
              gap={isFsmVariant ? 24 : 28}
              size={isFsmVariant ? 1 : 1.2}
              color={isFsmVariant ? "rgba(148, 163, 184, 0.22)" : "rgba(148, 163, 184, 0.18)"}
            />
            <Controls showInteractive={false} position="bottom-left" />
            {!useScrollableMobileFsmCanvas && (
              <MiniMap
                pannable
                zoomable
                position="bottom-right"
                nodeColor={(node) => {
                  const graphNode = (node.data as PipelineStateNodeData).node;
                  return isFsmVariant ? fsmStateTone(graphNode.id).stroke : "var(--th-accent-primary)";
                }}
              />
            )}
          </ReactFlow>
        </div>
      </div>

      <div className="flex flex-wrap items-center gap-3 text-xs" style={MUTED_TEXT_STYLE}>
        {isFsmVariant ? (
          <>
            <span className="inline-flex items-center gap-2">
              <span
                className="inline-block h-px w-5"
                style={{ background: "rgba(148, 163, 184, 0.7)" }}
              />
              <span style={{ fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace" }}>
                {tr("전환", "edge")}
              </span>
            </span>
            <span className="inline-flex items-center gap-2">
              <span
                className="inline-block h-px w-5"
                style={{ background: "var(--th-accent-primary)" }}
              />
              <span style={{ fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace" }}>
                {tr("선택됨", "selected")}
              </span>
            </span>
            <span
              className="ml-auto"
              style={{ fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace" }}
            >
              {`states:${graph.nodes.length} · transitions:${graph.edges.length}`}
            </span>
          </>
        ) : (
          <span>{graphPanelNote}</span>
        )}
      </div>
    </div>
  );
}
