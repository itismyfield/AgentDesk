import { useMemo, useRef, type CSSProperties, type ReactNode } from "react";
import { Background, MarkerType, ReactFlow, type Edge, type Node, type NodeProps, type ReactFlowInstance } from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import type { CableTie, DisplayEdge } from "./campaignGraphRouting";
import type { Tr } from "./campaignPresentation";
import { RelationsLegend, RelationsLine, ZoomTools, useLegible, type LineData } from "./RelationsCanvasChrome";
import { RelationsGroupNode, RelationsTailNode, RelationsTieNode, ViewTaskNode, type TieNodeData } from "./RelationsTaskNode";

/** A drawn line: its raw edges, how many, a loop lane when it runs backwards, and `faint` when it leaves the scope. */
export type ViewLine = DisplayEdge & { count: number; loopY?: number; faint?: boolean };

const tieNodeId = (id: string) => `campaign-tie:${id}`;
/** A saved tie, only drawn, kept at its own size on screen when zoomed out; its lines meet at the centre it scales around. */
function FixedTieNode(props: NodeProps) {
  return <div style={{ transform: `scale(${useLegible()})` }}><RelationsTieNode {...props} /></div>;
}
const NODE_TYPES = { relationsGroup: RelationsGroupNode, relationsTask: ViewTaskNode, relationsTail: RelationsTailNode, relationsTie: FixedTieNode };
const EDGE_TYPES = { relationsLine: RelationsLine };
/** The canvas fills the window's height below the page header, never under 560px. */
const CANVAS_HEIGHT = "max(560px, calc(100dvh - 21rem))";
/** Fitting leaves the bottom strip to the legend and the zoom tools. */
const FIT = { padding: { x: "32px", top: "24px", bottom: "72px" }, minZoom: 0.1, maxZoom: 1 } as const;

/** The read-only canvas both relations views share. `components` (looped parts, in display ids) and `cycles` (their
 * shortest loops) align by index: lines inside a part are red, and each part is labelled once, on a backwards line if any. */
export default function RelationsViewCanvas({ label, nodes, lines, ties, cycles, components, cycleText, lineLabel, onSelect, onOpen, inspector, tr }: {
  label: string; nodes: Node[]; lines: ViewLine[]; ties: CableTie[]; cycles: string[][]; components: Set<string>[]; cycleText: (path: string[]) => string;
  lineLabel: (line: ViewLine) => string; onSelect: (id: string | null) => void; onOpen?: (id: string) => void; inspector: ReactNode; tr: Tr;
}) {
  const flow = useRef<ReactFlowInstance | null>(null);
  // A card handed over as a new object is hidden until React Flow measures it again unless it brings its last size, and
  // the first click of a double-click (a new choice) rebuilds every card, so the second would land on the pane.
  const sizes = useRef(new Map<string, NonNullable<Node["measured"]>>());
  const { graphNodes, edges } = useMemo(() => {
    // Cards go in screen order so Tab walks the graph as drawn: linked cards column by column, then the shelf row by row.
    const linked = new Set(lines.flatMap((line) => [line.source, line.target]));
    const order = ({ id, position: { x, y } }: Node) => linked.has(id) ? [0, x, y] : [1, y, x];
    const graphNodes = [...nodes].sort((a, b) => { const [p, q] = [order(a), order(b)]; return p[0] - q[0] || p[1] - q[1] || p[2] - q[2]; });
    for (const tie of ties) {
      const count = lines.filter((line) => line.via.includes(tie.id)).reduce((sum, line) => sum + line.count, 0);
      graphNodes.push({ id: tieNodeId(tie.id), type: "relationsTie", position: tie.position, draggable: false, selectable: false, focusable: false,
        ariaLabel: tr(`케이블타이, 선 ${count}개`, `Cable tie, ${count} lines`), data: { count, menu: null, fixed: true } satisfies TieNodeData });
    }
    const at = (id: string) => nodes.find((node) => node.id === id)?.position.x ?? 0;
    const within = (part: Set<string>) => (line: ViewLine) => part.has(line.source) && part.has(line.target);
    const named = new Map<string, string>();
    components.forEach((part, index) => {
      const path = cycles[index], own = lines.filter(within(part));
      const steps = new Set(path.slice(1).map((to, step) => JSON.stringify([path[step], to])));
      const back = (pool: ViewLine[]) => pool.find((line) => at(line.target) <= at(line.source)) ?? pool[0];
      const host = back(own.filter((line) => steps.has(JSON.stringify([line.source, line.target])))) ?? back(own);
      if (host) named.set(host.id, cycleText(path));
    });
    const edges = lines.flatMap((line): Edge[] => {
      const error = components.some((part) => within(part)(line));
      const chain = [line.source, ...line.via.map(tieNodeId), line.target];
      return chain.slice(1).map((target, index) => {
        const last = index === chain.length - 2;
        const data: LineData & { edges: DisplayEdge["edges"] } = { error, faint: line.faint, edges: line.edges, loopY: line.loopY, ...last ? { count: line.count, cycle: named.get(line.id) } : {} };
        return { id: chain.length > 2 ? `${line.id}#${index}` : line.id, source: chain[index], target, type: "relationsLine", data, selectable: false, focusable: false,
          ...last ? { ariaLabel: lineLabel(line), markerEnd: { type: MarkerType.ArrowClosed, color: error ? "var(--err)" : "var(--fg-faint)", width: 12, height: 12 } } : {} };
      });
    });
    return { graphNodes: graphNodes.map((node) => sizes.current.has(node.id) ? { ...node, measured: sizes.current.get(node.id) } : node), edges };
  }, [nodes, lines, ties, cycles, components, cycleText, lineLabel, tr]);

  return <section aria-label={label} style={{ "--relations-height": CANVAS_HEIGHT } as CSSProperties}
    className="grid min-w-0 min-[1280px]:h-[var(--relations-height)] min-[1280px]:grid-cols-[minmax(0,1fr)_18rem] min-[1280px]:grid-rows-[minmax(0,1fr)]">
    <div data-relations-canvas className="relative h-[var(--relations-height)] min-w-0 bg-[var(--bg)]">
      <ReactFlow nodes={graphNodes} edges={edges} nodeTypes={NODE_TYPES} edgeTypes={EDGE_TYPES} proOptions={{ hideAttribution: true }}
        onInit={(instance) => { flow.current = instance; }} fitView fitViewOptions={FIT}
        minZoom={0.1} maxZoom={1.5} nodesDraggable={false} nodesConnectable={false} edgesFocusable={false} panOnScroll zoomOnScroll={false} zoomOnDoubleClick={false}
        deleteKeyCode={null} selectionKeyCode={null} multiSelectionKeyCode={null}
        // A click or Enter on a focused card arrives as a select change; Escape or the pane lets it go.
        onNodesChange={(changes) => {
          for (const change of changes) if (change.type === "dimensions" && change.dimensions) sizes.current.set(change.id, change.dimensions);
          const picked = changes.find((change) => change.type === "select" && change.selected);
          if (picked && "id" in picked) onSelect(picked.id);
          else if (changes.some((change) => change.type === "select")) onSelect(null);
        }}
        onNodeDoubleClick={(_, node) => onOpen?.(node.id)} onPaneClick={() => onSelect(null)}>
        <Background gap={20} size={1.6} color="var(--line)" />
      </ReactFlow>
      <RelationsLegend className="max-w-[calc(100%-12rem)]" tr={tr} />
      <ZoomTools role="toolbar" aria-label={tr("보기 도구", "View tools")} flow={flow} fit={FIT} fitLabel={tr("화면 맞춤", "Fit to screen")} tr={tr} />
    </div>
    {inspector}
  </section>;
}
