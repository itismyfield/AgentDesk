import { ArrowLeft, Monitor, PencilLine, Undo2 } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type RefObject } from "react";
import type { Campaign } from "../../api/campaigns";
import { getDefaultPipelineTemplate, getServerCampaignPipeline, type CampaignPipeline as Detail, type PipelineDefinition, type PipelineDiagnostic } from "../../api/serverCampaignPipeline";
import { Button } from "../../kit/free/button-clean";
import { Dialog } from "../../kit/free/dialog-clean";
import { AgentMetroMap, type MetroLine, type MetroStation } from "../../kit/pro/agent-metro-map";
import { AgentStateMachine, type MachineEdge, type MachineState } from "../../kit/pro/agent-state-machine";
import { Cap, Chip, Desc, InspSection, Num } from "../common/KitPrimitives";
import { useShellFocus } from "../../app/shellFocus";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import { WidgetState } from "../common/WidgetState";
import { campaignStageStep } from "./campaignModel";
import { legacyTask, type OverviewTask } from "./campaignOverviewModel";
import type { Tr } from "./campaignPresentation";
import PipelineCanvas, { type PinDrop, type StateNodeData } from "./PipelineCanvas";
import { changesText, connect, connectNew, diagnosticState, draftChanges } from "./pipelineEdit";
import PipelineEditPanel, { PipelineChecks } from "./PipelineEditor";
import PipelineInspector from "./PipelineInspector";
import PipelineStateList from "./PipelineStateList";
import PipelineStatePicker from "./PipelineStatePicker";
import { buildPipelineGraph, durationText, eventName, gateText, kindName, occupancyTexts, pinSummary, stateName } from "./pipelineModel";
import { usePipelineEditor } from "./usePipelineEditor";

/** A server campaign reads its own pipeline again whenever `stamp` (its view's as_of) moves, and checks a draft again
 * whenever `sequence` (its last event) does; a ledger campaign has neither. */
export type PipelineSource = { kind: "server"; id: string; stamp: string; sequence?: number } | { kind: "legacy" };
type Loaded = { pipeline: PipelineDefinition; detail: Detail | null; template: { id: string; version: number } | null };
/** The find-or-make window: for a pin (`from`), or for the canvas tools' 상태 추가; `at` when a pin was let go on the canvas. */
type Picking = { from: { state: string; pin: string } | null; anchor: RefObject<HTMLElement | null>; at?: PinDrop };

const METRO_COLORS = ["#0190fe", "#10b981", "#f59e0b", "#06b6d4", "#ec4899", "#3b82f6"];

type StageBucket = "plan" | "build" | "review" | "repair" | "merge" | "done" | "blocked";

function taskStageBucket(task: OverviewTask): StageBucket {
  if (task.status === "completed" || task.status === "skipped") return "done";
  if (task.status === "failed" || task.status === "blocked") return "blocked";
  const step = campaignStageStep(task.stage).index;
  if (step === 4) return "repair";
  if (step >= 5) return "merge";
  if (step === 3) return "review";
  if (step === 2) return "build";
  return "plan";
}

function metroStationForTask(task: OverviewTask): string {
  if (task.status === "blocked" || task.status === "failed") return "held";
  const step = campaignStageStep(task.stage).index;
  if (step === 4) return "move";
  if (step === 6 || task.gate) return "wsl";
  if (step === 5) return "push";
  if (step === 3) return "check";
  if (step === 2) return "edit";
  return "brief";
}

/** Compact stage state-machine & active-task metro-map summary for ledger campaigns. */
function LedgerStageSummary({ campaign, tasks, tr }: { campaign?: Campaign; tasks?: OverviewTask[]; tr: Tr }) {
  const resolved = useMemo(
    () => tasks ?? (campaign ? campaign.nodes.map((node) => legacyTask(node)) : []),
    [tasks, campaign],
  );

  const { states, edges, current, activeLines, stations } = useMemo(() => {
    const byBucket: Record<StageBucket, OverviewTask[]> = {
      plan: [], build: [], review: [], repair: [], merge: [], done: [], blocked: [],
    };
    for (const task of resolved) byBucket[taskStageBucket(task)].push(task);

    const detailFor = (bucket: StageBucket) => {
      const list = byBucket[bucket];
      if (!list.length) return tr("작업 0개", "0 tasks");
      const running = list.filter((t) => t.status === "running").length;
      return running > 0
        ? tr(`작업 ${list.length}개 · 진행 ${running}`, `${list.length} tasks · ${running} running`)
        : tr(`작업 ${list.length}개`, `${list.length} tasks`);
    };
    const waitingTone = (bucket: StageBucket) => byBucket[bucket].some((t) => t.gate !== null) ? ("waiting" as const) : ("normal" as const);

    const states: MachineState[] = [
      { id: "plan", title: tr("조사·설계", "Plan & Design"), column: 0, row: 0, count: byBucket.plan.length, detail: detailFor("plan"), tone: waitingTone("plan") },
      { id: "build", title: tr("구현", "Build"), column: 1, row: 0, count: byBucket.build.length, detail: detailFor("build"), tone: waitingTone("build") },
      { id: "review", title: tr("리뷰", "Review"), column: 2, row: 0, count: byBucket.review.length, detail: detailFor("review"), tone: waitingTone("review") },
      { id: "merge", title: tr("머지·배포", "Merge & Check"), column: 3, row: 0, count: byBucket.merge.length, detail: detailFor("merge"), tone: waitingTone("merge") },
      { id: "done", title: tr("완료", "Done"), column: 4, row: 0, count: byBucket.done.length, detail: detailFor("done"), tone: "final" },
      { id: "repair", title: tr("수리·재작업", "Fix & Rework"), column: 2, row: 1, count: byBucket.repair.length, detail: detailFor("repair"), tone: byBucket.repair.length > 0 ? "waiting" : "normal" },
      { id: "blocked", title: tr("막힘·실패", "Blocked & Failed"), column: 3, row: 1, count: byBucket.blocked.length, detail: detailFor("blocked"), tone: byBucket.blocked.length > 0 ? "error" : "normal" },
    ];

    const pastMerge = byBucket.done.length;
    const pastReview = pastMerge + byBucket.merge.length;
    const pastBuild = pastReview + byBucket.review.length + byBucket.repair.length;
    const pastPlan = pastBuild + byBucket.build.length;

    const edges: MachineEdge[] = [
      { from: "plan", to: "build", label: tr("설계 통과", "planned"), count: pastPlan },
      { from: "build", to: "review", label: tr("구현 완료", "built"), count: pastBuild },
      { from: "review", to: "merge", label: tr("리뷰 통과", "reviewed"), count: pastReview },
      { from: "merge", to: "done", label: tr("착지 완료", "landed"), count: pastMerge },
      { from: "review", to: "repair", label: tr("재작업", "rework"), count: byBucket.repair.length },
      { from: "repair", to: "review", label: tr("재리뷰", "re-review"), count: byBucket.repair.length },
      { from: "build", to: "blocked", label: tr("막힘·실패", "blocked"), count: byBucket.blocked.length },
    ];

    const gated = (["merge", "review", "build", "plan"] as StageBucket[]).find((b) => byBucket[b].some((t) => t.gate !== null));
    const mostActive = (["merge", "review", "repair", "build", "plan"] as StageBucket[])
      .map((b) => ({ b, r: byBucket[b].filter((t) => t.status === "running").length }))
      .sort((a, b) => b.r - a.r)[0];
    const current = gated
      ?? (mostActive && mostActive.r > 0 ? mostActive.b : null)
      ?? (byBucket.blocked.length > 0 ? "blocked" : byBucket.done.length > 0 && byBucket.done.length === resolved.length ? "done" : "plan");

    const runningTasks = resolved.filter((t) => t.status === "running");
    const byStation = new Map<string, OverviewTask[]>();
    for (const task of resolved) {
      if (task.status === "completed" || task.status === "skipped") continue;
      const stationId = metroStationForTask(task);
      byStation.set(stationId, [...(byStation.get(stationId) ?? []), task]);
    }
    const stationMeta = (id: string, ko: string, en: string, column: number, row: number): MetroStation => {
      const here = byStation.get(id) ?? [];
      const base = tr(ko, en);
      return {
        id,
        name: here.length > 0 ? `${base} (${here.length})` : base,
        column,
        row,
        waiting: id === "held" ? here.length > 0 : here.some((t) => t.gate !== null),
        note: here.length > 0
          ? here.slice(0, 4).map((t) => `${t.number ?? t.id} ${t.title}`).join(" · ")
          : tr("머무는 작업 없음", "No tasks at this station"),
      };
    };
    const stations: MetroStation[] = [
      stationMeta("brief", "조사·설계", "Plan", 0, 1),
      stationMeta("edit", "구현", "Build", 1, 0),
      stationMeta("check", "리뷰", "Review", 2, 0),
      stationMeta("push", "머지", "Merge", 3, 0),
      stationMeta("move", "수리", "Fix", 1, 2),
      stationMeta("wsl", "배포 확인", "Verify", 2, 2),
      stationMeta("held", "막힘", "Held", 0, 3),
    ];

    const activeLines: MetroLine[] = runningTasks.slice(0, 6).map((task, idx) => {
      const here = metroStationForTask(task);
      const lower = here === "move" || here === "wsl";
      const label = `${task.number ? `${task.number} ` : ""}${task.title}`;
      return {
        id: task.id,
        name: label.length > 22 ? `${label.slice(0, 21)}…` : label,
        stations: lower ? ["brief", "edit", "move", "wsl", "push"] : ["brief", "edit", "check", "push"],
        here,
        status: "running",
        color: METRO_COLORS[idx % METRO_COLORS.length],
      };
    });
    if (activeLines.length > 0 && (byStation.get("held")?.length ?? 0) > 0) {
      const heldCount = byStation.get("held")!.length;
      activeLines.push({
        id: "held-line",
        name: tr(`막힘·보류 ${heldCount}`, `${heldCount} held`),
        stations: ["held"],
        here: "held",
        status: "idle",
        color: "#e5a000",
      });
    }

    return { states, edges, current, activeLines, stations };
  }, [resolved, tr]);

  return (
    <div className={`grid min-w-0 gap-3 p-3 sm:p-4 ${activeLines.length > 0 ? "min-[1280px]:grid-cols-2" : ""}`}>
      <AgentStateMachine
        title={campaign?.title ?? tr("캠페인 단계 현황", "Campaign stage summary")}
        agent={tr(`작업 ${resolved.length}개`, `${resolved.length} tasks`)}
        current={current}
        states={states}
        edges={edges}
      />
      {activeLines.length > 0 && (
        <AgentMetroMap
          title={tr("활성 작업 위치 노선도", "Active task stage map")}
          lines={activeLines}
          stations={stations}
        />
      )}
    </div>
  );
}

/** 파이프라인: the campaign's state graph. Server campaigns show their own (with the tasks in each state) and, when the
 * server allows, edit it on wide screens in a full-window editor; ledger campaigns show a visual stage summary + template preview. */
export default function CampaignPipeline({ source, title, campaign, tasks: ledgerTasks, dispositions, approvals = null, leave, onEditing, tr }: {
  source: PipelineSource; tr: Tr;
  /** The campaign's name, at the head of the editor. */
  title?: string;
  /** Ledger campaign and its live-resolved overview tasks for stage occupancy & active metro map. */
  campaign?: Campaign;
  tasks?: OverviewTask[];
  /** A server campaign's task dispositions by node, and its workflow options' approvals by gate. */
  dispositions?: Record<string, string>; approvals?: Record<string, boolean> | null;
  /** Set while the draft has unsaved changes, so the page asks before leaving. */
  leave?: RefObject<((proceed: () => void) => void) | null>;
  /** Told when editing starts and ends, so the page can give the editor more width. */
  onEditing?: (editing: boolean) => void;
}) {
  const phone = useIsMobile();
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [chosen, setChosen] = useState<string | null>(null);
  const [reads, setReads] = useState(0);
  const [reveal, setReveal] = useState<{ id: string; n: number } | null>(null);
  const [templatePreviewOpen, setTemplatePreviewOpen] = useState(false);
  const serverId = source.kind === "server" ? source.id : null, stamp = source.kind === "server" ? source.stamp : null, sequence = source.kind === "server" ? source.sequence : null;
  useEffect(() => {
    const abort = new AbortController();
    const read = serverId !== null ? getServerCampaignPipeline(serverId, abort.signal).then((detail) => ({ pipeline: detail.pipeline, detail, template: null }))
      : getDefaultPipelineTemplate(abort.signal).then((value) => value && { pipeline: value.pipeline, detail: null, template: value.template });
    read.then((value) => { setLoaded(value); setError(value ? null : tr("등록된 파이프라인 템플릿이 없어요.", "No pipeline template is registered.")); },
      (cause: unknown) => { if (!abort.signal.aborted) setError(cause instanceof Error ? cause.message : String(cause)); });
    return () => abort.abort();
  }, [serverId, stamp, reads, tr]);
  const detail = loaded?.detail ?? null;
  // What the server's occupancy checks read, which only an event moves (attempts, actions and recovery fences included):
  // rechecking on this, not on every poll's timestamp, keeps a quiet run quiet.
  const runtime = JSON.stringify([detail?.execution ?? null, dispositions ?? null, sequence ?? null]);
  const editor = usePipelineEditor({ id: serverId, runtime, detail, reload: () => setReads((value) => value + 1), leave, entry: !phone, tr });
  const { edit, changes, checks, notice } = editor;
  const full = !!edit && !phone;
  useShellFocus(full);
  const shown = edit?.draft ?? loaded?.pipeline;
  const [picking, setPicking] = useState<Picking | null>(null);
  const spot = useRef<HTMLSpanElement>(null);
  useEffect(() => { if (!edit || editor.saving) setPicking(null); }, [!!edit, editor.saving]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => { if (!onEditing) return; onEditing(!!edit); return () => onEditing(false); }, [!!edit, onEditing]); // eslint-disable-line react-hooks/exhaustive-deps
  const placeOf = (value: PipelineDiagnostic) => { const id = edit && diagnosticState(value, [edit.draft]); return edit?.draft.states.some((state) => state.id === id) ? id : null; };
  // Errors mark their states on the canvas and their transitions in the form.
  const flagged = useMemo(() => new Set((checks?.errors ?? []).flatMap((value) => [placeOf(value), value.transition_id]).filter((value): value is string => !!value)), [checks, edit]); // eslint-disable-line react-hooks/exhaustive-deps
  // The parent hands over fresh copies on every poll; equal ones keep the drawn graph.
  const same = JSON.stringify([dispositions, approvals]);
  const tasks = useMemo(() => detail && Object.fromEntries(Object.entries(detail.execution.state_ids).map(([node, state]) => [node, { state, disposition: dispositions?.[node] }])), [detail, same]); // eslint-disable-line react-hooks/exhaustive-deps
  const graph = useMemo(() => shown && buildPipelineGraph(shown, tr, tasks, approvals, !!edit), [shown, tasks, same, tr, !!edit]); // eslint-disable-line react-hooks/exhaustive-deps
  const nodeData = useMemo(() => new Map(graph ? [...graph.states.values()].map((state) => {
    const pins = graph.pins.get(state.id)!, occupancy = occupancyTexts(graph, state.id, tr), title = stateName(state, state.id, tr);
    const timeout = state.timeout ? durationText(state.timeout.after_seconds, tr) : "";
    const data: StateNodeData & { aria: string } = {
      kind: state.kind, title, kindLabel: kindName(state.kind, tr), pins, entry: tr("진입", "In"), main: !graph.off.includes(state.id),
      occupancy, foot: state.owner || timeout ? [state.owner ?? "", timeout] : null, flagged: flagged.has(state.id),
      mark: changes?.states.added.includes(state.id) ? tr("새로", "New") : changes?.touched.has(state.id) ? tr("바뀜", "Changed") : null,
      add: edit && !state.kind.startsWith("end_") ? tr("+ 전환", "+ Transition") : null,
      badges: [...state.id === graph.main[0] ? [{ text: tr("시작", "Start"), start: true }] : [], ...(graph.gates.get(state.id) ?? []).map((gate) => ({ text: gateText(graph, gate, tr), off: !!graph.approvals && !graph.approvals[gate] }))],
      aria: [title, kindName(state.kind, tr), ...occupancy.map(({ text }) => text), pinSummary(graph, state.id, tr)].filter(Boolean).join(", "),
    };
    return [state.id, data];
  }) : []), [graph, flagged, changes, tr, !!edit]); // eslint-disable-line react-hooks/exhaustive-deps
  const eventData = useMemo(() => new Map(graph?.events.map((event) => [event.id, { title: eventName(event.type, tr), target: `→ ${stateName(graph.states.get(event.state_id), event.state_id, tr)}` }]) ?? []), [graph, tr]);

  if (graph?.states.size === 0 && !edit) return <WidgetState kind="empty" title={tr("이 파이프라인에는 상태가 없어요.", "This pipeline has no states.")} />;
  if (!graph) return error ? <WidgetState kind="error" title={tr("파이프라인을 불러오지 못했어요.", "Could not load the pipeline.")} description={error} />
    : <WidgetState kind="loading" title={tr("파이프라인을 불러오고 있어요.", "Loading the pipeline.")} />;
  const template = loaded!.template ?? (loaded!.pipeline.template_id ? { id: loaded!.pipeline.template_id, version: loaded!.pipeline.template_version ?? 0 } : null);
  const transitions = shown!.transitions.length;
  const note = !detail ? tr("원장 캠페인은 서버 파이프라인을 쓰지 않아요. 기본 템플릿을 보여 드려요.", "Ledger campaigns do not use a server pipeline. This is the built-in template.")
    : detail.source === "legacy_projection" ? tr("저장한 파이프라인이 없어서 기본 템플릿대로 보여요.", "No pipeline is saved, so the built-in template is shown.") : null;
  const summary = <>
    <InspSection className="border-t-0 gap-1">
      <Cap>{detail ? tr("이 캠페인의 파이프라인", "This campaign's pipeline") : tr("기본 템플릿", "Built-in template")}</Cap>
      <Num className="text-sm text-[var(--fg)]">{tr(`상태 ${graph.states.size} · 전환 ${transitions} · 이벤트 ${graph.events.length}`, `${graph.states.size} states · ${transitions} transitions · ${graph.events.length} events`)}</Num>
      {detail && <Cap><Num>{[tr(`정의 rev ${detail.definition_revision}`, `Definition rev ${detail.definition_revision}`), detail.execution.run_id ? tr(`실행 ${detail.execution.run_id}`, `Run ${detail.execution.run_id}`) : tr("실행 전", "No run yet")].join(" · ")}</Num></Cap>}
    </InspSection>
    <p className="m-0 px-4 py-3 text-xs text-[var(--fg-muted)]">{tr("상태를 고르면 담당과 전환을 여기에 보여 줘요.", "Choose a state to see its owner and transitions here.")}</p>
  </>;
  const errors = checks?.errors.length ?? 0;
  const blocked = editor.saving || !checks || checks.pending || errors > 0 || !editor.dirty || detail?.editable === false;
  const choose = (id: string | null) => { setChosen(id); if (id) setReveal((value) => ({ id, n: (value?.n ?? 0) + 1 })); };
  const locked = <Cap className="ml-auto">{tr("준비됨이나 중지됨일 때만 고칠 수 있어요.", "Only a prepared or stopped campaign can be edited.")}</Cap>;
  // A pin let go on a state points there and shows the state it leaves; one let go elsewhere, or a press of 찾기·만들기, asks where.
  const link = (from: string, pin: string, to: string) => { if (!edit) return; editor.update(() => connect(edit.draft, from, pin, to)[0]); setChosen(from); setPicking(null); };
  const make = (kind: string, name: string) => { if (!edit) return; const [next, id] = connectNew(edit.draft, kind, name, picking?.from ?? undefined); editor.update(() => next); choose(id); setPicking(null); };
  const pinLabel = (from: { state: string; pin: string }) => graph.pins.get(from.state)?.find((pin) => pin.id === from.pin)?.label ?? tr("+ 전환", "+ Transition");
  const tools = edit ? !phone && <div role="toolbar" aria-label={tr("편집 도구", "Edit tools")} className="ml-auto flex flex-wrap items-center gap-2">
    {detail?.editable === false && locked}
    {errors > 0 && <Chip tone="err"><Num>{tr(`검사 오류 ${errors}`, `${errors} check errors`)}</Num></Chip>}
    {editor.dirty && <Chip tone="warn"><Num>{tr(`저장 안 됨 · ${changesText(changes!, tr)}`, `Unsaved · ${changesText(changes!, tr)}`)}</Num></Chip>}
    <Button variant="secondary" size="lg" icon={<Undo2 aria-hidden />} disabled={!editor.dirty || editor.saving} onClick={editor.revert}>{tr("되돌리기", "Revert")}</Button>
    <Button variant="primary" size="lg" loading={editor.saving} disabled={blocked} onClick={() => void editor.save()}>{tr("저장", "Save")}</Button>
  </div> : detail && !phone && (editor.waiting ? <span className="ml-auto flex flex-wrap items-center gap-2">
    {detail.editable ? <Button variant="secondary" size="sm" icon={<PencilLine aria-hidden />} onClick={editor.resume}>{tr("이어서 편집", "Continue editing")}</Button> : locked}
    <Button variant="ghost" size="sm" onClick={editor.forget}>{tr("초안 버리기", "Discard the draft")}</Button>
  </span> : detail.editable ? <Button variant="secondary" size="sm" className="ml-auto" icon={<PencilLine aria-hidden />} onClick={() => editor.begin(detail)}>{tr("편집", "Edit")}</Button> : locked);
  const latest = notice?.kind === "conflict" ? notice.latest : null;
  const status = notice && <div role={notice.kind === "saved" ? "status" : "alert"} className="grid gap-2 border-b border-[var(--line-soft)] px-4 py-2.5 text-[13px] text-[var(--fg)]">
    {notice.kind === "saved" && tr(`정의 rev ${notice.revision}에 저장했어요.`, `Saved as definition rev ${notice.revision}.`)}
    {notice.kind === "lost" && tr("응답을 받지 못했어요. 같은 내용으로 다시 저장해도 두 번 적용되지 않아요.", "No answer arrived. Saving the same content again cannot apply it twice.")}
    {notice.kind === "error" && <span className="text-[var(--err)]">{notice.text}</span>}
    {latest && edit && <>
      <span>{tr(`다른 곳에서 정의가 바뀌었어요(rev ${latest.definition_revision}). 초안은 그대로 둬요.`, `The definition changed elsewhere (rev ${latest.definition_revision}). Your draft is kept.`)}</span>
      <Cap>{tr("다른 곳에서 바뀐 파이프라인", "Pipeline changed elsewhere")}: {changesText(draftChanges(edit.base, latest.pipeline), tr) || tr("없음", "none")}</Cap>
      <div className="flex flex-wrap gap-2">
        <Button variant="secondary" size="sm" disabled={editor.saving || errors > 0 || !latest.editable} onClick={() => void editor.save(latest)}>{tr(`rev ${latest.definition_revision} 위에 저장`, `Save over rev ${latest.definition_revision}`)}</Button>
        <Button variant="ghost" size="sm" disabled={editor.saving} onClick={() => editor.begin(latest)}>{tr("초안 버리고 최신으로", "Drop the draft for the latest")}</Button>
      </div>
    </>}
  </div>;
  // The editor covers the window (the shell drops its rail and bars, the page its heading and tabs) with a head of its own.
  return <div className={full ? "campaign-pipeline fixed inset-0 z-50 flex min-w-0 flex-col bg-[var(--bg)]" : "campaign-pipeline grid min-w-0 gap-3"}>
    <div className={full ? "flex min-h-0 min-w-0 flex-1 flex-col" : "min-w-0 overflow-hidden rounded-[var(--r-md)] border border-[var(--line-soft)] bg-[var(--bg-1)]"}>
      {full ? <header className="flex min-h-14 min-w-0 flex-wrap items-center gap-x-4 gap-y-2 border-b border-[var(--line-soft)] px-6 py-2">
        <Button variant="ghost" icon={<ArrowLeft aria-hidden className="size-4" />} disabled={editor.saving} onClick={editor.close}>{tr("캠페인", "Campaign")}</Button>
        <div className="grid min-w-0 flex-[0_1_auto] gap-0.5">
          <strong className="truncate text-[15px] text-[var(--fg)]">{title ?? tr("파이프라인", "Pipeline")}</strong>
          <Cap className="truncate"><Num>{[tr("파이프라인 편집", "Editing the pipeline"), template && tr(`템플릿 ${template.id} v${template.version}`, `Template ${template.id} v${template.version}`), tr(`정의 rev ${edit.revision}`, `Definition rev ${edit.revision}`)].filter(Boolean).join(" · ")}</Num></Cap>
        </div>
        {tools}
      </header> : <header className="flex min-h-14 min-w-0 flex-wrap items-center gap-x-3 gap-y-1.5 border-b border-[var(--line-soft)] px-4 py-2.5">
        <strong className="text-[15px] text-[var(--fg)]">{tr("파이프라인", "Pipeline")}</strong>
        {template && <Chip className="font-mono">{tr(`템플릿 ${template.id} v${template.version}`, `Template ${template.id} v${template.version}`)}</Chip>}
        {detail?.source === "saved" && <Chip tone="info">{tr("저장됨", "Saved")}</Chip>}
        {!edit && <Cap className="ml-auto max-[899px]:hidden"><Num>{tr(`상태 ${graph.states.size} · 전환 ${transitions}`, `${graph.states.size} states · ${transitions} transitions`)}</Num></Cap>}
        {!detail && <Button variant="secondary" size="sm" className={edit ? "" : "max-[899px]:ml-auto"} aria-expanded={templatePreviewOpen} onClick={() => setTemplatePreviewOpen((v) => !v)}>
          {templatePreviewOpen ? tr("템플릿 미리보기 접기", "Hide template preview") : tr("기본 9단계 템플릿 미리보기", "Preview built-in 9-stage template")}
        </Button>}
        {tools}
        {note && <Desc className="w-full">{note}</Desc>}
        {editor.waiting && detail && !edit && <Desc role="status" className="w-full">{[tr("저장하지 않은 초안이 있어요.", "An unsaved draft is waiting."),
          editor.waiting.revision !== detail.definition_revision && tr(`초안 기준 rev ${editor.waiting.revision} · 지금 서버 rev ${detail.definition_revision}. 저장하면 다른 곳에서 바뀐 내용을 먼저 보여 드려요.`,
            `The draft is on rev ${editor.waiting.revision}; the server is at rev ${detail.definition_revision}. Saving shows what changed elsewhere first.`)].filter(Boolean).join(" ")}</Desc>}
      </header>}
      {status}
      {error && <WidgetState kind="stale" title={tr("파이프라인을 갱신하지 못했어요.", "Could not refresh the pipeline.")} description={error} />}
      {!detail && <LedgerStageSummary campaign={campaign} tasks={ledgerTasks} tr={tr} />}
      {!detail && !templatePreviewOpen ? <div className="flex flex-wrap items-center justify-between gap-2 border-t border-[var(--line-soft)] px-4 py-3 text-xs text-[var(--fg-muted)]">
        <span>{tr("원장 캠페인은 작업별 상태·선행 DAG로 진행되며 서버 파이프라인 런타임을 실행하지 않아요. 필요할 때 위 버튼으로 기본 9단계 템플릿 구조를 펼쳐 볼 수 있어요.", "Ledger campaigns run on the task DAG rather than the server pipeline runtime. Expand the preview above to inspect the built-in 9-stage template.")}</span>
      </div> : phone ? <div className="grid gap-3 p-3">
        <p className="m-0 flex items-center gap-2 rounded-lg border border-[var(--line-soft)] bg-[var(--bg-2)] px-3 py-2 text-[13px] text-[var(--fg-dim)]"><Monitor aria-hidden className="size-4 shrink-0" />{detail ? tr("상태와 전환은 넓은 화면에서 고쳐요. 폰에서는 보기만 해요.", "States and transitions are edited on wider screens. Phones only show them.")
          : tr("폰에서는 보기만 해요. 넓은 화면에서 그래프로 볼 수 있어요.", "Phones only show the list. Wider screens draw the graph.")}</p>
        <PipelineStateList graph={graph} chosen={chosen} onChoose={setChosen} tr={tr} />
      </div> : <PipelineCanvas label={tr("파이프라인 그래프", "Pipeline graph")} graph={graph} nodeData={nodeData} eventData={eventData} chosen={chosen} onChoose={setChosen} reveal={reveal} tr={tr}
        edit={edit ? { busy: editor.saving, onLink: link, onDrop: (state, pin, at) => { setChosen(state); setPicking({ from: { state, pin }, anchor: spot, at }); }, onAdd: (button) => setPicking({ from: null, anchor: { current: button } }) } : undefined}
        checks={edit && <PipelineChecks checks={checks} graphs={[edit.draft, edit.base]} onPick={(value) => { const id = placeOf(value); if (id) choose(id); }} tr={tr} />}
        inspector={edit ? <PipelineEditPanel graph={graph} draft={edit.draft} revision={edit.revision} chosen={chosen} flagged={flagged} changes={changes} busy={editor.saving} update={editor.update} onChoose={setChosen}
          onFind={(transition, button) => setPicking({ from: { state: transition.from, pin: transition.id }, anchor: { current: button } })} tr={tr} />
          : <PipelineInspector graph={graph} id={chosen} summary={summary} tr={tr} />}>
        <div className="sr-only"><PipelineStateList graph={graph} open tr={tr} /></div>
        {/* A pin let go on the canvas: a dashed line from it to the window asking where it goes. */}
        {picking?.at && <svg aria-hidden className="pointer-events-none absolute inset-0 z-[5] size-full overflow-visible"><path d={`M${picking.at.fx},${picking.at.fy} L${picking.at.x},${picking.at.y}`} stroke="var(--accent)" strokeWidth={2} strokeDasharray="6 5" fill="none" /></svg>}
        <span ref={spot} aria-hidden className="pointer-events-none absolute size-px" style={picking?.at ? { left: picking.at.x, top: picking.at.y } : { left: 0, top: 0 }} />
        {edit && <PipelineStatePicker open={!!picking} anchor={picking?.anchor ?? spot} draft={edit.draft} tr={tr} onPick={(to) => picking?.from && link(picking.from.state, picking.from.pin, to)} onCreate={make} onClose={() => setPicking(null)}
          from={picking?.from ? { name: stateName(edit.draft.states.find((state) => state.id === picking.from!.state), picking.from.state, tr), label: pinLabel(picking.from) } : null} />}
      </PipelineCanvas>}
    </div>
    <Dialog open={editor.leaving !== null} onOpenChange={(open) => { if (!open) editor.decide(false); }} size="md"
      title={tr("저장하지 않은 변경이 있어요", "You have unsaved changes")}
      description={tr(`${changes ? changesText(changes, tr) : ""}. 나가면 이 변경은 사라져요.`, `${changes ? changesText(changes, tr) : ""}. Leaving drops them.`)}
      footer={<>
        <Button variant="secondary" disabled={editor.saving} onClick={() => editor.decide(true)}>{tr("버리고 나가기", "Discard and leave")}</Button>
        <Button variant="primary" onClick={() => editor.decide(false)}>{tr("계속 편집", "Keep editing")}</Button>
      </>} />
  </div>;
}
