import { CircleCheck, CircleX, Plus, Search, Trash2, TriangleAlert, X } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";
import { PIPELINE_KINDS, getPipelineConditionFields, type PipelineComparison, type PipelineDefinition, type PipelineDiagnostic, type PipelineFieldSpec, type PipelineState, type PipelineTransition } from "../../api/serverCampaignPipeline";
import { Button } from "../../kit/free/button-clean";
import { AlertDialog } from "../../kit/free/dialog-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { Switch } from "../../kit/free/switch-clean";
import { Cap, Desc, Eyebrow, InspSection, Num, cn } from "../common/KitPrimitives";
import { Select } from "../common/Select";
import { GATE_LABELS } from "./campaignOverviewModel";
import type { Tr } from "./campaignPresentation";
import { TONE_COLORS, kindIcon } from "./PipelineCanvas";
import {
  addTransition, conditionOf, removal, type DraftChanges, diagnosticPlace, diagnosticText, freshComparison, newRetry, patchState, patchTransition, removeState, removeTransition, setRetryMax,
} from "./pipelineEdit";
import { OPS, comparisons, durationText, gateName, kindName, stateName, type PipelineGraph } from "./pipelineModel";
import type { Checks } from "./usePipelineEditor";

const FIELD = "min-h-11 w-full min-w-0 rounded-lg border border-th-input-border bg-th-input-bg px-3 py-2 text-sm text-th-text-primary focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-[var(--th-accent-info)] disabled:opacity-55";
const GRID = "grid grid-cols-[4.5rem_minmax(0,1fr)] items-center gap-x-2 gap-y-2";
const whole = (text: string, least = 0) => Math.max(least, Math.trunc(Number(text)) || 0);
type Update = (change: (draft: PipelineDefinition) => PipelineDefinition) => void;
/** Opens 상태 찾기·만들기 for a way out, from the button pressed: the keyboard's way to what dragging its pin does. */
type Find = (transition: PipelineTransition, button: HTMLElement) => void;

function Field({ label, children }: { label: string; children: ReactNode }) {
  return <label className="contents"><span className="text-[13px] text-[var(--fg-muted)]">{label}</span>{children}</label>;
}

// The vocabulary is fixed per server build, so each kind and executor is read once per page.
const vocabulary = new Map<string, Promise<PipelineFieldSpec[]>>();
function useFields(state: PipelineState) {
  const key = `${state.kind}|${state.executor?.id ?? ""}|${state.executor?.version ?? ""}`;
  const [read, setRead] = useState<{ key: string; fields: PipelineFieldSpec[] | null } | null>(null);
  useEffect(() => {
    let live = true;
    if (!vocabulary.has(key)) vocabulary.set(key, getPipelineConditionFields(state.kind, state.executor).catch((cause: unknown) => { vocabulary.delete(key); throw cause; }));
    vocabulary.get(key)!.then((fields) => live && setRead({ key, fields }), () => live && setRead({ key, fields: null }));
    return () => { live = false; };
  }, [key]); // eslint-disable-line react-hooks/exhaustive-deps -- the key names the state's kind and executor
  return read?.key === key ? read : null;
}

/** One comparison: the field from the state's vocabulary, then the operator and a value of the field's type. */
function ConditionRow({ value, fields, busy, onChange, onRemove, tr }: {
  value: PipelineComparison; fields: PipelineFieldSpec[]; busy: boolean; onChange: (value: PipelineComparison) => void; onRemove?: () => void; tr: Tr;
}) {
  const spec = fields.find((field) => field.field === value.field);
  const type = spec?.type ?? (typeof value.value === "number" ? "integer" : typeof value.value);
  const set = (next: PipelineComparison["value"]) => onChange({ ...value, value: next });
  const label = tr(`${value.field} 값`, `${value.field} value`);
  const input = type === "boolean" ? <Select aria-label={label} disabled={busy} value={String(value.value)} onChange={(event) => set(event.target.value === "true")}><option value="true">true</option><option value="false">false</option></Select>
    : type === "integer" ? <input aria-label={label} type="number" step={1} className={FIELD} disabled={busy} value={Number(value.value)} onChange={(event) => set(Math.trunc(Number(event.target.value)) || 0)} />
    : type === "enum" ? <Select aria-label={label} disabled={busy} value={String(value.value)} onChange={(event) => set(event.target.value)}>
      {[...new Set([String(value.value), ...spec?.enum_values ?? []])].map((option) => <option key={option} value={option}>{option}</option>)}</Select>
    : <input aria-label={label} className={FIELD} disabled={busy} value={String(value.value)} onChange={(event) => set(event.target.value)} />;
  return <li className="grid gap-1.5">
    <div className="flex min-w-0 items-center gap-1">
      <Select aria-label={tr("조건 필드", "Condition field")} className="font-mono text-[13px]" disabled={busy} value={value.field} onChange={(event) => onChange(freshComparison(fields.find((field) => field.field === event.target.value)!))}>
        {!spec && <option value={value.field}>{value.field}</option>}
        {fields.filter((field) => field.available || field.field === value.field).map((field) => <option key={field.field} value={field.field}>{field.field}{field.available ? "" : tr(" (이 종류에서 못 써요)", " (not for this kind)")}</option>)}
      </Select>
      {onRemove && <IconButton icon={<X aria-hidden />} label={tr("이 조건 빼기", "Remove this condition")} disabled={busy} onClick={onRemove} />}
    </div>
    <div className="grid grid-cols-[4.5rem_minmax(0,1fr)] gap-1.5">
      <Select aria-label={tr("비교", "Comparison")} disabled={busy} value={value.op} onChange={(event) => onChange({ ...value, op: event.target.value })}>
        {[...new Set([value.op, ...spec?.operators ?? []])].map((op) => <option key={op} value={op}>{OPS[op] ?? op}</option>)}</Select>
      {input}
    </div>
  </li>;
}

/** One way out of the chosen state: where it goes, its priority, its conditions (all must hold), a retry budget and an approval gate. */
function TransitionForm({ transition, draft, graph, fields, flagged, fresh, mark, busy, update, onFind, tr }: {
  transition: PipelineTransition; draft: PipelineDefinition; graph: PipelineGraph; fields: PipelineFieldSpec[]; flagged: boolean; fresh: boolean; mark: string | null; busy: boolean; update: Update; onFind: Find; tr: Tr;
}) {
  const pin = graph.pins.get(transition.from)?.find((value) => value.id === transition.id);
  const list = comparisons(transition.condition);
  const patch = (value: Partial<PipelineTransition>) => update((current) => patchTransition(current, transition.id, value));
  const setList = (next: PipelineComparison[]) => patch({ condition: conditionOf(next) });
  const { retry } = transition;
  const sharing = retry ? draft.transitions.filter((value) => value.retry?.budget_id === retry.budget_id).length : 0;
  const options = draft.states.map((state) => <option key={state.id} value={state.id}>{state.id === transition.from ? tr("제자리", "In place") : stateName(state, state.id, tr)}</option>);
  return <li aria-label={tr(`전환 ${transition.id}`, `Transition ${transition.id}`)} className={cn("grid gap-2.5 rounded-xl border bg-[var(--th-card-bg)] p-3", flagged ? "border-[var(--err)]" : "border-[var(--line)]")}>
    <div className="flex min-w-0 items-center gap-1.5 text-[13px]">
      <i aria-hidden className="size-2 shrink-0 rounded-full" style={{ background: TONE_COLORS[pin?.tone ?? "other"] }} />
      <strong className="truncate font-semibold text-[var(--fg)]">{pin?.label}</strong>
      <span className={cn("truncate", !transition.to && "text-[var(--err)]")}>→ {transition.to ? stateName(draft.states.find((state) => state.id === transition.to), transition.to, tr) : tr("도착 상태 없음", "No target")}</span>
      {mark && <b className="shrink-0 font-semibold text-[var(--warn)]">{mark}</b>}
      <IconButton icon={<Trash2 aria-hidden />} className="ml-auto" label={tr("이 전환 지우기", "Delete this transition")} disabled={busy} onClick={() => update((current) => removeTransition(current, transition.id))} />
    </div>
    <div className={GRID}>
      <Field label={tr("도착", "Goes to")}><span className="flex min-w-0 items-center gap-1">
        <Select autoFocus={fresh} wrapperClassName="min-w-0 flex-1" disabled={busy} value={transition.to} onChange={(event) => patch({ to: event.target.value })}><option value="">{tr("골라 주세요", "Choose one")}</option>{options}</Select>
        <IconButton icon={<Search aria-hidden />} label={tr("상태 찾기·만들기", "Find or make a state")} disabled={busy} onClick={(event) => onFind(transition, event.currentTarget)} />
      </span></Field>
      <Field label={tr("우선순위", "Priority")}><input type="number" min={0} step={1} className={FIELD} disabled={busy} value={transition.priority} onChange={(event) => patch({ priority: whole(event.target.value) })} /></Field>
    </div>
    <fieldset className="m-0 grid gap-2 border-0 p-0">
      <legend className="mb-1.5 p-0 text-[13px] text-[var(--fg-muted)]">{tr("조건 (모두 맞아야 해요)", "Conditions (all must hold)")}</legend>
      <ul className="m-0 grid list-none gap-2.5 p-0">{list.map((value, index) => <ConditionRow key={index} value={value} fields={fields} busy={busy} tr={tr}
        onChange={(next) => setList(list.map((item, at) => at === index ? next : item))} onRemove={list.length > 1 ? () => setList(list.filter((_, at) => at !== index)) : undefined} />)}</ul>
      <div><Button variant="ghost" size="sm" icon={<Plus aria-hidden />} disabled={busy || !fields.some((field) => field.available)} onClick={() => setList([...list, freshComparison(fields.find((field) => field.available)!)])}>{tr("조건 더하기", "Add a condition")}</Button></div>
    </fieldset>
    <Switch label={tr("재시도 한도", "Retry budget")} disabled={busy} checked={!!retry} onChange={(event) => patch({ retry: event.target.checked ? newRetry(draft, transition) : undefined })} />
    {retry && <div className={GRID}>
      <Field label={tr("횟수", "Times")}><input type="number" min={1} step={1} className={FIELD} disabled={busy} value={retry.max} onChange={(event) => update((current) => setRetryMax(current, retry.budget_id, whole(event.target.value, 1)))} /></Field>
      <Field label={tr("다 쓰면", "Then")}><Select disabled={busy} value={retry.then_to} onChange={(event) => patch({ retry: { ...retry, then_to: event.target.value } })}><option value="">{tr("골라 주세요", "Choose one")}</option>{options}</Select></Field>
      {sharing > 1 && <Cap className="col-span-2">{tr(`한도 ${retry.budget_id}는 전환 ${sharing}개가 함께 써요.`, `Budget ${retry.budget_id} is shared by ${sharing} transitions.`)}</Cap>}
    </div>}
    <div className={GRID}>
      <Field label={tr("승인", "Approval")}><Select disabled={busy} value={transition.gate ?? ""} onChange={(event) => patch({ gate: event.target.value || undefined })}>
        <option value="">{tr("없음", "None")}</option>{Object.keys(GATE_LABELS).map((gate) => <option key={gate} value={gate}>{gateName(gate, tr)}</option>)}</Select></Field>
    </div>
  </li>;
}

/** The chosen state's fields and its ways out, all editable; deleting it takes the transitions from and to it. */
function StateForm({ state, draft, graph, flagged, changes, busy, update, onChoose, onFind, tr }: {
  state: PipelineState; draft: PipelineDefinition; graph: PipelineGraph; flagged: ReadonlySet<string>; changes: DraftChanges | null; busy: boolean; update: Update; onChoose: (id: string | null) => void; onFind: Find; tr: Tr;
}) {
  const [confirming, setConfirming] = useState(false);
  // A way out just added takes the focus, which also scrolls it into view.
  const [added, setAdded] = useState<string | null>(null);
  const vocab = useFields(state);
  const Icon = kindIcon(state.kind);
  const name = stateName(state, state.id, tr);
  const patch = (value: Partial<PipelineState>) => update((current) => patchState(current, state.id, value));
  const outgoing = draft.transitions.filter((value) => value.from === state.id).sort((a, b) => a.priority - b.priority);
  const incoming = draft.transitions.filter((value) => value.to === state.id && value.from !== state.id);
  const here = graph.occupancy?.get(state.id);
  const start = state.id === draft.start;
  const gone = removal(draft, state.id);
  return <>
    <InspSection className="border-t-0">
      <div className="flex min-w-0 items-center gap-2.5">
        <span className="grid size-8 shrink-0 place-items-center rounded-lg bg-[var(--bg-3)] text-[var(--fg-dim)]"><Icon aria-hidden className="size-4" /></span>
        <span className="grid min-w-0 flex-1"><h3 className="m-0 truncate text-base text-[var(--fg)]">{name}</h3><Cap className="truncate">{kindName(state.kind, tr)} · <span className="font-mono">{state.id}</span></Cap></span>
        <Button variant="ghost" size="sm" icon={<Trash2 aria-hidden />} disabled={busy || start} title={start ? tr("시작 상태는 지울 수 없어요.", "The start state cannot be deleted.") : undefined} onClick={() => setConfirming(true)}>{tr("지우기", "Delete")}</Button>
      </div>
      {here && <Cap><Num>{tr(`지금 머무는 작업 ${here.here + here.held}개`, `${here.here + here.held} tasks here now`)}</Num></Cap>}
    </InspSection>
    <InspSection className={GRID}>
      <Field label={tr("이름", "Name")}><input className={FIELD} disabled={busy} value={state.title ?? ""} placeholder={stateName(undefined, state.id, tr)} onChange={(event) => patch({ title: event.target.value })} /></Field>
      <Field label={tr("종류", "Kind")}><Select disabled={busy} value={state.kind} onChange={(event) => patch({ kind: event.target.value })}>
        {[...new Set([state.kind, ...PIPELINE_KINDS])].map((kind) => <option key={kind} value={kind}>{kindName(kind, tr)}</option>)}</Select></Field>
      <Field label={tr("담당", "Owner")}><input className={FIELD} disabled={busy} value={state.owner ?? ""} placeholder={tr("없음", "None")} onChange={(event) => patch({ owner: event.target.value })} /></Field>
      <Field label={tr("시간 초과(초)", "Timeout (s)")}><input type="number" min={1} step={1} className={FIELD} disabled={busy} value={state.timeout?.after_seconds ?? ""} placeholder={tr("없음", "None")}
        onChange={(event) => patch({ timeout: whole(event.target.value) ? { after_seconds: whole(event.target.value), transition_id: state.timeout?.transition_id ?? outgoing[0]?.id ?? "" } : undefined })} /></Field>
      {state.timeout && <Field label={tr("그다음", "Then")}><Select disabled={busy} value={state.timeout.transition_id} onChange={(event) => patch({ timeout: { ...state.timeout!, transition_id: event.target.value } })}>
        {[...new Set([state.timeout.transition_id, ...outgoing.map((value) => value.id)])].map((id) => <option key={id} value={id}>{id}</option>)}</Select></Field>}
      {state.timeout && <Cap className="col-span-2">{tr(`${durationText(state.timeout.after_seconds, tr)} 뒤에 이 전환으로 가요.`, `After ${durationText(state.timeout.after_seconds, tr)} it takes this transition.`)}</Cap>}
      <label className="col-span-2 grid gap-1 text-[13px] text-[var(--fg-muted)]">{tr("지시문", "Instructions")}
        <textarea rows={3} className={cn(FIELD, "resize-y")} disabled={busy} value={state.instructions ?? ""} onChange={(event) => patch({ instructions: event.target.value })} /></label>
      <span className="text-[13px] text-[var(--fg-muted)]">{tr("실행기", "Executor")}</span>
      <span className="min-w-0 font-mono text-xs text-[var(--fg)] [overflow-wrap:anywhere]">{state.executor ? `${state.executor.id} v${state.executor.version}` : tr("없음", "None")}</span>
    </InspSection>
    <InspSection>
      <div className="flex items-center justify-between"><Eyebrow>{tr("나가는 전환", "Transitions out")}</Eyebrow><Cap>{tr("우선순위 숫자가 작은 것부터 검사", "Lowest priority number first")}</Cap></div>
      {vocab?.fields === null && <Desc className="text-[var(--err)]">{tr("조건 어휘를 불러오지 못했어요. 지금 조건만 고칠 수 있어요.", "Could not load the condition vocabulary. Only the current fields are offered.")}</Desc>}
      {outgoing.length > 0 && <ul aria-label={tr("나가는 전환", "Transitions out")} className="m-0 grid list-none gap-2.5 p-0">{outgoing.map((transition) =>
        <TransitionForm key={transition.id} transition={transition} draft={draft} graph={graph} fields={vocab?.fields ?? []} flagged={flagged.has(transition.id)} fresh={transition.id === added} busy={busy}
          mark={changes?.transitions.added.includes(transition.id) ? tr("새로", "New") : changes?.transitions.changed.includes(transition.id) ? tr("바뀜", "Changed") : null} update={update} onFind={onFind} tr={tr} />)}</ul>}
      {!state.kind.startsWith("end_") && <div><Button variant="secondary" size="sm" icon={<Plus aria-hidden />} disabled={busy}
        onClick={() => { const [next, id] = addTransition(draft, state.id); update(() => next); setAdded(id); }}>{tr("나가는 전환 추가", "Add a way out")}</Button></div>}
    </InspSection>
    <InspSection>
      <Eyebrow>{tr("들어오는 전환", "Transitions in")}</Eyebrow>
      {incoming.length ? <ul className="m-0 grid list-none gap-1 p-0 text-[13px] text-[var(--fg-dim)]">{incoming.map((value) => <li key={value.id} className="min-w-0 truncate">
        <strong className="font-semibold text-[var(--fg)]">{stateName(draft.states.find((other) => other.id === value.from), value.from, tr)}</strong> · {value.id}</li>)}</ul>
        : <Desc>{start ? tr("시작 상태예요.", "This is the start state.") : tr("들어오는 전환이 없어요.", "No transitions lead here.")}</Desc>}
    </InspSection>
    <AlertDialog open={confirming} onOpenChange={setConfirming} danger title={tr(`'${name}' 상태를 지울까요?`, `Delete the state '${name}'?`)}
      description={<>{tr(`함께 지워져요: 전환 ${gone.transitions.length}개 · 이벤트 ${gone.events}개 · 다른 상태의 시간 초과 ${gone.timeouts.length}개.`,
        `Going with it: ${gone.transitions.length} transitions, ${gone.events} events, ${gone.timeouts.length} timeouts on other states.`)}
        {gone.retries > 0 && tr(` 이 상태로 재시도를 넘기는 전환 ${gone.retries}개는 새 대상을 골라야 해요.`, ` ${gone.retries} retries fall back to it and need a new target.`)}
        {tr(" 저장하기 전까지는 되돌릴 수 있어요.", " Nothing is lost until you save.")}</>}
      confirmLabel={tr("상태 지우기", "Delete state")} cancelLabel={tr("그대로 두기", "Keep it")} onConfirm={() => { update((current) => removeState(current, state.id)); onChoose(null); }} />
  </>;
}

/** 저장 전 검사: the server's diagnostics on the draft, each pointing at its state, which a press chooses and shows. */
export function PipelineChecks({ checks, graphs, onPick, tr }: { checks: Checks | null; graphs: PipelineDefinition[]; onPick: (diagnostic: PipelineDiagnostic) => void; tr: Tr }) {
  // Under the canvas: a short strip that scrolls, its results side by side.
  const errors = checks?.errors ?? [], warnings = checks?.warnings ?? [];
  const row = (value: PipelineDiagnostic, index: number, error: boolean) => <li key={`${error}-${index}`}>
    <button type="button" onClick={() => onPick(value)} className="!m-0 flex min-h-11 w-full min-w-0 items-start gap-2 !rounded-lg !border-0 !bg-transparent px-2 py-1.5 text-left hover:!bg-[color-mix(in_oklch,var(--fg)_5%,transparent)] focus-visible:outline-2 focus-visible:outline-[var(--accent)]">
      {error ? <CircleX aria-hidden className="mt-0.5 size-4 shrink-0 text-[var(--err)]" /> : <TriangleAlert aria-hidden className="mt-0.5 size-4 shrink-0 text-[var(--warn)]" />}
      <span className="grid min-w-0"><span className="text-[13px] text-[var(--fg)]">{diagnosticText(value.code, tr)}</span><Cap className="truncate">{diagnosticPlace(value, graphs, tr) || value.path}</Cap></span>
    </button>
  </li>;
  return <section aria-label={tr("저장 전 검사", "Checks before saving")} className="grid max-h-44 min-h-0 shrink-0 content-start gap-2 overflow-y-auto border-t border-[var(--line-soft)] bg-[var(--bg-1)] px-4 py-3">
    <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
      <Eyebrow>{tr("저장 전 검사", "Checks before saving")}</Eyebrow>
      <Cap className={errors.length ? "text-[var(--err)]" : undefined}><Num>{tr(`오류 ${errors.length}`, `${errors.length} errors`)}</Num></Cap>
      <Cap><Num>{tr(`경고 ${warnings.length}`, `${warnings.length} warnings`)}</Num></Cap>
      {checks?.pending && <Cap role="status">{tr("검사하는 중", "Checking")}</Cap>}
      <Cap className="ml-auto">{tr("고칠 때마다 다시 검사해요. 저장하면 서버가 한 번 더 검사해요.", "Each change is checked again. Saving checks once more on the server.")}</Cap>
    </div>
    {checks?.failed && <Desc className="text-[var(--warn)]">{tr(`검사를 하지 못했어요: ${checks.failed}`, `Could not check: ${checks.failed}`)}</Desc>}
    {errors.length + warnings.length > 0 ? <ul aria-label={tr("검사 결과", "Check results")} className="m-0 grid list-none grid-cols-[repeat(auto-fill,minmax(18rem,1fr))] gap-0.5 p-0">{errors.map((value, index) => row(value, index, true))}{warnings.map((value, index) => row(value, index, false))}</ul>
      : checks && !checks.pending && !checks.failed && <p className="m-0 flex items-center gap-1.5 text-[13px] text-[var(--fg-dim)]"><CircleCheck aria-hidden className="size-4 text-[var(--ok)]" />{tr("검사를 모두 통과했어요.", "Every check passes.")}</p>}
  </section>;
}

/** The inspector column while editing: the chosen state's form, or what a save does. */
export default function PipelineEditPanel({ graph, draft, revision, chosen, flagged, changes, busy, update, onChoose, onFind, tr }: {
  graph: PipelineGraph; draft: PipelineDefinition; revision: number; chosen: string | null; flagged: ReadonlySet<string>; changes: DraftChanges | null; busy: boolean; update: Update;
  onChoose: (id: string | null) => void; onFind: Find; tr: Tr;
}) {
  const state = draft.states.find((value) => value.id === chosen);
  return <aside aria-label={tr("파이프라인 편집", "Pipeline editor")} className="flex min-h-0 flex-col border-l border-[var(--line-soft)] bg-[var(--bg-1)]">
    <div className="min-h-0 flex-1 overflow-y-auto">
      {state ? <StateForm key={state.id} state={state} draft={draft} graph={graph} flagged={flagged} changes={changes} busy={busy} update={update} onChoose={onChoose} onFind={onFind} tr={tr} /> : <>
        <InspSection className="border-t-0 gap-1">
          <Cap>{tr("편집 중", "Editing")}</Cap>
          <Num className="text-sm text-[var(--fg)]">{tr(`상태 ${draft.states.length} · 전환 ${draft.transitions.length}`, `${draft.states.length} states · ${draft.transitions.length} transitions`)}</Num>
          <Cap><Num>{tr(`저장하면 정의 rev ${revision} → ${revision + 1}`, `Saving makes definition rev ${revision} → ${revision + 1}`)}</Num></Cap>
        </InspSection>
        <p className="m-0 grid gap-1 px-4 py-3 text-xs text-[var(--fg-muted)]">
          <span>{tr("상태를 고르면 여기서 고쳐요. 핀을 상태로 끌면 전환이 생기고 빈 곳에 놓으면 상태를 찾거나 만들어요.", "Choose a state to edit it here. Drag a pin onto a state to connect it, or onto empty canvas to find or make one.")}</span>
          <span>{tr("준비됨이나 중지됨일 때만 저장돼요. 작업이 남은 상태는 지울 수 없어요.", "Only a prepared or stopped campaign saves. A state with tasks in it cannot go.")}</span>
        </p>
      </>}
    </div>
  </aside>;
}
