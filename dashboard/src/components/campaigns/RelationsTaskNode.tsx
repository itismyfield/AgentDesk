import { Lock } from "lucide-react";
import { useState } from "react";
import { Handle, NodeToolbar, Position, type NodeProps } from "@xyflow/react";
import { StatusMarker, type MarkerTone } from "./StatusMarker";

export type TaskNodeData = {
  title: string; meta: string; tone: MarkerTone; ratio: number;
  pins: [needs: string, next: string]; summary: string;
  note: { tone: "lock" | "err"; text: string } | null;
  locked: boolean; error: boolean; picked: boolean;
};

/** Inline because React Flow's unlayered stylesheet outranks Tailwind's layered utilities. */
const pin = (locked: boolean) => ({ width: 10, height: 10, background: "var(--th-card-bg)", border: `1.5px solid ${locked ? "var(--fg-faint)" : "var(--fg-muted)"}` });

/** One task on the relations canvas: the 선행 pin takes dependencies, the 다음 pin hands this task on. */
export function RelationsTaskNode({ data, selected }: NodeProps) {
  const task = data as TaskNodeData;
  const ring = task.error ? "border-[var(--err)]" : task.picked ? "border-[var(--info)]" : "border-[var(--line)]";
  return <div className={`grid w-60 rounded-xl border bg-[var(--th-card-bg)] text-left text-xs text-[var(--fg-muted)] ${ring} ${selected ? "outline-2 outline-offset-2 outline-[var(--info)]" : ""}`}>
    <div className="flex min-w-0 items-start gap-2 px-3 pt-3 pb-2">
      <StatusMarker tone={task.tone} ratio={task.ratio} className="mt-0.5" />
      <span className="grid min-w-0"><strong className="truncate text-sm font-semibold text-[var(--fg)]">{task.title}</strong><span className="truncate tabular-nums">{task.meta}</span></span>
    </div>
    <div className="relative flex items-center justify-between border-t border-[var(--line)] px-3 py-1.5">
      <Handle type="target" position={Position.Left} isConnectable={!task.locked} style={pin(task.locked)} />
      <span>{task.pins[0]}</span><span>{task.pins[1]}</span>
      <Handle type="source" position={Position.Right} style={pin(false)} />
    </div>
    <p className="m-0 px-3 pb-2 tabular-nums">{task.summary}</p>
    {task.note && <p className={`m-0 flex items-center gap-1.5 border-t border-[var(--line)] px-3 py-1.5 ${task.note.tone === "err" ? "text-[var(--err)]" : ""}`}>
      {task.note.tone === "lock" && <Lock aria-hidden className="size-3" />}{task.note.text}
    </p>}
  </div>;
}

/** The chosen tie's menu; labels come translated, strands are the tie's lines as `#A → #B`. */
export type TieMenu = {
  labels: { menu: string; remove: string; detach: string; preview: string };
  strands: { key: string; label: string }[]; previewing: boolean;
  act: { remove: () => void; detach: (key: string) => void; preview: () => void };
};
export type TieNodeData = { count: number; menu: TieMenu | null };

/** Every line through a tie enters and leaves at its centre, so they gather at the dot and fan out again. */
const centre = { left: "50%", top: "50%", width: 1, height: 1, minWidth: 0, minHeight: 0, border: 0, background: "transparent" };
const ITEM = "!m-0 flex min-h-9 w-full items-center justify-between gap-6 !rounded-md !border-0 !bg-transparent !px-2.5 text-left text-[13px] text-[var(--fg)] hover:!bg-[color-mix(in_oklch,var(--fg)_6%,transparent)]";
const KEY = "rounded border border-[var(--line)] px-1.5 font-mono text-[11px] text-[var(--fg-muted)]";

/** A cable tie: a dot the lines pass through, with the number of lines it holds. Chosen, its menu opens above it. */
export function RelationsTieNode({ data, selected }: NodeProps) {
  const { count, menu } = data as TieNodeData;
  const [detaching, setDetaching] = useState(false);
  return <div className="grid size-11 cursor-grab place-items-center">
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
