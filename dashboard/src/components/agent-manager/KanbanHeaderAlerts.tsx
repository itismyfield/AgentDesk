import { SurfaceActionButton, SurfaceCard } from "../common/SurfacePrimitives";
import { parseCardMetadata } from "./kanban-utils";

interface KanbanHeaderAlertsProps {
  ctx: any;
}

export default function KanbanHeaderAlerts({ ctx }: KanbanHeaderAlertsProps) {
  const {
    bulkBusy,
    cards,
    deferredDodPopup,
    handleBulkAction,
    onPatchDeferDod,
    setActionError,
    setDeferredDodPopup,
    setStalledPopup,
    setStalledSelected,
    setVerifyingDeferredDodIds,
    stalledCards,
    stalledPopup,
    stalledSelected,
    tr,
    verifyingDeferredDodIds,
  } = ctx;

  return (
    <>
        {deferredDodPopup && (() => {
          const deferredItems = cards.flatMap((c: any) => {
            const meta = parseCardMetadata(c.metadata_json);
            return (meta.deferred_dod ?? []).map((d: any) => ({ ...d, cardId: c.id, cardTitle: c.title, issueNumber: c.github_issue_number }));
          }).filter((d: any) => !d.verified);
          return (
            <SurfaceCard
              className="space-y-3 rounded-[24px] p-4"
              style={{
                borderColor: "color-mix(in srgb, var(--th-accent-warn) 32%, var(--th-border) 68%)",
                background: "color-mix(in srgb, var(--th-badge-amber-bg) 72%, var(--th-card-bg) 28%)",
              }}
            >
              <div className="flex items-center justify-between">
                <span className="text-sm font-semibold" style={{ color: "#fbbf24" }}>
                  {tr(`미검증 DoD (${deferredItems.length}건)`, `Deferred DoD (${deferredItems.length})`)}
                </span>
                <SurfaceActionButton tone="warn" compact onClick={() => setDeferredDodPopup(false)}>
                  {tr("닫기", "Close")}
                </SurfaceActionButton>
              </div>
              {deferredItems.length === 0 ? (
                <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("미검증 항목 없음", "No deferred items")}</p>
              ) : (
                <div className="space-y-2 max-h-60 overflow-y-auto">
                  {deferredItems.map((item: any) => (
                    <label key={item.id} className="flex items-start gap-2 text-xs cursor-pointer">
                      <input
                        type="checkbox"
                        checked={verifyingDeferredDodIds.has(item.id)}
                        disabled={verifyingDeferredDodIds.has(item.id)}
                        onChange={async () => {
                          setActionError(null);
                          setVerifyingDeferredDodIds((prev: Set<string>) => {
                            const next = new Set(prev);
                            next.add(item.id);
                            return next;
                          });
                          try {
                            await onPatchDeferDod(item.cardId, { verify: item.id });
                          } catch (error) {
                            setActionError(error instanceof Error ? error.message : tr("DoD 검증에 실패했습니다.", "Failed to verify deferred DoD."));
                          } finally {
                            setVerifyingDeferredDodIds((prev: Set<string>) => {
                              const next = new Set(prev);
                              next.delete(item.id);
                              return next;
                            });
                          }
                        }}
                        className="mt-0.5"
                      />
                      <span style={{ color: "var(--th-text-primary)" }}>
                        {item.issueNumber ? `#${item.issueNumber} ` : ""}{item.label}
                        <span className="ml-1" style={{ color: "var(--th-text-muted)" }}>({item.cardTitle})</span>
                      </span>
                    </label>
                  ))}
                </div>
              )}
            </SurfaceCard>
          );
        })()}

        {stalledPopup && (
          <SurfaceCard
            className="space-y-3 rounded-[24px] p-4"
            style={{
              borderColor: "color-mix(in srgb, var(--th-accent-danger) 32%, var(--th-border) 68%)",
              background: "color-mix(in srgb, rgba(255, 107, 107, 0.18) 76%, var(--th-card-bg) 24%)",
            }}
          >
            <div className="flex items-center justify-between">
              <h3 className="text-sm font-semibold" style={{ color: "#fca5a5" }}>
                {tr(`정체 카드 ${stalledCards.length}건`, `${stalledCards.length} Stalled Cards`)}
              </h3>
              <div className="flex gap-2">
                <SurfaceActionButton
                  onClick={() => setStalledSelected(stalledSelected.size === stalledCards.length ? new Set() : new Set(stalledCards.map((c: any) => c.id)))}
                  tone="neutral"
                  compact
                >
                  {stalledSelected.size === stalledCards.length ? tr("해제", "Deselect") : tr("전체 선택", "Select all")}
                </SurfaceActionButton>
                <SurfaceActionButton
                  onClick={() => setStalledPopup(false)}
                  tone="neutral"
                  compact
                >
                  {tr("닫기", "Close")}
                </SurfaceActionButton>
              </div>
            </div>
            <div className="space-y-1 max-h-60 overflow-y-auto">
              {stalledCards.map((card: any) => (
                <label key={card.id} className="flex cursor-pointer items-center gap-2 rounded-lg px-2 py-1.5 text-sm transition-opacity hover:opacity-90" style={{ color: "var(--th-text-primary)" }}>
                  <input
                    type="checkbox"
                    checked={stalledSelected.has(card.id)}
                    onChange={() => {
                      setStalledSelected((prev: Set<string>) => {
                        const next = new Set(prev);
                        next.has(card.id) ? next.delete(card.id) : next.add(card.id);
                        return next;
                      });
                    }}
                    className="accent-red-400"
                  />
                  <span className="truncate flex-1">{card.title}</span>
                  <span className="text-[10px] px-1.5 py-0.5 rounded-full shrink-0" style={{ backgroundColor: "rgba(239,68,68,0.15)", color: "#f87171" }}>
                    {card.review_status}
                  </span>
                  <span className="text-[10px] shrink-0" style={{ color: "var(--th-text-muted)" }}>
                    {card.github_repo ? card.github_repo.split("/")[1] : ""}
                  </span>
                </label>
              ))}
            </div>
            {stalledSelected.size > 0 && (
              <div className="flex gap-2 pt-1">
                <SurfaceActionButton
                  onClick={() => void handleBulkAction("pass")}
                  disabled={bulkBusy}
                  tone="success"
                  compact
                >
                  {bulkBusy ? "…" : tr(`일괄 Pass (${stalledSelected.size})`, `Pass All (${stalledSelected.size})`)}
                </SurfaceActionButton>
                <SurfaceActionButton
                  onClick={() => void handleBulkAction("reset")}
                  disabled={bulkBusy}
                  tone="info"
                  compact
                >
                  {bulkBusy ? "…" : tr(`일괄 Reset (${stalledSelected.size})`, `Reset All (${stalledSelected.size})`)}
                </SurfaceActionButton>
                <SurfaceActionButton
                  onClick={() => void handleBulkAction("cancel")}
                  disabled={bulkBusy}
                  tone="danger"
                  compact
                >
                  {bulkBusy ? "…" : tr(`일괄 Cancel (${stalledSelected.size})`, `Cancel All (${stalledSelected.size})`)}
                </SurfaceActionButton>
              </div>
            )}
          </SurfaceCard>
        )}
    </>
  );
}
