import { AUTOQUEUE_RUN_STATUS_TONES } from "../../theme/statusTokens";
import { buildRequestGenerateGroups } from "./auto-queue-actions";
import { formatTs } from "./auto-queue-panel-utils";

export default function AutoQueuePanelHeader({ ctx }: { ctx: any }) {
  const {
    activating,
    completedCount,
    dispatchedCount,
    doneCount,
    error,
    expanded,
    failedCount,
    generating,
    handleActivate,
    handleFallbackActivate,
    handleGenerate,
    handleReset,
    handleRunAction,
    locale,
    primaryAction,
    readyEntries,
    requestProgress,
    run,
    selectedRepo,
    setExpanded,
    showRunStartControls,
    skippedCount,
    startActionLabel,
    totalCount,
    tr,
  } = ctx;
  const runTone = run
    ? AUTOQUEUE_RUN_STATUS_TONES[run.status as keyof typeof AUTOQUEUE_RUN_STATUS_TONES]
      ?? AUTOQUEUE_RUN_STATUS_TONES.pending
    : null;

  return (
    <>
      <div className="flex items-center justify-between gap-2 min-w-0">
        <button
          onClick={() => setExpanded((p: boolean) => !p)}
          className="flex items-center gap-1.5 min-w-0 flex-1"
        >
          <span className="text-sm shrink-0" style={{ color: "var(--th-text-muted)" }}>
            {expanded ? "▾" : "▸"}
          </span>
          <h3
            className="text-sm font-semibold shrink-0"
            style={{ color: "var(--th-text-heading)" }}
          >
            {tr("자동 큐", "Auto Queue")}
          </h3>
          {run && runTone && (
            <span
              className="text-[11px] px-1.5 py-0.5 rounded-full shrink-0"
              style={{
                backgroundColor: runTone.bg,
                color: runTone.text,
              }}
            >
              {tr(runTone.label, runTone.labelEn)}
            </span>
          )}
          {totalCount > 0 && (
            <span
              className="text-[11px] px-1.5 py-0.5 rounded bg-surface-medium shrink-0"
              style={{ color: "var(--th-text-muted)" }}
            >
              {completedCount}/{totalCount}
            </span>
          )}
        </button>

        <div className="flex items-center gap-1.5 shrink-0">
          {showRunStartControls && (
            <button
              onClick={() => void handleActivate()}
              disabled={activating}
              className="text-xs px-2.5 py-1 rounded-lg border font-medium"
              style={{
                borderColor: "rgba(245,158,11,0.4)",
                color: "#fbbf24",
                backgroundColor: "rgba(245,158,11,0.1)",
              }}
            >
              {activating ? "..." : startActionLabel}
            </button>
          )}
          {primaryAction === "generate" && (() => {
            const eligibleGroupCount = buildRequestGenerateGroups(readyEntries, selectedRepo).length;
            const disabledByReady = eligibleGroupCount === 0;
            const disabled = generating || disabledByReady || Boolean(requestProgress);
            const pendingCountDisplay = requestProgress?.pendingGroups.size ?? 0;
            return (
              <button
                onClick={() => void handleGenerate()}
                disabled={disabled}
                className="text-xs px-2.5 py-1 rounded-lg border font-medium"
                style={{
                  borderColor: disabled
                    ? "rgba(148,163,184,0.2)"
                    : "rgba(16,185,129,0.4)",
                  color: disabled ? "var(--th-text-muted)" : "#10b981",
                  backgroundColor: disabled
                    ? "rgba(148,163,184,0.05)"
                    : "rgba(16,185,129,0.1)",
                  cursor: disabled ? "not-allowed" : undefined,
                }}
                title={
                  disabledByReady
                    ? tr(
                        "준비됨 카드가 없습니다 (assignee + GitHub 이슈 필요)",
                        "No ready cards available (need assignee + GitHub issue)",
                      )
                    : requestProgress
                      ? tr(
                          `${pendingCountDisplay}개 큐 그룹 응답 대기 중`,
                          `Waiting on ${pendingCountDisplay} queue group(s)`,
                        )
                      : tr(
                          `${eligibleGroupCount}개 큐 생성 요청`,
                          `Request ${eligibleGroupCount} queue group(s)`,
                        )
                }
              >
                {requestProgress
                  ? tr(`요청 중... (${pendingCountDisplay})`, `Requesting... (${pendingCountDisplay})`)
                  : generating
                    ? tr("요청 전송 중...", "Dispatching...")
                    : tr("큐 생성", "Generate")}
              </button>
            );
          })()}
          {run && (
            <button
              onClick={() => void handleReset()}
              className="text-[11px] px-2 py-1 rounded-lg border"
              style={{
                borderColor: "rgba(248,113,113,0.3)",
                color: "#f87171",
                backgroundColor: "rgba(248,113,113,0.08)",
              }}
            >
              {tr("초기화", "Reset")}
            </button>
          )}
          {run?.status === "active" && (
            <button
              onClick={() => void handleRunAction(run, "paused")}
              className="text-xs px-2 py-1 rounded-lg border"
              style={{
                borderColor: "rgba(148,163,184,0.22)",
                color: "var(--th-text-muted)",
              }}
            >
              {tr("일시정지", "Pause")}
            </button>
          )}
          {run?.status === "paused" && (
            <button
              onClick={() => void handleRunAction(run, "active")}
              className="text-xs px-2 py-1 rounded-lg border"
              style={{ borderColor: "rgba(16,185,129,0.3)", color: "#10b981" }}
            >
              {tr("재개", "Resume")}
            </button>
          )}
        </div>
      </div>

      {error && (
        <div
          className="rounded-lg px-3 py-2 text-xs border"
          style={{
            borderColor: "rgba(248,113,113,0.4)",
            color: "#fecaca",
            backgroundColor: "rgba(127,29,29,0.2)",
          }}
        >
          {error}
        </div>
      )}

      {run?.ai_rationale && (
        <div
          className="rounded-lg px-3 py-2 text-[11px] border"
          style={{
            borderColor: "rgba(96,165,250,0.22)",
            color: "var(--th-text-secondary)",
            backgroundColor: "rgba(30,41,59,0.45)",
          }}
        >
          {run.ai_rationale}
        </div>
      )}

      {run?.status === "pending" && expanded && (
        <div
          className="rounded-xl p-3 space-y-2 border"
          style={{
            borderColor: "rgba(56,189,248,0.25)",
            backgroundColor: "rgba(56,189,248,0.06)",
          }}
        >
          <div className="flex items-center gap-2">
            <span className="animate-pulse text-lg">...</span>
            <span className="text-sm font-medium" style={{ color: "#7dd3fc" }}>
              {tr("PMD 순서 분석 대기 중", "Awaiting PMD order analysis")}
            </span>
          </div>
          <div
            className="text-xs space-y-1"
            style={{ color: "var(--th-text-muted)" }}
          >
            <div>
              {tr("요청 시각", "Requested")}:{" "}
              {run.created_at ? formatTs(run.created_at, locale) : "-"}
            </div>
            {run.repo && (
              <div>
                {tr("대상 레포", "Target repo")}: {run.repo}
              </div>
            )}
            <div>
              {tr(
                "PMD가 순서를 결정하면 자동으로 활성화됩니다.",
                "Queue will activate automatically when PMD submits the order.",
              )}
            </div>
          </div>
          {run.ai_rationale && (
            <div
              className="text-xs italic"
              style={{ color: "var(--th-text-muted)" }}
            >
              {run.ai_rationale}
            </div>
          )}
          <button
            onClick={() => void handleFallbackActivate(run.id)}
            disabled={activating}
            className="text-xs px-2.5 py-1 rounded-lg border font-medium"
            style={{
              borderColor: "rgba(148,163,184,0.3)",
              color: "var(--th-text-secondary)",
            }}
          >
            {tr("기본 순서로 바로 시작", "Start with default order")}
          </button>
        </div>
      )}

      {totalCount > 0 && (
        <div className="flex gap-0.5 h-1.5 rounded-full overflow-hidden bg-surface-subtle">
          {doneCount > 0 && (
            <div
              className="rounded-full"
              style={{
                width: `${(doneCount / totalCount) * 100}%`,
                backgroundColor: "#4ade80",
              }}
            />
          )}
          {dispatchedCount > 0 && (
            <div
              className="rounded-full"
              style={{
                width: `${(dispatchedCount / totalCount) * 100}%`,
                backgroundColor: "#fbbf24",
              }}
            />
          )}
          {failedCount > 0 && (
            <div
              className="rounded-full"
              style={{
                width: `${(failedCount / totalCount) * 100}%`,
                backgroundColor: "#ef4444",
              }}
            />
          )}
          {skippedCount > 0 && (
            <div
              className="rounded-full"
              style={{
                width: `${(skippedCount / totalCount) * 100}%`,
                backgroundColor: "#6b7280",
              }}
            />
          )}
        </div>
      )}
    </>
  );
}
