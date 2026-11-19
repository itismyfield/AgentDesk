import { useId, useMemo, useState, type Dispatch, type KeyboardEvent, type ReactNode, type SetStateAction } from "react";
import {
  ArrowRightLeft,
  Gavel,
  Hammer,
  MessageSquare,
  RefreshCw,
  Search,
  SearchCheck,
  Send,
  type LucideIcon,
} from "lucide-react";

import MarkdownContent from "../common/MarkdownContent";
import type { CardAuditLogEntry, GitHubComment } from "../../api";
import type { KanbanCard, KanbanCardStatus, TaskDispatch, UiLanguage } from "../../types";
import { Badge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { TextField } from "../../kit/free/text-field-clean";
import {
  CARD_ACTIVITY_SOURCES,
  buildCardActivity,
  cardActivityClock,
  cardActivityDayLabel,
  cardActivitySourceLabel,
  countCardActivity,
  filterCardActivity,
  groupCardActivityByDay,
  timelineStatusPresentation,
  type CardActivityFilter,
  type CardActivityRecord,
  type CardActivitySource,
} from "./card-detail-activity";
import { CopyWell, DetailHeading, KIT_TOUCH_BUTTON, KIT_TOUCH_FIELD, KIT_TOUCH_TABS, PropList } from "./CardDetailParts";
import { formatIso, labelForStatus, type GitHubTimelineKind } from "./kanban-utils";

type Tr = (ko: string, en: string) => string;

const SOURCE_MARK: Record<CardActivitySource, { icon: LucideIcon; color: string }> = {
  status: { icon: ArrowRightLeft, color: "var(--th-text-secondary)" },
  dispatch: { icon: Send, color: "var(--info)" },
  review: { icon: SearchCheck, color: "var(--accent-violet)" },
  pm: { icon: Gavel, color: "var(--warn)" },
  work: { icon: Hammer, color: "var(--ok)" },
  general: { icon: MessageSquare, color: "var(--th-text-muted)" },
};

const RESULT_TONE_CLASS = {
  default: "bg-surface-subtle text-th-text-secondary",
  warn: "bg-[color-mix(in_oklch,var(--warn)_14%,transparent)] text-[color-mix(in_oklch,var(--warn)_72%,var(--th-text-primary))]",
  danger: "bg-[color-mix(in_oklch,var(--err)_14%,transparent)] text-[color-mix(in_oklch,var(--err)_72%,var(--th-text-primary))]",
} as const;

function SourceMark({ source }: { source: CardActivitySource }) {
  const { icon: Icon, color } = SOURCE_MARK[source];
  return (
    <span aria-hidden="true" className="grid size-[26px] shrink-0 place-items-center rounded-[7px] bg-surface-medium" style={{ color }}>
      <Icon size={14} />
    </span>
  );
}

export interface CardTimelineProps {
  ghComments: GitHubComment[];
  timelineFilter: GitHubTimelineKind | null;
  setTimelineFilter: Dispatch<SetStateAction<GitHubTimelineKind | null>>;
  tr: Tr;
  locale: UiLanguage;
  onRefresh: () => void;
  /** Dispatches and state transitions join the same list when the card is given. */
  card?: Pick<KanbanCard, "id" | "latest_dispatch_id">;
  dispatches?: TaskDispatch[];
  auditLog?: CardAuditLogEntry[];
  getAgentLabel?: (agentId: string | null | undefined) => string;
  /** Clock for the day labels; defaults to now. */
  now?: number;
}

/** The card's activity in one dated list: filter tabs with counts, search, two-line rows on narrow panels and the full record on open. */
export default function CardTimeline({
  ghComments,
  timelineFilter,
  setTimelineFilter,
  tr,
  locale,
  onRefresh,
  card,
  dispatches,
  auditLog,
  getAgentLabel,
  now,
}: CardTimelineProps) {
  const id = useId();
  const [sourceFilter, setSourceFilter] = useState<"status" | "dispatch" | null>(null);
  const [query, setQuery] = useState("");
  const [openId, setOpenId] = useState<string | null>(null);
  const records = useMemo(
    () => buildCardActivity({
      card: card ?? { id: "", latest_dispatch_id: null },
      dispatches: card ? dispatches ?? [] : [],
      auditLog: auditLog ?? [],
      ghComments,
      tr,
      getAgentLabel: getAgentLabel ?? ((agentId) => agentId ?? ""),
    }),
    [auditLog, card, dispatches, getAgentLabel, ghComments, tr],
  );
  const counts = countCardActivity(records);
  const filter: CardActivityFilter = sourceFilter ?? timelineFilter ?? "all";
  const visible = filterCardActivity(records, filter, query, tr);
  const groups = groupCardActivityByDay(visible);
  const present = CARD_ACTIVITY_SOURCES.filter((source) => counts[source] > 0 || source === filter);
  const clock = now ?? Date.now();

  const choose = (value: CardActivityFilter) => {
    if (value === "status" || value === "dispatch") {
      setSourceFilter(value);
      setTimelineFilter(null);
    } else {
      setSourceFilter(null);
      setTimelineFilter(value === "all" ? null : value);
    }
  };

  // Arrow keys walk the visible rows, like the kit activity list.
  const onListKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
    const rows = Array.from(event.currentTarget.querySelectorAll<HTMLElement>("[data-activity-row]"));
    const index = rows.indexOf(event.target as HTMLElement);
    if (index < 0 || rows.length === 0) return;
    event.preventDefault();
    rows[Math.max(0, Math.min(rows.length - 1, index + (event.key === "ArrowDown" ? 1 : -1)))].focus();
  };

  return (
    <section className="@container/activity flex min-w-0 flex-col gap-3" aria-labelledby={`${id}-title`} data-testid="card-activity">
      <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-2">
        <h4 id={`${id}-title`} className="m-0 flex items-center gap-2 text-sm font-medium text-th-text-heading">
          {tr("활동", "Activity")}
          <Badge size="md">{String(records.length)}</Badge>
        </h4>
        <div className="ml-auto flex min-w-0 items-center gap-2">
          <Button variant="ghost" size="lg" className={KIT_TOUCH_BUTTON} icon={<RefreshCw />} onClick={onRefresh}>
            {tr("새로고침", "Refresh")}
          </Button>
        </div>
      </div>

      {records.length > 0 && (
        <div className="flex min-w-0 flex-col gap-2">
          {present.length > 1 && (
            <div className="min-w-0">
              <SegmentedControl<CardActivityFilter>
                kind="tabs"
                size="lg"
                className={KIT_TOUCH_TABS}
                aria-label={tr("활동 필터", "Activity filter")}
                value={filter}
                onChange={choose}
                options={[
                  { value: "all", label: tr("전체", "All"), count: counts.all, controls: `${id}-list` },
                  ...present.map((source) => ({ value: source, label: cardActivitySourceLabel(source, tr), count: counts[source], controls: `${id}-list` })),
                ]}
              />
            </div>
          )}
          <TextField
            type="search"
            size="lg"
            className={`w-full @min-[40rem]/activity:max-w-sm ${KIT_TOUCH_FIELD}`}
            prefix={<Search aria-hidden="true" />}
            clearable
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            onKeyDown={(event) => { if (event.key === "Escape" && query) { event.preventDefault(); event.stopPropagation(); setQuery(""); } }}
            placeholder={tr("활동 검색", "Search activity")}
            aria-label={tr("활동 검색", "Search activity")}
            aria-controls={`${id}-list`}
          />
        </div>
      )}

      <div
        id={`${id}-list`}
        role="region"
        aria-label={tr(`${filter === "all" ? "전체" : cardActivitySourceLabel(filter, tr)} 활동 기록`, `${filter === "all" ? "All" : cardActivitySourceLabel(filter, tr)} activity`)}
        className="flex max-h-[32rem] min-w-0 flex-col gap-4 overflow-y-auto overscroll-contain"
        onKeyDown={onListKeyDown}
        data-testid="card-activity-list"
      >
        {records.length === 0 ? (
          <EmptyActivity title={tr("아직 활동 기록이 없어요", "No activity yet")} body={tr("디스패치·상태 전환·GitHub 코멘트가 생기면 이곳에 한 줄씩 쌓입니다.", "Dispatches, status changes and GitHub comments land here, one line each.")} />
        ) : visible.length === 0 ? (
          <EmptyActivity
            title={tr("조건에 맞는 활동이 없어요", "No activity matches")}
            body={tr("검색어나 필터를 바꾸거나 전체 기록으로 돌아가세요.", "Change the search or filter, or go back to everything.")}
            action={<Button variant="secondary" size="lg" className={KIT_TOUCH_BUTTON} onClick={() => { setQuery(""); choose("all"); }}>{tr("전체 보기", "Show all")}</Button>}
          />
        ) : groups.map((group) => {
          const headId = `${id}-day-${group.key ?? "none"}`;
          return (
            <section key={group.key ?? "none"} aria-labelledby={headId} className="flex min-w-0 flex-col gap-1">
              <h5 id={headId} className="m-0 flex h-8 items-center gap-2 rounded-[10px] bg-surface-subtle px-3 text-[12.5px] font-medium text-th-text-primary">
                <span>{cardActivityDayLabel(group.key, clock, locale, tr)}</span>
                <span className="min-w-5 rounded-md bg-th-card-bg px-1.5 text-center text-[11.5px] font-normal leading-[18px] text-th-text-secondary" aria-label={tr(`표시 ${group.items.length}건`, `${group.items.length} shown`)}>
                  {group.items.length}
                </span>
              </h5>
              <ul className="m-0 flex list-none flex-col gap-0.5 p-0">
                {group.items.map((record) => (
                  <ActivityRow key={record.id} record={record} open={openId === record.id} locale={locale} tr={tr}
                    onToggle={() => setOpenId((current) => (current === record.id ? null : record.id))} />
                ))}
              </ul>
            </section>
          );
        })}
      </div>
    </section>
  );
}

function EmptyActivity({ title, body, action }: { title: string; body: string; action?: ReactNode }) {
  return (
    <div className="flex flex-col items-center gap-1.5 px-6 py-8 text-center">
      <span aria-hidden="true" className="grid size-9 place-items-center rounded-[10px] bg-surface-medium text-th-text-muted"><Search size={16} /></span>
      <p className="m-0 mt-1 text-[13px] font-medium text-th-text-primary">{title}</p>
      <p className="m-0 max-w-[22rem] text-xs leading-4 text-th-text-muted [word-break:keep-all]">{body}</p>
      {action && <div className="mt-2">{action}</div>}
    </div>
  );
}

function ActivityRow({ record, open, locale, tr, onToggle }: { record: CardActivityRecord; open: boolean; locale: UiLanguage; tr: Tr; onToggle: () => void }) {
  const detailId = useId();
  const time = cardActivityClock(record.at, locale);
  const sourceLabel = cardActivitySourceLabel(record.source, tr);
  const label = [time, record.text, sourceLabel, record.actor, record.badge?.label].filter(Boolean).join(", ");
  return (
    <li className={`min-w-0 rounded-lg ${open ? "bg-surface-medium" : ""}`} data-activity-id={record.id} data-source={record.source}>
      <button
        type="button"
        data-activity-row
        aria-expanded={open}
        aria-controls={open ? detailId : undefined}
        aria-label={label}
        onClick={onToggle}
        className="flex min-h-14 w-full min-w-0 items-center gap-3 rounded-lg py-2 pl-3 pr-2.5 text-left transition-colors hover:bg-surface-light focus-visible:-outline-offset-2"
      >
        <time className="w-[38px] shrink-0 text-xs tabular-nums text-th-text-muted @max-[35rem]/activity:hidden" dateTime={record.at == null ? undefined : new Date(record.at).toISOString()}>
          {time}
        </time>
        <SourceMark source={record.source} />
        <span className="flex min-w-0 flex-1 flex-col">
          <span className="truncate text-[13px] leading-5 text-th-text-primary @max-[35rem]/activity:line-clamp-2 @max-[35rem]/activity:whitespace-normal @max-[35rem]/activity:[overflow-wrap:anywhere]" title={record.text} data-activity-sentence>
            {record.text}
          </span>
          <span className="flex min-w-0 items-center gap-1.5 whitespace-nowrap text-xs leading-4 text-th-text-secondary">
            <span className="inline-flex shrink-0 items-center gap-1.5">
              <i aria-hidden="true" className="size-1.5 rounded-full" style={{ background: SOURCE_MARK[record.source].color }} />
              {sourceLabel}
            </span>
            {record.actor && <span className="truncate text-th-text-muted before:content-['·_']">{record.actor}</span>}
            {record.sub && <span className="hidden truncate text-th-text-muted before:content-['·_'] @min-[35rem]/activity:inline">{record.sub}</span>}
          </span>
        </span>
        {record.badge && (
          <span className="flex shrink-0 justify-end">
            <Badge tone={record.badge.tone} dot={!record.badge.live} live={record.badge.live}>{record.badge.label}</Badge>
          </span>
        )}
      </button>
      {open && (
        <div id={detailId} role="region" aria-label={tr("활동 상세", "Activity detail")} className="px-3 pb-4 pt-1" data-testid="card-activity-detail">
          <ActivityDetail record={record} locale={locale} tr={tr} />
        </div>
      )}
    </li>
  );
}

function ActivityDetail({ record, locale, tr }: { record: CardActivityRecord; locale: UiLanguage; tr: Tr }) {
  const when = record.at == null ? tr("시각 없음", "No time") : formatIso(record.at, locale);
  const detail = record.detail;
  return (
    <div className="flex min-w-0 flex-col gap-4 border-t border-th-border-subtle pt-3">
      <p className="m-0 text-[15px] font-medium leading-[22px] text-th-text-heading [overflow-wrap:anywhere] [word-break:keep-all]">{record.text}</p>
      {detail.kind === "status" && (
        <>
          <PropList items={[
            [tr("시각", "When"), when],
            [tr("출처", "Source"), detail.origin],
            [tr("이전 상태", "From"), detail.from ? labelForStatus(detail.from as KanbanCardStatus, tr) : "—"],
            [tr("새 상태", "To"), detail.to ? labelForStatus(detail.to as KanbanCardStatus, tr) : "—"],
          ]} />
          {detail.result && (
            <section className="flex flex-col gap-2">
              <DetailHeading>{tr("결과", "Result")}</DetailHeading>
              <p className={`m-0 whitespace-pre-wrap rounded-[10px] px-3 py-2.5 text-[13px] leading-5 [overflow-wrap:anywhere] ${RESULT_TONE_CLASS[detail.result.tone]}`}>{detail.result.text}</p>
            </section>
          )}
        </>
      )}
      {detail.kind === "dispatch" && (
        <>
          <PropList items={[
            [tr("시각", "When"), when],
            [tr("상태", "Status"), record.badge ? <Badge tone={record.badge.tone} dot>{record.badge.label}</Badge> : null],
            [tr("종류", "Type"), detail.dispatch.dispatch_type],
            [tr("받는 에이전트", "Agent"), record.actor],
            [tr("디스패치", "Dispatch"), <span className="font-mono text-xs">#{detail.dispatch.id.slice(0, 8)}</span>],
            [tr("깊이", "Depth"), detail.dispatch.chain_depth > 0 ? String(detail.dispatch.chain_depth) : null],
            [tr("최신", "Latest"), detail.latest ? <Badge tone="info">{tr("최신 디스패치", "Latest dispatch")}</Badge> : null],
            [tr("완료", "Completed"), detail.dispatch.completed_at ? formatIso(detail.dispatch.completed_at, locale) : null],
          ]} />
          {detail.summary && <CopyWell title={tr("결과 요약", "Result summary")} value={detail.summary} tr={tr} />}
        </>
      )}
      {detail.kind === "comment" && (
        <>
          <PropList items={[
            [tr("시각", "When"), detail.item.coalesced && detail.item.updatedAt !== detail.item.createdAt
              ? `${formatIso(detail.item.createdAt, locale)} – ${formatIso(detail.item.updatedAt, locale)}`
              : when],
            [tr("작성자", "Author"), detail.item.author],
            [tr("종류", "Kind"), cardActivitySourceLabel(record.source, tr)],
          ]} />
          {detail.item.entries.map((entry, index) => {
            const status = timelineStatusPresentation(entry.status, tr);
            return (
              <section key={`${detail.item.id}-${index}`} className="flex min-w-0 flex-col gap-2">
                <DetailHeading aside={detail.item.coalesced ? <span className="text-xs text-th-text-muted">{formatIso(entry.createdAt, locale)}</span> : undefined}>
                  <span className="inline-flex flex-wrap items-center gap-1.5">
                    {entry.title}
                    {detail.item.coalesced && status && <Badge tone={status.tone}>{status.label}</Badge>}
                  </span>
                </DetailHeading>
                {entry.details.length > 0 && (
                  <ul className="m-0 list-disc space-y-1 pl-5 text-xs text-th-text-secondary">
                    {entry.details.map((line, lineIndex) => <li key={lineIndex}>{line}</li>)}
                  </ul>
                )}
                <div className="min-w-0 rounded-[10px] bg-surface-subtle px-3 py-2.5 text-sm text-th-text-primary [overflow-wrap:anywhere]">
                  <MarkdownContent content={entry.body} />
                </div>
              </section>
            );
          })}
        </>
      )}
    </div>
  );
}
