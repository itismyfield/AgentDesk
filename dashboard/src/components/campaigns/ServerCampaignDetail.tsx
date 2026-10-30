import { useEffect, useRef } from "react";
import { serverCampaignProfile, type ServerCampaignDocument, type ServerCampaignEvent } from "../../api/serverCampaigns";
import type { Tr } from "./campaignPresentation";
import { safeCampaignLink } from "./campaignModel";
import ServerCampaignControls, { CommandButton, type ControlCommand } from "./ServerCampaignControls";
import {
  DISPOSITION_LABELS, REASON_LABELS, STAGE_LABELS, commandAvailability, displayText, feedbackText,
  nodeReasons, rejectionText, unknownObservations, type Connection, type StopIntent,
} from "./serverCampaignModel";

const VERIFICATION_LABELS: Record<string, [string, string]> = {
  test: ["시험", "Tests"], main: ["main 반영", "In main"], deployment: ["배포 확인", "Deployment"], operational: ["운영 확인", "Operational"],
};
const time = (value: string | null) => value ? new Date(value).toLocaleString() : "—";

export default function ServerCampaignDetail({
  document, events, eventsFailed, connection, selectedNode, onSelectNode, stopIntent, busy, commandError, onCommand, onEdit, onCopy, link, tr,
}: {
  document: ServerCampaignDocument;
  events: ServerCampaignEvent[] | null;
  eventsFailed: boolean;
  connection: Connection;
  selectedNode: string | null;
  onSelectNode: (id: string | null) => void;
  stopIntent: StopIntent | null;
  busy: boolean;
  commandError: string | null;
  onCommand: (command: ControlCommand, nodeId?: string) => void;
  onEdit: () => void;
  onCopy: () => void;
  link: string;
  tr: Tr;
}) {
  const { definition, view } = document;
  const profile = serverCampaignProfile(view);
  const unknown = unknownObservations(view);
  const runtimes = new Map(view.nodes.map((node) => [node.id, node]));
  const selected = definition.nodes.find((node) => node.id === selectedNode);
  const selectedView = selected ? runtimes.get(selected.id) : undefined;
  const edit = commandAvailability(view, "edit_definition", connection);
  const feedback = feedbackText(link, view, tr);
  const inspectorRef = useRef<HTMLElement>(null);
  // A narrow screen stacks the details under every task; bring the selected one into view.
  useEffect(() => {
    if (selectedNode && window.matchMedia?.("(max-width: 900px)").matches) inspectorRef.current?.scrollIntoView({ behavior: "smooth", block: "start" });
  }, [selectedNode]);
  return <div className="campaign-server-detail">
    <div className="campaign-row campaign-summary-header">
      <div className="campaign-server-title"><h3>{definition.title}</h3><p className="campaign-muted">{definition.repository === "adk" ? "AgentDesk" : "CookingHeart"} · {definition.goal}</p></div>
      <span className={`campaign-badge campaign-status-${view.display}`}>{displayText(view, tr)}</span>
      {view.unknown && <span className="campaign-badge campaign-status-blocked">{tr("확인 불가", "Unknown")}</span>}
    </div>
    <p className="campaign-freshness">
      {tr("서버 관측", "Server as of")} {time(view.as_of)} · {tr("정의", "definition")} r{view.definition_revision} · {tr("이벤트", "event")} #{view.last_event_sequence}{view.run_id && ` · run ${view.run_id}`}
    </p>
    {unknown.length > 0 && <ul className="campaign-filter-note" aria-label={tr("확인 불가 관측", "Unknown observations")}>
      {unknown.map((entry) => <li key={`${entry.host}:${entry.session}`}>{entry.host} · {entry.session} · {entry.reason} · {tr("마지막 확인", "last observed")} {time(entry.lastObservedAt)}</li>)}
    </ul>}
    {view.reasons.length > 0 && <ul className="campaign-server-reasons" aria-label={tr("서버 판정 이유", "Server reasons")}>
      {view.reasons.map((reason, index) => <li key={index}>{tr(...REASON_LABELS[reason.code])} · {reason.target}</li>)}
    </ul>}
    <div className="campaign-actions">
      <span className="campaign-server-command"><button type="button" disabled={!edit.enabled} onClick={onEdit}>{tr("정의 편집", "Edit definition")}</button>{edit.reason && <small>{rejectionText(edit.reason, tr)}</small>}</span>
      {view.control === "succeeded" && <button type="button" onClick={onCopy}>{tr("정의를 복사해 새 캠페인", "Copy definition to a new campaign")}</button>}
    </div>
    <ServerCampaignControls view={view} connection={connection} stopIntent={stopIntent} busy={busy} error={commandError} onCommand={(command) => onCommand(command)} tr={tr} />
    <section className="campaign-server-check grid min-w-0 gap-2 rounded-lg border border-th-border-subtle p-4" aria-label={tr("시작 점검", "Start check")}>
      <h4>{tr("시작 점검", "Start check")}</h4>
      <p>{tr("실행 준비", "Execution readiness")}: {view.execution_readiness === "unavailable" ? tr("미제공", "unavailable") : view.execution_readiness}</p>
      {profile && <>
        <ul>{profile.models.map((model) => <li key={model.role}>{model.role === "implementation" ? tr("구현", "Implementation") : tr("리뷰", "Review")}: {model.selection.state === "pinned" ? `${model.selection.model} · ${model.selection.effort}` : model.selection.reason}</li>)}</ul>
        <p>{tr(`동시 작업 ${profile.limits.concurrent_tasks} · 품질 수리 ${profile.limits.quality_repairs} · 설계 리뷰 ${profile.limits.design_review_rounds}`, `Concurrent tasks ${profile.limits.concurrent_tasks} · quality repairs ${profile.limits.quality_repairs} · design reviews ${profile.limits.design_review_rounds}`)}</p>
        <p className="campaign-muted">{profile.unavailable_reason}</p>
      </>}
    </section>
    <div className={`campaign-workbench ${selected ? "has-inspector" : ""}`}>
      <ul className="campaign-server-tasks grid min-w-0 gap-2" aria-label={tr("작업", "Tasks")}>
        {definition.nodes.map((node) => {
          const runtime = runtimes.get(node.id)?.runtime;
          const reasons = runtimes.has(node.id) ? nodeReasons(runtimes.get(node.id)!, view.reasons) : [];
          return <li key={node.id}><button type="button" className="campaign-server-task-row grid w-full min-w-0 gap-2 p-3 text-left [&_strong]:text-sm [&_strong]:font-medium [&_strong]:leading-6 [&_strong]:[overflow-wrap:anywhere]" data-node-id={node.id} aria-pressed={node.id === selectedNode} onClick={() => onSelectNode(node.id)}>
            <strong>{node.title}</strong>
            <span>{runtime ? <>
              <span className={`campaign-badge campaign-status-${runtime.disposition}`}>{tr(...DISPOSITION_LABELS[runtime.disposition])}</span>
              {" "}{tr(...STAGE_LABELS[runtime.stage])} · {tr(`계열 ${runtime.series}`, `series ${runtime.series}`)}
              {runtime.main_integrated && <> · {tr("main 반영", "in main")}</>}
              {runtime.main_integrated && !runtime.acceptance_complete && <> · {tr("최종 확인 남음", "final acceptance remaining")}</>}
            </> : tr("서버 상태 없음", "No server state")}</span>
            {reasons.length > 0 && <small>{reasons.map((code) => tr(...REASON_LABELS[code])).join(" · ")}</small>}
          </button></li>;
        })}
      </ul>
      {selected && <aside ref={inspectorRef} className="campaign-inspector min-w-0 rounded-lg border border-th-border-subtle bg-th-bg-surface p-4 [overflow-wrap:anywhere] scroll-mt-28" aria-label={tr("선택 작업", "Selected task")}>
        <div className="campaign-row campaign-inspector-header mb-3 flex items-center justify-between gap-3 border-b border-th-border-subtle pb-3 text-xs text-th-text-muted"><span>{selected.id}</span><button type="button" onClick={() => onSelectNode(null)} aria-label={tr("상세 닫기", "Close details")}>×</button></div>
        <h4>{selected.title}</h4>
        {safeCampaignLink(selected.issue_url) && <p><a href={safeCampaignLink(selected.issue_url)} target="_blank" rel="noreferrer">{selected.issue_url}</a></p>}
        <ul aria-label={tr("수용조건", "Acceptance")}>{selected.acceptance.map((ac) => <li key={ac.id}><code>{ac.id}</code> {ac.description} · {tr(...(VERIFICATION_LABELS[ac.verification] ?? [ac.verification, ac.verification]))}</li>)}</ul>
        {selectedView && <dl className="campaign-detail-grid grid grid-cols-1 gap-4 text-sm sm:grid-cols-2 [&_dt]:mb-1 [&_dt]:text-xs [&_dd]:[overflow-wrap:anywhere]">
          <div><dt>{tr("단계", "Stage")}</dt><dd>{tr(...STAGE_LABELS[selectedView.runtime.stage])}</dd></div>
          <div><dt>{tr("현재 시도", "Current attempt")}</dt><dd>{selectedView.runtime.current_attempt ?? "—"}</dd></div>
          <div><dt>{tr("품질 실패 · 수리 · 리뷰", "Failures · repairs · reviews")}</dt><dd>{selectedView.runtime.quality_failures} · {selectedView.runtime.repair_cycles} · {selectedView.runtime.review_rounds}</dd></div>
          <div><dt>{tr("main 반영 · 최종 수용", "In main · accepted")}</dt><dd>{selectedView.runtime.main_integrated ? "✓" : "—"} · {selectedView.runtime.acceptance_complete ? "✓" : "—"}</dd></div>
        </dl>}
        {selectedView && <>
          <h4>{tr("세션", "Sessions")}</h4>
          <ul>{view.sessions.filter((session) => session.attempt_id === selectedView.runtime.current_attempt).map((session) => <li key={`${session.identity.session}:${session.identity.incarnation}`}>
            {session.identity.gate.host} · {session.identity.session} · {session.observation.state}
          </li>)}</ul>
          <p>{tr("직접 후속", "Direct successors")}: {selectedView.direct_successors.join(", ") || tr("없음", "none")}</p>
          <p>{tr("이 작업이 main에 반영되면 준비될 후속", "Ready once this reaches main")}: {selectedView.ready_after_main.join(", ") || tr("없음", "none")}</p>
          {selectedView.runtime.disposition === "failed" && <CommandButton view={view} command="prepare_retry" connection={connection} busy={busy} onCommand={() => onCommand("prepare_retry", selected.id)} tr={tr} />}
        </>}
      </aside>}
    </div>
    <details className="campaign-connections">
      <summary>{tr("이벤트", "Events")} · {events?.length ?? "—"}{eventsFailed && ` · ${tr("이벤트를 갱신하지 못했습니다", "Could not refresh events")}`}</summary>
      <ol>{(events ?? []).slice().reverse().map((event) => <li key={event.sequence}>#{event.sequence} · {time(event.observed_at)} · {event.kind.event}{event.run_id && ` · ${event.run_id}`}</li>)}</ol>
    </details>
    <label className="campaign-server-feedback grid min-w-0 gap-3 [&_button]:justify-self-start">{tr("피드백용 복사", "Copy for feedback")}
      <textarea aria-label={tr("피드백용 복사", "Copy for feedback")} readOnly rows={5} value={feedback} />
      <button type="button" onClick={() => void globalThis.navigator?.clipboard?.writeText(feedback).catch(() => undefined)}>{tr("복사", "Copy")}</button>
    </label>
  </div>;
}
