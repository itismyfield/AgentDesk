import { AlertTriangle, CheckCircle2, CircleSlash, RefreshCw } from "lucide-react";
import type { CSSProperties } from "react";
import type { OperatorConnectorStatus, OperatorConnectorsResponse } from "../../api";
import { SurfaceEmptyState as SettingsEmptyState } from "../common/SurfacePrimitives";

interface SettingsOperatorConnectorsPanelProps {
  connectors: OperatorConnectorsResponse | null;
  error: string | null;
  loading: boolean;
  onReload: () => void;
  secondaryActionClass: string;
  secondaryActionStyle: CSSProperties;
  tr: (ko: string, en: string) => string;
}

export function SettingsOperatorConnectorsPanel({
  connectors,
  error,
  loading,
  onReload,
  secondaryActionClass,
  secondaryActionStyle,
  tr,
}: SettingsOperatorConnectorsPanelProps) {
  const rows = connectors?.connectors ?? [];
  const summary = connectors?.summary;

  return (
    <div className="space-y-4" data-testid="settings-operator-connectors-panel">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex flex-wrap gap-2">
          <SummaryPill label={tr("준비", "Ready")} value={summary?.ready ?? 0} tone="ready" />
          <SummaryPill
            label={tr("미설정", "Not configured")}
            value={summary?.missing_config ?? 0}
            tone="skipped"
          />
          <SummaryPill label={tr("건너뜀", "Skipped")} value={summary?.skipped ?? 0} tone="skipped" />
          <SummaryPill label={tr("확인 필요", "Needs setup")} value={summary?.invalid ?? 0} tone="invalid" />
        </div>
        <button
          type="button"
          onClick={onReload}
          disabled={loading}
          className={secondaryActionClass}
          style={secondaryActionStyle}
        >
          <RefreshCw size={13} />
          {loading ? tr("확인 중...", "Checking...") : tr("다시 확인", "Recheck")}
        </button>
      </div>

      {error ? (
        <div
          className="rounded-2xl border px-4 py-3 text-sm"
          style={{
            borderColor: "rgba(248, 113, 113, 0.38)",
            background: "rgba(248, 113, 113, 0.10)",
            color: "var(--th-text)",
          }}
        >
          {error}
        </div>
      ) : null}

      {loading && rows.length === 0 ? (
        <SettingsEmptyState className="text-sm">
          {tr("커넥터 상태를 확인 중입니다.", "Checking connector status.")}
        </SettingsEmptyState>
      ) : rows.length === 0 ? (
        <SettingsEmptyState className="text-sm">
          {tr("표시할 커넥터가 없습니다.", "No connectors to show.")}
        </SettingsEmptyState>
      ) : (
        <div className="grid gap-3">
          {rows.map((connector) => (
            <ConnectorRow key={connector.id} connector={connector} tr={tr} />
          ))}
        </div>
      )}
    </div>
  );
}

function SummaryPill({
  label,
  value,
  tone,
}: {
  label: string;
  value: number;
  tone: "ready" | "skipped" | "invalid";
}) {
  const colors = connectorTone(tone);
  return (
    <span
      className="inline-flex items-center gap-2 rounded-full border px-3 py-1.5 text-xs font-medium"
      style={{
        borderColor: colors.border,
        background: colors.bg,
        color: colors.fg,
      }}
    >
      <span>{label}</span>
      <span>{value}</span>
    </span>
  );
}

function ConnectorRow({
  connector,
  tr,
}: {
  connector: OperatorConnectorStatus;
  tr: (ko: string, en: string) => string;
}) {
  const state = normalizedState(connector.state);
  const colors = connectorTone(state);
  const Icon = state === "ready" ? CheckCircle2 : isSetupState(state) ? AlertTriangle : CircleSlash;

  return (
    <div
      className="rounded-2xl border px-4 py-4"
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
        background: "color-mix(in srgb, var(--th-bg-surface) 88%, transparent)",
      }}
      data-testid={`settings-connector-${connector.id}`}
    >
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {connector.name}
          </div>
          <div className="mt-1 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
            {connector.capabilities.join(", ")}
          </div>
        </div>
        <span
          className="inline-flex shrink-0 items-center gap-1.5 rounded-full border px-2.5 py-1 text-[11px] font-medium"
          style={{
            borderColor: colors.border,
            background: colors.bg,
            color: colors.fg,
          }}
        >
          <Icon size={12} />
          {stateLabel(state, tr)}
        </span>
      </div>

      <div className="mt-3 grid gap-2 text-[12px] leading-5" style={{ color: "var(--th-text-muted)" }}>
        <FieldLine label={tr("환경변수", "Env var")} value={connector.env_var} />
        <FieldLine label={tr("경로", "Path")} value={connector.source ?? tr("미확인", "Unavailable")} />
        {connector.reason ? <FieldLine label={tr("사유", "Reason")} value={connector.reason} /> : null}
      </div>

      {connector.setup_actions.length > 0 ? (
        <div className="mt-3 space-y-2">
          <div className="text-[11px] font-semibold uppercase" style={{ color: "var(--th-text-muted)" }}>
            {tr("설정 작업", "Setup actions")}
          </div>
          <ul className="space-y-1.5 text-[12px] leading-5" style={{ color: "var(--th-text)" }}>
            {connector.setup_actions.map((action) => (
              <li key={action} className="rounded-xl border px-3 py-2" style={{
                borderColor: "color-mix(in srgb, var(--th-border) 62%, transparent)",
                background: "color-mix(in srgb, var(--th-overlay-medium) 70%, transparent)",
              }}>
                {action}
              </li>
            ))}
          </ul>
        </div>
      ) : null}
    </div>
  );
}

function FieldLine({ label, value }: { label: string; value: string }) {
  return (
    <div className="grid gap-1 sm:grid-cols-[96px_minmax(0,1fr)]">
      <span>{label}</span>
      <span className="min-w-0 break-all font-mono" style={{ color: "var(--th-text)" }}>
        {value}
      </span>
    </div>
  );
}

type ConnectorDisplayState =
  | "ready"
  | "skipped"
  | "missing_config"
  | "missing_path"
  | "missing_provider"
  | "invalid_config";

function normalizedState(state: string): ConnectorDisplayState {
  if (
    state === "ready" ||
    state === "skipped" ||
    state === "missing_config" ||
    state === "missing_path" ||
    state === "missing_provider" ||
    state === "invalid_config"
  ) {
    return state;
  }
  return "invalid_config";
}

function stateLabel(state: ConnectorDisplayState, tr: (ko: string, en: string) => string): string {
  if (state === "ready") return tr("준비됨", "Ready");
  if (state === "missing_config") return tr("미설정", "Not configured");
  if (state === "missing_path") return tr("경로 없음", "Missing path");
  if (state === "missing_provider") return tr("제공자 없음", "Missing provider");
  if (state === "invalid_config") return tr("설정 오류", "Invalid config");
  return tr("건너뜀", "Skipped");
}

function isSetupState(state: ConnectorDisplayState): boolean {
  return state === "missing_path" || state === "missing_provider" || state === "invalid_config";
}

function connectorTone(state: ConnectorDisplayState | "invalid") {
  if (state === "ready") {
    return {
      bg: "rgba(34, 197, 94, 0.13)",
      fg: "rgba(187, 247, 208, 0.95)",
      border: "rgba(34, 197, 94, 0.36)",
    };
  }
  if (state === "invalid" || state === "missing_path" || state === "missing_provider" || state === "invalid_config") {
    return {
      bg: "rgba(251, 191, 36, 0.14)",
      fg: "rgba(253, 230, 138, 0.95)",
      border: "rgba(251, 191, 36, 0.38)",
    };
  }
  return {
    bg: "rgba(148, 163, 184, 0.14)",
    fg: "rgba(226, 232, 240, 0.86)",
    border: "rgba(148, 163, 184, 0.32)",
  };
}
