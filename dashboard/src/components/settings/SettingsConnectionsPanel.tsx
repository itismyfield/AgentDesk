import { useQuery } from "@tanstack/react-query";
import { getSettingsChannelName } from "../../api/settingsRuntime";
import type { ConnectionStatusRow } from "./SettingsConnectionsModel";
import { SETTINGS_SECONDARY_BUTTON, SettingsSection } from "./SettingsPanels";
import type { SettingRowMeta } from "./SettingsModel";
import type { SettingsPanelContext, SourceStatus } from "./SettingsPanelTypes";
import { SettingsOperatorConnectorsPanel } from "./SettingsOperatorConnectorsPanel";
import { SettingsRowSection, sectionTitle } from "./SettingsRowSection";
import type { useOperatorConnectors } from "./useSettingsRemoteData";

const DISCORD_ID_PATTERN = /^\d{17,20}$/;
const STATUS_KEYWORDS = "연결 상태 디스코드 봇 서버 소유자 AI 서비스 초기 설정 온보딩 discord bot server owner onboarding setup";
const CONNECTOR_KEYWORDS = "커넥터 obsidian kakao 옵시디언 카카오 connector";

interface Props {
  ctx: SettingsPanelContext;
  statusRows: ConnectionStatusRow[];
  connectors: ReturnType<typeof useOperatorConnectors>;
  configStatus: SourceStatus;
  onRerunSetup: () => void;
}

export function SettingsConnectionsPanel({ ctx, statusRows, connectors, configStatus, onRerunSetup }: Props) {
  const { tr } = ctx;
  const status = sectionTitle(ctx, "connections.status");
  const connectorCopy = sectionTitle(ctx, "connections.connectors");

  return (
    <div className="space-y-5">
      {ctx.matches(status.title, STATUS_KEYWORDS) ? (
        <SettingsSection
          title={status.title}
          description={status.description}
          testId="settings-section-connections-status"
          actions={(
            <button type="button" onClick={onRerunSetup} className={SETTINGS_SECONDARY_BUTTON}>
              {tr("초기 설정 다시 하기", "Run setup again")}
            </button>
          )}
        >
          {statusRows.map((row) => <ConnectionStatusLine key={row.key} row={row} />)}
        </SettingsSection>
      ) : null}
      <SettingsRowSection
        ctx={ctx}
        panel="connections"
        section="channels"
        status={configStatus}
        extraFor={(meta) => <ChannelNameHint meta={meta} tr={tr} />}
      />
      {ctx.matches(connectorCopy.title, CONNECTOR_KEYWORDS) ? (
        <SettingsSection title={connectorCopy.title} description={connectorCopy.description} variant="plain" testId="settings-section-connections-connectors">
          <SettingsOperatorConnectorsPanel
            connectors={connectors.data}
            error={connectors.isError}
            loading={connectors.isFetching}
            onReload={() => void connectors.refetch()}
            isKo={ctx.isKo}
            tr={tr}
          />
        </SettingsSection>
      ) : null}
    </div>
  );
}

function ConnectionStatusLine({ row }: { row: ConnectionStatusRow }) {
  const color = row.tone === "ok" ? "var(--th-badge-emerald-text)" : row.tone === "warn" ? "var(--th-badge-amber-text)" : "var(--th-text-muted)";
  return (
    <div className="flex min-h-14 flex-wrap items-center justify-between gap-x-4 gap-y-1 py-3" data-testid={`settings-connection-${row.key}`} data-tone={row.tone}>
      <span className="text-sm font-medium text-th-text-primary">{row.label}</span>
      <span className="min-w-0 text-right">
        <span className="inline-flex items-center gap-1.5 text-sm break-keep" style={{ color }}>
          <span aria-hidden className="inline-block h-2 w-2 shrink-0 rounded-full" style={{ background: color }} />
          {row.value}
        </span>
        {row.detail ? <span className="block text-xs text-th-text-muted [overflow-wrap:anywhere]">{row.detail}</span> : null}
      </span>
    </div>
  );
}

function ChannelNameHint({ meta, tr }: { meta: SettingRowMeta; tr: SettingsPanelContext["tr"] }) {
  const id = String(meta.effectiveValue ?? "").trim();
  const valid = DISCORD_ID_PATTERN.test(id);
  const channel = useQuery({
    queryKey: ["settings", "discord-channel", id],
    queryFn: () => getSettingsChannelName(id),
    enabled: valid,
    staleTime: 5 * 60_000,
    retry: false,
  });
  let text: string;
  let warn = false;
  if (!id) text = tr("아직 채널을 정하지 않았어요.", "No channel chosen yet.");
  else if (!valid) {
    text = tr("채널 ID는 17~20자리 숫자예요.", "A channel ID is a 17–20 digit number.");
    warn = true;
  } else if (channel.isPending) text = tr("채널 이름을 확인하는 중…", "Looking up the channel…");
  else if (channel.isError || !channel.data) {
    text = tr("이 ID의 채널을 찾지 못했어요. 숫자를 다시 확인해 주세요.", "No channel found for this ID. Check the number.");
    warn = true;
  } else text = `#${channel.data}`;
  return (
    <p
      className="mt-1.5 text-xs leading-5 break-keep"
      style={{ color: warn ? "var(--th-badge-amber-text)" : "var(--th-text-muted)" }}
      data-testid={`settings-channel-name-${meta.key}`}
    >
      {text}
    </p>
  );
}
