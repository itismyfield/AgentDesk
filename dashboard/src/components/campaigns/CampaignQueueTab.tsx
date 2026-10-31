import { useMemo, useState } from "react";
import type { Campaign } from "../../api/campaigns";
import { useKanban } from "../../contexts/KanbanContext";
import { useOffice } from "../../contexts/OfficeContext";
import type { KanbanCard, UiLanguage } from "../../types";
import { labelForStatus } from "../agent-manager/kanban-utils";
import { Select } from "../common/Select";
import { WidgetState } from "../common/WidgetState";
import type { Tr } from "./campaignPresentation";
import { EMPTY_QUEUE_FILTERS, filterQueueCards, queueCards, type QueueFilters, type QueueSort } from "./campaignQueueModel";
import QueueCardDetail from "./QueueCardDetail";
import QueueCardRow from "./QueueCardRow";
import QueueReadyDialog from "./QueueReadyDialog";
import { useQueueCardActions } from "./useQueueCardActions";

const SORTS: Array<[QueueSort, [string, string]]> = [
  ["priority", ["우선순위순", "By priority"]],
  ["recent", ["최근 변경순", "Recently changed"]],
  ["oldest", ["오래된 순", "Oldest first"]],
  ["number", ["이슈 번호순", "By issue number"]],
];

/** Open cards no campaign holds yet: hand them to ready, or put them into a campaign. */
export default function CampaignQueueTab({ language, tr, campaigns, campaignsLoaded, campaignsError, onRetryCampaigns, selectedCardId, onSelectCard, onCampaignSaved }: {
  language: string; tr: Tr; campaigns: Campaign[]; campaignsLoaded: boolean; campaignsError: string | null; onRetryCampaigns: () => void;
  selectedCardId: string | null; onSelectCard: (id: string | null) => void; onCampaignSaved: (campaign: Campaign) => void;
}) {
  const locale = language as UiLanguage;
  const { kanbanCards, kanbanCardsState, reloadKanbanCards } = useKanban();
  const { allAgents } = useOffice();
  const actions = useQueueCardActions();
  const [filters, setFilters] = useState<QueueFilters>(EMPTY_QUEUE_FILTERS);
  const [needsAssignee, setNeedsAssignee] = useState<KanbanCard | null>(null);
  const [busyCard, setBusyCard] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const agents = useMemo(() => new Map(allAgents.map((agent) => [agent.id, agent])), [allAgents]);
  const queued = useMemo(() => queueCards(kanbanCards, campaigns), [kanbanCards, campaigns]);
  const shown = useMemo(() => filterQueueCards(queued, filters), [queued, filters]);
  const statuses = useMemo(() => [...new Set(queued.map((card) => card.status))], [queued]);
  const repos = useMemo(() => [...new Set(queued.flatMap((card) => card.github_repo ? [card.github_repo] : []))].sort(), [queued]);
  const patch = (next: Partial<QueueFilters>) => setFilters((current) => ({ ...current, ...next }));

  const moveToReady = async (card: KanbanCard, assignee: string | null) => {
    setBusyCard(card.id); setError(null);
    try {
      await actions.moveToReady(card, assignee);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : tr("준비됨으로 넘기지 못했습니다.", "Could not move the card to ready."));
    } finally { setBusyCard(null); }
  };
  const requestReady = (card: KanbanCard) => {
    if (card.assignee_agent_id) void moveToReady(card, null);
    else setNeedsAssignee(card);
  };

  const loading = kanbanCardsState === "idle" || kanbanCardsState === "loading" || (!campaignsLoaded && !campaignsError);
  return <div className="grid min-w-0 gap-3">
    <div className="grid gap-2 sm:grid-cols-[minmax(0,1fr)_repeat(3,minmax(0,11rem))_auto] sm:items-end">
      <label className="grid min-w-0 gap-1 text-xs text-th-text-muted"><span className="max-sm:sr-only">{tr("카드 검색", "Search cards")}</span>
        <input type="search" value={filters.query} onChange={(event) => patch({ query: event.target.value })} placeholder={tr("제목, #번호, 담당…", "Title, #number, assignee…")} />
      </label>
      <div className="grid grid-cols-3 gap-2 sm:contents">
        <label className="grid min-w-0 gap-1 text-xs text-th-text-muted"><span className="max-sm:sr-only">{tr("상태", "Status")}</span>
          <Select value={filters.status} onChange={(event) => patch({ status: event.target.value })}>
            <option value="all">{tr("모든 상태", "All statuses")}</option>
            {statuses.map((status) => <option key={status} value={status}>{labelForStatus(status, tr)}</option>)}
          </Select>
        </label>
        <label className="grid min-w-0 gap-1 text-xs text-th-text-muted"><span className="max-sm:sr-only">{tr("저장소", "Repository")}</span>
          <Select value={filters.repo} onChange={(event) => patch({ repo: event.target.value })}>
            <option value="all">{tr("모든 저장소", "All repositories")}</option>
            {repos.map((repo) => <option key={repo} value={repo}>{repo.split("/").pop()}</option>)}
          </Select>
        </label>
        <label className="grid min-w-0 gap-1 text-xs text-th-text-muted"><span className="max-sm:sr-only">{tr("정렬", "Sort")}</span>
          <Select value={filters.sort} onChange={(event) => patch({ sort: event.target.value as QueueSort })}>
            {SORTS.map(([key, label]) => <option key={key} value={key}>{tr(...label)}</option>)}
          </Select>
        </label>
      </div>
      <button type="button" className="max-sm:hidden" disabled={kanbanCardsState === "loading"} onClick={reloadKanbanCards}>{tr("새로고침", "Refresh")}</button>
    </div>
    <p role="status" className="text-xs text-th-text-muted">{loading ? tr("카드를 불러오는 중…", "Loading cards…")
      : shown.length === queued.length ? tr(`캠페인 밖 카드 ${queued.length}장`, `${queued.length} cards outside campaigns`)
        : tr(`캠페인 밖 카드 ${queued.length}장 중 ${shown.length}장`, `${shown.length} of ${queued.length} cards outside campaigns`)}</p>
    {error && <p role="alert" className="campaign-filter-note">{error}</p>}
    {campaignsError ? <WidgetState kind="error" title={tr("캠페인을 읽지 못해 어느 카드가 캠페인 밖인지 알 수 없습니다.", "Campaigns did not load, so the queue cannot tell which cards they hold.")}
      description={campaignsError} action={<button type="button" onClick={onRetryCampaigns}>{tr("다시 시도", "Retry")}</button>} />
      : kanbanCardsState === "error" ? <WidgetState kind="error" title={tr("카드를 불러오지 못했습니다.", "Could not load cards.")} action={<button type="button" onClick={reloadKanbanCards}>{tr("다시 시도", "Retry")}</button>} />
      : loading ? <WidgetState kind="loading" title={tr("대기열을 불러오고 있습니다.", "Loading the queue.")} />
        : shown.length === 0 ? <WidgetState kind="empty" title={queued.length ? tr("조건에 맞는 카드가 없습니다.", "No cards match.") : tr("캠페인 밖에 남은 카드가 없습니다.", "Every open card is in a campaign.")}
          action={queued.length ? <button type="button" onClick={() => setFilters(EMPTY_QUEUE_FILTERS)}>{tr("필터 초기화", "Reset filters")}</button> : undefined} />
          : <ul aria-label={tr("대기열 카드", "Queued cards")} className="min-w-0 overflow-hidden rounded-lg border border-th-border-subtle bg-th-card-bg">
            {shown.map((card) => <QueueCardRow key={card.id} card={card} agent={card.assignee_agent_id ? agents.get(card.assignee_agent_id) : undefined} locale={locale}
              selected={card.id === selectedCardId} busy={busyCard === card.id} tr={tr} onOpen={() => onSelectCard(card.id)} onReady={() => requestReady(card)} />)}
          </ul>}
    {selectedCardId && <QueueCardDetail cardId={selectedCardId} card={kanbanCards.find((card) => card.id === selectedCardId)} agents={allAgents} campaigns={campaigns}
      locale={locale} tr={tr} actions={actions} onClose={() => onSelectCard(null)} onNeedsAssignee={setNeedsAssignee} onCampaignSaved={onCampaignSaved} />}
    {needsAssignee && <QueueReadyDialog card={needsAssignee} agents={allAgents} locale={locale} tr={tr} onCancel={() => setNeedsAssignee(null)}
      onConfirm={(agentId) => { const card = needsAssignee; setNeedsAssignee(null); void moveToReady(card, agentId); }} />}
  </div>;
}
