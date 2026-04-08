import { useEffect, useState } from "react";
import * as api from "../../api";
import type { PipelineStage, PipelineHistoryEntry, UiLanguage } from "../../types";

interface Props {
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  cardId: string;
  currentStageId: string | null;
}

const STATUS_COLORS: Record<string, string> = {
  completed: "#4ade80",
  active: "#fbbf24",
  failed: "#f87171",
  skipped: "#6b7280",
  retrying: "#f59e0b",
};

export default function PipelineProgress({ tr, cardId, currentStageId }: Props) {
  const [stages, setStages] = useState<PipelineStage[]>([]);
  const [history, setHistory] = useState<PipelineHistoryEntry[]>([]);
  const [currentStage, setCurrentStage] = useState<PipelineStage | null>(null);

  useEffect(() => {
    void api.getCardPipelineStatus(cardId).then((data) => {
      setStages(data.stages);
      setHistory(data.history);
      setCurrentStage(data.current_stage);
    }).catch(() => {
      setStages([]);
      setHistory([]);
    });
  }, [cardId, currentStageId]);

  if (stages.length === 0) return null;

  const historyByStage = new Map<string, PipelineHistoryEntry>();
  for (const h of history) {
    // Keep latest entry per stage
    const existing = historyByStage.get(h.stage_id);
    if (!existing || h.started_at > existing.started_at) {
      historyByStage.set(h.stage_id, h);
    }
  }

  return (
    <div className="space-y-1.5">
      <div className="text-xs font-medium" style={{ color: "var(--th-text-muted)" }}>
        {tr("파이프라인", "Pipeline")}
        {currentStage && (
          <span className="ml-1.5" style={{ color: "#fbbf24" }}>
            → {currentStage.stage_name}
          </span>
        )}
      </div>

      <div className="flex gap-0.5">
        {stages.map((stage) => {
          const hist = historyByStage.get(stage.id);
          const isCurrent = stage.id === currentStageId;
          const color = hist ? STATUS_COLORS[hist.status] ?? "#64748b" : isCurrent ? "#fbbf24" : "#334155";

          return (
            <div
              key={stage.id}
              className="relative group flex-1 h-2 rounded-full"
              style={{ backgroundColor: color, opacity: hist || isCurrent ? 1 : 0.4 }}
              title={`${stage.stage_name}${hist ? ` (${hist.status})` : ""}`}
            >
              {/* Tooltip on hover */}
              <div className="absolute bottom-full left-1/2 -translate-x-1/2 mb-1 hidden group-hover:block z-50">
                <div className="bg-th-bg-primary border border-th-border rounded px-2 py-1 text-xs whitespace-nowrap"
                  style={{ color: "var(--th-text-secondary)" }}>
                  {stage.stage_name}
                  {hist && (
                    <span className="ml-1" style={{ color }}>
                      {hist.status === "completed" ? "✓" :
                       hist.status === "active" ? "●" :
                       hist.status === "failed" ? "✕" :
                       hist.status === "skipped" ? "⊘" :
                       hist.status === "retrying" ? `↻ ${hist.attempt}` : ""}
                    </span>
                  )}
                  {hist?.failure_reason && (
                    <div className="text-xs max-w-[200px] truncate" style={{ color: "#f87171" }}>
                      {hist.failure_reason}
                    </div>
                  )}
                </div>
              </div>
            </div>
          );
        })}
      </div>

      {/* Retry/failure info */}
      {history.some((h) => h.status === "retrying") && (
        <div className="text-xs" style={{ color: "#f59e0b" }}>
          {tr("재시도 중", "Retrying")} (
          {history.filter((h) => h.status === "retrying" || h.status === "failed").length}
          {tr("회", " attempts")})
        </div>
      )}
    </div>
  );
}
