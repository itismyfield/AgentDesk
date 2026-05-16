import { Database } from "lucide-react";
import type {
  HealthResponse,
  PromptManifestRetentionStatus,
} from "../api";
import {
  chipClassFromTone,
  formatNumber,
} from "./OpsPageModel";

type OpsTone = "info" | "warn" | "danger" | "success";

interface OpsConnectionPanelProps {
  wsConnected: boolean;
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
    <div className="card">
      <div className="card-head">
        <div className="min-w-0">
          <div className="card-title">{tr("Connection & Delivery", "Connection & Delivery")}</div>
          <div className="mt-1 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
            {tr(
              "연결 상태와 전달 흐름의 건강도를 한눈에 확인합니다.",
              "Track connectivity and delivery health at a glance.",
            )}
          </div>
        </div>
      </div>
      <div data-testid="ops-connection-panel" className="card-body space-y-3">
        <div
          data-testid="ops-websocket-card"
          className="card ops-mini-card"
          style={{
            borderColor: wsConnected
              ? "color-mix(in srgb, var(--color-info) 18%, var(--th-border-subtle) 82%)"
              : "color-mix(in srgb, var(--color-danger) 18%, var(--th-border-subtle) 82%)",
            background:
              "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
          }}
        >
          <div className="card-body">
            <div className="flex items-start justify-between gap-3">
              <div className="min-w-0">
                <div className="text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                  websocket
                </div>
                <div className="mt-2 text-lg font-semibold" style={{ color: "var(--th-text-primary)" }}>
                  {wsConnected ? tr("실시간 연결됨", "Connected live") : tr("연결 끊김", "Disconnected")}
                </div>
                <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {wsConnected
                    ? tr("pcd-ws-event 수신 시 health refresh를 즉시 재스케줄합니다.", "Incoming pcd-ws-event messages reschedule health refreshes immediately.")
                    : tr("WS가 복구될 때까지 내부 polling으로 health를 유지합니다.", "Internal polling keeps health current until WS recovers.")}
                </div>
              </div>
              <span className={chipClassFromTone(wsConnected ? "success" : "danger")}>
                {wsConnected ? "LIVE" : "DISCONNECTED"}
              </span>
            </div>
          </div>
        </div>

        <div className="grid sm:grid-cols-2">
          <div data-testid="ops-dispatch-outbox-card" className="card ops-mini-card">
            <div className="card-body">
              <div className="metric-label">
                dispatch_outbox
              </div>
              <div className="metric-value" style={{ marginTop: 8, fontSize: 22, color: "var(--th-text-primary)" }}>
                {formatNumber(health?.dispatch_outbox?.pending ?? 0)}
              </div>
              <div className="metric-sub" style={{ marginTop: 6, fontSize: 12 }}>
                {tr(
                  `retry ${formatNumber(health?.dispatch_outbox?.retrying ?? 0)} · fail ${formatNumber(health?.dispatch_outbox?.permanent_failures ?? 0)}`,
                  `retry ${formatNumber(health?.dispatch_outbox?.retrying ?? 0)} · fail ${formatNumber(health?.dispatch_outbox?.permanent_failures ?? 0)}`,
                )}
              </div>
            </div>
          </div>

          <div data-testid="ops-providers-card" className="card ops-mini-card">
            <div className="card-body">
              <div className="metric-label">
                providers
              </div>
              <div className="metric-value" style={{ marginTop: 8, fontSize: 22, color: "var(--th-text-primary)" }}>
                {connectedProviders}/{providerCount}
              </div>
              <div className="metric-sub" style={{ marginTop: 6, fontSize: 12 }}>
                {tr(
                  `disconnect ${formatNumber(disconnectedProviders)} · restart ${formatNumber(restartPendingProviders)}`,
                  `disconnect ${formatNumber(disconnectedProviders)} · restart ${formatNumber(restartPendingProviders)}`,
                )}
              </div>
            </div>
          </div>
        </div>

        <div data-testid="ops-prompt-retention-card" className="card ops-mini-card">
          <div className="card-body">
            <div className="flex items-start justify-between gap-3">
              <div className="min-w-0">
                <div className="metric-label">
                  <Database size={12} />
                  prompt_manifest_retention
                </div>
                <div className="metric-value" style={{ marginTop: 8, fontSize: 22, color: "var(--th-text-primary)" }}>
                  {promptRetentionValue}
                </div>
                <div className="metric-sub" style={{ marginTop: 6, fontSize: 12 }}>
                  {promptRetentionConfigNote}
                </div>
                <div className="metric-sub" style={{ marginTop: 4, fontSize: 12 }}>
                  {promptRetentionStorageNote}
                </div>
              </div>
              <span className={chipClassFromTone(promptRetentionTone)}>
                {promptRetention?.hot_reload ? "HOT" : promptRetention?.config_applied_at?.toUpperCase() ?? "BOOT"}
              </span>
            </div>
            {promptRetention?.config_source ? (
              <div className="mt-3 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                {promptRetention.config_source}
              </div>
            ) : null}
            {promptRetentionError ? (
              <div className="mt-2 text-[11px] leading-5" style={{ color: "var(--color-warning)" }}>
                {promptRetentionError}
              </div>
            ) : null}
          </div>
        </div>
      </div>
    </div>
  );
}
