import { SurfaceActionButton, SurfaceCard, SurfaceNotice } from "../common/SurfacePrimitives";

interface KanbanAssignIssueModalProps {
  ctx: any;
}

export default function KanbanAssignIssueModal({ ctx }: KanbanAssignIssueModalProps) {
  const {
    agents,
    assignAssigneeId,
    assignIssue,
    assigningIssue,
    getAgentLabel,
    handleAssignIssue,
    selectedRepo,
    setAssignAssigneeId,
    setAssignIssue,
    SURFACE_CHIP_STYLE,
    SURFACE_FIELD_STYLE,
    SURFACE_GHOST_BUTTON_STYLE,
    SURFACE_MODAL_CARD_STYLE,
    SURFACE_PANEL_STYLE,
    tr,
  } = ctx;

  return (
    <>
      {assignIssue && (
        <div className="fixed inset-0 z-50 flex items-end justify-center sm:items-center p-0 sm:p-4" style={{ backgroundColor: "var(--th-modal-overlay)" }}>
          <SurfaceCard
            className="w-full max-w-lg rounded-t-3xl p-5 sm:rounded-3xl sm:p-6 space-y-4"
            style={{
              ...SURFACE_MODAL_CARD_STYLE,
              paddingBottom: "max(6rem, calc(6rem + env(safe-area-inset-bottom)))",
            }}
          >
            <div className="flex items-start justify-between gap-3">
              <div className="min-w-0 space-y-2">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="rounded-full border px-2 py-0.5 text-xs" style={{ ...SURFACE_CHIP_STYLE, color: "var(--th-text-secondary)" }}>
                    {selectedRepo}
                  </span>
                  <span className="rounded-full border px-2 py-0.5 text-xs" style={{ ...SURFACE_CHIP_STYLE, color: "var(--th-text-secondary)" }}>
                    #{assignIssue.number}
                  </span>
                </div>
                <h3 className="text-lg font-semibold" style={{ color: "var(--th-text-heading)" }}>
                  {assignIssue.title}
                </h3>
              </div>
              <SurfaceActionButton
                onClick={() => setAssignIssue(null)}
                tone="neutral"
                className="shrink-0 whitespace-nowrap"
                style={{ ...SURFACE_GHOST_BUTTON_STYLE, color: "var(--th-text-secondary)" }}
              >
                {tr("닫기", "Close")}
              </SurfaceActionButton>
            </div>

            <SurfaceNotice tone="info" compact>
              {tr("할당 시 카드는 ready로 생성되며, 저장된 repo 기본 담당자가 있으면 미리 선택됩니다.", "Assigning creates a ready card and preselects the repo default assignee when available.")}
            </SurfaceNotice>

            <SurfaceCard className="space-y-2 p-4" style={{ ...SURFACE_PANEL_STYLE }}>
              <label className="space-y-2 block">
                <span className="text-xs font-medium" style={{ color: "var(--th-text-muted)" }}>{tr("담당자", "Assignee")}</span>
                <select
                  value={assignAssigneeId}
                  onChange={(event) => setAssignAssigneeId(event.target.value)}
                  className="w-full rounded-xl border px-3 py-2 text-sm"
                  style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
                >
                  <option value="">{tr("에이전트 선택", "Select an agent")}</option>
                  {agents.map((agent: any) => (
                    <option key={agent.id} value={agent.id}>{getAgentLabel(agent.id)}</option>
                  ))}
                </select>
              </label>
            </SurfaceCard>

            <div className="flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
              <SurfaceActionButton
                onClick={() => setAssignIssue(null)}
                tone="neutral"
                className="px-4 py-2 text-sm"
                style={{ ...SURFACE_GHOST_BUTTON_STYLE, color: "var(--th-text-secondary)" }}
              >
                {tr("취소", "Cancel")}
              </SurfaceActionButton>
              <SurfaceActionButton
                onClick={() => void handleAssignIssue()}
                disabled={assigningIssue || !assignAssigneeId}
                tone="accent"
                className="px-4 py-2 text-sm"
                style={{ backgroundColor: "#2563eb", borderColor: "#2563eb", color: "white" }}
              >
                {assigningIssue ? tr("할당 중", "Assigning") : tr("ready로 할당", "Assign to ready")}
              </SurfaceActionButton>
            </div>
          </SurfaceCard>
        </div>
      )}
    </>
  );
}
