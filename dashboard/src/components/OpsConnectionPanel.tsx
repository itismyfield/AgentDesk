import { Database } from "lucide-react";
import type {
  HealthResponse,
  PromptManifestRetentionStatus,
} from "../api";
import {
  formatNumber,
  opsToneToHealth,
} from "./OpsPageModel";
import { StatusBadge } from "./common/StatusBadge";
import { FreshnessIndicator } from "./common/FreshnessIndicator";

type OpsTone = "info" | "warn" | "danger" | "success";

interface OpsConnectionPanelProps {
  wsConnected: boolean;
  /** Wall-clock ms timestamp of the most recent successful health refresh. */
  lastHealthAt?: number | null;
  health: HealthResponse | null;
  connectedProviders: number;
  providerCount: number;
  disconnectedProviders: number;
  restartPendingProviders: number;
  promptRetention: PromptManifestRetentionStatus | null;
  promptRetentionError: string | null;
  promptRetentionTone: OpsTone;
  promptRetentionValue: string;
  promptRetentionConfigNote: string;
  promptRetentionStorageNote: string;
  tr: (ko: string, en: string) => string;
}

export default function OpsConnectionPanel({
  wsConnected,
  lastHealthAt,
  health,
  connectedProviders,
  providerCount,
  disconnectedProviders,
  restartPendingProviders,
  promptRetention,
  promptRetentionError,
  promptRetentionTone,
  promptRetentionValue,
  promptRetentionConfigNote,
  promptRetentionStorageNote,
  tr,
}: OpsConnectionPanelProps) {
  return (
    <section className="min-w-0 self-start rounded-lg border border-th-border-subtle bg-th-bg-surface" aria-labelledby="ops-connection-title">
      <div className="flex flex-wrap items-start justify-between gap-3 border-b border-th-border-subtle px-4 py-4 sm:px-5">
        <div className="min-w-0"><h2 id="ops-connection-title" className="text-sm font-semibold text-th-text-primary">{tr("연결과 전달", "Connection & Delivery")}</h2><p className="mt-1 text-xs leading-5 text-th-text-muted">{tr("연결 상태, 전달 대기와 프롬프트 보관 정책입니다.", "Connectivity, delivery queues and prompt retention policy.")}</p></div>
        <FreshnessIndicator timestamp={lastHealthAt ?? null} compact staleAfterSeconds={20} criticalAfterSeconds={60} />
      </div>
      <div data-testid="ops-connection-panel" className="divide-y divide-th-border-subtle px-4 sm:px-5">
        <div data-testid="ops-websocket-card" className="flex min-w-0 items-start justify-between gap-3 py-4">
          <div className="min-w-0"><h3 className="text-xs text-th-text-muted">{tr("실시간 이벤트", "WebSocket")}</h3><p className="mt-2 text-xs leading-5 text-th-text-secondary">{wsConnected ? tr("이벤트를 수신하면 운영 상태를 새로 확인합니다.", "Health refreshes when an event arrives.") : tr("연결을 복구하는 동안 주기적으로 상태를 확인합니다.", "Health is polled while the connection recovers.")}</p></div>
          <StatusBadge tone={wsConnected ? "healthy" : "critical"} size="sm" pulse={wsConnected} title={wsConnected ? tr("WS 연결 정상", "WS connection healthy") : tr("WS 끊김", "WS disconnected")}>{wsConnected ? "LIVE" : "DISCONNECTED"}</StatusBadge>
        </div>
        <div className="grid grid-cols-2 divide-x divide-th-border-subtle">
          <div data-testid="ops-dispatch-outbox-card" className="min-w-0 py-4 pr-4"><h3 className="text-xs text-th-text-muted">{tr("전달 대기", "Dispatch outbox")}</h3><p className="mt-2 text-xl font-semibold tabular-nums text-th-text-primary">{health ? formatNumber(health.dispatch_outbox?.pending ?? 0) : "—"}</p><p className="mt-1 text-xs leading-5 text-th-text-muted">{tr(`재시도 ${formatNumber(health?.dispatch_outbox?.retrying ?? 0)} · 실패 ${formatNumber(health?.dispatch_outbox?.permanent_failures ?? 0)}`, `retry ${formatNumber(health?.dispatch_outbox?.retrying ?? 0)} · fail ${formatNumber(health?.dispatch_outbox?.permanent_failures ?? 0)}`)}</p></div>
          <div data-testid="ops-providers-card" className="min-w-0 py-4 pl-4"><h3 className="text-xs text-th-text-muted">{tr("프로바이더", "Providers")}</h3><p className="mt-2 text-xl font-semibold tabular-nums text-th-text-primary">{health ? `${connectedProviders}/${providerCount}` : "—"}</p><p className="mt-1 text-xs leading-5 text-th-text-muted">{tr(`연결 끊김 ${formatNumber(disconnectedProviders)} · 재시작 ${formatNumber(restartPendingProviders)}`, `disconnect ${formatNumber(disconnectedProviders)} · restart ${formatNumber(restartPendingProviders)}`)}</p></div>
        </div>
        <div data-testid="ops-prompt-retention-card" className="min-w-0 py-4">
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div className="min-w-0"><h3 className="flex items-center gap-2 text-xs text-th-text-muted"><Database size={14} />{tr("프롬프트 보관", "Prompt manifest retention")}</h3><p className="mt-2 text-xl font-semibold tabular-nums text-th-text-primary">{promptRetentionValue}</p></div>
            <StatusBadge tone={opsToneToHealth(promptRetentionTone)} size="sm">{promptRetention ? promptRetention.hot_reload ? "HOT" : promptRetention.config_applied_at?.toUpperCase() ?? "BOOT" : tr("미확인", "Unknown")}</StatusBadge>
          </div>
          <p className="mt-2 text-xs leading-5 text-th-text-muted">{promptRetentionConfigNote}</p><p className="mt-1 text-xs leading-5 text-th-text-muted">{promptRetentionStorageNote}</p>
          {promptRetention?.config_source ? <p className="mt-2 text-xs leading-5 text-th-text-muted">{promptRetention.config_source}</p> : null}
          {promptRetentionError ? <p role="status" className="mt-2 text-xs leading-5 text-[var(--th-badge-amber-text)]">{promptRetentionError}</p> : null}
        </div>
      </div>
    </section>
  );
}
