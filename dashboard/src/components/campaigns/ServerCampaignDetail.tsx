import { useState } from "react";
import { serverCampaignProfile, type ServerCampaignDocument } from "../../api/serverCampaigns";
import { Button } from "../../kit/free/button-clean";
import { BottomSheet } from "../common/overlay/BottomSheet";
import { Modal } from "../common/overlay/Modal";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import { Badge, type Tr } from "./campaignPresentation";
import { safeCampaignLink } from "./campaignModel";
import { GATE_LABELS, type PendingGate } from "./campaignOverviewModel";
import { CommandButton, type ControlCommand } from "./ServerCampaignControls";
import { DISPOSITION_LABELS, REASON_LABELS, STAGE_LABELS, feedbackText, unknownObservations, type Connection } from "./serverCampaignModel";

const VERIFICATION_LABELS: Record<string, [string, string]> = {
  test: ["시험", "Tests"], main: ["main 반영", "In main"], deployment: ["배포 확인", "Deployment"], operational: ["운영 확인", "Operational"],
};
const time = (value: string | null) => value ? new Date(value).toLocaleString() : "—";

/** Server-only parts under the overview: why the server holds, sessions it cannot see, the start check and the feedback copy. */
export default function ServerCampaignSections({ document, link, tr }: { document: ServerCampaignDocument; link: string; tr: Tr }) {
  const { view } = document;
  const profile = serverCampaignProfile(view);
  const unknown = unknownObservations(view);
  const feedback = feedbackText(link, view, tr);
  return <div className="campaign-server-detail grid min-w-0 gap-3">
    <p className="campaign-freshness">
      {tr("서버 관측", "Server as of")} {time(view.as_of)} · {tr("정의", "definition")} r{view.definition_revision} · {tr("이벤트", "event")} #{view.last_event_sequence}{view.run_id && ` · run ${view.run_id}`}
    </p>
    {unknown.length > 0 && <ul className="campaign-filter-note" aria-label={tr("확인 불가 관측", "Unknown observations")}>
      {unknown.map((entry) => <li key={`${entry.host}:${entry.session}`}>{entry.host} · {entry.session} · {entry.reason} · {tr("마지막 확인", "last observed")} {time(entry.lastObservedAt)}</li>)}
    </ul>}
    {view.reasons.length > 0 && <ul className="campaign-server-reasons" aria-label={tr("서버 판정 이유", "Server reasons")}>
      {view.reasons.map((reason, index) => <li key={index}>{tr(...REASON_LABELS[reason.code])} · {reason.target}</li>)}
    </ul>}
    <section className="campaign-server-check grid min-w-0 gap-2 rounded-lg border border-th-border-subtle p-4" aria-label={tr("시작 점검", "Start check")}>
      <h4>{tr("시작 점검", "Start check")}</h4>
      <p>{tr("실행 준비", "Execution readiness")}: {view.execution_readiness === "unavailable" ? tr("미제공", "unavailable") : view.execution_readiness}</p>
      {profile && <>
        <ul>{profile.models.map((model) => <li key={model.role}>{model.role === "implementation" ? tr("구현", "Implementation") : tr("리뷰", "Review")}: {model.selection.state === "pinned" ? `${model.selection.model} · ${model.selection.effort}` : model.selection.reason}</li>)}</ul>
        <p>{tr(`동시 작업 ${profile.limits.concurrent_tasks} · 품질 수리 ${profile.limits.quality_repairs} · 설계 리뷰 ${profile.limits.design_review_rounds}`, `Concurrent tasks ${profile.limits.concurrent_tasks} · quality repairs ${profile.limits.quality_repairs} · design reviews ${profile.limits.design_review_rounds}`)}</p>
        <p className="campaign-muted">{profile.unavailable_reason}</p>
      </>}
    </section>
    <label className="campaign-server-feedback grid min-w-0 gap-3 [&_button]:justify-self-start">{tr("피드백용 복사", "Copy for feedback")}
      <textarea aria-label={tr("피드백용 복사", "Copy for feedback")} readOnly rows={5} value={feedback} />
      <button type="button" onClick={() => void globalThis.navigator?.clipboard?.writeText(feedback).catch(() => undefined)}>{tr("복사", "Copy")}</button>
    </label>
  </div>;
}

/** A server task in TaskPhone order: where it stands now, its acceptance and links, then attempts and sessions. */
export function ServerTaskDetails({ document, nodeId, gate, connection, busy, onCommand, tr }: {
  document: ServerCampaignDocument; nodeId: string; gate: PendingGate | null; connection: Connection; busy: boolean;
  onCommand: (command: ControlCommand, nodeId?: string) => void; tr: Tr;
}) {
  const { definition, view } = document;
  const [approving, setApproving] = useState(false);
  const phone = useIsMobile();
  const node = definition.nodes.find((value) => value.id === nodeId);
  const nodeView = view.nodes.find((value) => value.id === nodeId);
  const runtime = nodeView?.runtime;
  if (!node) return null;
  const gateName = gate && tr(...GATE_LABELS[gate.gate]);
  // The sheet only shows what the server gave; approving waits for the approval API (#8249).
  const sheet = gate && <div className="grid gap-3 text-sm text-[var(--fg)]">
    <p>{tr(`${node.title}의 ${gateName} 승인을 기다려요.`, `${node.title} is waiting for ${gateName} approval.`)}</p>
    <ul className="grid gap-1 text-xs">
      {runtime && <li>{tr("단계", "Stage")}: {tr(...STAGE_LABELS[runtime.stage])} · {tr(`리뷰 ${runtime.review_rounds}회 · 수리 ${runtime.repair_cycles}회`, `${runtime.review_rounds} reviews · ${runtime.repair_cycles} repairs`)}</li>}
      <li>{tr("수용조건", "Acceptance")}: {node.acceptance.length}</li>
      {gate.opened_at && <li>{tr("요청 시각", "Requested")}: {time(gate.opened_at)}</li>}
    </ul>
    <div className="grid grid-cols-2 gap-2">
      <Button variant="secondary" onClick={() => setApproving(false)}>{tr("나중에", "Later")}</Button>
      <Button variant="primary" disabled>{tr("승인", "Approve")}</Button>
    </div>
    <p className="text-xs text-[var(--fg-muted)]">{tr("승인 API 준비 중(#8249)", "Approval API in progress (#8249)")}</p>
  </div>;
  return <>
    <div className="campaign-row"><h4>{node.title}</h4>{runtime && <Badge status={runtime.disposition} label={tr(...DISPOSITION_LABELS[runtime.disposition])} tr={tr} />}</div>
    {runtime && <p>{tr(...STAGE_LABELS[runtime.stage])} · {tr(`계열 ${runtime.series}`, `series ${runtime.series}`)}{runtime.wait_reason && ` · ${tr(...REASON_LABELS[runtime.wait_reason])}`}</p>}
    {gate && <div className="grid gap-2 rounded-lg border border-[color-mix(in_srgb,var(--warn)_45%,var(--line))] bg-[color-mix(in_srgb,var(--warn)_8%,transparent)] p-3">
      <strong className="text-[var(--warn)]">{tr(`${gateName} 승인을 기다려요`, `Waiting for ${gateName} approval`)}</strong>
      <div><Button variant="secondary" size="sm" onClick={() => setApproving(true)}>{tr("승인 검토", "Review approval")}</Button></div>
    </div>}
    {gate && approving && (phone
      ? <BottomSheet open onClose={() => setApproving(false)} title={tr(`${gateName} 승인`, `${gateName} approval`)}>{sheet}</BottomSheet>
      : <Modal open onClose={() => setApproving(false)} title={tr(`${gateName} 승인`, `${gateName} approval`)} size="sm">{sheet}</Modal>)}
    <ul aria-label={tr("수용조건", "Acceptance")}>{node.acceptance.map((ac) => <li key={ac.id}><code>{ac.id}</code> {ac.description} · {tr(...(VERIFICATION_LABELS[ac.verification] ?? [ac.verification, ac.verification]))}</li>)}</ul>
    {safeCampaignLink(node.issue_url) && <p><a href={safeCampaignLink(node.issue_url)} target="_blank" rel="noreferrer">{node.issue_url}</a></p>}
    {runtime && <dl className="campaign-detail-grid grid grid-cols-1 gap-4 text-sm sm:grid-cols-2 [&_dt]:mb-1 [&_dt]:text-xs [&_dd]:[overflow-wrap:anywhere]">
      <div><dt>{tr("현재 시도", "Current attempt")}</dt><dd>{runtime.current_attempt ?? "—"}</dd></div>
      <div><dt>{tr("품질 실패 · 수리 · 리뷰", "Failures · repairs · reviews")}</dt><dd>{runtime.quality_failures} · {runtime.repair_cycles} · {runtime.review_rounds}</dd></div>
      <div><dt>{tr("main 반영 · 최종 수용", "In main · accepted")}</dt><dd>{runtime.main_integrated ? "✓" : "—"} · {runtime.acceptance_complete ? "✓" : "—"}</dd></div>
    </dl>}
    {runtime && <>
      <h4>{tr("세션", "Sessions")}</h4>
      <ul>{view.sessions.filter((session) => session.attempt_id === runtime.current_attempt).map((session) => <li key={`${session.identity.session}:${session.identity.incarnation}`}>
        {session.identity.gate.host} · {session.identity.session} · {session.observation.state}
      </li>)}</ul>
      <p>{tr("직접 후속", "Direct successors")}: {nodeView?.direct_successors.join(", ") || tr("없음", "none")}</p>
      <p>{tr("이 작업이 main에 반영되면 준비될 후속", "Ready once this reaches main")}: {nodeView?.ready_after_main.join(", ") || tr("없음", "none")}</p>
      {runtime.disposition === "failed" && <CommandButton view={view} command="prepare_retry" connection={connection} busy={busy} onCommand={() => onCommand("prepare_retry", node.id)} tr={tr} />}
    </>}
  </>;
}
