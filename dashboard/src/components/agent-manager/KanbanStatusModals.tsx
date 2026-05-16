import { SurfaceActionButton, SurfaceCard } from "../common/SurfacePrimitives";

interface KanbanStatusModalsProps {
  ctx: any;
}

export default function KanbanStatusModals({ ctx }: KanbanStatusModalsProps) {
  const {
    agents,
    assignBeforeReady,
    cancelBusy,
    cancelConfirm,
    cardsById,
    executeBulkCancel,
    invalidateCardActivity,
    onUpdateCard,
    setActionError,
    setAssignBeforeReady,
    setCancelConfirm,
    SURFACE_FIELD_STYLE,
    tr,
  } = ctx;

  return (
    <>
      {/* Assignee selection modal: shown when moving to "ready" without an assignee.
          Rendered outside the collapsible header so a card-status transition that
          fires `setAssignBeforeReady(...)` still surfaces the modal even when the
          header is collapsed. */}
      {assignBeforeReady && (
        <div className="fixed inset-0 z-50 flex items-center justify-center p-4" style={{ backgroundColor: "var(--th-modal-overlay)" }} onClick={() => setAssignBeforeReady(null)}>
          <SurfaceCard
            onClick={(e) => e.stopPropagation()}
            className="w-full max-w-sm space-y-4 rounded-[28px] p-5"
            style={{
              background: "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
              borderColor: "color-mix(in srgb, var(--th-accent-info) 18%, var(--th-border) 82%)",
            }}
          >
            <h3 className="text-lg font-semibold" style={{ color: "var(--th-text-heading)" }}>{tr("담당자 할당", "Assign Agent")}</h3>
            <p className="text-sm" style={{ color: "var(--th-text-secondary)" }}>{tr("준비됨 상태로 이동하려면 담당자를 지정해야 합니다.", "Assign an agent before moving to ready.")}</p>
            <select
              value={assignBeforeReady.agentId}
              onChange={(e) => setAssignBeforeReady((prev: any) => prev ? { ...prev, agentId: e.target.value } : null)}
              className="w-full rounded-xl border px-3 py-2 text-sm"
              style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
            >
              <option value="">{tr("선택...", "Select...")}</option>
              {agents.map((a: any) => (
                <option key={a.id} value={a.id}>{a.name_ko || a.name} ({a.id})</option>
              ))}
            </select>
            <div className="flex justify-end gap-2">
              <SurfaceActionButton
                onClick={() => setAssignBeforeReady(null)}
                tone="neutral"
              >
                {tr("취소", "Cancel")}
              </SurfaceActionButton>
              <SurfaceActionButton
                disabled={!assignBeforeReady.agentId}
                tone="success"
                onClick={async () => {
                  const { cardId, agentId } = assignBeforeReady;
                  setAssignBeforeReady(null);
                  try {
                    await onUpdateCard(cardId, { assignee_agent_id: agentId });
                    await onUpdateCard(cardId, { status: "ready" });
                    invalidateCardActivity(cardId);
                  } catch (error) {
                    setActionError(error instanceof Error ? error.message : tr("상태 전환에 실패했습니다.", "Failed to change status."));
                  }
                }}
              >
                {tr("할당 후 준비됨", "Assign & Ready")}
              </SurfaceActionButton>
            </div>
          </SurfaceCard>
        </div>
      )}

      {/* Cancel confirmation modal — ask whether to also close GitHub issues */}
      {cancelConfirm && (() => {
        const ghCards = cancelConfirm.cardIds
          .map((id: string) => cardsById.get(id))
          .filter((c: any): c is any => !!(c?.github_repo && c.github_issue_number));
        return (
          <div className="fixed inset-0 z-50 flex items-center justify-center p-4" style={{ backgroundColor: "var(--th-modal-overlay)" }}>
            <SurfaceCard
              onClick={(e) => e.stopPropagation()}
              className="w-full max-w-md space-y-4 rounded-[28px] p-5"
              style={{
                background: "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
                borderColor: "color-mix(in srgb, var(--th-accent-danger) 18%, var(--th-border) 82%)",
              }}
            >
              <h3 className="text-base font-semibold" style={{ color: "var(--th-text-heading)" }}>
                {tr("카드 취소 확인", "Cancel cards")}
              </h3>
              <p className="text-sm" style={{ color: "var(--th-text-secondary)" }}>
                {tr(
                  `${cancelConfirm.cardIds.length}건의 카드를 취소합니다.`,
                  `Cancel ${cancelConfirm.cardIds.length} card(s).`,
                )}
              </p>
              {ghCards.length > 0 && (
                <div className="space-y-2">
                  <p className="text-sm" style={{ color: "var(--th-text-secondary)" }}>
                    {tr(
                      `GitHub 이슈가 연결된 카드 ${ghCards.length}건:`,
                      `${ghCards.length} card(s) linked to GitHub issues:`,
                    )}
                  </p>
                  <ul className="text-xs space-y-1 pl-2" style={{ color: "var(--th-text-muted)" }}>
                    {ghCards.map((c: any) => (
                      <li key={c.id}>
                        #{c.github_issue_number} — {c.title}
                      </li>
                    ))}
                  </ul>
                  <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                    {tr(
                      "※ GitHub 이슈는 카드 완료 시 자동으로 닫힙니다.",
                      "※ GitHub issues are automatically closed when the card is completed.",
                    )}
                  </p>
                </div>
              )}
              <div className="flex justify-end gap-2 pt-2">
                <SurfaceActionButton
                  onClick={() => setCancelConfirm(null)}
                  disabled={cancelBusy}
                  tone="neutral"
                >
                  {tr("돌아가기", "Go back")}
                </SurfaceActionButton>
                <SurfaceActionButton
                  onClick={() => void executeBulkCancel()}
                  disabled={cancelBusy}
                  tone="danger"
                >
                  {cancelBusy ? tr("처리 중…", "Processing…") : tr("취소 확정", "Confirm cancel")}
                </SurfaceActionButton>
              </div>
            </SurfaceCard>
          </div>
        );
      })()}
    </>
  );
}
