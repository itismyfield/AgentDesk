import { Plus, RefreshCw, Search } from "lucide-react";
import { useState, type CSSProperties } from "react";
import { Button } from "../../kit/free/button-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { TextField } from "../../kit/free/text-field-clean";
import { Cap, Chip, Desc, KitCard, Num, ROW_HOVER_CLASS, cn } from "../common/KitPrimitives";
import { BUCKET_LABELS, type CampaignBucket, type CampaignEntry } from "./campaignOverviewModel";
import { Badge, SplitBar, type Tr } from "./campaignPresentation";

const SELECTED_CARD_STYLE = {
  borderColor: "var(--accent)",
  background: "color-mix(in srgb, var(--accent) 7%, var(--bg-2))",
  "--ui-crdc-shadow-selected": "none",
} as CSSProperties;

/** Ledger and server campaigns in one list under running · ready · done; search filters by title, repo, or ID. */
export default function CampaignList({ entries, selectedKey, onSelect, onCreate, createDisabled, onRefresh, refreshing, tr }: {
  entries: CampaignEntry[]; selectedKey: string | null; onSelect: (entry: CampaignEntry) => void;
  onCreate: () => void; createDisabled: boolean; onRefresh: () => void; refreshing: boolean; tr: Tr;
}) {
  const [picked, setPicked] = useState<CampaignBucket | null>(null);
  const [query, setQuery] = useState("");
  const bucket = picked ?? entries.find((entry) => entry.key === selectedKey)?.bucket ?? "running";
  const needle = query.trim().toLocaleLowerCase();
  const matched = needle
    ? entries.filter((entry) => [entry.title, entry.repo, entry.id].some((value) => value?.toLocaleLowerCase().includes(needle)))
    : entries;
  const shown = needle ? matched : entries.filter((entry) => entry.bucket === bucket);
  return <section className="grid min-w-0 content-start gap-2.5" aria-label={tr("캠페인 목록", "Campaign list")}>
    <div className="flex min-w-0 items-center gap-2">
      <Cap className="mr-auto font-display tabular-nums">{tr(`캠페인 ${entries.length}개`, `${entries.length} campaigns`)}</Cap>
      <Button variant="secondary" size="sm" icon={<Plus aria-hidden />} disabled={createDisabled} onClick={onCreate}>{tr("새 캠페인", "New campaign")}</Button>
      <IconButton icon={<RefreshCw aria-hidden />} label={refreshing ? tr("불러오는 중…", "Refreshing…") : tr("새로고침", "Refresh")} disabled={refreshing} onClick={onRefresh} />
    </div>
    {(entries.length > 0 || query.length > 0) && (
      <TextField
        type="search"
        size="sm"
        clearable
        prefix={<Search aria-hidden />}
        aria-label={tr("캠페인 검색", "Search campaigns")}
        placeholder={tr("제목, 저장소, ID 검색", "Search title, repo, or ID")}
        value={query}
        onChange={(event) => setQuery(event.target.value)}
      />
    )}
    <div className="w-full">
      <SegmentedControl<CampaignBucket> variant="tray" fullWidth size="sm" aria-label={tr("캠페인 상태", "Campaign status")} value={bucket} onChange={setPicked}
        options={(Object.keys(BUCKET_LABELS) as CampaignBucket[]).map((value) => ({ value, label: tr(...BUCKET_LABELS[value]), count: matched.filter((entry) => entry.bucket === value).length }))} />
    </div>
    {shown.length === 0 && <p className="px-1 text-xs text-[var(--fg-muted)]">{needle ? tr("일치하는 캠페인이 없어요.", "No matching campaigns.") : tr("이 상태의 캠페인이 없어요.", "No campaigns in this state.")}</p>}
    <ul className="campaign-list m-0 grid list-none min-w-0 gap-2.5 p-0">
      {shown.map((entry) => {
        const selected = entry.key === selectedKey;
        return <KitCard as="li" key={entry.key} pad="none" selected={selected} style={selected ? SELECTED_CARD_STYLE : undefined}>
          <button type="button" data-campaign-key={entry.key} aria-current={selected || undefined} onClick={() => onSelect(entry)}
            className={cn("m-0 grid w-full min-w-0 cursor-pointer gap-1.5 border-0 bg-transparent px-3.5 py-3 text-left font-inherit text-inherit motion-reduce:transition-none", !selected && ROW_HOVER_CLASS)}>
            <span className="flex min-w-0 items-start gap-2">
              <strong className="min-w-0 flex-1 text-sm font-semibold leading-5 text-[var(--fg)] [overflow-wrap:anywhere] [word-break:keep-all]">{entry.title}</strong>
              <Badge status={entry.status} label={entry.statusLabel} tr={tr} />
            </span>
            {entry.description && <Desc className="truncate">{entry.description.split("\n")[0]}</Desc>}
            {!!entry.split?.length && <EntrySplit entry={entry} tr={tr} />}
            <Cap className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
              <Chip className="h-[18px] px-1.5 text-[11px]">{entry.kind === "legacy" ? tr("원장", "Ledger") : tr("서버", "Server")}</Chip>
              {entry.repo && <span className="font-mono">{entry.repo}</span>}
              {entry.total !== null && <Num>{tr(`작업 ${entry.done}/${entry.total} 완료`, `${entry.done}/${entry.total} tasks done`)}</Num>}
              {entry.attention && <span className="text-[var(--warn)]">{entry.attention}</span>}
            </Cap>
          </button>
        </KitCard>;
      })}
    </ul>
  </section>;
}

/** The list card's bar: a ledger's tasks by status, a server campaign's by pipeline stage. */
export function EntrySplit({ entry, tr, className }: { entry: CampaignEntry; tr: Tr; className?: string }) {
  return <SplitBar parts={entry.split ?? []} label={entry.kind === "legacy" ? tr("상태별 작업", "Tasks by status") : tr("단계별 작업", "Tasks by stage")} tr={tr} className={className} />;
}
