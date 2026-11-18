import { Inbox, RefreshCw, Search } from "lucide-react";
import { useMemo, useState } from "react";
import type { Campaign } from "../../api/campaigns";
import { useKanban } from "../../contexts/KanbanContext";
import { useWorkspace } from "../../contexts/WorkspaceContext";
import { Button } from "../../kit/free/button-clean";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { Select } from "../../kit/free/select-clean";
import { Loading, Skeleton } from "../../kit/free/skeleton-clean";
import { TextField } from "../../kit/free/text-field-clean";
import type { KanbanCard, UiLanguage } from "../../types";
import { labelForStatus } from "../agent-manager/kanban-utils";
import { DrawerDockContext } from "../common/overlay/DrawerDock";
import { WidgetState } from "../common/WidgetState";
import type { Tr } from "./campaignPresentation";
import { EMPTY_QUEUE_FILTERS, filterQueueCards, groupQueueCards, queueCards, type QueueFilters, type QueueSort } from "./campaignQueueModel";
import QueueCardDetail from "./QueueCardDetail";
import QueueCardRow from "./QueueCardRow";
import QueueReadyDialog from "./QueueReadyDialog";
import { DetailDockPane, TaskGroupHeading, moveListFocus, useDetailDock } from "./TaskListParts";
import { useQueueCardActions } from "./useQueueCardActions";

const SORTS: Array<[QueueSort, [string, string]]> = [
  ["priority", ["우선순위순", "By priority"]],
  ["recent", ["최근 변경순", "Recently changed"]],
  ["oldest", ["오래된 순", "Oldest first"]],
  ["number", ["이슈 번호순", "By issue number"]],
];
/** Open cards no campaign holds yet: hand them to ready, or put them into a campaign. */
export default function CampaignQueueTab({ language, tr, campaigns, campaignsLoaded, campaignsError, onRetryCampaigns, heldIssueUrls, serverHoldsIncomplete, selectedCardId, onSelectCard, onCampaignSaved }: {
  language: string; tr: Tr; campaigns: Campaign[]; campaignsLoaded: boolean; campaignsError: string | null; onRetryCampaigns: () => void;
  /** Issue links server campaigns hold; incomplete when a server campaign could not be read. */
  heldIssueUrls: readonly string[]; serverHoldsIncomplete: boolean;
  selectedCardId: string | null; onSelectCard: (id: string | null) => void; onCampaignSaved: (campaign: Campaign) => void;
}) {
  const locale = language as UiLanguage;
  const { kanbanCards, kanbanCardsState, reloadKanbanCards } = useKanban();
  const { agents: allAgents } = useWorkspace();
  const actions = useQueueCardActions();
  const [filters, setFilters] = useState<QueueFilters>(EMPTY_QUEUE_FILTERS);
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(() => new Set());
  const [needsAssignee, setNeedsAssignee] = useState<KanbanCard | null>(null);
  const [busyCard, setBusyCard] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const agents = useMemo(() => new Map(allAgents.map((agent) => [agent.id, agent])), [allAgents]);
  const queued = useMemo(() => queueCards(kanbanCards, campaigns, heldIssueUrls), [kanbanCards, campaigns, heldIssueUrls]);
  const shown = useMemo(() => filterQueueCards(queued, filters), [queued, filters]);
  const groups = useMemo(() => groupQueueCards(shown), [shown]);
  const statuses = useMemo(() => [...new Set(queued.map((card) => card.status))], [queued]);
  const repos = useMemo(() => [...new Set(queued.flatMap((card) => card.github_repo ? [card.github_repo] : []))].sort(), [queued]);
  const patch = (next: Partial<QueueFilters>) => setFilters((current) => ({ ...current, ...next }));
  const toggleGroup = (repo: string) => setCollapsed((current) => {
    const next = new Set(current);
    if (!next.delete(repo)) next.add(repo);
    return next;
  });

  // Wide viewports keep the card detail docked beside the list; phones and tablets open it as a bottom sheet.
  const detailDock = useDetailDock();
  const { dock, id: dockId } = detailDock;

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
  const refreshLabel = tr("새로고침", "Refresh");
  const repoName = (repo: string) => repo ? repo.split("/").pop() ?? repo : tr("저장소 없음", "No repository");
  return <div ref={detailDock.root} className="@container/tasks grid min-w-0 gap-3" style={detailDock.rootStyle}>
    {/* Search and refresh share the first row, the three filters the second; a wide list puts all on one row. */}
    <div className="grid min-w-0 grid-cols-[minmax(0,1fr)_auto] items-center gap-2 @3xl/tasks:grid-cols-[minmax(0,1fr)_repeat(3,9rem)_auto]">
      <TextField type="search" value={filters.query} onChange={(event) => patch({ query: event.target.value })}
        aria-label={tr("카드 검색", "Search cards")} placeholder={tr("제목, #번호, 담당…", "Title, #number, assignee…")} prefix={<Search aria-hidden />} />
      <div className="@3xl/tasks:order-last">
        <Button variant="secondary" icon={<RefreshCw aria-hidden />} aria-label={refreshLabel} title={refreshLabel} disabled={kanbanCardsState === "loading"} onClick={reloadKanbanCards}>
          <span className="@max-[30rem]/tasks:sr-only">{refreshLabel}</span>
        </Button>
      </div>
      <div className="col-span-2 grid min-w-0 grid-cols-3 gap-2 @3xl/tasks:contents">
        <Select aria-label={tr("상태", "Status")} value={filters.status} onChange={(value) => patch({ status: value })}
          options={[{ value: "all", label: tr("모든 상태", "All statuses") }, ...statuses.map((status) => ({ value: status, label: labelForStatus(status, tr) }))]} />
        <Select aria-label={tr("저장소", "Repository")} value={filters.repo} onChange={(value) => patch({ repo: value })}
          options={[{ value: "all", label: tr("모든 저장소", "All repositories") }, ...repos.map((repo) => ({ value: repo, label: repoName(repo), description: repo }))]} />
        <Select aria-label={tr("정렬", "Sort")} value={filters.sort} onChange={(value) => patch({ sort: value as QueueSort })}
          options={SORTS.map(([key, label]) => ({ value: key, label: tr(...label) }))} />
      </div>
    </div>
    <p role="status" className="text-xs text-[var(--fg-muted)]">{loading ? tr("카드를 불러오는 중…", "Loading cards…")
      : shown.length === queued.length ? tr(`캠페인 밖 카드 ${queued.length}장`, `${queued.length} cards outside campaigns`)
        : tr(`캠페인 밖 카드 ${queued.length}장 중 ${shown.length}장`, `${shown.length} of ${queued.length} cards outside campaigns`)}</p>
    {error && <p role="alert" className="campaign-filter-note">{error}</p>}
    {serverHoldsIncomplete && !loading && <WidgetState kind="stale" title={tr("서버 캠페인 일부를 읽지 못했습니다.", "Some server campaigns did not load.")}
      description={tr("여기 보이는 카드 가운데 서버 캠페인이 이미 맡은 것이 있을 수 있습니다.", "Some cards shown here may already belong to a server campaign.")}
      action={<button type="button" onClick={onRetryCampaigns}>{tr("다시 시도", "Retry")}</button>} />}
    {campaignsError ? <WidgetState kind="error" title={tr("캠페인을 읽지 못해 어느 카드가 캠페인 밖인지 알 수 없습니다.", "Campaigns did not load, so the queue cannot tell which cards they hold.")}
      description={campaignsError} action={<button type="button" onClick={onRetryCampaigns}>{tr("다시 시도", "Retry")}</button>} />
      : kanbanCardsState === "error" ? <WidgetState kind="error" title={tr("카드를 불러오지 못했습니다.", "Could not load cards.")} action={<button type="button" onClick={reloadKanbanCards}>{tr("다시 시도", "Retry")}</button>} />
      : loading ? <Loading label={tr("대기열을 불러오고 있습니다.", "Loading the queue.")} className="grid gap-1">
        {[3, 2].map((rows, group) => <div key={group} className="grid gap-1">
          <div className="flex h-11 items-center gap-2 px-2"><Skeleton width={14} height={14} radius={4} /><Skeleton width={[96, 72][group]} height={10} /></div>
          {Array.from({ length: rows }, (_, row) => <div key={row} className="flex h-11 items-center gap-3 overflow-hidden px-3">
            <Skeleton width={14} height={14} radius="50%" /><Skeleton width={[184, 148, 212][row]} height={10} /><Skeleton width={[104, 128, 88][row]} height={10} />
          </div>)}
        </div>)}
      </Loading>
        : shown.length === 0 ? <EmptyState icon={<Inbox aria-hidden />} title={queued.length ? tr("조건에 맞는 카드가 없습니다.", "No cards match.") : tr("캠페인 밖에 남은 카드가 없습니다.", "Every open card is in a campaign.")}
          text={queued.length ? tr("검색어나 필터를 바꿔 보세요.", "Try another search or filter.") : tr("새 카드가 캠페인에 묶이지 않고 들어오면 여기에 모입니다.", "New cards that no campaign holds gather here.")}
          action={queued.length ? <Button variant="secondary" onClick={() => setFilters(EMPTY_QUEUE_FILTERS)}>{tr("필터 초기화", "Reset filters")}</Button> : undefined} />
          : <ul aria-label={tr("대기열 카드", "Queued cards")} className="grid min-w-0 gap-3" onKeyDown={moveListFocus}>
            {groups.map(({ repo, cards }) => {
              const open = !collapsed.has(repo);
              const listId = `${dockId}-group-${repo || "none"}`;
              return <li key={repo || "none"} className="grid min-w-0 gap-0.5">
                <TaskGroupHeading open={open} controls={listId} label={repoName(repo)} title={repo || undefined} count={cards.length} onToggle={() => toggleGroup(repo)} />
                {open && <ul id={listId} aria-label={repoName(repo)} className="grid min-w-0 gap-0.5">
                  {cards.map((card) => <QueueCardRow key={card.id} card={card} agent={card.assignee_agent_id ? agents.get(card.assignee_agent_id) : undefined} locale={locale}
                    selected={card.id === selectedCardId} busy={busyCard === card.id} controls={dock ? dockId : undefined} tr={tr} onOpen={() => onSelectCard(card.id)} onReady={() => requestReady(card)} />)}
                </ul>}
              </li>;
            })}
          </ul>}
    <DetailDockPane dock={detailDock} label={tr("카드 상세", "Card details")} empty={selectedCardId ? null : {
      title: tr("고른 카드가 없습니다", "No card selected"),
      text: tr("목록에서 카드를 열면 내용과 활동, 캠페인 넣기를 여기서 봅니다.", "Open a card from the list to read it, follow its activity and put it into a campaign here."),
    }} />
    <DrawerDockContext.Provider value={dock}>
      {selectedCardId && <QueueCardDetail key={selectedCardId} cardId={selectedCardId} card={kanbanCards.find((card) => card.id === selectedCardId)} agents={allAgents} campaigns={campaigns}
        locale={locale} tr={tr} actions={actions} onClose={() => onSelectCard(null)} onNeedsAssignee={setNeedsAssignee} onCampaignSaved={onCampaignSaved} />}
    </DrawerDockContext.Provider>
    {needsAssignee && <QueueReadyDialog card={needsAssignee} agents={allAgents} locale={locale} tr={tr} onCancel={() => setNeedsAssignee(null)}
      onConfirm={(agentId) => { const card = needsAssignee; setNeedsAssignee(null); void moveToReady(card, agentId); }} />}
  </div>;
}
