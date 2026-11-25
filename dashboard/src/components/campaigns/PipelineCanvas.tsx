import { Bot, Box, CircleX, Flag, Hand, Hourglass, Lock, Maximize, MessageCircleQuestion, MousePointer2, Play, RotateCw, ShieldCheck, SquarePlus, Zap, type LucideIcon } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { Background, BaseEdge, EdgeLabelRenderer, Handle, Position, ReactFlow, getBezierPath, type Edge, type EdgeProps, type Node, type NodeProps, type OnConnectEnd, type ReactFlowInstance } from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { IconButton } from "../../kit/free/icon-button-clean";
import { GraphMiniMap } from "../common/graph/GraphChrome";
import type { Tr } from "./campaignPresentation";
import { NEW_PIN } from "./pipelineEdit";
import { PIPE_WIDTH, cardHeight, type Line, type Pin, type PipelineGraph } from "./pipelineModel";
import { FLOAT, TOOL, ZoomTools, useLegible } from "./RelationsCanvasChrome";

const KIND_ICONS: Record<string, LucideIcon> = {
  agent_task: Bot, auto_check: ShieldCheck, auto_action: Zap, human_decision: MessageCircleQuestion, wait: Hourglass, end_success: Flag, end_failure: CircleX,
};
export const kindIcon = (kind: string) => KIND_ICONS[kind] ?? Box;
export const TONE_COLORS: Record<Line["tone"], string> = { pass: "var(--ok)", fail: "var(--err)", retry: "var(--warn)", other: "var(--fg-muted)", event: "var(--warn)" };
const CANVAS_HEIGHT = "max(560px, calc(100dvh - 25rem))";
const FIT = { padding: { x: "32px", top: "72px", bottom: "72px" }, minZoom: 0.1, maxZoom: 1 } as const;

/** Text the parent has already put into words, so the card only lays it out. */
export type StateNodeData = {
  kind: string; title: string; kindLabel: string; pins: Pin[]; entry: string; main: boolean; occupancy: { text: string; held: boolean }[];
  badges: { text: string; start?: boolean; off?: boolean }[]; foot: [string, string] | null;
  /** While editing: a check error on this state, whether the draft added or changed it, and its '+ 전환' pin's label. */
  flagged?: boolean; mark?: string | null; add?: string | null;
};
/** A pin let go over empty canvas: where, and where its pin sits, in pixels from the canvas's top left. */
export type PinDrop = { x: number; y: number; fx: number; fy: number };
/** Inline because React Flow's unlayered stylesheet outranks Tailwind's layered utilities. */
const dot = (color: string): CSSProperties => ({ position: "absolute", width: 10, height: 10, minWidth: 0, minHeight: 0, borderRadius: 999, border: `2px solid ${color}`, background: "var(--th-card-bg)" });

/** One state as a blueprint card: kind icon, name and kind, a 진입 pin at its left (top, off the main flow) and a
 * coloured pin per result at its right; a retry pin carries ↻n. Start and approval gates ride above it. */
function StateNode({ data, selected, isConnectable }: NodeProps) {
  const node = data as StateNodeData;
  const Icon = kindIcon(node.kind);
  // While editing, result pins start a line and the 진입 pin takes one.
  const entry = { type: "target", isConnectable, isConnectableStart: false } as const, out = { type: "source", isConnectable, isConnectableEnd: false } as const;
  const rows: (Pin | null)[] = [...node.pins, ...node.add ? [null] : []];
  return <div className={`relative grid rounded-[14px] border bg-[var(--th-card-bg)] text-xs text-[var(--fg-dim)] shadow-[var(--shadow-sm)] ${node.flagged ? "border-[var(--err)]" : selected ? "border-[var(--accent)]" : "border-[var(--line)]"} ${selected ? "shadow-[0_0_0_3px_var(--accent-soft)]" : node.flagged ? "shadow-[0_0_0_3px_var(--err-soft)]" : ""}`}
    style={{ width: PIPE_WIDTH }}>
    {node.badges.length > 0 && <div className="absolute bottom-full left-1/2 mb-2 flex -translate-x-1/2 gap-1.5">{node.badges.map((badge) => <span key={badge.text}
      className={`flex h-[26px] items-center gap-1.5 rounded-full border px-2.5 font-semibold whitespace-nowrap ${badge.start ? "border-[var(--accent)] bg-[var(--accent-soft)] text-[var(--accent-fg)]" : badge.off ? "border-dashed border-[var(--line)] bg-[var(--bg-2)] text-[var(--fg-muted)]" : "border-[color-mix(in_srgb,var(--warn)_45%,transparent)] bg-[var(--warn-soft)] text-[var(--warn)]"}`}>
      {badge.start ? <Play aria-hidden className="size-3" /> : <Lock aria-hidden className="size-3" />}{badge.text}</span>)}</div>}
    {!node.main && <Handle {...entry} position={Position.Top} style={{ ...dot("var(--fg-dim)"), zIndex: 1 }} />}
    <div className="flex min-w-0 items-center gap-2 border-b border-[var(--line-soft)] px-3 pt-2.5 pb-2">
      <span className={`grid size-6 shrink-0 place-items-center rounded-[7px] bg-[var(--bg-3)] ${node.kind === "end_success" ? "text-[var(--ok)]" : node.kind === "end_failure" ? "text-[var(--err)]" : ""}`}><Icon aria-hidden className="size-3.5" /></span>
      <span className="grid min-w-0 flex-1"><strong title={node.title} className="truncate text-sm leading-5 font-semibold text-[var(--fg)]">{node.title}</strong><span className="truncate text-[var(--fg-muted)]">{node.kindLabel}{node.mark && <b className="font-semibold text-[var(--warn)]"> · {node.mark}</b>}</span></span>
      {node.occupancy.map(({ text, held }) => <span key={text} className={`shrink-0 rounded-full px-1.5 leading-5 font-medium ${held ? "bg-[var(--warn-soft)] text-[var(--warn)]" : "bg-[var(--info-soft)] text-[var(--info)]"}`}>{text}</span>)}
    </div>
    <div className="grid content-start gap-1.5 px-3 py-2">{Array.from({ length: Math.max(1, rows.length) }, (_, row) => {
      const pin = rows[row];
      return <div key={pin?.id ?? row} className="relative -mx-3 flex min-h-[18px] items-center justify-between gap-2 px-3 text-[13px] leading-[18px]">
        {row === 0 && node.main ? <span><Handle {...entry} position={Position.Left} style={dot("var(--fg-dim)")} />{node.entry}</span> : <span />}
        {pin && <span className="flex min-w-0 items-center gap-1" style={{ color: TONE_COLORS[pin.tone] }}>
          <span className="truncate">{pin.label}{pin.hint && ` → ${pin.hint}`}</span>{pin.tone === "retry" && <><RotateCw aria-hidden className="size-3 shrink-0" /><span className="tabular-nums">{pin.transition.retry!.max}</span></>}
          <Handle {...out} id={pin.id} position={Position.Right} style={dot(TONE_COLORS[pin.tone])} />
        </span>}
        {pin === null && <span className="text-[var(--fg-muted)]">{node.add}<Handle {...out} id={NEW_PIN} position={Position.Right} style={dot("var(--accent)")} /></span>}
      </div>;
    })}</div>
    {node.foot && <div className="flex min-w-0 justify-between gap-2 border-t border-[var(--line-soft)] px-3 pt-1.5 pb-2 text-xs leading-[14px] text-[var(--fg-muted)]"><span className="truncate">{node.foot[0]}</span><span className="shrink-0">{node.foot[1]}</span></div>}
  </div>;
}

/** An event that wakes a state: a dashed card in the bottom row, its line dashed too. */
function EventNode({ data }: NodeProps) {
  const { title, target } = data as { title: string; target: string };
  return <div className="grid w-[184px] gap-0.5 rounded-xl border border-dashed border-[var(--warn)] bg-[var(--th-card-bg)] px-3 py-2 text-xs">
    <Handle type="source" id="event" position={Position.Top} isConnectable={false} style={{ ...dot("var(--warn)"), zIndex: 1 }} />
    <strong className="truncate text-[13px] font-semibold text-[var(--fg)]">{title}</strong><span className="truncate text-[var(--fg-muted)]">{target}</span>
  </div>;
}

type LineData = Pick<Line, "tone" | "lane" | "drop" | "retry"> & { dim: boolean };
const TURN = 24, R = 10;
/** A result line in its result's colour: a curve to the next column, a lane over the cards for a skip, under them for a
 * loop back, and a drop to the row below that runs along one shared lane. Width and badge keep their size zoomed out. */
function PipelineLine({ id, sourceX: sx, sourceY: sy, targetX: tx, targetY: ty, sourcePosition, targetPosition, data }: EdgeProps) {
  const { tone, lane, drop, retry, dim } = data as LineData;
  const scale = useLegible();
  const [curve, midX, midY] = getBezierPath({ sourceX: sx, sourceY: sy, sourcePosition, targetX: tx, targetY: ty, targetPosition });
  const turn = sx + TURN, side = tx < turn ? -1 : 1;
  const [path, x, y] = lane === undefined ? [curve, midX, midY]
    : drop ? [`M${sx},${sy} L${turn - R},${sy} Q${turn},${sy} ${turn},${sy + R} L${turn},${lane - R} Q${turn},${lane} ${turn + side * R},${lane} L${tx - side * R},${lane} Q${tx},${lane} ${tx},${lane + R} L${tx},${ty}`, tx, lane]
    : tx <= sx ? [`M${sx},${sy} C${turn},${sy} ${turn},${lane} ${sx},${lane} L${tx},${lane} C${tx - TURN},${lane} ${tx - TURN},${ty} ${tx},${ty}`, (sx + tx) / 2, lane]
    : [`M${sx},${sy} C${turn},${sy} ${turn},${lane} ${sx + 2 * TURN},${lane} L${tx - 2 * TURN},${lane} C${tx - TURN},${lane} ${tx - TURN},${ty} ${tx},${ty}`, (sx + tx) / 2, lane];
  const width = 2 * Math.min(scale, 2.5);
  return <>
    <BaseEdge id={id} path={path} style={{ stroke: TONE_COLORS[tone], strokeWidth: width, strokeLinecap: "round", strokeDasharray: tone === "event" ? `${3 * width} ${2.5 * width}` : undefined, opacity: dim ? 0.25 : 1 }} />
    {retry !== undefined && <EdgeLabelRenderer>
      <span className="pointer-events-none absolute flex items-center gap-0.5 rounded-full bg-[var(--warn-soft)] px-1.5 text-[11px] leading-[18px] font-semibold text-[var(--warn)] tabular-nums shadow-[0_0_0_3px_var(--bg)]"
        style={{ transform: `translate(-50%, -50%) translate(${x}px, ${y}px) scale(${scale})`, opacity: dim ? 0.4 : 1 }}><RotateCw aria-hidden className="size-3" />{retry}</span>
    </EdgeLabelRenderer>}
  </>;
}

const NODE_TYPES = { pipelineState: StateNode, pipelineEvent: EventNode };
const EDGE_TYPES = { pipelineLine: PipelineLine };
const swatch = (color: string, dashed = false) => <i aria-hidden className={`w-5 border-t-2 ${dashed ? "border-dashed" : ""}`} style={{ borderColor: color }} />;

/** The pipeline canvas: cards in tab order along the drawn flow, coloured lines, the key, zoom tools and a minimap. While
 * editing it fills the window over the checks, and a pin dragged to a state points there, or let go elsewhere asks where. */
export default function PipelineCanvas({ label, graph, nodeData, eventData, chosen, onChoose, reveal = null, edit, checks, inspector, children, tr }: {
  label: string; graph: PipelineGraph; nodeData: Map<string, StateNodeData & { aria: string }>; eventData: Map<string, { title: string; target: string }>;
  chosen: string | null; onChoose: (id: string | null) => void; inspector: ReactNode; children: ReactNode; tr: Tr;
  /** A state to bring into view (a check pointed at it). */
  reveal?: { id: string; n: number } | null;
  /** `busy` (a save is out) holds the pins and the tools that change the draft. */
  edit?: { busy: boolean; onLink: (from: string, pin: string, to: string) => void; onDrop: (from: string, pin: string, at: PinDrop) => void; onAdd: (button: HTMLElement) => void };
  checks?: ReactNode;
}) {
  const flow = useRef<ReactFlowInstance | null>(null);
  const [minimapOpen, setMinimapOpen] = useState(true);
  const [flowReady, setFlowReady] = useState(false);
  // While editing, select connects pins and picks cards; move only pans, cards included.
  const [tool, setTool] = useState<"select" | "move">("select");
  useEffect(() => { if (reveal) void flow.current?.fitView({ nodes: [{ id: reveal.id }], minZoom: 0.8, maxZoom: 1, duration: 200 }); }, [reveal]);
  // A card handed over as a new object keeps its last size, or React Flow hides it until it measures it again.
  const sizes = useRef(new Map<string, NonNullable<Node["measured"]>>());
  const nodes = useMemo(() => [...graph.main, ...graph.off].map((id): Node => ({ id, type: "pipelineState", position: graph.positions.get(id)!, data: nodeData.get(id)!,
    selected: id === chosen, draggable: false, ariaLabel: nodeData.get(id)!.aria, measured: sizes.current.get(id), initialWidth: PIPE_WIDTH, initialHeight: cardHeight(graph.rows.get(id)!) }))
    .sort((a, b) => Number(graph.off.includes(a.id)) - Number(graph.off.includes(b.id)) || a.position.x - b.position.x || a.position.y - b.position.y)
    .concat(graph.events.map((event) => ({ id: `event:${event.id}`, type: "pipelineEvent", position: graph.positions.get(`event:${event.id}`)!, data: eventData.get(event.id)!,
      draggable: false, selectable: false, focusable: false, initialWidth: PIPE_WIDTH, initialHeight: 56 }))), [graph, nodeData, eventData, chosen]);
  const edges = useMemo(() => graph.lines.map((line): Edge => ({ id: line.id, source: line.source, sourceHandle: line.handle, target: line.target, type: "pipelineLine", selectable: false, focusable: false,
    data: { tone: line.tone, lane: line.lane, drop: line.drop, retry: line.retry, dim: chosen !== null && line.source !== chosen && line.target !== chosen } satisfies LineData })), [graph, chosen]);
  const canvas = useRef<HTMLDivElement>(null);
  const full = !!edit, moving = full && tool === "move";
  // The click that ends a pin's drag lands on the pane; it keeps the choice the drag made.
  const ended = useRef(0);
  const dropped: OnConnectEnd = (event, state) => {
    ended.current = Date.now();
    const from = state.fromNode?.id, pin = state.fromHandle?.id, box = canvas.current?.getBoundingClientRect();
    if (!edit || !from || !pin || !box || !flow.current || !state.from) return;
    const point = "changedTouches" in event ? event.changedTouches[0] : event, under = document.elementFromPoint(point.clientX, point.clientY);
    const to = state.isValid && state.toNode ? state.toNode.id : under?.closest<HTMLElement>(".react-flow__node-pipelineState")?.dataset.id;
    if (to) return to === from && !state.isValid ? undefined : edit.onLink(from, pin, to);
    const start = flow.current.flowToScreenPosition(state.from);
    if (under?.closest(".react-flow__pane")) edit.onDrop(from, pin, { x: point.clientX - box.left, y: point.clientY - box.top, fx: start.x - box.left, fy: start.y - box.top });
  };
  // In view mode, automatically fit the entire pipeline graph so wide 9-stage templates never clip under the right inspector;
  // in full-window edit mode, keep at least 80% zoom on the start so pins remain easy to drag.
  const open = (instance: ReactFlowInstance) => {
    if (!full) {
      void instance.fitView(FIT);
      requestAnimationFrame(() => void instance.fitView(FIT));
      return;
    }
    const { x0, x1, y0, y1 } = graph.bounds, width = canvas.current?.clientWidth ?? 0, height = canvas.current?.clientHeight ?? 0;
    const fit = Math.min((width - 64) / (x1 - x0), (height - 120) / (y1 - y0)), zoom = Math.min(1, Math.max(0.8, fit));
    void instance.setViewport({ zoom, x: (zoom === fit ? (width - (x1 - x0) * zoom) / 2 : 32) - x0 * zoom, y: 24 + Math.max(0, (height - 120 - (y1 - y0) * zoom) / 2) - y0 * zoom });
  };
  // Re-fit only when entering/leaving the full-window editor or when the viewed graph's structure actually changes (never on 15s polls or draft edits).
  const shapeKey = `${[...graph.states.keys()].join("|")}#${graph.lines.length}`;
  const prevView = useRef({ full, shapeKey });
  useEffect(() => {
    const fullChanged = prevView.current.full !== full;
    const shapeChanged = prevView.current.shapeKey !== shapeKey;
    prevView.current = { full, shapeKey };
    if (flow.current && (fullChanged || (!full && shapeChanged))) open(flow.current);
  }, [full, shapeKey]); // eslint-disable-line react-hooks/exhaustive-deps
  return <section aria-label={label} style={{ "--pipeline-height": CANVAS_HEIGHT } as CSSProperties}
    className={full ? "grid min-h-0 flex-1 grid-cols-[minmax(0,1fr)_18rem] grid-rows-[minmax(0,1fr)] min-[1280px]:grid-cols-[minmax(0,1fr)_21rem]"
      : "grid min-w-0 min-[1280px]:h-[var(--pipeline-height)] min-[1280px]:grid-cols-[minmax(0,1fr)_18rem] min-[1280px]:grid-rows-[minmax(0,1fr)]"}>
    <div className="flex min-h-0 min-w-0 flex-col">
      <div ref={canvas} data-pipeline-canvas data-full={full || undefined} className="relative h-[var(--pipeline-height)] min-w-0 bg-[var(--bg)] data-full:h-auto data-full:min-h-0 data-full:flex-1 [&_.react-flow\_\_node:focus-visible>div]:outline-2 [&_.react-flow\_\_node:focus-visible>div]:outline-offset-2 [&_.react-flow\_\_node:focus-visible>div]:outline-[var(--accent)]">
        <ReactFlow nodes={nodes} edges={edges} nodeTypes={NODE_TYPES} edgeTypes={EDGE_TYPES} proOptions={{ hideAttribution: true }}
          onInit={(instance) => { flow.current = instance; setFlowReady(true); open(instance); }}
          minZoom={0.1} maxZoom={1.5} nodesDraggable={false} edgesFocusable={false} panOnScroll zoomOnScroll={false} zoomOnDoubleClick={false}
          nodesConnectable={full && !moving && !edit.busy} elementsSelectable={!moving} connectOnClick={false} onConnectEnd={dropped} connectionLineStyle={{ stroke: "var(--accent)", strokeWidth: 2, strokeDasharray: "6 5" }}
          deleteKeyCode={null} selectionKeyCode={null} multiSelectionKeyCode={null}
          // A click or Enter on a focused card arrives as a select change; the pane lets it go.
          onNodesChange={(changes) => {
            for (const change of changes) if (change.type === "dimensions" && change.dimensions) sizes.current.set(change.id, change.dimensions);
            const picked = changes.find((change) => change.type === "select" && change.selected);
            if (picked && "id" in picked) onChoose(picked.id);
            else if (changes.some((change) => change.type === "select") && Date.now() - ended.current > 300) onChoose(null);
          }}
          onPaneClick={() => Date.now() - ended.current > 300 && onChoose(null)}>
          <Background gap={20} size={1.6} color="var(--line)" />
          <GraphMiniMap open={minimapOpen} ready={flowReady} ariaLabel={tr("파이프라인 미니맵", "Pipeline minimap")} chosenId={chosen}
            nodeColor={(node) => node.id === chosen ? "var(--accent)" : "var(--fg-faint)"} />
        </ReactFlow>
        <div className={`${FLOAT} bottom-4 left-4 flex max-w-[calc(100%-12rem)] flex-wrap items-center gap-x-3.5 gap-y-1 px-3 py-2 text-xs text-[var(--fg-muted)]`}>
          <span className="flex items-center gap-1.5">{swatch("var(--ok)")}{tr("통과", "Pass")}</span>
          <span className="flex items-center gap-1.5">{swatch("var(--err)")}{tr("실패", "Fail")}</span>
          <span className="flex items-center gap-1.5"><RotateCw aria-hidden className="size-3 text-[var(--warn)]" />{tr("재시도", "Retry")}</span>
          <span className="flex items-center gap-1.5"><Lock aria-hidden className="size-3 text-[var(--warn)]" />{tr("승인", "Approval")}</span>
          {graph.events.length > 0 && <span className="flex items-center gap-1.5">{swatch("var(--warn)", true)}{tr("이벤트", "Event")}</span>}
        </div>
        <ZoomTools role="toolbar" aria-label={tr("보기 도구", "View tools")} flow={flow} fit={FIT} fitLabel={tr("화면 맞춤", "Fit to screen")}
          minimap={{ open: minimapOpen, onToggle: () => setMinimapOpen((v) => !v) }} tr={tr} />
        {edit && <div role="toolbar" aria-orientation="vertical" aria-label={tr("캔버스 도구", "Canvas tools")} className={`${FLOAT} top-1/2 left-4 flex -translate-y-1/2 flex-col gap-0.5 p-1`}>
          <IconButton icon={<MousePointer2 aria-hidden />} className={TOOL} label={tr("선택 · 핀을 끌어 이어요", "Select · drag a pin to connect")} pressed={tool === "select"} onClick={() => setTool("select")} />
          <IconButton icon={<Hand aria-hidden />} className={TOOL} label={tr("이동", "Move")} pressed={tool === "move"} onClick={() => setTool("move")} />
          <IconButton icon={<SquarePlus aria-hidden />} className={TOOL} label={tr("상태 추가", "Add a state")} disabled={edit.busy} onClick={(event) => edit.onAdd(event.currentTarget)} />
          <IconButton icon={<Maximize aria-hidden />} className={TOOL} label={tr("맞춤", "Fit")} onClick={() => void flow.current?.fitView(FIT)} />
        </div>}
        {children}
      </div>
      {checks}
    </div>
    {inspector}
  </section>;
}
