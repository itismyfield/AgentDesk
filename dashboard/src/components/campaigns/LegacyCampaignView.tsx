import { ChevronDown, ListChecks } from "lucide-react";
import type { ReactNode } from "react";
import type { Campaign, CampaignNodeLive } from "../../api/campaigns";
import { Badge as KitBadge } from "../../kit/free/badge-clean";
import { EmptyState } from "../../kit/free/empty-state-clean";
import CampaignAutoQueueToggle from "./CampaignAutoQueueToggle";
import CampaignExplorer from "./CampaignExplorer";
import type { CampaignDraft } from "./CampaignNodeDetails";
import { campaignProgress } from "./campaignModel";
import { Badge, type Tr } from "./campaignPresentation";

/** A ledger campaign: the picker, status, round, completion and auto-run on one line, a thin progress bar, the
 * description folded away, then its tasks. */
export default function LegacyCampaignView({ campaign, live, selector, refreshedAt, checkedRead, language, drafts, onDraftChange, node, onNode, onSaved, tr }: {
  campaign: Campaign; live: Record<string, CampaignNodeLive>; selector: ReactNode; refreshedAt: number | null; checkedRead: number | null; language: string;
  drafts: Record<string, CampaignDraft>; onDraftChange: (key: string, draft: CampaignDraft | null, expected?: CampaignDraft) => void;
  node: string | null; onNode: (id: string | null) => void; onSaved: (updated: Campaign) => void; tr: Tr;
}) {
  const progress = campaignProgress(campaign.nodes);
  return <div className="campaign-main grid min-w-0 gap-3">
    <div className="campaign-summary-header flex min-w-0 flex-wrap items-center gap-x-3 gap-y-2">
      {selector}
      <div className="flex min-w-0 flex-1 flex-wrap items-center gap-x-3 gap-y-1 text-xs text-[var(--fg-muted)]">
        <Badge status={campaign.status} tr={tr} />
        <span className="max-sm:hidden"><KitBadge title={tr("작업 기록부로 관리하는 캠페인", "Campaign kept as a task ledger")}>{tr("기존 방식", "Legacy mode")}</KitBadge></span>
        <span className="tabular-nums">{tr("라운드", "Round")} {campaign.round}</span>
        <strong className="font-medium tabular-nums text-[var(--fg)]">{tr(`완료 ${progress.counts.completed}/${progress.total}`, `${progress.counts.completed}/${progress.total} done`)}</strong>
        {refreshedAt && <span className="tabular-nums max-sm:hidden">{tr("확인", "Checked")} {new Date(refreshedAt).toLocaleTimeString(language)}</span>}
      </div>
      <CampaignAutoQueueToggle key={campaign.id} campaign={campaign} tr={tr} onSaved={onSaved} checkedRead={checkedRead} />
    </div>
    <progress className="campaign-progress block h-1 w-full appearance-none overflow-hidden rounded-full border-0 bg-[var(--line)] [&::-moz-progress-bar]:bg-[var(--ok)] [&::-webkit-progress-bar]:bg-[var(--line)] [&::-webkit-progress-value]:rounded-full [&::-webkit-progress-value]:bg-[var(--ok)]"
      max={100} value={progress.percent} aria-label={tr("작업 완료율", "Task completion")} title={`${progress.percent}%`} />
    {campaign.description && <details className="group min-w-0 text-xs text-[var(--fg-muted)]">
      <summary className="flex w-fit cursor-pointer list-none items-center gap-1.5 rounded-[var(--r-sm)] px-1 hover:text-[var(--fg)] [&::-webkit-details-marker]:hidden">
        <ChevronDown aria-hidden className="size-3.5 -rotate-90 transition-transform group-open:rotate-0 motion-reduce:transition-none" />{tr("캠페인 설명", "Campaign description")}
      </summary>
      <p className="campaign-description mt-1 max-w-[72ch] px-1 text-[var(--fg-dim)]">{campaign.description}</p>
    </details>}
    {campaign.nodes.length
      ? <CampaignExplorer key={campaign.id} campaign={campaign} live={live} tr={tr} onSaved={onSaved} drafts={drafts} onDraftChange={onDraftChange} initialSelectedId={node} onSelectedChange={onNode} />
      : <EmptyState icon={<ListChecks aria-hidden />} title={tr("아직 작업이 없습니다.", "No tasks yet.")} text={tr("작업이 기록되면 여기에 그룹별로 모입니다.", "Recorded tasks gather here by group.")} />}
  </div>;
}
