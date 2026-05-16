import type { ReactNode } from "react";
import type { DispatchQueueEntry as DispatchQueueEntryType } from "../../api";
import { getBatchPhaseColor } from "../../theme/statusTokens";
import { EntryRow } from "./AutoQueueEntryRow";
import { batchPhaseLabel, isCompletedEntry, threadGroupColor } from "./auto-queue-panel-utils";

export function createAutoQueuePhaseRenderers(ctx: any) {
  const {
    currentBatchPhase,
    deployPhases,
    gatesByPhase,
    hasBatchPhases,
    handleEntryStatusUpdate,
    locale,
    threadGroups,
    tr,
  } = ctx;

  const renderPhaseBlock = (
    phase: number,
    phaseEntries: DispatchQueueEntryType[],
    content: ReactNode,
  ) => {
    const activePhase = currentBatchPhase === phase;
    const phaseColor = getBatchPhaseColor(phase);
    const doneInPhase = phaseEntries.filter(isCompletedEntry).length;

    return (
      <div
        key={phase}
        className="rounded-xl border p-2 space-y-2"
        style={{
          borderColor: activePhase
            ? `${phaseColor}66`
            : "rgba(148,163,184,0.16)",
          backgroundColor: activePhase ? `${phaseColor}10` : "transparent",
        }}
      >
        <div className="flex items-center gap-2 px-1">
          <span
            className="text-xs font-mono font-bold px-2 py-0.5 rounded"
            style={{ backgroundColor: `${phaseColor}26`, color: phaseColor }}
          >
            {batchPhaseLabel(phase)}
          </span>
          <span
            className="text-xs px-1.5 py-0.5 rounded"
            style={{
              backgroundColor: activePhase
                ? "rgba(245,158,11,0.18)"
                : "rgba(100,116,139,0.18)",
              color: activePhase ? "#fbbf24" : "var(--th-text-muted)",
            }}
          >
            {activePhase
              ? tr("현재 phase", "Current phase")
              : phase <= 0
                ? tr("즉시 가능", "Always eligible")
                : doneInPhase === phaseEntries.length
                  ? tr("완료", "Completed")
                  : currentBatchPhase != null && phase < currentBatchPhase
                    ? tr("완료", "Completed")
                    : tr("대기 phase", "Queued phase")}
          </span>
          <div
            className="flex-1 h-px"
            style={{ backgroundColor: `${phaseColor}40` }}
          />
          <span
            className="text-xs font-mono"
            style={{ color: "var(--th-text-muted)" }}
          >
            {doneInPhase}/{phaseEntries.length}
          </span>
        </div>
        {content}
      </div>
    );
  };

  const renderPhaseGateIndicator = (phase: number) => {
    const gates = gatesByPhase.get(phase) ?? [];
    const isDeploy = deployPhases.has(phase);

    const gate = gates[0];
    const gateStatus = gate?.status ?? "pending";
    const isPassed = gateStatus === "passed";
    const isFailed = gateStatus === "failed";
    const isPending = !isPassed && !isFailed;
    const isActive = isPending && currentBatchPhase === phase;

    const baseColor = isPassed
      ? "#4ade80"
      : isFailed
        ? "#ef4444"
        : isActive
          ? isDeploy ? "#60a5fa" : "#f59e0b"
          : "#6b7280";
    const statusIcon = isPassed ? "✓" : isFailed ? "✗" : isActive ? (isDeploy ? "🚀" : "⏳") : "○";
    const statusLabel = isPassed
      ? tr("통과", "Passed")
      : isFailed
        ? tr("실패", "Failed")
        : isActive
          ? tr("진행중", "In Progress")
          : tr("대기", "Pending");
    const gateLabel = isDeploy ? tr("배포 게이트", "Deploy Gate") : tr("게이트", "Gate");

    return (
      <div
        key={`gate-${phase}`}
        className="flex items-center gap-2 px-3 py-1.5"
      >
        <div
          className="flex-1 h-px"
          style={{ backgroundColor: `${baseColor}40` }}
        />
        <div
          className={`flex items-center gap-1.5 px-2.5 py-1 rounded-lg border${isActive ? " animate-pulse" : ""}`}
          style={{
            borderColor: `${baseColor}40`,
            backgroundColor: `${baseColor}10`,
          }}
        >
          <span style={{ color: baseColor, fontSize: 14 }}>
            {statusIcon}
          </span>
          <span
            className="text-xs font-mono font-semibold"
            style={{ color: baseColor }}
          >
            {gateLabel}
          </span>
          {gate && (
            <span
              className="text-xs px-1.5 py-0.5 rounded"
              style={{
                backgroundColor: `${baseColor}18`,
                color: baseColor,
              }}
            >
              {statusLabel}
            </span>
          )}
          {gate?.failure_reason && (
            <span
              className="text-xs truncate max-w-[200px]"
              style={{ color: "#f87171" }}
              title={gate.failure_reason}
            >
              {gate.failure_reason}
            </span>
          )}
        </div>
        <div
          className="flex-1 h-px"
          style={{ backgroundColor: `${baseColor}40` }}
        />
      </div>
    );
  };

  const renderThreadGroupCard = (
    groupNum: number,
    groupEntries: DispatchQueueEntryType[],
  ) => {
    const isActive = groupEntries.some((entry) => entry.status === "dispatched");
    const hasPending = groupEntries.some((entry) => entry.status === "pending");
    const hasFailed = groupEntries.some((entry) => entry.status === "failed");
    const completedEntries = groupEntries.filter(isCompletedEntry).length;
    const isDone = completedEntries === groupEntries.length && !hasFailed;
    const groupStatusLabel = isActive
      ? tr("진행", "Active")
      : hasPending
        ? tr("대기", "Pending")
        : hasFailed
          ? tr("실패", "Failed")
          : isDone
            ? tr("완료", "Done")
        : tr("대기", "Pending");
    const color = threadGroupColor(groupNum);
    const reason =
      groupEntries.find((entry) => !!entry.reason)?.reason ??
      threadGroups[String(groupNum)]?.reason;
    const headerColor = isActive ? "#fbbf24" : hasFailed ? "#f87171" : isDone ? "#4ade80" : "#94a3b8";
    const borderColor = isActive
      ? `${color}55`
      : hasFailed
        ? "rgba(239,68,68,0.28)"
        : isDone
          ? "rgba(34,197,94,0.2)"
          : "rgba(148,163,184,0.12)";

    return (
      <div
        key={groupNum}
        className="rounded-xl border p-2 space-y-1"
        style={{
          borderColor,
          backgroundColor: isActive
            ? `${color}0a`
            : hasFailed
              ? "rgba(239,68,68,0.04)"
              : "transparent",
        }}
      >
        <div className="flex items-center gap-2 px-1 mb-1">
          <span
            className="text-xs font-mono font-bold px-2 py-0.5 rounded"
            style={{ backgroundColor: `${color}30`, color }}
          >
            G{groupNum}
          </span>
          <span
            className="text-xs px-1.5 py-0.5 rounded"
            style={{
              backgroundColor: isActive
                ? "rgba(245,158,11,0.18)"
                : hasFailed
                  ? "rgba(239,68,68,0.16)"
                : isDone
                  ? "rgba(34,197,94,0.18)"
                  : "rgba(100,116,139,0.18)",
              color: headerColor,
            }}
          >
            {groupStatusLabel}
          </span>
          <div
            className="flex-1 h-px"
            style={{ backgroundColor: `${color}40` }}
          />
          <span
            className="text-xs font-mono"
            style={{ color: "var(--th-text-muted)" }}
          >
            {completedEntries}/{groupEntries.length}
          </span>
        </div>
        {reason && (
          <div
            className="px-1 text-[10px]"
            style={{ color: "var(--th-text-muted)" }}
          >
            {reason}
          </div>
        )}
        {groupEntries.map((entry, idx) => (
          <EntryRow
            key={entry.id}
            entry={entry}
            idx={idx}
            tr={tr}
            locale={locale}
            onUpdateStatus={handleEntryStatusUpdate}
            showBatchPhase={hasBatchPhases}

          />
        ))}
      </div>
    );
  };

  return { renderPhaseBlock, renderPhaseGateIndicator, renderThreadGroupCard };
}
