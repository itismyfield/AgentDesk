import { Plus, RefreshCw } from "lucide-react";
import { useState } from "react";
import { Badge as KitBadge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { BUCKET_LABELS, type CampaignBucket, type CampaignEntry } from "./campaignOverviewModel";
import { Badge, type Tr } from "./campaignPresentation";

/** Ledger and server campaigns in one list under running · ready · done; the tab follows the open campaign until one is picked. */
export default function CampaignList({ entries, selectedKey, onSelect, onCreate, createDisabled, onRefresh, refreshing, tr }: {
  entries: CampaignEntry[]; selectedKey: string | null; onSelect: (entry: CampaignEntry) => void;
  onCreate: () => void; createDisabled: boolean; onRefresh: () => void; refreshing: boolean; tr: Tr;
}) {
  const [picked, setPicked] = useState<CampaignBucket | null>(null);
  const bucket = picked ?? entries.find((entry) => entry.key === selectedKey)?.bucket ?? "running";
  const shown = entries.filter((entry) => entry.bucket === bucket);
  return <section className="grid min-w-0 content-start gap-3" aria-label={tr("캠페인 목록", "Campaign list")}>
    <div className="flex min-w-0 items-center gap-2">
      <span className="mr-auto text-xs text-[var(--fg-muted)] tabular-nums">{tr(`캠페인 ${entries.length}개`, `${entries.length} campaigns`)}</span>
      <Button variant="secondary" size="sm" icon={<Plus aria-hidden />} disabled={createDisabled} onClick={onCreate}>{tr("새 캠페인", "New campaign")}</Button>
      <IconButton icon={<RefreshCw aria-hidden />} label={refreshing ? tr("불러오는 중…", "Refreshing…") : tr("새로고침", "Refresh")} disabled={refreshing} onClick={onRefresh} />
    </div>
    <div className="w-full">
      <SegmentedControl<CampaignBucket> variant="tray" fullWidth size="sm" aria-label={tr("캠페인 상태", "Campaign status")} value={bucket} onChange={setPicked}
        options={(Object.keys(BUCKET_LABELS) as CampaignBucket[]).map((value) => ({ value, label: tr(...BUCKET_LABELS[value]), count: entries.filter((entry) => entry.bucket === value).length }))} />
    </div>
    {shown.length === 0 && <p className="px-1 text-xs text-[var(--fg-muted)]">{tr("이 상태의 캠페인이 없어요.", "No campaigns in this state.")}</p>}
    <ul className="campaign-list grid min-w-0 gap-2">
      {shown.map((entry) => {
        const selected = entry.key === selectedKey;
        return <li key={entry.key} className="min-w-0">
          <button type="button" data-campaign-key={entry.key} aria-current={selected || undefined} onClick={() => onSelect(entry)}
            className={`!m-0 grid w-full min-w-0 gap-1.5 !rounded-xl !border !px-3.5 !py-3 text-left transition-colors motion-reduce:transition-none ${selected
              ? "!border-[var(--accent)] !bg-[var(--accent-soft)]" : "!border-[var(--line)] !bg-[var(--th-card-bg)] hover:!bg-[color-mix(in_oklch,var(--fg)_4%,var(--th-card-bg))]"}`}>
            <span className="flex min-w-0 items-start gap-2">
              <strong className="min-w-0 flex-1 text-sm font-semibold leading-5 text-[var(--fg)] [overflow-wrap:anywhere] [word-break:keep-all]">{entry.title}</strong>
              <Badge status={entry.status} label={entry.statusLabel} tr={tr} />
            </span>
            {entry.description && <span className="truncate text-xs text-[var(--fg-muted)]">{entry.description.split("\n")[0]}</span>}
            <span className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-xs tabular-nums text-[var(--fg-muted)]">
              <KitBadge>{entry.kind === "legacy" ? tr("원장", "Ledger") : tr("서버", "Server")}</KitBadge>
              {entry.repo && <span className="font-mono">{entry.repo}</span>}
              {entry.total !== null && <span>{tr(`작업 ${entry.done}/${entry.total} 완료`, `${entry.done}/${entry.total} tasks done`)}</span>}
              {entry.attention && <span className="text-[var(--warn)]">{entry.attention}</span>}
            </span>
          </button>
        </li>;
      })}
    </ul>
  </section>;
}
