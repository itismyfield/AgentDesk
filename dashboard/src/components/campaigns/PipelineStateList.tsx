import { ChevronDown } from "lucide-react";
import { useState, type ReactNode } from "react";
import { Cap, cn } from "../common/KitPrimitives";
import type { Tr } from "./campaignPresentation";
import { TONE_COLORS } from "./PipelineCanvas";
import { KIND_LABELS, durationText, eventName, gateName, pinSummary, retryText, stateName, type Pin, type PipelineGraph } from "./pipelineModel";

/** One result in words: its colour, the result, where it goes, its approval gate and its retry budget. */
export function PinText({ graph, pin, tr, children }: { graph: PipelineGraph; pin: Pin; tr: Tr; children?: ReactNode }) {
  const { transition } = pin;
  const to = pin.target === transition.from ? tr("제자리", "in place") : stateName(graph.states.get(pin.target), pin.target, tr);
  return <li className="grid gap-0.5">
    <span className="flex min-w-0 flex-wrap items-center gap-x-1.5 text-[13px] text-[var(--fg-dim)]">
      <i aria-hidden className="size-2 shrink-0 rounded-full" style={{ background: TONE_COLORS[pin.tone] }} />
      <strong className="font-semibold text-[var(--fg)]">{pin.label}</strong><span>→ {to}</span>
      {transition.gate && !pin.exhausted && <span className="text-[var(--warn)]">· {gateName(transition.gate, tr)}</span>}
    </span>
    {transition.retry && !pin.exhausted && <Cap className="pl-3.5">{retryText(graph, transition, tr)}</Cap>}
    {children}
  </li>;
}

/** The list phones show in place of the canvas, and screen readers beside it: the main flow numbered, each with its kind,
 * the tasks in it and its results; the chosen one (every one, `open`) adds its owner and results in full. */
export default function PipelineStateList({ graph, chosen, onChoose, open = false, tr }: {
  graph: PipelineGraph; chosen?: string | null; onChoose?: (id: string | null) => void; open?: boolean; tr: Tr;
}) {
  const [shownOff, setShownOff] = useState(false);
  const row = (id: string, number: number | null) => {
    const state = graph.states.get(id)!, count = graph.occupancy?.get(id) ?? 0, expanded = open || chosen === id;
    const head = <>
      <span className="flex min-w-0 items-baseline gap-2"><strong className="min-w-0 truncate text-sm font-semibold text-[var(--fg)]">{stateName(state, id, tr)}</strong>
        <Cap className="ml-auto shrink-0">{[id === graph.main[0] && tr("시작", "Start"), tr(...KIND_LABELS[state.kind])].filter(Boolean).join(" · ")}</Cap>
        {count > 0 && <span className="shrink-0 rounded-full bg-[var(--info-soft)] px-1.5 text-xs leading-5 font-medium text-[var(--info)]">{tr(`작업 ${count}`, `${count} tasks`)}</span>}</span>
      {!expanded && <Cap className="truncate text-[13px] text-[var(--fg-dim)]">{pinSummary(graph, id, tr) || tr("나가는 전환 없음", "No way out")}</Cap>}
    </>;
    return <li key={id} className={number === null ? "grid" : "grid grid-cols-[1.75rem_minmax(0,1fr)] gap-x-2.5"}>
      {number !== null && <span aria-hidden className={cn("mt-2.5 grid size-7 place-items-center rounded-full text-xs font-semibold tabular-nums", expanded && !open ? "bg-[var(--accent-soft)] text-[var(--accent-fg)]" : "bg-[var(--bg-3)] text-[var(--fg-muted)]")}>{number}</span>}
      <div className={cn("min-w-0 rounded-xl", expanded && !open && "border border-[var(--accent)] bg-[var(--th-card-bg)]")}>
        {onChoose ? <button type="button" aria-expanded={expanded} onClick={() => onChoose(expanded ? null : id)}
          className="!m-0 grid min-h-11 w-full min-w-0 gap-0.5 !rounded-xl !border-0 !bg-transparent px-3 py-2 text-left focus-visible:outline-2 focus-visible:outline-[var(--accent)]">{head}</button>
          : <div className="grid gap-0.5 px-3 py-2">{head}</div>}
        {expanded && <div className="grid gap-2 px-3 pb-3">
          {(state.owner || state.timeout) && <Cap>{[state.owner && tr(`담당 ${state.owner}`, `Owner ${state.owner}`), state.timeout && `${tr("시간 제한", "Timeout")} ${durationText(state.timeout.after_seconds, tr)}`].filter(Boolean).join(" · ")}</Cap>}
          <ul aria-label={tr("나가는 전환", "Transitions out")} className="m-0 grid list-none gap-1.5 p-0">{graph.pins.get(id)!.map((pin) => <PinText key={pin.id} graph={graph} pin={pin} tr={tr} />)}</ul>
        </div>}
      </div>
    </li>;
  };
  const fold = (label: string, names: string, body: ReactNode, shown: boolean, toggle: () => void) => <div className="border-t border-[var(--line-soft)] first:border-t-0">
    {open ? <strong className="block px-4 py-2 text-sm font-medium">{label}</strong> : <button type="button" aria-expanded={shown} onClick={toggle} className="!m-0 flex min-h-14 w-full min-w-0 items-center gap-2 !rounded-none !border-0 !bg-transparent px-4 py-2 text-left">
      <span className="grid min-w-0 flex-1"><strong className="text-sm font-medium text-[var(--fg)]">{label}</strong><Cap className="truncate">{names}</Cap></span>
      <ChevronDown aria-hidden className={cn("size-4 shrink-0 text-[var(--fg-muted)] transition-transform motion-reduce:transition-none", shown && "rotate-180")} />
    </button>}
    {shown && <div className="px-3 pb-3">{body}</div>}
  </div>;
  const [shownEvents, setShownEvents] = useState(false);
  const transitions = [...graph.pins.values()].flat().filter((pin) => !pin.exhausted).length;
  return <div className="grid min-w-0 gap-3">
    <ol aria-label={tr("주 흐름", "Main flow")} className="m-0 grid list-none gap-1 p-0">{graph.main.map((id, index) => row(id, index + 1))}</ol>
    {(graph.off.length > 0 || graph.events.length > 0) && <div className="overflow-hidden rounded-xl border border-[var(--line-soft)] bg-[var(--bg-1)]">
      {graph.off.length > 0 && fold(tr(`흐름 밖 상태 ${graph.off.length}`, `${graph.off.length} off the flow`), graph.off.map((id) => stateName(graph.states.get(id), id, tr)).join(" · "),
        <ul className="m-0 grid list-none gap-1 p-0">{graph.off.map((id) => row(id, null))}</ul>, open || shownOff, () => setShownOff(!shownOff))}
      {graph.events.length > 0 && fold(tr(`이벤트 ${graph.events.length}`, `${graph.events.length} events`), graph.events.map((event) => eventName(event.type, tr)).join(" · "),
        <ul className="m-0 grid list-none gap-1 p-0 text-[13px] text-[var(--fg-dim)]">{graph.events.map((event) => <li key={event.id}>{eventName(event.type, tr)} → {stateName(graph.states.get(event.state_id), event.state_id, tr)}</li>)}</ul>,
        open || shownEvents, () => setShownEvents(!shownEvents))}
    </div>}
    <Cap className="text-[13px]"><span className="font-semibold text-[var(--fg)]">{tr(`상태 ${graph.states.size} · 전환 ${transitions}`, `${graph.states.size} states · ${transitions} transitions`)}</span></Cap>
  </div>;
}
