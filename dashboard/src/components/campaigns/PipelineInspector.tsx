import type { ReactNode } from "react";
import { Cap, Desc, Eyebrow, InspSection, Num } from "../common/KitPrimitives";
import type { Tr } from "./campaignPresentation";
import { KIND_ICONS, TONE_COLORS } from "./PipelineCanvas";
import { PinText } from "./PipelineStateList";
import { KIND_LABELS, comparisonText, comparisons, durationText, eventName, gateName, stateName, type PipelineGraph } from "./pipelineModel";

const ASIDE = "min-h-0 overflow-y-auto border-[var(--line-soft)] bg-[var(--bg-1)] max-[1279px]:border-t min-[1280px]:border-l";

/** The canvas's right column, all text: the chosen state's fields, its results with conditions, retries and gates, and
 * the transitions into it; with nothing chosen, `summary`. */
export default function PipelineInspector({ graph, id, summary, tr }: { graph: PipelineGraph; id: string | null; summary: ReactNode; tr: Tr }) {
  const state = id === null ? undefined : graph.states.get(id);
  if (!state) return <aside aria-label={tr("파이프라인 요약", "Pipeline summary")} className={ASIDE}>{summary}</aside>;
  const Icon = KIND_ICONS[state.kind];
  const name = (target: string) => stateName(graph.states.get(target), target, tr);
  const pins = graph.pins.get(state.id)!;
  const incoming = [...graph.pins.entries()].flatMap(([from, list]) => list.filter((pin) => pin.target === state.id && from !== state.id).map((pin) => ({ from, pin })));
  const events = graph.events.filter((event) => event.state_id === state.id);
  const fields: [string, ReactNode][] = [
    [tr("담당", "Owner"), state.owner ?? tr("없음", "None")],
    [tr("시간 제한", "Timeout"), state.timeout ? `${durationText(state.timeout.after_seconds, tr)} → ${state.timeout.transition_id}` : tr("없음", "None")],
    [tr("실행기", "Executor"), state.executor ? <span className="font-mono text-xs">{state.executor.id} v{state.executor.version}</span> : tr("없음", "None")],
    ...state.executor?.parameters?.effect_gates?.length ? [[tr("동작 승인", "Action approval"), state.executor.parameters.effect_gates.map((gate) => gateName(gate, tr)).join(" · ")] as [string, ReactNode]] : [],
    [tr("지시", "Instructions"), state.instructions ? <span className="line-clamp-4 whitespace-pre-wrap">{state.instructions}</span> : tr("없음", "None")],
  ];
  return <aside aria-label={tr("선택 상태", "Selected state")} className={ASIDE}>
    <InspSection className="border-t-0">
      <div className="flex min-w-0 items-center gap-2.5">
        <span className="grid size-8 shrink-0 place-items-center rounded-lg bg-[var(--bg-3)] text-[var(--fg-dim)]"><Icon aria-hidden className="size-4" /></span>
        <span className="grid min-w-0"><h3 className="m-0 truncate text-base text-[var(--fg)]">{name(state.id)}</h3><Cap className="truncate">{tr(...KIND_LABELS[state.kind])} · <span className="font-mono">{state.id}</span></Cap></span>
      </div>
      {graph.occupancy && <Cap><Num>{tr(`지금 머무는 작업 ${graph.occupancy.get(state.id)}개`, `${graph.occupancy.get(state.id)} tasks here now`)}</Num></Cap>}
    </InspSection>
    <InspSection as="dl" className="grid grid-cols-[5.5rem_minmax(0,1fr)] gap-x-3 gap-y-1.5 text-[13px]">
      {fields.map(([label, value]) => <div key={label} className="contents"><dt>{label}</dt><dd className="m-0 min-w-0 text-[var(--fg)] [overflow-wrap:anywhere]">{value}</dd></div>)}
    </InspSection>
    <InspSection>
      <div className="flex items-center justify-between"><Eyebrow>{tr("나가는 전환", "Transitions out")}</Eyebrow><Cap><Num>{pins.filter((pin) => !pin.exhausted).length}</Num></Cap></div>
      {pins.length ? <ul aria-label={tr("나가는 전환", "Transitions out")} className="m-0 grid list-none gap-3 p-0">{pins.filter((pin) => !pin.exhausted).map((pin) => <PinText key={pin.id} graph={graph} pin={pin} tr={tr}>
        <ul className="m-0 grid list-none gap-0.5 p-0 pl-3.5">{comparisons(pin.transition.condition).map((value, index) => <li key={index} className="font-mono text-[11px] text-[var(--fg-muted)] [overflow-wrap:anywhere]">{comparisonText(value)}</li>)}</ul>
        <Cap className="pl-3.5"><Num>{tr(`우선순위 ${pin.transition.priority}`, `Priority ${pin.transition.priority}`)}</Num></Cap>
      </PinText>)}</ul> : <Desc>{state.kind.startsWith("end_") ? tr("끝 상태라 나가는 전환이 없어요.", "An end state has no way out.") : tr("나가는 전환이 없어요.", "No transitions lead out.")}</Desc>}
    </InspSection>
    <InspSection>
      <div className="flex items-center justify-between"><Eyebrow>{tr("들어오는 전환", "Transitions in")}</Eyebrow><Cap><Num>{incoming.length}</Num></Cap></div>
      {incoming.length ? <ul aria-label={tr("들어오는 전환", "Transitions in")} className="m-0 grid list-none gap-1 p-0 text-[13px] text-[var(--fg-dim)]">{incoming.map(({ from, pin }) => <li key={pin.id} className="flex min-w-0 items-center gap-1.5">
        <i aria-hidden className="size-2 shrink-0 rounded-full" style={{ background: TONE_COLORS[pin.tone] }} /><strong className="font-semibold text-[var(--fg)]">{name(from)}</strong><span className="truncate">· {pin.label}</span>
      </li>)}</ul> : <Desc>{id === graph.main[0] ? tr("시작 상태예요.", "This is the start state.") : tr("들어오는 전환이 없어요.", "No transitions lead here.")}</Desc>}
    </InspSection>
    {events.length > 0 && <InspSection>
      <Eyebrow>{tr("이벤트", "Events")}</Eyebrow>
      <ul className="m-0 grid list-none gap-1 p-0 text-[13px] text-[var(--fg-dim)]">{events.map((event) => <li key={event.id}>{eventName(event.type, tr)}</li>)}</ul>
    </InspSection>}
  </aside>;
}
