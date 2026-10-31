import { useEffect, useMemo, useState } from "react";
import type { Campaign } from "../../api/campaigns";
import { useTaskDispatches } from "../../contexts/KanbanContext";
import type { Agent, KanbanCard, UiLanguage } from "../../types";
import KanbanCardDetail from "../agent-manager/KanbanCardDetail";
import { EMPTY_EDITOR, coerceEditor, type EditorState } from "../agent-manager/kanban-utils";
import { reviewDecisionMap, useKanbanCardActivity } from "../agent-manager/useKanbanCardActivity";
import { Drawer } from "../common/overlay/Drawer";
import { Modal } from "../common/overlay/Modal";
import { WidgetState } from "../common/WidgetState";
import type { Tr } from "./campaignPresentation";
import { cardWithBody, isFullCard } from "./campaignQueueModel";
import QueueAddToCampaign from "./QueueAddToCampaign";
import type { useQueueCardActions } from "./useQueueCardActions";

type Actions = ReturnType<typeof useQueueCardActions>;

/** The card drawer (a bottom sheet on phones): the full card read on open, edited with the kanban card editor. */
export default function QueueCardDetail({ cardId, card, agents, campaigns, locale, tr, actions, onClose, onNeedsAssignee, onCampaignSaved }: {
  cardId: string; card: KanbanCard | undefined; agents: Agent[]; campaigns: Campaign[]; locale: UiLanguage; tr: Tr; actions: Actions;
  onClose: () => void; onNeedsAssignee: (card: KanbanCard) => void; onCampaignSaved: (campaign: Campaign) => void;
}) {
  // The list holds summaries without a description, so the drawer waits for the full card and keeps it.
  const [loaded, setLoaded] = useState<{ id: string; error: string | null } | null>(null);
  const [full, setFull] = useState<KanbanCard | null>(null);
  const [reads, setReads] = useState(0);
  useEffect(() => {
    let current = true;
    actions.load(cardId).then(
      (value) => { if (current) { setFull(value); setLoaded({ id: cardId, error: null }); } },
      // A failed re-read keeps the card already on screen.
      (cause) => { if (current && reads === 0) setLoaded({ id: cardId, error: cause instanceof Error ? cause.message : String(cause) }); },
    );
    return () => { current = false; };
  }, [cardId, actions, reads]);
  // Writes and WS events bring full cards; keep the newest one.
  useEffect(() => { if (card && isFullCard(card)) setFull(card); }, [card]);
  // A newer summary means the body may have changed too, so the full card is read again.
  const summaryIsNewer = !!card && !isFullCard(card) && !!full && full.id === card.id && card.updated_at !== full.updated_at;
  useEffect(() => { if (summaryIsNewer) setReads((value) => value + 1); }, [summaryIsNewer]);
  const shown = useMemo(() => cardWithBody(card, full), [card, full]);
  const ready = loaded?.id === cardId && !loaded.error && shown;
  if (!ready) return <Drawer open onClose={onClose} title={tr("카드 상세", "Card details")} width="min(820px, 100vw)">
    {loaded?.id === cardId && loaded.error
      ? <WidgetState kind="error" title={tr("카드를 불러오지 못했습니다.", "Could not load the card.")} description={loaded.error} />
      : <WidgetState kind="loading" title={tr("카드를 불러오고 있습니다.", "Loading the card.")} />}
  </Drawer>;
  return <LoadedCardDetail card={shown} agents={agents} campaigns={campaigns} locale={locale} tr={tr} actions={actions}
    onClose={onClose} onNeedsAssignee={onNeedsAssignee} onCampaignSaved={onCampaignSaved} />;
}

function LoadedCardDetail({ card, agents, campaigns, locale, tr, actions, onClose, onNeedsAssignee, onCampaignSaved }: {
  card: KanbanCard; agents: Agent[]; campaigns: Campaign[]; locale: UiLanguage; tr: Tr; actions: Actions;
  onClose: () => void; onNeedsAssignee: (card: KanbanCard) => void; onCampaignSaved: (campaign: Campaign) => void;
}) {
  const dispatches = useTaskDispatches();
  const { auditLog, clearCardReviews, ghComments, githubIssueBody, invalidateCardActivity, reviewData } = useKanbanCardActivity({ selectedCard: card, selectedCardId: card.id });
  const [editor, setEditor] = useState<EditorState>(EMPTY_EDITOR);
  const [savingCard, setSavingCard] = useState(false);
  const [retryingCard, setRetryingCard] = useState(false);
  const [redispatching, setRedispatching] = useState(false);
  const [redispatchReason, setRedispatchReason] = useState("");
  const [retryAssigneeId, setRetryAssigneeId] = useState("");
  const [actionError, setActionError] = useState<string | null>(null);
  const [reviewDecisions, setReviewDecisions] = useState<Record<string, "accept" | "reject">>({});
  const [timelineFilter, setTimelineFilter] = useState<"review" | "pm" | "work" | "general" | null>(null);
  const [cancelling, setCancelling] = useState<"confirm" | "sending" | null>(null);
  useEffect(() => {
    setEditor(coerceEditor(card));
    setRetryAssigneeId(card.assignee_agent_id ?? "");
  }, [card]);
  useEffect(() => {
    if (githubIssueBody != null) setEditor((prev) => ({ ...prev, description: githubIssueBody }));
  }, [githubIssueBody, card.id]);
  useEffect(() => setReviewDecisions(reviewDecisionMap(reviewData)), [reviewData]);

  const cancel = async () => {
    setCancelling("sending");
    setActionError(null);
    try {
      await actions.cancel(card.id);
      invalidateCardActivity(card.id);
    } catch (cause) {
      setActionError(cause instanceof Error ? cause.message : tr("카드를 취소하지 못했습니다.", "Could not cancel the card."));
    } finally { setCancelling(null); }
  };
  return <>
    <KanbanCardDetail card={card} tr={tr} locale={locale} agents={agents} dispatches={dispatches}
      editor={editor} setEditor={setEditor} savingCard={savingCard} setSavingCard={setSavingCard}
      retryingCard={retryingCard} setRetryingCard={setRetryingCard} redispatching={redispatching} setRedispatching={setRedispatching}
      redispatchReason={redispatchReason} setRedispatchReason={setRedispatchReason} retryAssigneeId={retryAssigneeId} setRetryAssigneeId={setRetryAssigneeId}
      actionError={actionError} setActionError={setActionError} auditLog={auditLog} ghComments={ghComments}
      reviewData={reviewData} setReviewData={() => clearCardReviews(card.id)} reviewDecisions={reviewDecisions} setReviewDecisions={setReviewDecisions}
      timelineFilter={timelineFilter} setTimelineFilter={setTimelineFilter}
      setCancelConfirm={() => setCancelling("confirm")}
      onClose={onClose}
      onUpdateCard={async (id, patch) => {
        // Ready needs an assignee, chosen first when the card has none.
        if (patch.status === "ready" && !card.assignee_agent_id) { onNeedsAssignee(card); return; }
        await actions.update(id, patch);
      }}
      onRetryCard={actions.retry} onRedispatchCard={actions.redispatch} onDeleteCard={actions.remove}
      invalidateCardActivity={invalidateCardActivity}
      extra={<QueueAddToCampaign key={card.id} card={card} campaigns={campaigns} tr={tr} onAdded={onCampaignSaved} />} />
    {cancelling && <Modal open onClose={() => { if (cancelling !== "sending") setCancelling(null); }} title={tr("카드를 취소할까요?", "Cancel this card?")} size="sm"
      description={card.github_issue_number
        ? tr(`카드를 완료로 닫고, 연결된 GitHub 이슈 #${card.github_issue_number}도 닫습니다.`, `Closes the card as done, together with its GitHub issue #${card.github_issue_number}.`)
        : tr("카드를 완료로 닫습니다.", "Closes the card as done.")}>
      <div className="flex justify-end gap-2 pt-2">
        <button type="button" disabled={cancelling === "sending"} onClick={() => setCancelling(null)} className="min-h-11 rounded-md border border-th-border-subtle px-4 text-sm text-th-text-secondary">{tr("돌아가기", "Go back")}</button>
        <button type="button" disabled={cancelling === "sending"} onClick={() => void cancel()} className="min-h-11 rounded-md px-4 text-sm font-medium text-white disabled:opacity-60" style={{ background: "var(--th-accent-danger)" }}>
          {cancelling === "sending" ? tr("처리 중…", "Working…") : tr("카드 취소", "Cancel card")}
        </button>
      </div>
    </Modal>}
  </>;
}
