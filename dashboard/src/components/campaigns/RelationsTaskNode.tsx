import { Lock } from "lucide-react";
import { useState, type CSSProperties } from "react";
import { Handle, NodeToolbar, Position, type NodeProps } from "@xyflow/react";
import { Chip, MeterBar } from "../common/KitPrimitives";
import { getGraphStatusTone, graphHandleDot } from "../common/graph/GraphChrome";
import { FAR, useLegible } from "./RelationsCanvasChrome";
import { StatusMarker, type MarkerTone } from "./StatusMarker";

export type TaskNodeData = {
  title: string;
  meta: string;
  code?: string;
  status?: string;
  statusLabel?: string;
  tone: MarkerTone;
  ratio: number;
  summary: string;
  note: { tone: "lock" | "err" | "warn"; text: string } | null;
  locked: boolean;
  error: boolean;
  picked: boolean;
};

/** One task on the relations canvas: compact blueprint card with left/right edge handles and status accent bar. */
export function RelationsTaskNode({ data, selected }: NodeProps) {
  const task = data as TaskNodeData;
  const statusTone = getGraphStatusTone(task.error ? "failed" : task.status ?? task.tone);
  const borderColor = selected
    ? "var(--accent)"
    : task.error
      ? "var(--err)"
      : task.picked
        ? "var(--info)"
        : statusTone.border;
  const ringShadow = selected
    ? "0 0 0 3px var(--accent-soft)"
    : task.error
      ? "0 0 0 3px var(--err-soft)"
      : task.picked
        ? "0 0 0 3px var(--info-soft)"
        : statusTone.quiet
          ? "none"
          : "var(--shadow-sm)";
  return (
    <div
      data-quiet={statusTone.quiet || undefined}
      className={`relative grid w-60 rounded-[14px] border border-solid text-left text-xs text-[var(--fg-muted)] ${
        statusTone.quiet && !selected ? "bg-[var(--bg-1)] opacity-80" : "bg-[var(--th-card-bg)]"
      }`}
      style={{ borderColor, boxShadow: ringShadow }}
    >
      <Handle
        type="target"
        position={Position.Left}
        isConnectable={!task.locked}
        style={{ ...graphHandleDot(task.locked ? "var(--fg-faint)" : statusTone.pinColor), zIndex: 2 }}
      />
      <div className="relative grid overflow-hidden rounded-[13px]">
        <i
          aria-hidden
          className="pointer-events-none absolute inset-y-0 left-0 w-[3px]"
          style={{ background: task.error ? "var(--err)" : statusTone.accent }}
        />
        <div
          className="grid gap-1 border-b border-[var(--line-soft)] pt-2.5 pr-3 pb-2 pl-3.5"
          style={{ background: statusTone.headerBg }}
        >
          <div className="flex min-w-0 items-center gap-1.5">
            <StatusMarker tone={task.tone} ratio={task.ratio} />
            {task.code && (
              <span className="shrink-0 font-mono text-[11px] font-medium text-[var(--fg-muted)] tabular-nums">
                {task.code}
              </span>
            )}
            <span className="min-w-0 flex-1 truncate text-[11px] text-[var(--fg-muted)] tabular-nums">{task.meta}</span>
            {task.statusLabel && (
              <Chip tone={statusTone.chipTone} className="h-[19px] px-1.5 text-[10.5px]">
                {task.statusLabel}
              </Chip>
            )}
          </div>
          <strong
            title={task.title}
            className={`line-clamp-2 text-[13px] leading-[1.35] font-semibold [word-break:keep-all] [overflow-wrap:break-word] ${
              statusTone.quiet ? "text-[var(--fg-dim)]" : "text-[var(--fg)]"
            }`}
          >
            {task.title}
          </strong>
        </div>
        <div className="grid gap-1.5 py-2 pr-3 pl-3.5 text-[11px] leading-4">
          {task.ratio > 0 && !statusTone.quiet && <MeterBar ratio={task.ratio} tone="info" className="h-1" />}
          <div className="flex min-w-0 items-center justify-between gap-2">
            <span className="min-w-0 truncate tabular-nums">{task.summary}</span>
            {task.note && (
              <span
                className={`flex shrink-0 items-center gap-1 font-medium ${
                  task.note.tone === "err"
                    ? "text-[var(--err)]"
                    : task.note.tone === "warn"
                      ? "text-[var(--warn)]"
                      : "text-[var(--fg-muted)]"
                }`}
              >
                {task.note.tone === "lock" && <Lock aria-hidden className="size-3" />}
                {task.note.text}
              </span>
            )}
          </div>
        </div>
      </div>
      <Handle
        type="source"
        position={Position.Right}
        style={{ ...graphHandleDot(statusTone.pinColor), zIndex: 2 }}
      />
    </div>
  );
}

/** `far` is the short state line shown once zoomed out; `blocked` is the bottleneck warning text, or null. */
export type GroupNodeData = {
  title: string;
  meta: string;
  code?: string;
  statusLabel?: string;
  tone: MarkerTone;
  ratio: number;
  summary: string;
  blocked: string | null;
  far: string;
  openLabel?: string;
  onOpen?: () => void;
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

/** Zoomed out, a card's name, meter bar, and one state line cover it at a legible size. */
function FarCover({ scale, title, line, ratio, error, className }: {
  scale: number; title: string; line: string; ratio?: number; error?: boolean; className: string;
}) {
  const roomy = scale < 1.9;
  const sidePad = Math.max(14, Math.round(6.5 * scale));
  const innerWidth = Math.max(120, 240 - sidePad * 2 - 4);
  const maxEm = longestWordEm(title);
  const wordCap = maxEm > 0 ? Math.floor(innerWidth / maxEm) : 38;
  const baseFont = Math.min(12 * scale, wordCap, roomy ? 24 : 38);
  return <div data-far aria-hidden className={`absolute inset-0 flex flex-col justify-center gap-1.5 overflow-hidden rounded-[inherit] py-2.5 ${className}`} style={{ fontSize: baseFont, paddingInline: sidePad }}>
    <strong className="line-clamp-2 pb-[0.12em] text-[1.02em] leading-[1.42] font-semibold text-[var(--fg)] [word-break:keep-all] [overflow-wrap:break-word]">{title}</strong>
    {ratio !== undefined && <MeterBar ratio={ratio} tone="ok" className="h-[0.28em] shrink-0" />}
    {roomy && <span className={`truncate pb-[0.08em] leading-snug tabular-nums ${error ? "text-[var(--err)]" : ""}`}>{line}</span>}
  </div>;
}

/** Edge handle pins scaled on screen over the shrunken handles when zoomed far out. */
function FarPins({ scale, color = "var(--fg-muted)" }: { scale: number; color?: string }) {
  const dot = (side: "left" | "right") => <i aria-hidden className="absolute z-[2] rounded-full border-solid bg-[var(--th-card-bg)]"
    style={{ [side]: 0, top: "50%", width: 10 * scale, height: 10 * scale, borderWidth: 2 * scale, borderColor: color, transform: `translate(${side === "left" ? "-50%" : "50%"}, -50%)` }} />;
  return <>{dot("left")}{dot("right")}</>;
}

/** Zoomed out, a card keeps a 1px edge on screen (red when blocked/failing); the chosen one adds a blue band outside it. */
const farRing = (scale: number, selected: boolean, error: boolean) =>
  ({ boxShadow: [`0 0 0 ${scale}px ${error ? "var(--err)" : "var(--line)"}`, selected && `0 0 0 ${3 * scale}px var(--info)`].filter(Boolean).join(", ") });

/** One group on the relations view, styled as a blueprint card with an explicit Open action button. */
export function RelationsGroupNode({ data, selected }: NodeProps) {
  const group = data as GroupNodeData;
  const scale = useLegible(), far = scale > FAR;
  const statusTone = getGraphStatusTone(group.blocked ? "blocked" : group.tone);
  const borderColor = selected && !far ? "var(--accent)" : group.blocked ? "var(--warn)" : statusTone.border;
  const nearShadow = selected ? "0 0 0 3px var(--accent-soft)" : statusTone.quiet ? "none" : "var(--shadow-sm)";
  return (
    <div
      data-quiet={statusTone.quiet || undefined}
      className={`relative grid w-60 rounded-[14px] border border-solid text-left text-xs text-[var(--fg-muted)] ${
        statusTone.quiet && !selected ? "bg-[var(--bg-1)] opacity-80" : "bg-[var(--th-card-bg)]"
      }`}
      style={{
        "--legible": scale,
        borderColor,
        ...(far ? farRing(scale, selected, !!group.blocked) : { boxShadow: nearShadow }),
      } as CSSProperties}
    >
      <Handle type="target" position={Position.Left} isConnectable={false} style={{ ...graphHandleDot(statusTone.pinColor), zIndex: 2 }} />
      <div className="relative grid overflow-hidden rounded-[13px]">
        <i aria-hidden className="pointer-events-none absolute inset-y-0 left-0 w-[3px]" style={{ background: statusTone.accent }} />
        <div className="grid gap-1 border-b border-[var(--line-soft)] pt-2.5 pr-3 pb-2 pl-3.5" style={{ background: statusTone.headerBg }}>
          <div className="flex min-w-0 items-center gap-1.5">
            <StatusMarker tone={group.tone} ratio={group.ratio} />
            {group.code && <span className="shrink-0 font-mono text-[11px] font-medium text-[var(--fg-muted)] tabular-nums">{group.code}</span>}
            <span className="min-w-0 flex-1 truncate text-[11px] text-[var(--fg-muted)] tabular-nums">{group.meta}</span>
            {group.statusLabel && <Chip tone={statusTone.chipTone} className="h-[19px] px-1.5 text-[10.5px]">{group.statusLabel}</Chip>}
          </div>
          <strong
            title={group.title}
            className={`line-clamp-2 text-sm leading-[1.35] font-semibold [word-break:keep-all] [overflow-wrap:break-word] ${
              statusTone.quiet ? "text-[var(--fg-dim)]" : "text-[var(--fg)]"
            }`}
          >
            {group.title}
          </strong>
        </div>
        <div className="grid gap-2 py-2.5 pr-3 pl-3.5">
          <MeterBar ratio={group.ratio} tone={group.blocked ? "warn" : "ok"} />
          <div className="flex min-w-0 items-center justify-between gap-2 text-[11px] leading-4">
            <span className="min-w-0 truncate tabular-nums">
              {group.blocked ? <span className="font-medium text-[var(--warn)]">{group.blocked}</span> : group.summary}
            </span>
            {group.onOpen && !far && (
              <button
                type="button"
                data-open-group
                aria-label={`${group.title} ${group.openLabel ?? "Open →"}`}
                onClick={(event) => {
                  event.stopPropagation();
                  group.onOpen?.();
                }}
                className="nodrag nopan !m-0 !min-h-6 [@media(pointer:coarse)]:!min-h-11 !min-w-0 shrink-0 cursor-pointer !rounded-full !border !border-[var(--line)] !bg-[var(--bg-2)] !px-2.5 !py-0.5 !text-[11px] leading-4 font-semibold !text-[var(--accent-fg)] transition-colors hover:!border-[var(--accent)] hover:!bg-[var(--accent-soft)]"
              >
                {group.openLabel ?? "Open →"}
              </button>
            )}
          </div>
        </div>
      </div>
      <Handle type="source" position={Position.Right} isConnectable={false} style={{ ...graphHandleDot(statusTone.pinColor), zIndex: 2 }} />
      {far && <><FarCover scale={scale} title={group.title} line={group.far} ratio={group.ratio} error={!!group.blocked} className="bg-[var(--th-card-bg)]" /><FarPins scale={scale} color={statusTone.pinColor} /></>}
    </div>
  );
}

/** Zoomed out, `cover` (number and title) heads the card and `far` is its state line. */
export type ViewTaskData = TaskNodeData & { cover: string; far: string };

/** A task on the read-only view: the editor's blueprint card, covered by its title and state once zoomed out. */
export function ViewTaskNode(props: NodeProps) {
  const task = props.data as ViewTaskData;
  const scale = useLegible(), far = scale > FAR;
  const statusTone = getGraphStatusTone(task.error ? "failed" : task.status ?? task.tone);
  return <div className="relative rounded-[14px]" style={{ "--legible": scale, ...far ? farRing(scale, props.selected, task.error) : {} } as CSSProperties}>
    <RelationsTaskNode {...props} selected={props.selected && !far} />
    {far && <><FarCover scale={scale} title={task.cover} line={task.far} ratio={task.ratio > 0 ? task.ratio : undefined} error={task.error} className="bg-[var(--th-card-bg)]" /><FarPins scale={scale} color={statusTone.pinColor} /></>}
  </div>;
}

/** `in` tails are dependencies outside the group, at its left; `out` tails depend on it, at its right. */
export type TailNodeData = { title: string; cover: string; group: string; side: "in" | "out"; ends: { enters: boolean; leaves: boolean } };

/** A task outside the group, faint at the canvas's edge with its group's name. */
export function RelationsTailNode({ data }: NodeProps) {
  const tail = data as TailNodeData;
  const scale = useLegible(), far = scale > FAR;
  const tailPin = (shown: boolean): CSSProperties => ({ ...graphHandleDot("var(--fg-faint)"), zIndex: 2, opacity: shown ? 1 : 0 });
  return <div className="relative grid w-60 gap-0.5 rounded-[14px] border border-dashed border-[var(--line)] bg-[var(--bg-1)] px-3.5 py-2.5 text-left text-xs text-[var(--fg-muted)]"
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
