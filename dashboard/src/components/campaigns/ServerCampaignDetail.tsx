import { useState } from "react";
import { SERVER_CAMPAIGN_STAGES, serverCampaignProfile, type ServerCampaignDocument } from "../../api/serverCampaigns";
import { Button } from "../../kit/free/button-clean";
import { TextArea } from "../../kit/free/text-area-clean";
import { Cap, Chip, Desc, Eyebrow, InspSection, KeyValue, KitCard, Row, RuleItem, StatusDot, Well } from "../common/KitPrimitives";
import { BottomSheet } from "../common/overlay/BottomSheet";
import { Modal } from "../common/overlay/Modal";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import { safeCampaignLink } from "./campaignModel";
import { GATE_LABELS, type PendingGate } from "./campaignOverviewModel";
import { Badge, formatCampaignTime, type Tr } from "./campaignPresentation";
import { CommandButton, type ControlCommand } from "./ServerCampaignControls";
import { DISPOSITION_LABELS, REASON_LABELS, STAGE_LABELS, feedbackText, nodeReasons, unknownObservations, type Connection } from "./serverCampaignModel";
import { StatusCard, StepList, useScreenTitle, type Step } from "./TaskScreen";

const VERIFICATION_LABELS: Record<string, [string, string]> = {
  test: ["시험", "Tests"], main: ["main 반영", "In main"], deployment: ["배포 확인", "Deployment"], operational: ["운영 확인", "Operational"],
};

/** Server-only parts under the overview: why the server holds, sessions it cannot see, the start check and the feedback copy. */
export default function ServerCampaignSections({ document, link, tr }: { document: ServerCampaignDocument; link: string; tr: Tr }) {
  const { view } = document;
  const profile = serverCampaignProfile(view);
  const unknown = unknownObservations(view);
  const feedback = feedbackText(link, view, tr);
  return <div className="campaign-server-detail grid min-w-0 gap-3">
    <Cap className="campaign-freshness font-display tabular-nums">
      {tr("서버 관측", "Server as of")} {formatCampaignTime(view.as_of, tr)} · {tr("정의", "definition")} r{view.definition_revision} · {tr("이벤트", "event")} #{view.last_event_sequence}{view.run_id && ` · run ${view.run_id}`}
    </Cap>
    {unknown.length > 0 && <ul className="campaign-filter-note" aria-label={tr("확인 불가 관측", "Unknown observations")}>
      {unknown.map((entry) => <li key={`${entry.host}:${entry.session}`}>{entry.host} · {entry.session} · {entry.reason} · {tr("마지막 확인", "last observed")} {formatCampaignTime(entry.lastObservedAt, tr)}</li>)}
    </ul>}
    {view.reasons.length > 0 && <ul className="campaign-server-reasons" aria-label={tr("서버 판정 이유", "Server reasons")}>
      {view.reasons.map((reason, index) => <li key={index}>{tr(...REASON_LABELS[reason.code])} · {reason.target}</li>)}
    </ul>}
    <KitCard as="section" pad="none" aria-label={tr("시작 점검", "Start check")}>
      <InspSection className="campaign-server-check border-t-0">
        <Eyebrow>{tr("시작 점검", "Start check")}</Eyebrow>
        <KeyValue label={tr("실행 준비", "Execution readiness")}>{view.execution_readiness === "unavailable" ? tr("미제공", "unavailable") : view.execution_readiness}</KeyValue>
        {profile && <>
          {profile.models.map((model) => <KeyValue key={model.role} label={model.role === "implementation" ? tr("구현", "Implementation") : tr("리뷰", "Review")}>
            {model.selection.state === "pinned" ? `${model.selection.model} · ${model.selection.effort}` : model.selection.reason}
          </KeyValue>)}
          <KeyValue num label={tr("한도", "Limits")}>
            {tr(`동시 작업 ${profile.limits.concurrent_tasks} · 품질 수리 ${profile.limits.quality_repairs} · 설계 리뷰 ${profile.limits.design_review_rounds}`, `Concurrent tasks ${profile.limits.concurrent_tasks} · quality repairs ${profile.limits.quality_repairs} · design reviews ${profile.limits.design_review_rounds}`)}
          </KeyValue>
          {profile.unavailable_reason && <Desc>{profile.unavailable_reason}</Desc>}
        </>}
      </InspSection>
    </KitCard>
    <div className="campaign-server-feedback grid min-w-0 gap-2 justify-items-start">
      <TextArea label={tr("피드백용 복사", "Copy for feedback")} aria-label={tr("피드백용 복사", "Copy for feedback")} readOnly minRows={5} maxRows={8} value={feedback} className="w-full" />
      <Button variant="secondary" size="sm" onClick={() => void globalThis.navigator?.clipboard?.writeText(feedback).catch(() => undefined)}>{tr("복사", "Copy")}</Button>
    </div>
  </div>;
}

type Runtime = ServerCampaignDocument["view"]["nodes"][number]["runtime"];

/** 단계 rows from the server's stage and counts: steps before the current one are done. Review counts sit on the review
 * row once the task got there, else on the current row; an open gate says how long it has waited. */
function serverSteps(runtime: Runtime, gate: PendingGate | null, tr: Tr): Step[] {
  const at = SERVER_CAMPAIGN_STAGES.indexOf(runtime.stage);
  const countsOn = at >= SERVER_CAMPAIGN_STAGES.indexOf("review") ? "review" : runtime.stage;
  const minutes = gate ? Math.max(0, Math.floor((Date.now() - Date.parse(gate.openedAt)) / 60_000)) : 0;
  const waited = minutes < 60 ? tr(`승인 대기 ${minutes}분`, `Awaiting approval ${minutes} min`) : tr(`승인 대기 ${Math.floor(minutes / 60)}시간`, `Awaiting approval ${Math.floor(minutes / 60)} h`);
  return SERVER_CAMPAIGN_STAGES.map((stage, index) => {
    const state = index < at || (index === at && runtime.disposition === "complete") ? "done" : index === at ? "now" : "next";
    const value = [
      stage === countsOn && (runtime.review_rounds || runtime.repair_cycles) ? tr(`리뷰 ${runtime.review_rounds}회 · 수리 ${runtime.repair_cycles}회`, `${runtime.review_rounds} reviews · ${runtime.repair_cycles} repairs`) : null,
      stage === "main_verification" && runtime.main_integrated ? tr("main 반영", "In main") : null,
      stage === "final_verification" && runtime.acceptance_complete ? tr("수용 완료", "Accepted") : null,
      state === "now" && runtime.quality_failures ? tr(`품질 실패 ${runtime.quality_failures}회`, `${runtime.quality_failures} quality failures`) : null,
      state === "now" ? gate ? waited : tr(...DISPOSITION_LABELS[runtime.disposition]) : null,
    ].filter(Boolean).join(" · ");
    const tone = state !== "now" ? undefined : gate || runtime.disposition === "blocked" ? "warn" : runtime.disposition === "failed" ? "err" : undefined;
    return { key: stage, label: tr(...STAGE_LABELS[stage]), state, value, tone };
  });
}

/** A server task in TaskPhone order: a status card, the stages, its acceptance and link, then sessions and successors. */
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
  const issue = node && safeCampaignLink(node.issue_url) ? new URL(node.issue_url).pathname.match(/\/issues\/(\d+)\/?$/)?.[1] : undefined;
  const inScreen = useScreenTitle(node?.title ?? nodeId, issue ? `#${issue}` : null);
  if (!node) return null;
  const gateName = gate && tr(...GATE_LABELS[gate.gate]);
  const reasons = nodeView ? nodeReasons(nodeView, view.reasons).map((code) => tr(...REASON_LABELS[code])) : [];
  const stuck = runtime?.disposition === "blocked" || runtime?.disposition === "failed";
  // The sheet only shows what the server gave; approving waits for the approval API (#8249).
  const facts: Array<[string, string]> = gate ? [
    ...runtime ? [[tr("단계", "Stage"), tr(...STAGE_LABELS[runtime.stage])], [tr("리뷰 · 수리", "Reviews · repairs"), tr(`${runtime.review_rounds}회 · ${runtime.repair_cycles}회`, `${runtime.review_rounds} · ${runtime.repair_cycles}`)]] as Array<[string, string]> : [],
    [tr("수용조건", "Acceptance"), String(node.acceptance.length)], [tr("요청 시각", "Requested"), formatCampaignTime(gate.openedAt, tr)],
  ] : [];
  const sheet = gate && <div className="grid gap-3 text-sm text-[var(--fg)]">
    <Desc>{tr(`${node.title}의 ${gateName} 승인을 기다려요.`, `${node.title} is waiting for ${gateName} approval.`)}</Desc>
    <Well className="overflow-hidden">
      <ul className="m-0 list-none p-0">
        {facts.map(([label, value]) => <Row as="li" key={label} density="sm" className="min-h-11">
          <StatusDot tone="ok" />
          <span className="min-w-0 flex-1">{label}</span>
          <Cap className="shrink-0 font-display tabular-nums">{value}</Cap>
        </Row>)}
      </ul>
    </Well>
    <div className="grid grid-cols-[1fr_2fr] gap-2">
      <Button variant="secondary" className="!min-h-12 w-full" onClick={() => setApproving(false)}>{tr("나중에", "Later")}</Button>
      <Button variant="primary" className="!min-h-12 w-full" disabled>{tr("승인", "Approve")}</Button>
    </div>
    <Cap>{tr("승인 API 준비 중(#8249)", "Approval API in progress (#8249)")}</Cap>
  </div>;
  return <>
    {!inScreen && <div className="campaign-row"><h4>{node.title}</h4></div>}
    <StatusCard label={tr("지금 상태", "Now")} warn={!!gate}>
      {gate ? <strong className="text-sm text-[var(--warn)]">{tr(`${gateName} 승인을 기다려요`, `Waiting for ${gateName} approval`)}</strong>
        : runtime && <Badge status={runtime.disposition} label={tr(...DISPOSITION_LABELS[runtime.disposition])} tr={tr} />}
      <p className="text-[var(--fg)]">{runtime ? `${tr(...STAGE_LABELS[runtime.stage])} · ${tr(`계열 ${runtime.series}`, `series ${runtime.series}`)}` : tr("서버 상태 없음", "No server state")}</p>
      {reasons.length > 0 && <p className={stuck ? "text-[var(--warn)]" : ""}>{stuck ? tr("막힘", "Blocked") : tr("대기 사유", "Waiting on")}: {reasons.join(" · ")}</p>}
      {gate && <Button variant="secondary" size="sm" onClick={() => setApproving(true)}>{tr("승인 검토", "Review approval")}</Button>}
      {runtime?.disposition === "failed" && <CommandButton view={view} command="prepare_retry" connection={connection} busy={busy} onCommand={() => onCommand("prepare_retry", node.id)} tr={tr} />}
    </StatusCard>
    {gate && approving && (phone
      ? <BottomSheet open onClose={() => setApproving(false)} title={tr(`${gateName} 승인`, `${gateName} approval`)}>{sheet}</BottomSheet>
      : <Modal open onClose={() => setApproving(false)} title={tr(`${gateName} 승인`, `${gateName} approval`)} size="sm">{sheet}</Modal>)}
    {runtime && <StepList steps={serverSteps(runtime, gate, tr)} tr={tr} />}
    <InspSection className="px-0 sm:px-0">
      <Eyebrow>{tr("수용조건", "Acceptance")}</Eyebrow>
      <ul aria-label={tr("수용조건", "Acceptance")} className="!m-0 grid gap-1.5 !list-none !p-0">
        {node.acceptance.map((ac) => <li key={ac.id} className="flex min-w-0 items-start gap-2 text-[13px] leading-[18px]">
          <Chip tone={ac.verification === "operational" ? "accent" : "neutral"} className="h-5 px-1.5 text-[11px]">{tr(...(VERIFICATION_LABELS[ac.verification] ?? [ac.verification, ac.verification]))}</Chip>
          <span className="min-w-0 flex-1"><code>{ac.id}</code> {ac.description}</span>
        </li>)}
      </ul>
    </InspSection>
    {safeCampaignLink(node.issue_url) && <p><a href={safeCampaignLink(node.issue_url)} target="_blank" rel="noreferrer">{node.issue_url}</a></p>}
    {runtime && <InspSection className="px-0 sm:px-0">
      <Eyebrow>{tr("세션", "Sessions")}{runtime.current_attempt && ` · ${tr("현재 시도", "current attempt")} ${runtime.current_attempt}`}</Eyebrow>
      <ul className="!m-0 grid gap-1 !list-none !p-0">{view.sessions.filter((session) => session.attempt_id === runtime.current_attempt).map((session) => <li key={`${session.identity.session}:${session.identity.incarnation}`}>
        <RuleItem tone={session.observation.state === "alive" ? "ok" : session.observation.state === "unknown" ? "warn" : "neutral"}>
          {session.identity.gate.host} · {session.identity.session} · {session.observation.state}
        </RuleItem>
      </li>)}</ul>
      <KeyValue label={tr("직접 후속", "Direct successors")}>{nodeView?.direct_successors.join(", ") || tr("없음", "none")}</KeyValue>
      <KeyValue label={tr("이 작업이 main에 반영되면 준비될 후속", "Ready once this reaches main")}>{nodeView?.ready_after_main.join(", ") || tr("없음", "none")}</KeyValue>
    </InspSection>}
  </>;
}
