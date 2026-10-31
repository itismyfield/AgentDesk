import type { ReactNode } from "react";
import type { Tr } from "./campaignPresentation";

export type CampaignTab = "campaigns" | "queue";

/** Title, page actions and the campaigns / queue tabs; one short block so tasks start high on a phone. */
export default function CampaignPageHeader({ tab, onTab, actions, tr }: {
  tab: CampaignTab; onTab: (tab: CampaignTab) => void; actions: ReactNode; tr: Tr;
}) {
  const tabs: Array<[CampaignTab, string]> = [["campaigns", tr("캠페인", "Campaigns")], ["queue", tr("대기열", "Queue")]];
  return <header className="grid gap-3 border-b border-th-border-subtle pb-3">
    <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
      <div className="min-w-0">
        <h2>{tr("캠페인", "Campaigns")}</h2>
        <p className="max-sm:hidden">{tab === "queue"
          ? tr("어느 캠페인에도 묶이지 않은 카드입니다. 준비됨으로 넘기거나 캠페인에 넣으세요.", "Cards no campaign holds yet. Move them to ready or put them into a campaign.")
          : tr("그룹별 작업을 살펴보고, 선택한 작업의 다음 행동부터 이어가세요.", "Browse tasks by group and pick up the next action.")}</p>
      </div>
      <div className="campaign-actions">{actions}</div>
    </div>
    <div role="tablist" aria-label={tr("캠페인 화면", "Campaign views")} className="flex gap-1">
      {tabs.map(([key, label]) => <button key={key} type="button" role="tab" id={`campaign-tab-${key}`} aria-selected={tab === key} aria-controls={`campaign-panel-${key}`}
        onClick={() => onTab(key)}
        className={`!rounded-full !px-4 text-sm ${tab === key ? "!border-[var(--th-accent-info)] !bg-[color-mix(in_srgb,var(--th-accent-info)_14%,var(--th-card-bg))] font-semibold" : "!border-th-border-subtle text-th-text-muted"}`}>
        {label}
      </button>)}
    </div>
  </header>;
}
