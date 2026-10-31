import { AlertTriangle, CheckCircle2, CircleSlash, RefreshCw } from "lucide-react";
import type { OperatorConnectorStatus, OperatorConnectorsResponse } from "../../api";
import { connectorCopy, connectorReason } from "./SettingsCopy";
import { SETTINGS_SECONDARY_BUTTON, SettingsStatusMessage } from "./SettingsPanels";

type Tr = (ko: string, en: string) => string;

interface SettingsOperatorConnectorsPanelProps {
  connectors: OperatorConnectorsResponse | undefined;
  error: boolean;
  loading: boolean;
  onReload: () => void;
  isKo: boolean;
  tr: Tr;
}

export function SettingsOperatorConnectorsPanel({ connectors, error, loading, onReload, isKo, tr }: SettingsOperatorConnectorsPanelProps) {
  const rows = connectors?.connectors ?? [];
  const ready = connectors?.summary?.ready ?? rows.filter((row) => row.state === "ready").length;
  const needsSetup = connectors?.summary?.invalid ?? 0;

  return (
    <div className="space-y-3" data-testid="settings-operator-connectors-panel">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="min-w-0 flex-1 text-sm break-keep text-th-text-secondary" data-testid="settings-connectors-summary">
          {rows.length === 0
            ? null
            : needsSetup > 0
              ? tr(`${rows.length}개 중 ${ready}개 준비됐고, ${needsSetup}개는 확인이 필요해요.`, `${ready} of ${rows.length} ready; ${needsSetup} need attention.`)
              : tr(`${rows.length}개 중 ${ready}개 준비됐어요.`, `${ready} of ${rows.length} ready.`)}
        </p>
        <button type="button" onClick={onReload} disabled={loading} className={SETTINGS_SECONDARY_BUTTON}>
          <RefreshCw size={13} aria-hidden className={loading ? "animate-spin" : undefined} />
          {loading ? tr("확인 중…", "Checking…") : tr("다시 확인", "Check again")}
        </button>
      </div>

      {error ? (
        <SettingsStatusMessage tone="error">{tr("커넥터 상태를 불러오지 못했어요.", "Could not load connector status.")}</SettingsStatusMessage>
      ) : loading && rows.length === 0 ? (
        <SettingsStatusMessage>{tr("커넥터 상태를 확인하는 중…", "Checking connectors…")}</SettingsStatusMessage>
      ) : rows.length === 0 ? (
        <SettingsStatusMessage>{tr("표시할 커넥터가 없어요.", "No connectors to show.")}</SettingsStatusMessage>
      ) : (
        <div className="divide-y divide-th-border-subtle">
          {rows.map((connector) => <ConnectorRow key={connector.id} connector={connector} isKo={isKo} tr={tr} />)}
        </div>
      )}
    </div>
  );
}

type ConnectorTone = "ready" | "setup" | "off";

function connectorTone(state: string): ConnectorTone {
  if (state === "ready") return "ready";
  if (state === "skipped" || state === "missing_config") return "off";
  return "setup";
}

function stateLabel(state: string, tr: Tr): string {
  if (state === "ready") return tr("준비됨", "Ready");
  if (state === "skipped") return tr("꺼짐", "Off");
  if (state === "missing_config") return tr("설정 안 함", "Not set up");
  if (state === "missing_path") return tr("폴더 없음", "Folder missing");
  if (state === "missing_provider") return tr("제공자 없음", "Provider missing");
  return tr("설정 오류", "Setup error");
}

function ConnectorRow({ connector, isKo, tr }: { connector: OperatorConnectorStatus; isKo: boolean; tr: Tr }) {
  const tone = connectorTone(connector.state);
  const copy = connectorCopy(connector.id, connector.name);
  const Icon = tone === "ready" ? CheckCircle2 : tone === "setup" ? AlertTriangle : CircleSlash;
  const color = tone === "ready" ? "var(--th-badge-emerald-text)" : tone === "setup" ? "var(--th-badge-amber-text)" : "var(--th-text-muted)";

  return (
    <div className="py-4 first:pt-1 last:pb-1" data-testid={`settings-connector-${connector.id}`}>
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0 flex-1">
          <div className="text-sm font-medium break-keep text-th-text-primary">{isKo ? copy.labelKo : copy.labelEn}</div>
          {copy.hintKo ? <p className="mt-1 text-xs leading-5 break-keep text-th-text-muted">{isKo ? copy.hintKo : copy.hintEn}</p> : null}
        </div>
        <span
          className="inline-flex shrink-0 items-center gap-1.5 rounded-full border border-th-border-subtle px-2.5 py-1 text-xs font-medium"
          style={{ color, background: "var(--th-overlay-subtle)" }}
        >
          <Icon size={12} aria-hidden />
          {stateLabel(connector.state, tr)}
        </span>
      </div>
      {connector.reason && tone !== "ready" ? (
        <p className="mt-2 text-xs leading-5 break-keep text-th-text-secondary">{connectorReason(connector.reason, tr)}</p>
      ) : null}
      <dl className="mt-2 grid gap-1 text-xs leading-5 text-th-text-muted sm:grid-cols-[7rem_minmax(0,1fr)]">
        <dt>{tr("환경 변수", "Environment variable")}</dt>
        <dd className="min-w-0 font-mono text-th-text-primary [overflow-wrap:anywhere]">{connector.env_var}</dd>
        {connector.source ? <>
          <dt>{tr("읽는 곳", "Reads from")}</dt>
          <dd className="min-w-0 font-mono text-th-text-primary [overflow-wrap:anywhere]">{connector.source}</dd>
        </> : null}
      </dl>
      {connector.setup_actions.length > 0 ? (
        <details className="mt-2 text-xs">
          <summary className="inline-flex min-h-11 cursor-pointer items-center font-medium text-th-text-secondary">
            {tr("설정 방법 보기", "How to set up")}
          </summary>
          <ul className="mt-1 space-y-1.5 leading-5 text-th-text-primary">
            {connector.setup_actions.map((action) => (
              <li key={action} className="rounded-md border border-th-border-subtle px-3 py-2 font-mono [overflow-wrap:anywhere]" lang="en">
                {action}
              </li>
            ))}
          </ul>
        </details>
      ) : null}
    </div>
  );
}
