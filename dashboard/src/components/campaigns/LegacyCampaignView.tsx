import type { ReactNode } from "react";
import type { Campaign, CampaignNodeLive } from "../../api/campaigns";
import { WidgetState } from "../common/WidgetState";
import CampaignAutoQueueToggle from "./CampaignAutoQueueToggle";
import CampaignExplorer from "./CampaignExplorer";
import type { CampaignDraft } from "./CampaignNodeDetails";
import { campaignProgress } from "./campaignModel";
import { Badge, type Tr } from "./campaignPresentation";

/** A ledger campaign: one compact summary line with the auto-run switch, then its tasks. */
export default function LegacyCampaignView({ campaign, live, selector, refreshedAt, checkedRead, language, drafts, onDraftChange, node, onNode, onSaved, tr }: {
  campaign: Campaign; live: Record<string, CampaignNodeLive>; selector: ReactNode; refreshedAt: number | null; checkedRead: number | null; language: string;
  drafts: Record<string, CampaignDraft>; onDraftChange: (key: string, draft: CampaignDraft | null, expected?: CampaignDraft) => void;
  node: string | null; onNode: (id: string | null) => void; onSaved: (updated: Campaign) => void; tr: Tr;
}) {
  const progress = campaignProgress(campaign.nodes);
  return <div className="campaign-main gap-4 rounded-lg p-3 sm:p-5">
    <div className="campaign-summary-header flex flex-wrap items-center gap-x-3 gap-y-2">
      {selector}
      <div className="flex min-w-0 flex-1 flex-wrap items-center gap-x-3 gap-y-1 text-xs text-th-text-muted">
        <Badge status={campaign.status} tr={tr} />
        <span className="campaign-badge max-sm:hidden" title={tr("작업 기록부로 관리하는 캠페인", "Campaign kept as a task ledger")}>{tr("기존 방식", "Legacy mode")}</span>
        <span className="campaign-summary-round">{tr("라운드", "Round")} {campaign.round}</span>
        <strong className="font-medium text-th-text-primary tabular-nums">{tr(`완료 ${progress.counts.completed}/${progress.total}`, `${progress.counts.completed}/${progress.total} done`)}</strong>
        {refreshedAt && <span className="campaign-freshness">{tr("확인", "Checked")} {new Date(refreshedAt).toLocaleTimeString(language)}</span>}
      </div>
      <CampaignAutoQueueToggle key={campaign.id} campaign={campaign} tr={tr} onSaved={onSaved} checkedRead={checkedRead} />
    </div>
    <progress className="campaign-progress" max={100} value={progress.percent} aria-label={tr("작업 완료율", "Task completion")} />
    {campaign.description && <details className="text-xs text-th-text-muted"><summary className="cursor-pointer">{tr("캠페인 설명", "Campaign description")}</summary><p className="campaign-description mt-2">{campaign.description}</p></details>}
    {campaign.nodes.length
      ? <CampaignExplorer key={campaign.id} campaign={campaign} live={live} tr={tr} onSaved={onSaved} drafts={drafts} onDraftChange={onDraftChange} initialSelectedId={node} onSelectedChange={onNode} />
      : <WidgetState kind="empty" title={tr("아직 작업이 없습니다.", "No tasks yet.")} />}
  </div>;
}
