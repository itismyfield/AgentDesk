import {
  memo,
  useCallback,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
  type CSSProperties,
} from "react";
import { Map, Maximize, ZoomIn, ZoomOut } from "lucide-react";
import {
  Background,
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
  type ReactFlowInstance,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";

import { IconButton } from "../../kit/free/icon-button-clean";
import { Select } from "../../kit/free/select-clean";
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

const FLOAT_CHROME =
  "absolute z-10 rounded-xl border border-[var(--line-soft)] bg-[var(--th-card-bg)] shadow-[var(--shadow-sm)]";
const TOOL_BTN =
  "!size-10 [&>span]:!size-[18px] aria-pressed:!bg-[var(--accent-soft)] aria-pressed:!text-[var(--fg)]";

function subscribeDocumentTheme(onStoreChange: () => void): () => void {
  if (typeof document === "undefined" || typeof MutationObserver === "undefined") {
    return () => {};
  }
  const observer = new MutationObserver(onStoreChange);
  observer.observe(document.documentElement, {
    attributes: true,
    attributeFilter: ["data-theme", "class"],
  });
  return () => observer.disconnect();
}

function getDocumentColorMode(): "light" | "dark" {
  if (typeof document === "undefined") return "dark";
  return document.documentElement.dataset.theme === "light" ? "light" : "dark";
}

function getServerColorMode(): "light" | "dark" {
  return "dark";
}

/** Inline port style so React Flow's unlayered stylesheet does not override the 10x10 blueprint handle. */
function handlePortStyle(color: string): CSSProperties {
  return {
    width: 10,
    height: 10,
    minWidth: 0,
    minHeight: 0,
    borderRadius: 999,
    border: `2px solid ${color}`,
    background: "var(--th-card-bg)",
  };
}

const PipelineStateNodeView = memo(function PipelineStateNodeView({
  data,
}: NodeProps<PipelineStateNode>) {
  const { compactGraph, isFsmVariant, node, selected, tr } = data;
  const tone = fsmStateTone(node.id, node.terminal);
  const indicatorColor = selected ? "var(--accent)" : tone.stroke;
  const meta = isFsmVariant
    ? `n=${node.index + 1}`
    : [
        node.hookCount > 0 ? `${node.hookCount}h` : null,
        node.hasClock ? "clock" : null,
      ]
        .filter(Boolean)
        .join(" · ") || tr("속성 없음", "No extras");

  return (
    <div
      className={`relative rounded-[14px] border bg-[var(--th-card-bg)] px-3 py-2.5 text-xs text-[var(--fg-dim)] shadow-[var(--shadow-sm)] ${
        selected
          ? "border-[var(--accent)] shadow-[0_0_0_3px_var(--accent-soft)]"
          : "border-[var(--line)]"
      }`}
      style={{
        width: node.width,
        minHeight: node.height,
      }}
    >
      <Handle type="target" position={Position.Left} style={handlePortStyle("var(--fg-dim)")} />
      <Handle type="source" position={Position.Right} style={handlePortStyle(indicatorColor)} />
      <div
        aria-hidden
        className="-mx-3 -mt-2.5 mb-2 h-[3px] rounded-t-[13px]"
        style={{ background: indicatorColor }}
      />
      <div
        className={`truncate whitespace-nowrap font-mono text-[11px] ${
          selected ? "text-[var(--accent)]" : "text-[var(--fg-muted)]"
        }`}
        title={node.id}
      >
        {node.id}
      </div>
      <div
        className={`mt-0.5 truncate whitespace-nowrap text-sm leading-5 font-semibold ${
          node.terminal && !isFsmVariant ? "text-[var(--ok)]" : "text-[var(--fg)]"
        }`}
        title={node.label}
      >
        {node.label}
      </div>
      <div
        className={`${
          compactGraph && !isFsmVariant ? "mt-1 pt-1 text-[10px]" : "mt-1.5 pt-1.5 text-[11px]"
        } truncate whitespace-nowrap border-t border-[var(--line-soft)] text-[var(--fg-muted)]`}
        title={meta}
      >
        {meta}
      </div>
    </div>
  );
});

const nodeTypes = {
  pipelineState: PipelineStateNodeView,
} satisfies NodeTypes;

function edgeLabel(
  edge: GraphEdge,
  isFsmVariant: boolean,
  fsmEdgeBindings: Record<string, FsmEdgeBinding>,
  tr: Tr,
) {
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
  const colorMode = useSyncExternalStore(
    subscribeDocumentTheme,
    getDocumentColorMode,
    getServerColorMode,
  );
  const flowRef = useRef<ReactFlowInstance<PipelineStateNode, PipelineEdge> | null>(null);
  const minimapAllowed = !compactGraph && !useScrollableMobileFsmCanvas;
  const [showMiniMap, setShowMiniMap] = useState(false);
  const fitOptions = useMemo(() => ({ padding: 0.18 }), []);

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
        const stroke = selected
          ? "var(--accent)"
          : isFsmVariant
            ? "var(--fg-muted)"
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
            fill: selected ? "var(--accent-soft)" : "var(--th-card-bg)",
            stroke,
            strokeWidth: 1,
          },
          labelStyle: {
            fill: selected ? "var(--accent-fg)" : isFsmVariant ? "var(--fg-dim)" : "var(--fg)",
            fontSize: 11,
            fontWeight: 600,
            whiteSpace: "nowrap",
            fontFamily: isFsmVariant
              ? "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace"
              : undefined,
          },
          style: {
            stroke,
            strokeOpacity: selected ? 1 : 0.8,
            strokeWidth: selected ? 2.6 : 2,
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
      <div className="sm:hidden">
        <Select
          label={tr("편집할 상태 또는 전환", "State or transition to edit")}
          aria-label={tr("편집할 상태 또는 전환", "State or transition to edit")}
          placeholder={tr("선택하세요", "Choose an item")}
          value={
            selection?.kind === "state" || selection?.kind === "transition"
              ? JSON.stringify(selection)
              : null
          }
          onChange={(nextValue) => {
            if (nextValue) onSelectionChange(JSON.parse(nextValue) as Selection);
          }}
          options={[
            ...graph.nodes.map((node) => ({
              value: JSON.stringify({ kind: "state", stateId: node.id }),
              label: `${node.label} · ${node.id}`,
              description: tr("상태", "State"),
            })),
            ...graph.edges.map((edge) => ({
              value: JSON.stringify({ kind: "transition", index: edge.index }),
              label: `${edge.from} → ${edge.to}`,
              description: tr("전환", "Transition"),
            })),
          ]}
        />
      </div>

      <div
        className="overflow-hidden rounded-[14px] border border-[var(--line-soft)] bg-[var(--bg)]"
        data-testid={useScrollableMobileFsmCanvas ? "fsm-canvas-scroll" : "pipeline-flow-canvas"}
      >
        <div
          className="relative [&_.react-flow__node:focus-visible>div]:outline-2 [&_.react-flow__node:focus-visible>div]:outline-offset-2 [&_.react-flow__node:focus-visible>div]:outline-[var(--accent)]"
          style={{
            height: Math.min(440, Math.max(300, graph.height + 56)),
            minHeight: 300,
          }}
        >
          <ReactFlow
            nodes={nodes}
            edges={edges}
            nodeTypes={nodeTypes}
            onInit={(instance) => {
              flowRef.current = instance;
            }}
            fitView
            fitViewOptions={fitOptions}
            maxZoom={1.6}
            minZoom={0.35}
            nodesDraggable={false}
            nodesConnectable={!isFsmVariant}
            elementsSelectable
            panOnDrag
            panOnScroll={false}
            zoomOnPinch={false}
            zoomOnScroll={false}
            zoomOnDoubleClick={false}
            preventScrolling={false}
            onNodeClick={handleNodeClick}
            onEdgeClick={handleEdgeClick}
            onConnect={handleConnect}
            connectionLineStyle={{ stroke: "var(--accent)", strokeWidth: 2, strokeDasharray: "6 5" }}
            proOptions={{ hideAttribution: true }}
            colorMode={colorMode}
          >
            <Background gap={20} size={1.6} color="var(--line)" />
            {minimapAllowed && showMiniMap && (
              <MiniMap
                pannable
                zoomable={false}
                ariaLabel={tr("파이프라인 미니맵", "Pipeline minimap")}
                position="bottom-right"
                bgColor="var(--th-card-bg)"
                maskColor="color-mix(in srgb, var(--bg) 55%, transparent)"
                nodeBorderRadius={6}
                style={{
                  marginBottom: 12,
                  marginRight: 196,
                  width: 132,
                  height: 76,
                  border: "1px solid var(--line-soft)",
                  borderRadius: 12,
                }}
                nodeColor={(node) => {
                  const graphNode = (node.data as PipelineStateNodeData).node;
                  if (node.selected) return "var(--accent)";
                  return fsmStateTone(graphNode.id, graphNode.terminal).stroke;
                }}
              />
            )}
          </ReactFlow>

          <div
            role="toolbar"
            aria-label={tr("보기 도구", "View tools")}
            className={`${FLOAT_CHROME} right-3 bottom-3 flex gap-0.5 p-1`}
          >
            <IconButton
              icon={<ZoomOut aria-hidden />}
              className={TOOL_BTN}
              label={tr("축소", "Zoom out")}
              onClick={() => void flowRef.current?.zoomOut()}
            />
            <IconButton
              icon={<ZoomIn aria-hidden />}
              className={TOOL_BTN}
              label={tr("확대", "Zoom in")}
              onClick={() => void flowRef.current?.zoomIn()}
            />
            <IconButton
              icon={<Maximize aria-hidden />}
              className={TOOL_BTN}
              label={tr("화면 맞춤", "Fit to screen")}
              onClick={() => void flowRef.current?.fitView(fitOptions)}
            />
            {minimapAllowed && (
              <IconButton
                icon={<Map aria-hidden />}
                className={TOOL_BTN}
                label={tr("미니맵 표시 전환", "Toggle minimap")}
                pressed={showMiniMap}
                onClick={() => setShowMiniMap((prev) => !prev)}
              />
            )}
          </div>
        </div>
      </div>

      <div className="flex flex-wrap items-center gap-3 text-xs text-[var(--fg-muted)]">
        {isFsmVariant ? (
          <>
            <span className="inline-flex items-center gap-2">
              <span className="inline-block h-px w-5 bg-[var(--fg-muted)]" />
              <span className="font-mono">{tr("전환", "edge")}</span>
            </span>
            <span className="inline-flex items-center gap-2">
              <span className="inline-block h-px w-5 bg-[var(--accent)]" />
              <span className="font-mono">{tr("선택됨", "selected")}</span>
            </span>
            <span className="ml-auto font-mono">
              {tr(
                `상태:${graph.nodes.length} · 전환:${graph.edges.length}`,
                `states:${graph.nodes.length} · transitions:${graph.edges.length}`,
              )}
            </span>
          </>
        ) : (
          <span>{graphPanelNote}</span>
        )}
      </div>
    </div>
  );
}
