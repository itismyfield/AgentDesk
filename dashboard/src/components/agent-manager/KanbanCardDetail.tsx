import type { ReactNode } from "react";
import { Repeat2, Send } from "lucide-react";
import { ActionMenu } from "../common/ActionMenu";
import { Select } from "../common/Select";
import { Drawer } from "../common/overlay/Drawer";
import type {
  CardAuditLogEntry,
  GitHubComment,
  KanbanReview,
} from "../../api";
import { Badge, type BadgeTone } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { TextField } from "../../kit/free/text-field-clean";
import { KANBAN_STATUS_TONES } from "../../theme/statusTokens";
import CardIssueContent from "./CardIssueContent";
import CardTimeline from "./CardTimeline";
import TurnTranscriptPanel from "./TurnTranscriptPanel";
import KanbanCardActivitySections from "./KanbanCardActivitySections";
import KanbanCardReviewPanel from "./KanbanCardReviewPanel";
import { KIT_TOUCH_BUTTON, KIT_TOUCH_FIELD } from "./CardDetailParts";
import { localeName } from "../../i18n";
import { SurfaceNotice } from "../common/SurfacePrimitives";
import type {
  Agent,
  KanbanCard,
  KanbanCardMetadata,
  KanbanCardPriority,
  KanbanCardStatus,
  TaskDispatch,
  UiLanguage,
} from "../../types";
import {
  PRIORITY_OPTIONS,
  STATUS_TRANSITIONS,
  buildGitHubIssueUrl,
  cardTitle,
  formatIso,
  labelForStatus,
  parseCardMetadata,
  priorityLabel,
  stringifyCardMetadata,
  type EditorState,
} from "./kanban-utils";

/** Badge tone for the card's own status; running states pulse. */
function cardStatusBadge(status: KanbanCardStatus): { tone: BadgeTone; live?: boolean } {
  switch (status) {
    case "ready":
    case "requested": return { tone: "info" };
    case "in_progress":
    case "qa_in_progress": return { tone: "info", live: true };
    case "review":
    case "qa_pending": return { tone: "purple" };
    case "blocked":
    case "qa_failed": return { tone: "danger" };
    case "done": return { tone: "success" };
    default: return { tone: "neutral" };
  }
}

export function canRetryCard(card: KanbanCard | null) {
  return Boolean(card && ["blocked", "requested", "in_progress"].includes(card.status));
}

export function canRedispatchCard(card: KanbanCard | null) {
  return Boolean(card && ["requested", "in_progress"].includes(card.status));
}

// ---------------------------------------------------------------------------
// Props
// ---------------------------------------------------------------------------

export interface KanbanCardDetailProps {
  card: KanbanCard;
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  agents: Agent[];
  dispatches: TaskDispatch[];

  // Editor state (lifted)
  editor: EditorState;
  setEditor: React.Dispatch<React.SetStateAction<EditorState>>;

  // Loading states
  savingCard: boolean;
  setSavingCard: React.Dispatch<React.SetStateAction<boolean>>;
  retryingCard: boolean;
  setRetryingCard: React.Dispatch<React.SetStateAction<boolean>>;
  redispatching: boolean;
  setRedispatching: React.Dispatch<React.SetStateAction<boolean>>;
  redispatchReason: string;
  setRedispatchReason: React.Dispatch<React.SetStateAction<string>>;
  retryAssigneeId: string;
  setRetryAssigneeId: React.Dispatch<React.SetStateAction<string>>;

  // Error
  actionError: string | null;
  setActionError: React.Dispatch<React.SetStateAction<string | null>>;

  // Activity data
  auditLog: CardAuditLogEntry[];
  ghComments: GitHubComment[];
  reviewData: KanbanReview | null;
  setReviewData: React.Dispatch<React.SetStateAction<KanbanReview | null>>;
  reviewDecisions: Record<string, "accept" | "reject">;
  setReviewDecisions: React.Dispatch<React.SetStateAction<Record<string, "accept" | "reject">>>;
  timelineFilter: "review" | "pm" | "work" | "general" | null;
  setTimelineFilter: React.Dispatch<React.SetStateAction<"review" | "pm" | "work" | "general" | null>>;

  // Cancel confirm
  setCancelConfirm: React.Dispatch<React.SetStateAction<{ cardIds: string[]; source: "bulk" | "single" } | null>>;

  // Callbacks
  onClose: () => void;
  onUpdateCard: (
    id: string,
    patch: Partial<KanbanCard> & { before_card_id?: string | null },
  ) => Promise<void>;
  onRetryCard: (
    id: string,
    payload?: { assignee_agent_id?: string | null; request_now?: boolean },
  ) => Promise<void>;
  onRedispatchCard: (
    id: string,
    payload?: { reason?: string | null },
  ) => Promise<void>;
  onDeleteCard: (id: string) => Promise<void>;
  invalidateCardActivity: (cardId: string) => void;
  /** Shown under the card heading, e.g. putting the card into a campaign. */
  extra?: ReactNode;
  /** Saving would send this body back, so it stays off while a newer version is unread. */
  saveLock?: { reason: string; onRetry?: () => void } | null;
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export default function KanbanCardDetail({
  card: selectedCard,
  tr,
  locale,
  agents,
  dispatches,
  editor,
  setEditor,
  savingCard,
  setSavingCard,
  retryingCard,
  setRetryingCard,
  redispatching,
  setRedispatching,
  redispatchReason,
  setRedispatchReason,
  retryAssigneeId,
  setRetryAssigneeId,
  actionError,
  setActionError,
  auditLog,
  ghComments,
  reviewData,
  setReviewData,
  reviewDecisions,
  setReviewDecisions,
  timelineFilter,
  setTimelineFilter,
  setCancelConfirm,
  onClose,
  onUpdateCard,
  onRetryCard,
  onRedispatchCard,
  onDeleteCard,
  invalidateCardActivity,
  extra,
  saveLock,
}: KanbanCardDetailProps) {

  const agentMap = new Map(agents.map((agent) => [agent.id, agent]));
  const githubIssueUrl = buildGitHubIssueUrl(
    selectedCard.github_repo,
    selectedCard.github_issue_number,
    selectedCard.github_issue_url,
  );

  const getAgentLabel = (agentId: string | null | undefined) => {
    if (!agentId) return tr("미할당", "Unassigned");
    const agent = agentMap.get(agentId);
    if (!agent) return agentId;
    return localeName(locale, agent);
  };

  const handleSaveCard = async () => {
    if (saveLock) return;
    setSavingCard(true);
    setActionError(null);
    try {
      const metadata = {
        ...parseCardMetadata(selectedCard.metadata_json),
        review_checklist: editor.review_checklist
          .map((item, index) => ({
            id: item.id || `check-${index}`,
            label: item.label.trim(),
            done: item.done,
          }))
          .filter((item) => item.label),
      } satisfies KanbanCardMetadata;

      await onUpdateCard(selectedCard.id, {
        title: editor.title.trim(),
        description: editor.description.trim() || null,
        assignee_agent_id: editor.assignee_agent_id || null,
        priority: editor.priority,
        metadata_json: stringifyCardMetadata(metadata),
      });
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("카드 저장에 실패했습니다.", "Failed to save card."));
    } finally {
      setSavingCard(false);
    }
  };

  const handleRetryCard = async () => {
    setRetryingCard(true);
    setActionError(null);
    try {
      await onRetryCard(selectedCard.id, {
        assignee_agent_id: retryAssigneeId || selectedCard.assignee_agent_id,
        request_now: true,
      });
      invalidateCardActivity(selectedCard.id);
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("재시도에 실패했습니다.", "Failed to retry card."));
    } finally {
      setRetryingCard(false);
    }
  };

  const handleDeleteCard = async () => {
    setSavingCard(true);
    setActionError(null);
    try {
      await onDeleteCard(selectedCard.id);
      onClose();
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("카드 삭제에 실패했습니다.", "Failed to delete card."));
    } finally {
      setSavingCard(false);
    }
  };

  const handleRedispatch = async () => {
    setRedispatching(true);
    setActionError(null);
    try {
      await onRedispatchCard(selectedCard.id, {
        reason: redispatchReason.trim() || null,
      });
      invalidateCardActivity(selectedCard.id);
      setRedispatchReason("");
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("재디스패치에 실패했습니다.", "Failed to redispatch."));
    }
    setRedispatching(false);
  };

  const transitionTargets = STATUS_TRANSITIONS[selectedCard.status] ?? [];
  const changeStatus = async (target: KanbanCardStatus) => {
    if (target === "done" && editor.review_checklist.some((item) => !item.done)) {
      setActionError(tr("review checklist를 모두 완료해야 done으로 이동할 수 있습니다.", "Complete the review checklist before moving to done."));
      return;
    }
    if (target === "backlog") {
      setCancelConfirm({ cardIds: [selectedCard.id], source: "single" });
      return;
    }
    setSavingCard(true);
    setActionError(null);
    try {
      await onUpdateCard(selectedCard.id, { status: target });
      invalidateCardActivity(selectedCard.id);
      setEditor((prev) => ({ ...prev, status: target }));
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("상태 전환에 실패했습니다.", "Failed to change status."));
    } finally {
      setSavingCard(false);
    }
  };
  const statusBadge = cardStatusBadge(selectedCard.status);
  const timestamps: Array<[string, string]> = [
    [tr("생성", "Created"), formatIso(selectedCard.created_at, locale)],
    [tr("요청", "Requested"), formatIso(selectedCard.requested_at, locale)],
    [tr("시작", "Started"), formatIso(selectedCard.started_at, locale)],
    [tr("완료", "Completed"), formatIso(selectedCard.completed_at, locale)],
  ];

  return (
    <Drawer open onClose={onClose} title={tr("카드 상세", "Card details")} ariaLabel={tr("카드 상세", "Card details")} width="min(820px, 100vw)" closeButtonClassName="min-h-11 min-w-11">
      <div data-testid="kanban-card-drawer" className="@container/card flex min-w-0 flex-col gap-6 pb-4">
        <header className="flex items-start justify-between gap-3">
          <div className="min-w-0 flex-1">
            <div className="flex flex-wrap items-center gap-x-3 gap-y-2 text-xs text-th-text-muted">
              <span className="inline-flex items-center gap-1.5">
                <span className="sr-only">{tr("상태", "Status")}</span>
                <Badge size="md" tone={statusBadge.tone} dot={!statusBadge.live} live={statusBadge.live}>{labelForStatus(selectedCard.status, tr)}</Badge>
              </span>
              <span>{tr("우선순위", "Priority")} <strong className="ml-1 font-medium text-th-text-primary">{priorityLabel(selectedCard.priority, tr)}</strong></span>
              {selectedCard.github_repo ? <span className="[overflow-wrap:anywhere]">{selectedCard.github_repo}</span> : null}
            </div>
            <h3 className="m-0 mt-3 text-xl font-semibold leading-7 text-th-text-heading [overflow-wrap:anywhere]">
              {selectedCard.github_issue_number ? <span className="mr-2 text-base font-normal tabular-nums text-th-text-muted">#{selectedCard.github_issue_number}</span> : null}
              {cardTitle(selectedCard.title)}
            </h3>
          </div>
          <ActionMenu
            label={tr("카드 메뉴", "Card menu")}
            items={[
              ...(selectedCard.status !== "done" ? [{
                key: "cancel",
                label: tr("카드 취소", "Cancel card"),
                disabled: savingCard,
                onSelect: () => setCancelConfirm({ cardIds: [selectedCard.id], source: "single" }),
              }] : []),
              {
                key: "delete",
                label: tr("카드 삭제", "Delete card"),
                danger: true,
                disabled: savingCard,
                onSelect: handleDeleteCard,
                confirm: {
                  title: tr("이 카드를 삭제할까요?", "Delete this card?"),
                  description: tr("카드가 대시보드에서 사라집니다. GitHub 이슈는 그대로 남습니다.", "The card leaves the dashboard. The GitHub issue stays."),
                  confirmLabel: tr("삭제", "Delete"),
                  cancelLabel: tr("취소", "Cancel"),
                },
              },
            ]}
          />
        </header>

        {extra}

        {actionError && (
          <SurfaceNotice tone="danger">
            {actionError}
          </SurfaceNotice>
        )}

        {selectedCard.blocked_reason && (
          <section role="note" className="flex min-w-0 flex-col gap-1.5 rounded-xl bg-[color-mix(in_oklch,var(--err)_12%,transparent)] px-4 py-3 shadow-[inset_0_0_0_0.5px_color-mix(in_oklch,var(--err)_40%,transparent)]">
            <Badge tone="danger" dot className="self-start">{tr("수동 개입 사유", "Manual Intervention Reason")}</Badge>
            <p className="m-0 whitespace-pre-wrap text-sm leading-5 text-th-text-primary [overflow-wrap:anywhere]">{selectedCard.blocked_reason}</p>
          </section>
        )}

        <KanbanCardReviewPanel
          card={selectedCard}
          dispatches={dispatches}
          reviewData={reviewData}
          setReviewData={setReviewData}
          reviewDecisions={reviewDecisions}
          setReviewDecisions={setReviewDecisions}
          setActionError={setActionError}
          tr={tr}
        />

        <section className="flex min-w-0 flex-col gap-3" aria-label={tr("카드 편집", "Edit card")}>
          <div className="grid gap-3 @min-[40rem]/card:grid-cols-2">
            <TextField
              label={tr("제목", "Title")}
              size="lg"
              className={KIT_TOUCH_FIELD}
              value={editor.title}
              onChange={(event) => setEditor((prev) => ({ ...prev, title: event.target.value }))}
            />
            <div className="flex min-w-0 flex-col gap-1.5">
              <span className="text-[13px] font-medium leading-5 text-th-text-primary">{tr("상태 전환", "Status")}</span>
              <div className="flex flex-wrap gap-1.5">
                {transitionTargets.map((target) => (
                  <Button
                    key={target}
                    variant="secondary"
                    size="lg"
                    className={KIT_TOUCH_BUTTON}
                    disabled={savingCard}
                    icon={<i aria-hidden="true" className="block size-2 rounded-full" style={{ background: KANBAN_STATUS_TONES[target as keyof typeof KANBAN_STATUS_TONES]?.accent ?? "var(--th-text-muted)" }} />}
                    onClick={() => void changeStatus(target)}
                  >
                    {`→ ${labelForStatus(target, tr)}`}
                  </Button>
                ))}
              </div>
            </div>
          </div>
          <div className="grid gap-3 @min-[40rem]/card:grid-cols-2 @min-[48rem]/card:grid-cols-3">
            <label className="flex min-w-0 flex-col gap-1.5">
              <span className="text-[13px] font-medium leading-5 text-th-text-primary">{tr("담당자", "Assignee")}</span>
              <Select
                value={editor.assignee_agent_id}
                onChange={(event) => setEditor((prev) => ({ ...prev, assignee_agent_id: event.target.value }))}
              >
                <option value="">{tr("없음", "None")}</option>
                {agents.map((agent) => (
                  <option key={agent.id} value={agent.id}>{getAgentLabel(agent.id)}</option>
                ))}
              </Select>
            </label>
            <label className="flex min-w-0 flex-col gap-1.5">
              <span className="text-[13px] font-medium leading-5 text-th-text-primary">{tr("우선순위", "Priority")}</span>
              <Select
                value={editor.priority}
                onChange={(event) => setEditor((prev) => ({ ...prev, priority: event.target.value as KanbanCardPriority }))}
              >
                {PRIORITY_OPTIONS.map((priority) => (
                  <option key={priority} value={priority}>{priorityLabel(priority, tr)}</option>
                ))}
              </Select>
            </label>
            <div className="flex min-w-0 flex-col gap-1.5">
              <span className="text-[13px] font-medium leading-5 text-th-text-primary">GitHub</span>
              <div className="flex min-h-11 items-center rounded-lg bg-surface-subtle px-3 text-sm text-th-text-primary shadow-[inset_0_0_0_0.5px_var(--th-border-subtle)]">
                {githubIssueUrl ? (
                  <a href={githubIssueUrl} target="_blank" rel="noopener noreferrer" className="inline-flex min-h-11 min-w-11 items-center tabular-nums text-[var(--info)] hover:underline">
                    #{selectedCard.github_issue_number ?? "-"}
                  </a>
                ) : (
                  selectedCard.github_issue_number ? `#${selectedCard.github_issue_number}` : "-"
                )}
              </div>
            </div>
          </div>
        </section>

        <CardIssueContent
          card={selectedCard}
          editor={editor}
          setEditor={setEditor}
          tr={tr}
        />

        <div className="min-w-0 rounded-2xl bg-th-card-bg shadow-[inset_0_0_0_0.5px_var(--th-border-subtle)]">
          <TurnTranscriptPanel
            source={{
              type: "card",
              id: selectedCard.id,
              refreshSeed: `${selectedCard.latest_dispatch_id ?? ""}:${selectedCard.updated_at}`,
            }}
            tr={tr}
            isKo={locale === "ko"}
            title={tr("연결된 턴 트랜스크립트", "Linked Turn Transcript")}
          />
        </div>

        {canRedispatchCard(selectedCard) && (
          <section className="flex min-w-0 flex-col gap-2.5" aria-labelledby="kanban-card-redispatch">
            <div>
              <h4 id="kanban-card-redispatch" className="m-0 text-sm font-medium text-th-text-heading">
                {tr("이슈 변경 후 재전송", "Resend with Updated Issue")}
              </h4>
              <p className="m-0 mt-0.5 text-xs text-th-text-muted [word-break:keep-all]">
                {tr(
                  "이슈 본문을 수정한 뒤, 기존 dispatch를 취소하고 새로 전송합니다.",
                  "Cancel current dispatch and resend with the updated issue body.",
                )}
              </p>
            </div>
            <div className="grid gap-2.5 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-end">
              <TextField
                size="lg"
                className={KIT_TOUCH_FIELD}
                placeholder={tr("사유 (선택)", "Reason (optional)")}
                aria-label={tr("재전송 사유 (선택)", "Resend reason (optional)")}
                value={redispatchReason}
                onChange={(e) => setRedispatchReason(e.target.value)}
              />
              <Button variant="secondary" size="lg" className={KIT_TOUCH_BUTTON} icon={<Send />} disabled={redispatching} onClick={() => void handleRedispatch()}>
                {redispatching ? tr("전송 중...", "Sending...") : tr("재전송", "Resend")}
              </Button>
            </div>
          </section>
        )}

        {canRetryCard(selectedCard) && (
          <section className="flex min-w-0 flex-col gap-2.5" aria-labelledby="kanban-card-retry">
            <div>
              <h4 id="kanban-card-retry" className="m-0 text-sm font-medium text-th-text-heading">
                {tr("재시도 / 담당자 변경", "Retry / Change Assignee")}
              </h4>
              <p className="m-0 mt-0.5 text-xs text-th-text-muted [word-break:keep-all]">
                {tr("동일 내용으로 재전송하거나 다른 에이전트에게 전환합니다.", "Resend as-is or switch to another agent.")}
              </p>
            </div>
            <div className="grid gap-2.5 sm:grid-cols-[minmax(0,1fr)_auto]">
              <Select
                aria-label={tr("재시도 담당자", "Retry assignee")}
                value={retryAssigneeId}
                onChange={(event) => setRetryAssigneeId(event.target.value)}
              >
                {agents.map((agent) => (
                  <option key={agent.id} value={agent.id}>{getAgentLabel(agent.id)}</option>
                ))}
              </Select>
              <Button
                variant="secondary"
                size="lg"
                className={KIT_TOUCH_BUTTON}
                icon={<Repeat2 />}
                onClick={() => void handleRetryCard()}
                disabled={retryingCard || !(retryAssigneeId || selectedCard.assignee_agent_id)}
              >
                {retryingCard ? tr("전송 중...", "Sending...") : tr("재시도", "Retry")}
              </Button>
            </div>
          </section>
        )}

        <dl className="m-0 grid grid-cols-2 gap-2.5 @min-[40rem]/card:grid-cols-4" aria-label={tr("카드 시각", "Card times")}>
          {timestamps.map(([label, value]) => (
            <div key={label} className="flex min-w-0 flex-col gap-0.5 rounded-xl bg-surface-subtle px-3.5 py-3 shadow-[inset_0_0_0_0.5px_var(--th-border-subtle)]">
              <dt className="truncate text-xs leading-4 text-th-text-muted">{label}</dt>
              <dd className="m-0 truncate text-[13px] font-medium leading-5 text-th-text-primary" title={value}>{value}</dd>
            </div>
          ))}
        </dl>

        <KanbanCardActivitySections
          card={selectedCard}
          dispatches={dispatches}
          auditLog={auditLog}
          tr={tr}
          locale={locale}
          getAgentLabel={getAgentLabel}
        />

        <CardTimeline
          ghComments={ghComments}
          timelineFilter={timelineFilter}
          setTimelineFilter={setTimelineFilter}
          tr={tr}
          locale={locale}
          onRefresh={() => invalidateCardActivity(selectedCard.id)}
          card={selectedCard}
          dispatches={dispatches}
          auditLog={auditLog}
          getAgentLabel={getAgentLabel}
        />

        <footer className="flex flex-col gap-3 border-t border-th-border-subtle pt-4 sm:flex-row sm:items-center sm:justify-end">
          {saveLock && (
            <div role="status" className="flex min-w-0 flex-wrap items-center gap-2 text-xs leading-5 text-th-text-secondary [word-break:keep-all]" data-testid="card-save-lock">
              <span>{saveLock.reason}</span>
              {saveLock.onRetry && (
                <Button variant="secondary" size="lg" className={KIT_TOUCH_BUTTON} onClick={saveLock.onRetry}>
                  {tr("다시 읽기", "Load again")}
                </Button>
              )}
            </div>
          )}
          <div className="flex flex-col-reverse gap-2 sm:flex-row">
            <Button variant="secondary" size="lg" className={KIT_TOUCH_BUTTON} onClick={onClose}>
              {tr("닫기", "Close")}
            </Button>
            <Button
              variant="primary"
              size="lg"
              className={KIT_TOUCH_BUTTON}
              onClick={() => void handleSaveCard()}
              disabled={savingCard || !editor.title.trim() || !!saveLock}
            >
              {savingCard ? tr("저장 중", "Saving") : tr("저장", "Save")}
            </Button>
          </div>
        </footer>
      </div>
    </Drawer>
  );
}
