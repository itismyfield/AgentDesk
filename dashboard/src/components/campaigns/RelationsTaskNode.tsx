import { Lock } from "lucide-react";
import { Handle, Position, type NodeProps } from "@xyflow/react";
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
      <span className="grid min-w-0"><strong className="truncate text-[13px] font-semibold text-[var(--fg)]">{task.title}</strong><span className="truncate tabular-nums">{task.meta}</span></span>
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
