import { Fragment, useEffect, useRef, useState, type PointerEvent } from "react";
import { Handle, Position, type NodeProps } from "@xyflow/react";
import { tieHeight, tieSlotTop, type Point } from "./campaignTieGeometry";

export type TieNodeData = { slots: number; selected: boolean };

/** Inline because React Flow's unlayered stylesheet outranks the layered campaign styles. */
const slotHandle = (slot: number) => ({ top: tieSlotTop(slot), width: 4, height: 4, minWidth: 0, minHeight: 0, border: 0, background: "transparent" });

/** A tie handle: each strand enters and leaves at its own slot, so tied lines run side by side. */
export function CampaignTieNode({ data }: NodeProps) {
  const { slots, selected } = data as TieNodeData;
  return <div className="campaign-tie" data-selected={selected} style={{ height: tieHeight(slots) }}>
    <i aria-hidden />
    {Array.from({ length: slots }, (_, slot) => <Fragment key={slot}>
      <Handle type="target" id={`in-${slot}`} position={Position.Left} isConnectable={false} style={slotHandle(slot)} />
      <Handle type="source" id={`out-${slot}`} position={Position.Right} isConnectable={false} style={slotHandle(slot)} />
    </Fragment>)}
  </div>;
}

type Stroke = { screen: Point; local: Point };

/**
 * Tie mode's drawing surface over the graph: a stroke across lines selects them, and it takes the pointer
 * from pan so drawing never moves the view. Mouse, pen and touch share the pointer path.
 */
export function CrossingOverlay({ label, toFlow, onActive, onStroke }: {
  label: string;
  toFlow: (screen: Point) => Point;
  /** True from pointer down until release or cancel, so the stroke counts as an action in progress. */
  onActive: (active: boolean) => void;
  /** `done` is false while drawing (preview), true on release; a cancelled stroke reports null. */
  onStroke: (stroke: { from: Point; to: Point } | null, done: boolean) => void;
}) {
  const start = useRef<Stroke | null>(null);
  const [line, setLine] = useState<{ a: Point; b: Point } | null>(null);
  const read = (event: PointerEvent<HTMLDivElement>): Stroke => {
    const rect = event.currentTarget.getBoundingClientRect();
    return { screen: { x: event.clientX, y: event.clientY }, local: { x: event.clientX - rect.left, y: event.clientY - rect.top } };
  };
  const report = (event: PointerEvent<HTMLDivElement>, done: boolean) => {
    const from = start.current;
    if (!from) return;
    const to = read(event);
    if (done) { start.current = null; setLine(null); } else setLine({ a: from.local, b: to.local });
    onStroke({ from: toFlow(from.screen), to: toFlow(to.screen) }, done);
    if (done) onActive(false);
  };
  const cancel = () => { if (start.current) { start.current = null; setLine(null); onStroke(null, true); onActive(false); } };
  // Leaving tie mode mid-stroke unmounts the surface; the stroke must not stay in progress.
  const activeRef = useRef(onActive);
  activeRef.current = onActive;
  useEffect(() => () => { if (start.current) activeRef.current(false); }, []);
  return <div className="campaign-tie-stroke nopan nodrag nowheel" role="presentation" aria-label={label}
    onPointerDown={(event) => { event.currentTarget.setPointerCapture?.(event.pointerId); start.current = read(event); onActive(true); }}
    onPointerMove={(event) => report(event, false)} onPointerUp={(event) => report(event, true)} onPointerCancel={cancel} onLostPointerCapture={cancel}>
    {line && <svg aria-hidden><line x1={line.a.x} y1={line.a.y} x2={line.b.x} y2={line.b.y} /></svg>}
  </div>;
}
