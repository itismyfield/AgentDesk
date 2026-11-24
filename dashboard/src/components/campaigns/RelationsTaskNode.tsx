import { Lock } from "lucide-react";
import { useState, type CSSProperties } from "react";
import { Handle, NodeToolbar, Position, type NodeProps } from "@xyflow/react";
import { MeterBar } from "../common/KitPrimitives";
import { GROUP_PIN_Y } from "./campaignGroupLayout";
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
  return <div className="flex min-w-0 items-start gap-2 px-3 pt-3 pb-2.5">
    <StatusMarker tone={tone} ratio={ratio} className="mt-0.5" />
    <span className="grid min-w-0 gap-0.5"><strong title={title} className="line-clamp-2 text-sm leading-[1.35] font-semibold text-[var(--fg)] [word-break:keep-all] [overflow-wrap:break-word]">{title}</strong><span className="truncate tabular-nums">{meta}</span></span>
  </div>;
}

/** One task on the relations canvas: the 선행 pin takes dependencies, the 다음 pin hands this task on. */
export function RelationsTaskNode({ data, selected }: NodeProps) {
  const task = data as TaskNodeData;
  const ring = task.error ? "border-[var(--err)]" : task.picked ? "border-[var(--info)]" : "border-[var(--line)]";
  return <div className={`${CARD} min-h-[148px] ${ring} ${selected ? CHOSEN : ""}`}>
    <NodeHead {...task} />
    <div className={PINS}>
      <Handle type="target" position={Position.Left} isConnectable={!task.locked} style={pin(task.locked)} />
      <span>{task.pins[0]}</span><span>{task.pins[1]}</span>
      <Handle type="source" position={Position.Right} style={pin(false)} />
    </div>
    <p className="m-0 px-3 pb-2.5 tabular-nums">{task.summary}</p>
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

/** Longest unbroken word width in `em` units so `dashboard`, `statechart`, or `클라우드감사` scales to one line cleanly. */
function longestWordEm(text: string): number {
  let max = 0;
  for (const part of text.split(/[\s·/|-]+/)) {
    let width = 0;
    for (const ch of part) width += (ch.codePointAt(0) ?? 0) > 0x2e7f ? 1.04 : 0.61;
    if (width > max) max = width;
  }
  return max;
}

/** Zoomed out, a card's name, its 선행/다음 words and one state line cover it at a legible size; generous line-height & padding
 * prevent descenders (`y`, `g`, `p`, `_`) from clipping, and word-aware sizing prevents mid-word breaks (`dashboa/rd`). */
function FarCover({ scale, title, line, ratio, error, pins, className }: {
  scale: number; title: string; line: string; ratio?: number; error?: boolean; pins?: [string, string]; className: string;
}) {
  const roomy = scale < 1.9;
  const sidePad = Math.max(14, Math.round(6.5 * scale));
  const innerWidth = Math.max(120, 240 - sidePad * 2 - 4);
  const maxEm = longestWordEm(title);
  const wordCap = maxEm > 0 ? Math.floor(innerWidth / maxEm) : 38;
  const baseFont = Math.min(12 * scale, wordCap, roomy ? 24 : 38);
  return <div data-far aria-hidden className={`absolute inset-0 flex flex-col justify-center gap-1.5 overflow-hidden rounded-[inherit] py-2.5 ${className}`} style={{ fontSize: baseFont, paddingInline: sidePad }}>
    <strong className="line-clamp-2 pb-[0.12em] text-[1.02em] leading-[1.42] font-semibold text-[var(--fg)] [word-break:keep-all] [overflow-wrap:break-word]">{title}</strong>
    {roomy && pins && <span className="flex justify-between text-[0.85em] leading-snug text-[var(--fg-muted)]"><span>{pins[0]}</span><span>{pins[1]}</span></span>}
    {ratio !== undefined && <MeterBar ratio={ratio} tone="ok" className="h-[0.28em] shrink-0" />}
    {roomy && <span className={`truncate pb-[0.08em] leading-snug tabular-nums ${error ? "text-[var(--err)]" : ""}`}>{line}</span>}
  </div>;
}
/** The 선행 and 다음 pins at their own size on screen over the shrunken handles, so a far card still reads as a node. */
function FarPins({ scale }: { scale: number }) {
  const dot = (side: "left" | "right") => <i aria-hidden className="absolute z-[1] rounded-full border-solid border-[var(--fg-muted)] bg-[var(--th-card-bg)]"
    style={{ [side]: 0, top: GROUP_PIN_Y, width: 10 * scale, height: 10 * scale, borderWidth: 1.5 * scale, transform: `translate(${side === "left" ? "-50%" : "50%"}, -50%)` }} />;
  return <>{dot("left")}{dot("right")}</>;
}
/** Zoomed out, a card keeps a 1px edge on screen (red when it fails); the chosen one adds a blue band outside it. */
const farRing = (scale: number, selected: boolean, error: boolean) =>
  ({ boxShadow: [`0 0 0 ${scale}px ${error ? "var(--err)" : "var(--line)"}`, selected && `0 0 0 ${3 * scale}px var(--info)`].filter(Boolean).join(", ") });

/** One group on the relations view, built like a task node. `--legible` lets a keyboard focus ring keep 2px on screen. */
export function RelationsGroupNode({ data, selected }: NodeProps) {
  const group = data as GroupNodeData;
  const scale = useLegible(), far = scale > FAR;
  return <div className={`${CARD} min-h-[172px] ${group.blocked ? "border-[var(--err)]" : "border-[var(--line)]"} ${selected && !far ? CHOSEN : ""}`}
    style={{ "--legible": scale, ...far ? farRing(scale, selected, !!group.blocked) : {} } as CSSProperties}>
    <NodeHead {...group} />
    <div className={PINS}>
      <Handle type="target" position={Position.Left} isConnectable={false} style={{ ...pin(false), zIndex: 1 }} />
      <span>{group.pins[0]}</span><span>{group.pins[1]}</span>
      <Handle type="source" position={Position.Right} isConnectable={false} style={{ ...pin(false), zIndex: 1 }} />
    </div>
    <div className="grid gap-1.5 px-3 pt-2 pb-3"><MeterBar ratio={group.ratio} tone="ok" /><span className="truncate leading-snug tabular-nums">{group.summary}</span></div>
    {group.blocked && <p className={`${FOOT} text-[var(--err)]`}>{group.blocked}</p>}
    {far && <><FarCover scale={scale} title={group.title} line={group.far} ratio={group.ratio} error={!!group.blocked} pins={group.pins} className="bg-[var(--th-card-bg)]" /><FarPins scale={scale} /></>}
  </div>;
}

/** Zoomed out, `cover` (number and title) heads the card and `far` is its state line. */
export type ViewTaskData = TaskNodeData & { cover: string; far: string };

/** A task on the read-only view: the editor's card, covered by its title, pins and state once zoomed out. */
export function ViewTaskNode(props: NodeProps) {
  const task = props.data as ViewTaskData;
  const scale = useLegible(), far = scale > FAR;
  return <div className="relative rounded-xl" style={{ "--legible": scale, ...far ? farRing(scale, props.selected, task.error) : {} } as CSSProperties}>
    <RelationsTaskNode {...props} selected={props.selected && !far} />
    {far && <><FarCover scale={scale} title={task.cover} line={task.far} error={task.error} pins={task.pins} className="bg-[var(--th-card-bg)]" /><FarPins scale={scale} /></>}
  </div>;
}

/** `in` tails are dependencies outside the group, at its left; `out` tails depend on it, at its right. `cover` is the
 * task's number, all a tail's short card has room for once zoomed out; `ends` says whether a line enters and leaves it. */
export type TailNodeData = { title: string; cover: string; group: string; side: "in" | "out"; ends: { enters: boolean; leaves: boolean } };

/** A task outside the group, faint at the canvas's edge with its group's name. Both pins are always there, since a task
 * can come before and after the group at once, and one no line uses is hidden. Zoomed out, its dashed edge keeps 1px on
 * screen. Choosing one leaves this view, so it draws no chosen ring. */
export function RelationsTailNode({ data }: NodeProps) {
  const tail = data as TailNodeData;
  const scale = useLegible(), far = scale > FAR;
  const tailPin = (shown: boolean) => ({ ...pin(false), zIndex: 1, opacity: shown ? 1 : 0 });
  return <div className="relative grid w-60 gap-0.5 rounded-xl border border-dashed border-[var(--line)] px-3 py-2.5 text-left text-xs text-[var(--fg-muted)]"
    style={{ "--legible": scale, ...far ? { borderWidth: scale } : {} } as CSSProperties}>
    <strong className="truncate text-[13px] font-medium text-[var(--fg-dim)]">{tail.title}</strong><span className="truncate">{tail.group}</span>
    <Handle type="target" position={Position.Left} isConnectable={false} style={tailPin(tail.ends.enters)} />
    <Handle type="source" position={Position.Right} isConnectable={false} style={tailPin(tail.ends.leaves)} />
    {far && <FarCover scale={scale} title={tail.cover} line={tail.group} className="bg-[var(--bg)] text-[var(--fg-muted)]" />}
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
