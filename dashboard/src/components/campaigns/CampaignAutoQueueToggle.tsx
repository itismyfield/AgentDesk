import { useState } from "react";
import { getCampaign, isRejectedSave, setCampaignAutoQueue, type Campaign, type CampaignHandoff } from "../../api/campaigns";
import type { Tr } from "./campaignPresentation";

const WAITING_REASONS: Record<string, [string, string]> = {
  no_issue_card: ["이슈 카드 없음", "no issue card"],
  no_assigned_agent: ["담당 에이전트 없음", "no assigned agent"],
  previous_attempt_stopped: ["이전 실행이 멈춤", "previous run stopped"],
  card_not_ready: ["카드가 다른 단계에 있음", "card is in another step"],
  not_enqueueable: ["준비 상태로 옮길 수 없음", "cannot move to ready"],
  run_paused: ["에이전트 자동큐 일시정지", "agent queue paused"],
  already_in_run: ["이미 그 실행에 있었음", "already in that run"],
};

export function handoffSummary(handoff: CampaignHandoff, tr: Tr): string {
  const counts = new Map<string, number>();
  for (const item of handoff.waiting) counts.set(item.reason, (counts.get(item.reason) ?? 0) + 1);
  const waiting = Array.from(counts, ([reason, count]) => {
    const [ko, en] = WAITING_REASONS[reason] ?? [reason, reason];
    return `${tr(ko, en)} ${count}`;
  }).join(", ");
  const queued = tr(`작업 ${handoff.queued.length}개를 자동큐에 넘겼습니다.`, `Sent ${handoff.queued.length} tasks to auto-queue.`);
  return waiting ? `${queued} ${tr("못 넘긴 작업", "Held back")}: ${waiting}` : queued;
}

export default function CampaignAutoQueueToggle({ campaign, tr, onSaved }: { campaign: Campaign; tr: Tr; onSaved: (updated: Campaign) => void }) {
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const toggle = async () => {
    const enabled = !campaign.auto_queue;
    setBusy(true); setMessage(null);
    try {
      const result = await setCampaignAutoQueue(campaign, enabled);
      onSaved(result.campaign);
      if (result.handoffError) setMessage(tr("켰지만 자동큐로 넘기지 못했습니다: ", "Turned on, but the handoff failed: ") + result.handoffError);
      else if (result.handoff) setMessage(handoffSummary(result.handoff, tr));
      else if (result.campaign.auto_queue) setMessage(tr("캠페인이 진행 중 상태가 되면 넘기기 시작합니다.", "Tasks are sent once the campaign is active."));
      else setMessage(tr("새 작업을 넘기는 것만 멈췄습니다. 이미 넘긴 작업은 자동큐에서 계속 실행됩니다.", "Stopped sending new tasks. Tasks already sent keep running in auto-queue."));
    } catch (cause) {
      const reason = cause instanceof Error ? cause.message : tr("저장하지 못했습니다.", "Could not save.");
      if (isRejectedSave(cause)) { setMessage(reason); return; }
      // A timeout or bad response can hide a save that went through, so show what the server has now.
      try {
        const latest = await getCampaign(campaign.id);
        onSaved(latest);
        setMessage(latest.auto_queue === enabled
          ? tr(`저장됐지만 응답을 받지 못했습니다 (${reason}). 넘긴 작업은 노드 상태에서 확인하세요.`, `Saved, but the response was lost (${reason}). Check the nodes for what was sent.`)
          : tr(`저장되지 않았습니다: ${reason}`, `Not saved: ${reason}`));
      } catch {
        setMessage(tr(`저장 결과를 확인하지 못했습니다 (${reason}). 새로고침해 확인하세요.`, `Could not confirm the save (${reason}). Reload to check.`));
      }
    } finally { setBusy(false); }
  };
  return <>
    <button type="button" className="campaign-auto-queue" aria-pressed={campaign.auto_queue} disabled={busy} onClick={() => void toggle()}
      title={tr("켜면 선행 작업이 끝난 작업을 자동큐가 차례로 실행합니다. 끄면 새로 넘기는 것만 멈춥니다.", "When on, auto-queue runs each task as soon as its prerequisites are done. Turning it off only stops sending new tasks.")}>
      {campaign.auto_queue ? tr("자동 진행 켜짐", "Auto-run on") : tr("자동 진행 꺼짐", "Auto-run off")}
    </button>
    {message && <p className="campaign-handoff-result" role="status">{message}</p>}
  </>;
}
