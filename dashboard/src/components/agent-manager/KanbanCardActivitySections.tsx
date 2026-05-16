import type { CardAuditLogEntry } from "../../api";
import type {
  KanbanCard,
  KanbanCardStatus,
  TaskDispatch,
  UiLanguage,
} from "../../types";
import {
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceNotice,
} from "../common/SurfacePrimitives";
import {
  TRANSITION_STYLE,
  formatIso,
  labelForStatus,
  parseCardMetadata,
} from "./kanban-utils";
import {
  formatAuditResult,
  formatDispatchSummary,
} from "./card-detail-activity";

const ACTIVITY_RESULT_TONE_STYLE = {
  default: {
    backgroundColor: "rgba(148,163,184,0.08)",
    borderColor: "rgba(148,163,184,0.16)",
    color: "var(--th-text-secondary)",
  },
  warn: {
    backgroundColor: "rgba(245,158,11,0.10)",
    borderColor: "rgba(245,158,11,0.24)",
    color: "#fbbf24",
  },
  danger: {
    backgroundColor: "rgba(248,113,113,0.10)",
    borderColor: "rgba(248,113,113,0.24)",
    color: "#fca5a5",
  },
} as const;

interface KanbanCardActivitySectionsProps {
  card: KanbanCard;
  dispatches: TaskDispatch[];
  auditLog: CardAuditLogEntry[];
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  getAgentLabel: (agentId: string | null | undefined) => string;
}

export default function KanbanCardActivitySections({
  card,
  dispatches,
  auditLog,
  tr,
  locale,
  getAgentLabel,
}: KanbanCardActivitySectionsProps) {
  const cardDispatches = dispatches
    .filter((d) => d.kanban_card_id === card.id)
    .sort((a, b) => {
      const ta = typeof a.created_at === "number" ? a.created_at : new Date(a.created_at).getTime();
      const tb = typeof b.created_at === "number" ? b.created_at : new Date(b.created_at).getTime();
      return tb - ta;
    });
  const hasAnyDispatch = cardDispatches.length > 0 || card.latest_dispatch_status;

  return (
    <>
      {hasAnyDispatch ? (
        <SurfaceCard className="space-y-3">
          <h4 className="font-medium" style={{ color: "var(--th-text-heading)" }}>
            {tr("Dispatch 이력", "Dispatch history")}
            {cardDispatches.length > 0 && (
              <span className="ml-2 text-xs font-normal" style={{ color: "var(--th-text-muted)" }}>
                ({cardDispatches.length})
              </span>
            )}
          </h4>
          {parseCardMetadata(card.metadata_json).timed_out_reason && (
            <SurfaceNotice tone="warn" compact className="text-sm">
              {parseCardMetadata(card.metadata_json).timed_out_reason}
            </SurfaceNotice>
          )}
          {cardDispatches.length > 0 ? (
            <div className="space-y-2 max-h-64 overflow-y-auto">
              {cardDispatches.map((d) => {
                const dispatchStatusColor: Record<string, string> = {
                  pending: "#fbbf24",
                  dispatched: "#38bdf8",
                  in_progress: "#f59e0b",
                  completed: "#4ade80",
                  failed: "#f87171",
                  cancelled: "#9ca3af",
                };
                return (
                  <div
                    key={d.id}
                    className="rounded-xl border px-3 py-2 text-sm"
                    style={{ borderColor: "rgba(148,163,184,0.12)", backgroundColor: d.id === card.latest_dispatch_id ? "rgba(37,99,235,0.08)" : "transparent" }}
                  >
                    <div className="flex items-center gap-2 flex-wrap">
                      <span
                        className="inline-block w-2 h-2 rounded-full shrink-0"
                        style={{ backgroundColor: dispatchStatusColor[d.status] ?? "#94a3b8" }}
                      />
                      <span className="font-mono text-xs" style={{ color: "var(--th-text-muted)" }}>
                        #{d.id.slice(0, 8)}
                      </span>
                      <span
                        className="px-1.5 py-0.5 rounded text-xs font-medium"
                        style={{ backgroundColor: "rgba(148,163,184,0.12)", color: dispatchStatusColor[d.status] ?? "#94a3b8" }}
                      >
                        {d.status}
                      </span>
                      {d.dispatch_type && (
                        <span className="px-1.5 py-0.5 rounded text-xs" style={{ backgroundColor: "rgba(148,163,184,0.08)", color: "var(--th-text-secondary)" }}>
                          {d.dispatch_type}
                        </span>
                      )}
                      {d.to_agent_id && (
                        <span className="text-xs" style={{ color: "var(--th-text-secondary)" }}>
                          → {getAgentLabel(d.to_agent_id)}
                        </span>
                      )}
                    </div>
                    <div className="flex items-center gap-3 mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                      <span>{formatIso(d.created_at, locale)}</span>
                      {d.chain_depth > 0 && <span>depth {d.chain_depth}</span>}
                    </div>
                    {(() => {
                      const dispatchSummary = formatDispatchSummary(d.result_summary);
                      if (!dispatchSummary) return null;
                      return (
                        <div
                          className="mt-2 rounded-lg border px-2 py-1.5 text-xs leading-relaxed whitespace-pre-wrap break-words"
                          style={{
                            borderColor: "rgba(148,163,184,0.16)",
                            backgroundColor: "rgba(148,163,184,0.06)",
                            color: "var(--th-text-secondary)",
                          }}
                        >
                          {dispatchSummary}
                        </div>
                      );
                    })()}
                  </div>
                );
              })}
            </div>
          ) : (
            <SurfaceEmptyState className="grid gap-2 md:grid-cols-2 text-sm">
              <div>{tr("dispatch 상태", "Dispatch status")}: {card.latest_dispatch_status ?? "-"}</div>
              <div>{tr("최신 dispatch", "Latest dispatch")}: {card.latest_dispatch_id ? `#${card.latest_dispatch_id.slice(0, 8)}` : "-"}</div>
            </SurfaceEmptyState>
          )}
        </SurfaceCard>
      ) : null}

      {auditLog.length > 0 && (
        <SurfaceCard className="space-y-3">
          <h4 className="font-medium" style={{ color: "var(--th-text-heading)" }}>
            {tr("상태 전환 이력", "State Transition History")}
            <span className="ml-2 text-xs font-normal" style={{ color: "var(--th-text-muted)" }}>
              ({auditLog.length})
            </span>
          </h4>
          <div className="space-y-1.5 max-h-48 overflow-y-auto">
            {auditLog.map((log) => {
              const resultPresentation = formatAuditResult(log.result, tr);
              return (
                <div
                  key={log.id}
                  className="rounded-lg px-2.5 py-2 text-xs space-y-1.5"
                  style={{ backgroundColor: "rgba(255,255,255,0.03)" }}
                >
                  <div className="flex items-center gap-2">
                    <span className="shrink-0" style={{ color: "var(--th-text-muted)" }}>
                      {formatIso(log.created_at, locale)}
                    </span>
                    <span
                      className="ml-auto px-1.5 py-0.5 rounded text-xs"
                      style={{ backgroundColor: "rgba(148,163,184,0.12)", color: "var(--th-text-muted)" }}
                    >
                      {log.source}
                    </span>
                  </div>
                  <div className="flex items-center gap-2 flex-wrap">
                    <span style={{ color: TRANSITION_STYLE[log.from_status ?? ""]?.text ?? "var(--th-text-secondary)" }}>
                      {log.from_status ? labelForStatus(log.from_status as KanbanCardStatus, tr) : "—"}
                    </span>
                    <span style={{ color: "var(--th-text-muted)" }}>→</span>
                    <span style={{ color: TRANSITION_STYLE[log.to_status ?? ""]?.text ?? "var(--th-text-secondary)" }}>
                      {log.to_status ? labelForStatus(log.to_status as KanbanCardStatus, tr) : "—"}
                    </span>
                  </div>
                  {resultPresentation && (
                    <div
                      className="rounded-md border px-2 py-1.5 text-xs leading-relaxed whitespace-pre-wrap break-words"
                      style={ACTIVITY_RESULT_TONE_STYLE[resultPresentation.tone]}
                    >
                      {resultPresentation.text}
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        </SurfaceCard>
      )}
    </>
  );
}
