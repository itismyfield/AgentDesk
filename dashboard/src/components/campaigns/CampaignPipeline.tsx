import { Monitor } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { getDefaultPipelineTemplate, getServerCampaignPipeline, type CampaignPipeline as Detail, type PipelineDefinition } from "../../api/serverCampaignPipeline";
import { Cap, Chip, Desc, InspSection, Num } from "../common/KitPrimitives";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import { WidgetState } from "../common/WidgetState";
import type { Tr } from "./campaignPresentation";
import PipelineCanvas, { type StateNodeData } from "./PipelineCanvas";
import PipelineInspector from "./PipelineInspector";
import PipelineStateList from "./PipelineStateList";
import { buildPipelineGraph, durationText, eventName, gateText, kindName, occupancyTexts, pinSummary, stateName } from "./pipelineModel";

/** A server campaign reads its own pipeline again whenever `stamp` (its view's as_of) moves; a ledger campaign has none. */
export type PipelineSource = { kind: "server"; id: string; stamp: string } | { kind: "legacy" };
type Loaded = { pipeline: PipelineDefinition; detail: Detail | null; template: { id: string; version: number } | null };

/** 파이프라인: the campaign's state graph, read only. Server campaigns show their own (with the tasks in each state);
 * ledger campaigns show the built-in template and say so. Phones get the numbered list instead of the canvas. */
export default function CampaignPipeline({ source, dispositions, approvals = null, tr }: {
  source: PipelineSource; tr: Tr;
  /** A server campaign's task dispositions by node, and its workflow options' approvals by gate. */
  dispositions?: Record<string, string>; approvals?: Record<string, boolean> | null;
}) {
  const phone = useIsMobile();
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [chosen, setChosen] = useState<string | null>(null);
  const serverId = source.kind === "server" ? source.id : null, stamp = source.kind === "server" ? source.stamp : null;
  useEffect(() => {
    const abort = new AbortController();
    const read = serverId !== null ? getServerCampaignPipeline(serverId, abort.signal).then((detail) => ({ pipeline: detail.pipeline, detail, template: null }))
      : getDefaultPipelineTemplate(abort.signal).then((value) => value && { pipeline: value.pipeline, detail: null, template: value.template });
    read.then((value) => { setLoaded(value); setError(value ? null : tr("등록된 파이프라인 템플릿이 없어요.", "No pipeline template is registered.")); },
      (cause: unknown) => { if (!abort.signal.aborted) setError(cause instanceof Error ? cause.message : String(cause)); });
    return () => abort.abort();
  }, [serverId, stamp, tr]);
  const detail = loaded?.detail ?? null;
  // The parent hands over fresh copies on every poll; equal ones keep the drawn graph.
  const same = JSON.stringify([dispositions, approvals]);
  const tasks = useMemo(() => detail && Object.fromEntries(Object.entries(detail.execution.state_ids).map(([node, state]) => [node, { state, disposition: dispositions?.[node] }])), [detail, same]); // eslint-disable-line react-hooks/exhaustive-deps
  const graph = useMemo(() => loaded && buildPipelineGraph(loaded.pipeline, tr, tasks, approvals), [loaded, tasks, same, tr]); // eslint-disable-line react-hooks/exhaustive-deps
  const nodeData = useMemo(() => new Map(graph ? [...graph.states.values()].map((state) => {
    const pins = graph.pins.get(state.id)!, occupancy = occupancyTexts(graph, state.id, tr), title = stateName(state, state.id, tr);
    const timeout = state.timeout ? durationText(state.timeout.after_seconds, tr) : "";
    const data: StateNodeData & { aria: string } = {
      kind: state.kind, title, kindLabel: kindName(state.kind, tr), pins, entry: tr("진입", "In"), main: !graph.off.includes(state.id),
      occupancy, foot: state.owner || timeout ? [state.owner ?? "", timeout] : null,
      badges: [...state.id === graph.main[0] ? [{ text: tr("시작", "Start"), start: true }] : [], ...(graph.gates.get(state.id) ?? []).map((gate) => ({ text: gateText(graph, gate, tr), off: !!graph.approvals && !graph.approvals[gate] }))],
      aria: [title, kindName(state.kind, tr), ...occupancy.map(({ text }) => text), pinSummary(graph, state.id, tr)].filter(Boolean).join(", "),
    };
    return [state.id, data];
  }) : []), [graph, tr]);
  const eventData = useMemo(() => new Map(graph?.events.map((event) => [event.id, { title: eventName(event.type, tr), target: `→ ${stateName(graph.states.get(event.state_id), event.state_id, tr)}` }]) ?? []), [graph, tr]);

  if (graph?.states.size === 0) return <WidgetState kind="empty" title={tr("이 파이프라인에는 상태가 없어요.", "This pipeline has no states.")} />;
  if (!graph) return error ? <WidgetState kind="error" title={tr("파이프라인을 불러오지 못했어요.", "Could not load the pipeline.")} description={error} />
    : <WidgetState kind="loading" title={tr("파이프라인을 불러오고 있어요.", "Loading the pipeline.")} />;
  const template = loaded!.template ?? (loaded!.pipeline.template_id ? { id: loaded!.pipeline.template_id, version: loaded!.pipeline.template_version ?? 0 } : null);
  const transitions = loaded!.pipeline.transitions.length;
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
  return <div className="campaign-pipeline grid min-w-0 gap-3">
    <div className="min-w-0 overflow-hidden rounded-[var(--r-md)] border border-[var(--line-soft)] bg-[var(--bg-1)]">
      <header className="flex min-h-14 min-w-0 flex-wrap items-center gap-x-3 gap-y-1 border-b border-[var(--line-soft)] px-4 py-2">
        <strong className="text-[15px] text-[var(--fg)]">{tr("파이프라인", "Pipeline")}</strong>
        {template && <Chip className="font-mono">{tr(`템플릿 ${template.id} v${template.version}`, `Template ${template.id} v${template.version}`)}</Chip>}
        {detail?.source === "saved" && <Chip tone="info">{tr("저장됨", "Saved")}</Chip>}
        <Cap className="ml-auto max-[899px]:hidden"><Num>{tr(`상태 ${graph.states.size} · 전환 ${transitions}`, `${graph.states.size} states · ${transitions} transitions`)}</Num></Cap>
        {note && <Desc className="w-full">{note}</Desc>}
      </header>
      {error && <WidgetState kind="stale" title={tr("파이프라인을 갱신하지 못했어요.", "Could not refresh the pipeline.")} description={error} />}
      {phone ? <div className="grid gap-3 p-3">
        <p className="m-0 flex items-center gap-2 rounded-lg border border-[var(--line-soft)] bg-[var(--bg-2)] px-3 py-2 text-[13px] text-[var(--fg-dim)]"><Monitor aria-hidden className="size-4 shrink-0" />{tr("폰에서는 보기만 해요. 넓은 화면에서 그래프로 볼 수 있어요.", "Phones only show the list. Wider screens draw the graph.")}</p>
        <PipelineStateList graph={graph} chosen={chosen} onChoose={setChosen} tr={tr} />
      </div> : <PipelineCanvas label={tr("파이프라인 그래프", "Pipeline graph")} graph={graph} nodeData={nodeData} eventData={eventData} chosen={chosen} onChoose={setChosen} tr={tr}
        inspector={<PipelineInspector graph={graph} id={chosen} summary={summary} tr={tr} />}>
        <div className="sr-only"><PipelineStateList graph={graph} open tr={tr} /></div>
      </PipelineCanvas>}
    </div>
  </div>;
}
