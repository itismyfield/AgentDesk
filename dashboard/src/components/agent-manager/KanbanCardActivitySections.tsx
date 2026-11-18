import type { ReactNode } from "react";
import type { CardAuditLogEntry } from "../../api";
import type { KanbanCard, TaskDispatch, UiLanguage } from "../../types";
import { Badge } from "../../kit/free/badge-clean";
import { SurfaceNotice } from "../common/SurfacePrimitives";
import { DetailHeading } from "./CardDetailParts";
import { coerceTimestampMs, formatIso, parseCardMetadata } from "./kanban-utils";
import { dispatchStatusPresentation } from "./card-detail-activity";

interface KanbanCardActivitySectionsProps {
  card: KanbanCard;
  dispatches: TaskDispatch[];
  auditLog: CardAuditLogEntry[];
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  getAgentLabel: (agentId: string | null | undefined) => string;
}

/** Execution summary cells above the activity list: latest dispatch, dispatch and transition counts, and the receiving agent. */
export default function KanbanCardActivitySections({
  card,
  dispatches,
  auditLog,
  tr,
  locale,
  getAgentLabel,
}: KanbanCardActivitySectionsProps) {
  const cardDispatches = dispatches
    .filter((dispatch) => dispatch.kanban_card_id === card.id)
    .sort((a, b) => (coerceTimestampMs(b.created_at) ?? 0) - (coerceTimestampMs(a.created_at) ?? 0));
  const latest = cardDispatches.find((dispatch) => dispatch.id === card.latest_dispatch_id) ?? cardDispatches[0] ?? null;
  const latestStatus = latest?.status ?? card.latest_dispatch_status ?? null;
  const latestId = latest?.id ?? card.latest_dispatch_id ?? null;
  const failures = cardDispatches.filter((dispatch) => dispatch.status === "failed").length;
  const timedOut = parseCardMetadata(card.metadata_json).timed_out_reason;
  if (!latestStatus && cardDispatches.length === 0 && auditLog.length === 0) return null;

  const status = latestStatus ? dispatchStatusPresentation(latestStatus, tr) : null;
  const cells: Array<{ name: string; value: ReactNode; title?: string; fail?: boolean }> = [
    {
      name: tr("최신 디스패치", "Latest dispatch"),
      value: status ? <Badge size="md" tone={status.tone} dot={!status.live} live={status.live}>{status.label}</Badge> : "—",
      title: latestId ? `#${latestId.slice(0, 8)}` : undefined,
    },
    {
      name: tr("디스패치", "Dispatches"),
      value: failures > 0 ? tr(`${cardDispatches.length} · 실패 ${failures}`, `${cardDispatches.length} · ${failures} failed`) : String(cardDispatches.length),
      fail: failures > 0,
    },
    { name: tr("상태 전환", "Transitions"), value: String(auditLog.length) },
    {
      name: tr("받는 에이전트", "Agent"),
      value: latest?.to_agent_id ? getAgentLabel(latest.to_agent_id) : "—",
      title: latest ? formatIso(latest.created_at, locale) : undefined,
    },
  ];

  return (
    <section className="@container/summary flex min-w-0 flex-col gap-2.5" data-testid="kanban-execution-trace">
      <DetailHeading aside={latestId ? <span className="font-mono text-xs text-th-text-muted">#{latestId.slice(0, 8)}</span> : undefined}>
        {tr("실행 요약", "Execution summary")}
      </DetailHeading>
      {timedOut && (
        <SurfaceNotice tone="warn" compact className="text-sm">
          {timedOut}
        </SurfaceNotice>
      )}
      <dl className="m-0 grid grid-cols-2 gap-2.5 @min-[40rem]/summary:grid-cols-4" data-testid="kanban-state-history">
        {cells.map((cell) => (
          <div key={cell.name} className="flex min-w-0 flex-col gap-0.5 rounded-xl bg-surface-subtle px-3.5 py-3 shadow-[inset_0_0_0_0.5px_var(--th-border-subtle)]" title={cell.title}>
            <dt className="truncate text-xs leading-4 text-th-text-muted">{cell.name}</dt>
            <dd className={`m-0 flex min-h-[22px] min-w-0 items-center truncate text-[15px] font-medium leading-[22px] ${cell.fail ? "text-[color-mix(in_oklch,var(--err)_72%,var(--th-text-primary))]" : "text-th-text-primary"}`}>
              {cell.value}
            </dd>
          </div>
        ))}
      </dl>
    </section>
  );
}
