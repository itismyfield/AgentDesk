import { Lock } from "lucide-react";
import { useState } from "react";
import { Handle, NodeToolbar, Position, type NodeProps } from "@xyflow/react";
import { MeterBar } from "../common/KitPrimitives";
import { FAR, useLegible } from "./RelationsCanvasChrome";
import { StatusMarker, type MarkerTone } from "./StatusMarker";

export type TaskNodeData = {
  title: string; meta: string; tone: MarkerTone; ratio: number;
  pins: [needs: string, next: string]; summary: string;
  note: { tone: "lock" | "err"; text: string } | null;
  locked: boolean; error: boolean; picked: boolean;
};

/** Inline because React Flow's unlayered stylesheet outranks Tailwind's layered utilities. */
const pin = (locked: boolean) => ({ width: 10, height: 10, background: "var(--th-card-bg)", border: `1.5px solid ${locked ? "var(--fg-faint)" : "var(--fg-muted)"}` });
/** The card, head, pin row and foot band shared by task and group nodes, so both canvases draw one shape. */
const CARD = "relative grid w-60 rounded-xl border bg-[var(--th-card-bg)] text-left text-xs text-[var(--fg-muted)]";
const CHOSEN = "outline-2 outline-offset-2 outline-[var(--info)]";
const PINS = "relative flex items-center justify-between border-t border-[var(--line)] px-3 py-1.5";
const FOOT = "m-0 flex items-center gap-1.5 border-t border-[var(--line)] px-3 py-1.5";

function NodeHead({ tone, ratio, title, meta }: { tone: MarkerTone; ratio: number; title: string; meta: string }) {
  return <div className="flex min-w-0 items-start gap-2 px-3 pt-3 pb-2">
    <StatusMarker tone={tone} ratio={ratio} className="mt-0.5" />
    <span className="grid min-w-0"><strong title={title} className="truncate text-sm font-semibold text-[var(--fg)]">{title}</strong><span className="truncate tabular-nums">{meta}</span></span>
  </div>;
}

/** One task on the relations canvas: the 선행 pin takes dependencies, the 다음 pin hands this task on. */
export function RelationsTaskNode({ data, selected }: NodeProps) {
  const task = data as TaskNodeData;
  const ring = task.error ? "border-[var(--err)]" : task.picked ? "border-[var(--info)]" : "border-[var(--line)]";
  return <div className={`${CARD} ${ring} ${selected ? CHOSEN : ""}`}>
    <NodeHead {...task} />
    <div className={PINS}>
      <Handle type="target" position={Position.Left} isConnectable={!task.locked} style={pin(task.locked)} />
      <span>{task.pins[0]}</span><span>{task.pins[1]}</span>
      <Handle type="source" position={Position.Right} style={pin(false)} />
    </div>
    <p className="m-0 px-3 pb-2 tabular-nums">{task.summary}</p>
    {task.note && <p className={`${FOOT} ${task.note.tone === "err" ? "text-[var(--err)]" : ""}`}>
      {task.note.tone === "lock" && <Lock aria-hidden className="size-3" />}{task.note.text}
    </p>}
  </div>;
}

/** `far` is the short state line shown once zoomed out; `blocked` is the foot band's text, or null. */
export type GroupNodeData = {
  title: string; meta: string; tone: MarkerTone; ratio: number; pins: [needs: string, next: string];
  summary: string; blocked: string | null; far: string;
};

/** One group on the relations view, built like a task node. Zoomed out, its name and state cover it at a legible size. */
export function RelationsGroupNode({ data, selected }: NodeProps) {
  const group = data as GroupNodeData;
  const scale = useLegible();
  const far = scale > FAR;
  const ring = [selected && `0 0 0 ${2 * scale}px var(--info)`, group.blocked && `0 0 0 ${scale}px var(--err)`].filter(Boolean).join(", ");
  return <div className={`${CARD} ${group.blocked ? "border-[var(--err)]" : "border-[var(--line)]"} ${selected && !far ? CHOSEN : ""}`} style={far && ring ? { boxShadow: ring } : undefined}>
    <NodeHead {...group} />
    <div className={PINS}>
      <Handle type="target" position={Position.Left} isConnectable={false} style={{ ...pin(false), zIndex: 1 }} />
      <span>{group.pins[0]}</span><span>{group.pins[1]}</span>
      <Handle type="source" position={Position.Right} isConnectable={false} style={{ ...pin(false), zIndex: 1 }} />
    </div>
    <div className="grid gap-1.5 px-3 pt-2 pb-2.5"><MeterBar ratio={group.ratio} tone="ok" /><span className="truncate tabular-nums">{group.summary}</span></div>
    {group.blocked && <p className={`${FOOT} text-[var(--err)]`}>{group.blocked}</p>}
    {/* The state line goes first once the card is too short on screen for it; the meter and the ring still show it. */}
    {far && <div aria-hidden className="absolute inset-0 grid content-center gap-[0.35em] overflow-hidden rounded-[inherit] bg-[var(--th-card-bg)] px-[0.6em]" style={{ fontSize: 12 * scale }}>
      <strong className="line-clamp-2 text-[1.05em] leading-tight font-semibold text-[var(--fg)] [word-break:keep-all] [overflow-wrap:anywhere]">{group.title}</strong>
      <MeterBar ratio={group.ratio} tone="ok" className="h-[0.3em]" />
      {scale < 1.9 && <span className={`truncate tabular-nums ${group.blocked ? "text-[var(--err)]" : ""}`}>{group.far}</span>}
    </div>}
  </div>;
}

/** The chosen tie's menu; labels come translated, strands are the tie's lines as `#A → #B`. */
export type TieMenu = {
  labels: { menu: string; remove: string; detach: string; preview: string };
  strands: { key: string; label: string }[]; previewing: boolean;
  act: { remove: () => void; detach: (key: string) => void; preview: () => void };
};
/** `fixed` ties are only drawn: no grab cursor. */
export type TieNodeData = { count: number; menu: TieMenu | null; fixed?: boolean };

/** Every line through a tie enters and leaves at its centre, so they gather at the dot and fan out again. */
const centre = { left: "50%", top: "50%", width: 1, height: 1, minWidth: 0, minHeight: 0, border: 0, background: "transparent" };
const ITEM = "!m-0 flex min-h-9 w-full items-center justify-between gap-6 !rounded-md !border-0 !bg-transparent !px-2.5 text-left text-[13px] text-[var(--fg)] hover:!bg-[color-mix(in_oklch,var(--fg)_6%,transparent)]";
const KEY = "rounded border border-[var(--line)] px-1.5 font-mono text-[11px] text-[var(--fg-muted)]";

/** A cable tie: a dot the lines pass through, with the number of lines it holds. Chosen, its menu opens above it. */
export function RelationsTieNode({ data, selected }: NodeProps) {
  const { count, menu, fixed } = data as TieNodeData;
  const [detaching, setDetaching] = useState(false);
  return <div className={`grid size-11 place-items-center ${fixed ? "" : "cursor-grab"}`}>
    <Handle type="target" position={Position.Left} isConnectable={false} style={centre} />
    <span className={`relative size-5 rounded-full border-2 bg-[var(--th-card-bg)] ${selected ? "border-[var(--info)] ring-4 ring-[color-mix(in_oklch,var(--info)_25%,transparent)]" : "border-[var(--fg-muted)]"}`}>
      <span className="absolute -top-4 -right-4 min-w-5 rounded-full border border-[var(--line)] bg-[var(--th-card-bg)] px-1 text-center text-[11px] leading-4 text-[var(--fg)] tabular-nums">{count}</span>
    </span>
    <Handle type="source" position={Position.Right} isConnectable={false} style={centre} />
    {menu && <NodeToolbar isVisible position={Position.Top} offset={12}>
      <div role="group" aria-label={menu.labels.menu} className="nodrag nopan grid w-60 gap-0.5 rounded-lg border border-[var(--line)] bg-[var(--th-card-bg)] p-1 shadow-lg">
        <button type="button" className={ITEM} onClick={menu.act.remove}>{menu.labels.remove}<kbd className={KEY}>Delete</kbd></button>
        <button type="button" className={ITEM} aria-expanded={detaching} onClick={() => setDetaching(!detaching)}>{menu.labels.detach}</button>
        {detaching && menu.strands.map((strand) => <button key={strand.key} type="button" className={`${ITEM} !pl-5 text-[var(--fg-muted)]`} onClick={() => menu.act.detach(strand.key)}>{strand.label}</button>)}
        <button type="button" className={ITEM} aria-pressed={menu.previewing} onClick={menu.act.preview}>{menu.labels.preview}<kbd className={KEY}>H</kbd></button>
      </div>
    </NodeToolbar>}
  </div>;
}
