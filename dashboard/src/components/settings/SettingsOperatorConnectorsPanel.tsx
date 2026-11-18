import { AlertTriangle, CheckCircle2, ChevronRight, CircleSlash, RefreshCw } from "lucide-react";
import type { OperatorConnectorStatus, OperatorConnectorsResponse } from "../../api";
import { connectorCopy, connectorReason, connectorSetupSteps } from "./SettingsCopy";
import { SETTINGS_ROW_PAD_CLASS, SETTINGS_TRAY_CLASS, SettingsAction, SettingsBadge, SettingsCard, SettingsSkeletonRows, SettingsStatusMessage } from "./SettingsPrimitives";

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
        <p className="min-w-0 flex-1 text-[13px] leading-5 break-keep text-th-text-secondary" data-testid="settings-connectors-summary">
          {rows.length === 0
            ? null
            : needsSetup > 0
              ? tr(`${rows.length}개 중 ${ready}개 준비됐고, ${needsSetup}개는 확인이 필요해요.`, `${ready} of ${rows.length} ready; ${needsSetup} need attention.`)
              : tr(`${rows.length}개 중 ${ready}개 준비됐어요.`, `${ready} of ${rows.length} ready.`)}
        </p>
        <SettingsAction
          onClick={onReload}
          disabled={loading}
          icon={<RefreshCw className={loading ? "animate-spin motion-reduce:animate-none" : undefined} />}
        >
          {loading ? tr("확인 중…", "Checking…") : tr("다시 확인", "Check again")}
        </SettingsAction>
      </div>

      <SettingsCard>
        {error ? (
          <SettingsStatusMessage tone="error">{tr("커넥터 상태를 불러오지 못했어요.", "Could not load connector status.")}</SettingsStatusMessage>
        ) : loading && rows.length === 0 ? (
          <SettingsSkeletonRows label={tr("커넥터 상태를 확인하는 중…", "Checking connectors…")} />
        ) : rows.length === 0 ? (
          <SettingsStatusMessage>{tr("표시할 커넥터가 없어요.", "No connectors to show.")}</SettingsStatusMessage>
        ) : (
          rows.map((connector) => <ConnectorRow key={connector.id} connector={connector} isKo={isKo} tr={tr} />)
        )}
      </SettingsCard>
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
  const setup = connectorSetupSteps(connector, isKo);
  const Icon = tone === "ready" ? CheckCircle2 : tone === "setup" ? AlertTriangle : CircleSlash;

  return (
    <div className={SETTINGS_ROW_PAD_CLASS} data-testid={`settings-connector-${connector.id}`}>
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0 flex-1">
          <div className="text-[13px] font-medium leading-5 break-keep text-th-text-primary">{isKo ? copy.labelKo : copy.labelEn}</div>
          {copy.hintKo ? <p className="text-xs leading-4 break-keep text-th-text-muted">{isKo ? copy.hintKo : copy.hintEn}</p> : null}
        </div>
        <SettingsBadge tone={tone === "ready" ? "ok" : tone === "setup" ? "warn" : "neutral"}>
          <Icon size={11} aria-hidden className="mr-1" />
          {stateLabel(connector.state, tr)}
        </SettingsBadge>
      </div>
      {connector.reason && tone !== "ready" ? (
        <p className="mt-2 text-xs leading-5 break-keep text-th-text-secondary">{connectorReason(connector.reason, tr)}</p>
      ) : null}
      <dl className="mt-2 grid gap-x-3 gap-y-1 text-xs leading-5 text-th-text-muted sm:grid-cols-[7rem_minmax(0,1fr)]">
        <dt>{tr("환경 변수", "Environment variable")}</dt>
        <dd className="min-w-0 font-mono text-th-text-primary [overflow-wrap:anywhere]">{connector.env_var}</dd>
        {connector.source ? <>
          <dt>{tr("읽는 곳", "Reads from")}</dt>
          <dd className="min-w-0 font-mono text-th-text-primary [overflow-wrap:anywhere]">{connector.source}</dd>
        </> : null}
      </dl>
      {setup.steps.length > 0 ? (
        <details className="group mt-1 text-xs">
          <summary className="inline-flex min-h-11 cursor-pointer list-none items-center gap-1 rounded-[var(--r-sm)] font-medium text-th-text-secondary hover:text-th-text-primary [&::-webkit-details-marker]:hidden">
            <ChevronRight size={14} aria-hidden className="transition-transform group-open:rotate-90 motion-reduce:transition-none" />
            {tr("설정 방법 보기", "How to set up")}
          </summary>
          <ul className="space-y-1.5 leading-5 text-th-text-primary" data-testid={`settings-connector-steps-${connector.id}`}>
            {setup.steps.map((action) => (
              <li key={action} className={`px-3 py-2 [overflow-wrap:anywhere] ${SETTINGS_TRAY_CLASS} ${setup.translated ? "break-keep" : "font-mono"}`}
                lang={setup.translated ? undefined : "en"}>
                {action}
              </li>
            ))}
          </ul>
        </details>
      ) : null}
    </div>
  );
}
