import { ChevronDown, ExternalLink, GitPullRequest, Layers, LockKeyhole, ShieldCheck } from "lucide-react";
import { useMemo, useRef, useState } from "react";
import type { Campaign } from "../../api/campaigns";
import { request } from "../../api/httpClient";
import { serverCampaignRejection } from "../../api/serverCampaigns";
import { ApprovalCard, type ApprovalDetail } from "../../kit/pro/approval-card";
import { Button } from "../../kit/free/button-clean";
import { TextField } from "../../kit/free/text-field-clean";
import { Cap, Chip, Num, StageTrack, Well, type TrackSegment } from "../common/KitPrimitives";
import { safeCampaignLink } from "./campaignModel";
import { GATE_LABELS, taskCounts, type OverviewTask, type PendingGate } from "./campaignOverviewModel";
import { formatCampaignTime, type Tr } from "./campaignPresentation";
import { newIdempotencyKey, rejectionText, type Connection } from "./serverCampaignModel";

export function waitedGateText(since: string | null | undefined, tr: Tr): string | null {
  if (!since || Number.isNaN(Date.parse(since))) return null;
  const minutes = Math.max(0, Math.floor((Date.now() - Date.parse(since)) / 60_000));
  return minutes < 60 ? tr(`${minutes}분째 기다려요`, `Waiting ${minutes} min`) : tr(`${Math.floor(minutes / 60)}시간째 기다려요`, `Waiting ${Math.floor(minutes / 60)} h`);
}

function validForumEvidence(decisionRef: string, responseRef: string, responseAt: string): boolean {
  if (!decisionRef.trim() || Number.isNaN(Date.parse(responseAt.trim()))) return false;
  try {
    const url = new URL(responseRef.trim());
    return url.protocol === "https:" && Boolean(url.hostname) && !url.username && !url.password;
  } catch { return false; }
}

/** Standard kit `ApprovalCard` for server campaign gates (`landing`, `review`, etc.) and ledger human-approval nodes. */
export function CampaignGateApprovalCard({
  taskId, taskTitle, taskNumber, gate, stageText, prUrl, campaignId = null, connection = "live", onOpenTask, onApproved, tr,
}: {
  taskId: string; taskTitle: string; taskNumber?: string | null; gate: PendingGate; stageText?: string | null;
  prUrl?: string | null; campaignId?: string | null; connection?: Connection; onOpenTask?: (id: string) => void; onApproved?: () => void; tr: Tr;
}) {
  const [formOpen, setFormOpen] = useState(false);
  const [decisionRef, setDecisionRef] = useState("");
  const [responseRef, setResponseRef] = useState("");
  const [responseAt, setResponseAt] = useState("");
  const sentRef = useRef<{ payload: string; key: string } | null>(null);

  const gateWord = tr(...GATE_LABELS[gate.gate]);
  const waitLabel = waitedGateText(gate.openedAt, tr);
  const binding = gate.binding ?? null;
  const canSubmitForum = Boolean(campaignId && binding && connection !== "stale");
  const readyToApprove = canSubmitForum && validForumEvidence(decisionRef, responseRef, responseAt);

  const details: ApprovalDetail[] = [];
  if (stageText) details.push({ label: tr("단계", "Stage"), value: stageText });
  if (gate.reviewRounds !== undefined || gate.repairCycles !== undefined) {
    details.push({ label: tr("리뷰 · 수리", "Reviews · repairs"), value: tr(`${gate.reviewRounds ?? 0}회 · ${gate.repairCycles ?? 0}회`, `${gate.reviewRounds ?? 0} · ${gate.repairCycles ?? 0}`) });
  }
  if (gate.evidenceNote) details.push({ label: tr("검증 · CI", "Checks · CI"), value: gate.evidenceNote });
  if (binding?.target) {
    const target = binding.target;
    const targetValue = target.kind === "candidate" ? `${target.base_sha.slice(0, 7)} → ${target.head_sha.slice(0, 7)}`
      : target.kind === "pull_request" ? `PR #${target.number} (${target.head_sha.slice(0, 7)})` : `main ${target.sha.slice(0, 7)}`;
    details.push({ label: tr("대상", "Target"), value: <code className="font-mono text-xs">{targetValue}</code> });
  } else if (prUrl && safeCampaignLink(prUrl)) {
    const link = safeCampaignLink(prUrl)!;
    const prNo = new URL(link).pathname.match(/\/pull\/(\d+)/)?.[1];
    details.push({ label: "PR", value: <a href={link} target="_blank" rel="noreferrer" className="inline-flex items-center gap-1 underline min-[900px]:!min-h-5 min-[900px]:!py-0">{prNo ? `#${prNo}` : tr("PR 열기", "Open PR")}<ExternalLink aria-hidden className="size-3" /></a> });
  }
  if (gate.openedAt) details.push({ label: tr("요청 시각", "Requested"), value: formatCampaignTime(gate.openedAt, tr) });
  if (gate.evidenceUrl && safeCampaignLink(gate.evidenceUrl)) {
    details.push({ label: tr("승인 요청", "Decision post"), value: <a href={safeCampaignLink(gate.evidenceUrl)} target="_blank" rel="noreferrer" className="inline-flex items-center gap-1 underline min-[900px]:!min-h-5 min-[900px]:!py-0">{tr("결정 포럼 열기", "Open decision forum")}<ExternalLink aria-hidden className="size-3" /></a> });
  }

  const disabledReason = !canSubmitForum
    ? connection === "stale" ? tr("서버 연결이 끊겨 지금은 승인할 수 없어요", "Disconnected from server; approval unavailable") : tr("승인 API 준비 중(#8249) · 결정 포럼에서 승인해요", "Approval API in progress (#8249)")
    : !readyToApprove ? tr("포럼 승인 근거를 입력하면 승인돼요", "Enter forum evidence to approve") : undefined;

  const submitApproval = async () => {
    if (!campaignId || !binding || !readyToApprove) return;
    const command = {
      command: "approve" as const, expected_definition_revision: binding.definition_revision, expected_run: binding.run_id,
      node_id: binding.node_id, gate: binding.gate, target: binding.target, policy_hash: binding.policy_hash,
      evidence: { decision_ref: decisionRef.trim(), response_ref: responseRef.trim(), response_at: new Date(Date.parse(responseAt.trim())).toISOString() },
    };
    const payload = JSON.stringify(command);
    const key = sentRef.current?.payload === payload ? sentRef.current.key : newIdempotencyKey();
    sentRef.current = { payload, key };
    try {
      await request(`/api/server-campaigns/${encodeURIComponent(campaignId)}/commands`, { method: "POST", body: JSON.stringify({ idempotency_key: key, command }), suppressErrorToast: true });
      onApproved?.();
    } catch (cause) {
      const refusal = serverCampaignRejection(cause);
      if (refusal && refusal.status < 500) sentRef.current = null;
      throw new Error(!refusal ? tr("응답을 받지 못했어요. 같은 내용으로 다시 시도해도 두 번 기록되지 않아요.", "No answer arrived. Retrying reuses the same key.") : refusal.rejection ? rejectionText(refusal.rejection, tr) : refusal.message);
    }
  };

  const preview = (onOpenTask || canSubmitForum) ? <div className="grid min-w-0 gap-2">
    <div className="flex flex-wrap items-center gap-2">
      {onOpenTask && <Button variant="secondary" size="sm" data-task-id={taskId} className="max-[899px]:!min-h-11" onClick={() => onOpenTask(taskId)}>{taskNumber ? `${taskNumber} · ` : ""}{tr("작업 상세 보기", "View task details")}</Button>}
      {canSubmitForum && <Button variant="ghost" size="sm" aria-expanded={formOpen} className="max-[899px]:!min-h-11" onClick={() => setFormOpen((v) => !v)}><ChevronDown aria-hidden className={`size-3.5 transition-transform ${formOpen ? "" : "-rotate-90"}`} />{tr("포럼 승인 근거 입력", "Enter forum evidence")}</Button>}
    </div>
    {canSubmitForum && formOpen && <div className="grid gap-2 pt-1 sm:grid-cols-3">
      <TextField size="sm" label={tr("결정 ID (decision_ref)", "Decision ID (decision_ref)")} placeholder="i7438-landing-1" value={decisionRef} onChange={(e) => setDecisionRef(e.target.value)} />
      <TextField size="sm" type="url" label={tr("응답 링크 (HTTPS)", "Response permalink (HTTPS)")} placeholder="https://discord.com/channels/..." value={responseRef} onChange={(e) => setResponseRef(e.target.value)} />
      <TextField size="sm" label={tr("응답 시각 (RFC 3339)", "Response time (RFC 3339)")} placeholder="2026-11-24T04:00:00Z" value={responseAt} onChange={(e) => setResponseAt(e.target.value)} />
    </div>}
  </div> : undefined;

  return <div className="campaign-gate min-w-0 [&_.aui-approval-card-button]:max-[899px]:!min-h-11">
    <ApprovalCard
      title={`${tr(`${gateWord} 승인 대기`, `Waiting for ${GATE_LABELS[gate.gate][1].toLowerCase()} approval`)} · ${taskTitle}`}
      context={[taskNumber, waitLabel].filter(Boolean).join(" · ") || undefined}
      description={gate.gate === "landing" ? tr("착지 게이트가 열려 있어요. 리뷰·검증 결과를 확인한 뒤 승인을 기록하세요.", "The landing gate is open. Verify review and check results before approving.") : undefined}
      risk={gate.gate === "landing" || gate.gate === "merge" || gate.gate === "deploy" ? "medium" : "low"}
      icon={gate.gate === "landing" ? <ShieldCheck /> : <LockKeyhole />}
      details={details} preview={preview} allowAlways={false} shortcuts={false}
      approveLabel={tr("승인", "Approve")} approveDisabledReason={disabledReason}
      onApprove={canSubmitForum ? () => submitApproval() : undefined}
    />
  </div>;
}

export interface BatchTrackerSummary {
  total: number; done: number; running: number; blocked: number; failed: number; pending: number; skipped: number;
  percent: number; piecesDone: number; piecesTotal: number; pieceTaskCount: number;
  landReadyIds: string[]; openPrCount: number; holdCount: number; batches: Array<{ code: string; taskId: string }>;
}

const PIECE_RE = /조각\s*[:：]?\s*(?:완료\s*)?(\d+)\s*\/\s*(\d+)|pieces?\s*[:：]?\s*(?:done\s*)?(\d+)\s*\/\s*(\d+)/gi;
const BATCH_CODE_RE = /\b(\d{4}[a-z])\b/g;
const LAND_READY_RE = /\bLAND_READY\b|착지\s*대기|착지\s*승인\s*대기|리뷰·검증\s*통과/i;

/** Parses structured batch/piece landing progress from ledger campaign nodes & description. */
export function buildBatchTracker(campaign: Campaign, tasks: Array<OverviewTask & { hold?: string | null }>): BatchTrackerSummary {
  const counts = taskCounts(tasks);
  const pending = tasks.filter((t) => t.status === "pending").length;
  const skipped = tasks.filter((t) => t.status === "skipped").length;
  const total = tasks.length;
  const percent = total > 0 ? Math.round((counts.done / total) * 100) : 0;
  let piecesDone = 0, piecesTotal = 0, pieceTaskCount = 0;
  const landReadyIds: string[] = [];
  const batchMap = new Map<string, string>();
  const scanPieces = (text: string): boolean => {
    let matched = false;
    for (const match of text.matchAll(PIECE_RE)) {
      const done = Number(match[1] ?? match[3]), all = Number(match[2] ?? match[4]);
      if (all > 0 && done <= all) { piecesDone += done; piecesTotal += all; matched = true; }
    }
    return matched;
  };
  scanPieces(campaign.description || "");
  for (const node of campaign.nodes) {
    const blob = [node.summary, node.stage, node.next_action].filter(Boolean).join(" \n ");
    if (scanPieces(blob)) pieceTaskCount += 1;
    const open = node.status !== "completed" && node.status !== "skipped";
    if (open && LAND_READY_RE.test(blob)) landReadyIds.push(node.id);
    if (open) for (const m of blob.matchAll(BATCH_CODE_RE)) if (!batchMap.has(m[1])) batchMap.set(m[1], node.id);
  }
  const openPrCount = tasks.filter((t) => t.status !== "completed" && t.status !== "skipped" && Boolean(t.prUrl)).length;
  const holdCount = tasks.filter((t) => Boolean(t.hold)).length;
  const batches = [...batchMap.entries()].sort((a, b) => b[0].localeCompare(a[0])).slice(0, 5).map(([code, taskId]) => ({ code, taskId }));
  return { total, done: counts.done, running: counts.running, blocked: counts.blocked, failed: counts.failed, pending, skipped, percent, piecesDone, piecesTotal, pieceTaskCount, landReadyIds, openPrCount, holdCount, batches };
}

/** Top header tracker bar for ledger campaigns: ratio bar + piece counts + LAND_READY / batch tags. */
export function CampaignBatchTracker({ campaign, tasks, onOpenTask, tr }: {
  campaign: Campaign; tasks: Array<OverviewTask & { hold?: string | null }>; onOpenTask: (id: string) => void; tr: Tr;
}) {
  const summary = useMemo(() => buildBatchTracker(campaign, tasks), [campaign, tasks]);
  const segments = useMemo((): TrackSegment[] => {
    if (summary.total === 0) return [];
    const BUCKETS = 20, out: TrackSegment[] = [];
    const push = (count: number, tone: TrackSegment) => {
      const slots = count > 0 ? Math.max(1, Math.round((count / summary.total) * BUCKETS)) : 0;
      for (let i = 0; i < slots && out.length < BUCKETS; i += 1) out.push(tone);
    };
    push(summary.done + summary.skipped, "done"); push(summary.running, "now"); push(summary.blocked, "wait"); push(summary.failed, "fail");
    while (out.length < BUCKETS) out.push("todo");
    return out;
  }, [summary]);
  if (summary.total === 0) return null;

  return <Well as="section" data-testid="campaign-batch-tracker" aria-label={tr("배치 착지 진행도", "Batch landing tracker")} className="grid min-w-0 gap-2 px-3.5 py-2.5">
    <div className="flex min-w-0 flex-wrap items-center justify-between gap-x-4 gap-y-1.5 text-xs">
      <span className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1">
        <strong className="inline-flex items-center gap-1.5 font-semibold text-[var(--fg)]"><Layers aria-hidden className="size-3.5 text-[var(--accent)]" /><Num>{tr(`완료 ${summary.done}/${summary.total} (${summary.percent}%)`, `${summary.done}/${summary.total} done (${summary.percent}%)`)}</Num></strong>
        <Cap><Num>{tr(`진행 ${summary.running}`, `${summary.running} running`)}</Num></Cap>
        <Cap><Num>{tr(`대기 ${summary.pending}`, `${summary.pending} waiting`)}</Num></Cap>
        {summary.skipped > 0 && <Cap><Num>{tr(`건너뜀 ${summary.skipped}`, `${summary.skipped} skipped`)}</Num></Cap>}
        {(summary.blocked > 0 || summary.failed > 0) && <span className="font-medium text-[var(--warn)]"><Num>{[summary.blocked > 0 && tr(`막힘 ${summary.blocked}`, `${summary.blocked} blocked`), summary.failed > 0 && tr(`실패 ${summary.failed}`, `${summary.failed} failed`)].filter(Boolean).join(" · ")}</Num></span>}
      </span>
      <span className="flex min-w-0 flex-wrap items-center gap-1.5">
        {summary.piecesTotal > 0 && <Chip tone="info" title={tr(`진행 중 작업 ${summary.pieceTaskCount}건의 조각 합계`, `Pieces summed across ${summary.pieceTaskCount} tasks`)}><Num>{tr(`조각 ${summary.piecesDone}/${summary.piecesTotal}`, `Pieces ${summary.piecesDone}/${summary.piecesTotal}`)}</Num></Chip>}
        {summary.landReadyIds.length > 0 && <button type="button" onClick={() => onOpenTask(summary.landReadyIds[0])} style={{ padding: "2px 9px", borderRadius: 999 }} className="!m-0 inline-flex cursor-pointer items-center gap-1 !border !border-[color-mix(in_srgb,var(--warn)_45%,var(--line))] !bg-[var(--warn-soft)] !text-xs font-medium !text-[var(--warn)] max-[899px]:!min-h-11 min-[900px]:!min-h-6 min-[900px]:!min-w-0"><ShieldCheck aria-hidden className="size-3" /><Num>{tr(`착지 후보 ${summary.landReadyIds.length}`, `${summary.landReadyIds.length} land-ready`)}</Num></button>}
        {summary.openPrCount > 0 && <Chip tone="neutral"><GitPullRequest aria-hidden className="size-3" /><Num>{tr(`열린 PR ${summary.openPrCount}`, `${summary.openPrCount} open PRs`)}</Num></Chip>}
        {summary.holdCount > 0 && <Chip tone="warn"><Num>{tr(`머지 후 보류 ${summary.holdCount}`, `${summary.holdCount} post-merge hold`)}</Num></Chip>}
        {summary.batches.map(({ code, taskId }) => <button key={code} type="button" onClick={() => onOpenTask(taskId)} title={tr(`배치 ${code} 작업 열기`, `Open batch ${code} task`)} style={{ padding: "2px 8px", borderRadius: 999 }} className="!m-0 inline-flex cursor-pointer items-center !border !border-[var(--line)] !bg-[var(--bg-2)] font-mono !text-[11px] !text-[var(--fg-dim)] hover:!border-[var(--accent)] hover:!text-[var(--fg)] max-[899px]:!min-h-11 min-[900px]:!min-h-6 min-[900px]:!min-w-0">{code}</button>)}
      </span>
    </div>
    <StageTrack segments={segments} />
  </Well>;
}
