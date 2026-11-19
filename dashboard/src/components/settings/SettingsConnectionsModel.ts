import type {
  HealthResponse,
  OnboardingConnectionStatus,
  OperatorConnectorStatus,
  OperatorConnectorsResponse,
  PromptManifestRetentionStatus,
} from "../../api/settingsRuntime";
import {
  buildBottlenecks,
  buildSignalCards,
  formatBytes,
  formatDurationCompact,
  formatNumber,
  publicOnlyHealthNote,
  translateStatus,
} from "../OpsPageModel";
import { connectorReason, connectorSetupSteps, providerDisplayName, settingKeyMeta } from "./SettingsCopy";
import type { CatalogItem, StatusDetail, StatusTone } from "./SettingsModel";

type Tr = (ko: string, en: string) => string;

export type ConnectionTone = "ok" | "warn" | "unknown";

export interface ConnectionStatusRow {
  key: "bot" | "server" | "owner" | "ai";
  label: string;
  value: string;
  detail?: string;
  tone: ConnectionTone;
}

type Source<T> = { data: T | undefined; isError: boolean; isPending: boolean };

/** "Checking…" or "Could not check" while a source has no data, else null. */
function pendingText(source: Source<unknown>, tr: Tr): string | null {
  if (source.isPending) return tr("확인 중…", "Checking…");
  return source.isError || !source.data ? tr("확인하지 못했어요", "Could not check") : null;
}

function unknownRow(key: ConnectionStatusRow["key"], label: string, source: Source<unknown>, tr: Tr): ConnectionStatusRow | null {
  const value = pendingText(source, tr);
  return value ? { key, label, value, tone: "unknown" } : null;
}

/** Read-only rows for the bot, server, owner and AI services set up during onboarding. */
export function connectionStatusRows(
  onboarding: Source<OnboardingConnectionStatus>,
  health: Source<HealthResponse>,
  tr: Tr,
): ConnectionStatusRow[] {
  const botLabel = tr("Discord 봇", "Discord bot");
  const serverLabel = tr("Discord 서버", "Discord server");
  const ownerLabel = tr("소유자", "Owner");
  const aiLabel = tr("AI 서비스", "AI services");

  const bot = unknownRow("bot", botLabel, health, tr) ?? (() => {
    const limited = publicOnlyHealthNote(health.data, tr);
    if (limited) return { key: "bot" as const, label: botLabel, value: tr("이 기기에서는 확인할 수 없어요", "Not visible from this device"), detail: limited, tone: "unknown" as const };
    const bots = health.data?.providers ?? [];
    const names = bots.map((item) => providerDisplayName(item.name)).join(", ");
    const offline = bots.filter((item) => !item.connected).map((item) => providerDisplayName(item.name));
    if (bots.length === 0) return { key: "bot" as const, label: botLabel, value: tr("연결된 봇이 없어요", "No bot connected"), tone: "warn" as const };
    if (offline.length > 0) {
      return { key: "bot" as const, label: botLabel, value: tr(`연결 끊김: ${offline.join(", ")}`, `Disconnected: ${offline.join(", ")}`), tone: "warn" as const };
    }
    return { key: "bot" as const, label: botLabel, value: tr("연결됨", "Connected"), detail: names, tone: "ok" as const };
  })();

  const fromOnboarding = (key: "server" | "owner" | "ai", label: string): ConnectionStatusRow => {
    const pending = unknownRow(key, label, onboarding, tr);
    if (pending) return pending;
    const data = onboarding.data ?? {};
    if (key === "server") {
      return data.guild_id
        ? { key, label, value: tr("연결됨", "Connected"), detail: tr(`서버 ID ${data.guild_id}`, `Server ID ${data.guild_id}`), tone: "ok" }
        : { key, label, value: tr("아직 연결하지 않았어요", "Not connected yet"), tone: "warn" };
    }
    if (key === "owner") {
      return data.owner_id
        ? { key, label, value: tr("설정됨", "Set"), detail: tr(`사용자 ID ${data.owner_id}`, `User ID ${data.owner_id}`), tone: "ok" }
        : { key, label, value: tr("아직 정하지 않았어요", "Not set yet"), tone: "warn" };
    }
    const providers = [data.bot_providers?.command, data.bot_providers?.command2]
      .filter((item): item is string => Boolean(item))
      .map(providerDisplayName);
    return providers.length > 0
      ? { key, label, value: tr(`${providers.join(", ")} 사용 중`, `Using ${providers.join(", ")}`), tone: "ok" }
      : { key, label, value: tr("아직 고르지 않았어요", "Not chosen yet"), tone: "warn" };
  };

  return [bot, fromOnboarding("server", serverLabel), fromOnboarding("owner", ownerLabel), fromOnboarding("ai", aiLabel)];
}

/** Connectors that are set up but broken; optional ones left off do not count. */
export function connectorsNeedingSetup(connectors: OperatorConnectorsResponse | undefined): number {
  return connectors?.summary?.invalid ?? 0;
}

const ROW_KEYS: Record<ConnectionStatusRow["key"], string> = {
  bot: "health.providers",
  server: "onboarding.guild_id",
  owner: "onboarding.owner_id",
  ai: "onboarding.bot_providers",
};

function statusItem(key: string, label: string | undefined, saved: string, status: StatusDetail, isKo: boolean): CatalogItem {
  const meta = settingKeyMeta(key);
  return {
    key,
    source: "status",
    category: meta?.category ?? "connections",
    label: label ?? (isKo ? meta?.labelKo : meta?.labelEn) ?? key,
    description: (isKo ? meta?.descKo : meta?.descEn) ?? "",
    input: "status",
    saved,
    draft: null,
    baseline: null,
    baselineSource: null,
    overridden: false,
    restartNeeded: false,
    applies: "status",
    related: [],
    dangerous: false,
    status,
  };
}

function connectorTone(state: string): StatusTone {
  if (state === "ready") return "ok";
  return state === "skipped" || state === "missing_config" ? "unknown" : "warn";
}

function connectorState(state: string, tr: Tr): string {
  if (state === "ready") return tr("준비됨", "Ready");
  if (state === "skipped") return tr("꺼짐", "Off");
  if (state === "missing_config") return tr("설정 안 함", "Not set up");
  if (state === "missing_path") return tr("폴더 없음", "Folder missing");
  if (state === "missing_provider") return tr("제공자 없음", "Provider missing");
  return tr("설정 오류", "Setup error");
}

function connectorItem(connector: OperatorConnectorStatus, tr: Tr, isKo: boolean): CatalogItem {
  const setup = connectorSetupSteps(connector, isKo);
  const facts: Array<[string, string]> = [];
  if (connector.reason && connector.state !== "ready") facts.push([tr("이유", "Reason"), connectorReason(connector.reason, tr)]);
  facts.push([tr("환경 변수", "Environment variable"), connector.env_var]);
  if (connector.source) facts.push([tr("읽는 곳", "Reads from"), connector.source]);
  return statusItem(connector.id, settingKeyMeta(connector.id) ? undefined : connector.name, connectorState(connector.state, tr), {
    tone: connectorTone(connector.state),
    facts,
    steps: setup.steps.length > 0 ? { lines: setup.steps, translated: setup.translated } : undefined,
  }, isKo);
}

export interface ConnectionSources {
  statusRows: ConnectionStatusRow[];
  health: Source<HealthResponse>;
  retention: Source<PromptManifestRetentionStatus>;
  connectors: Source<OperatorConnectorsResponse>;
}

/** Catalog rows for what the ops page showed (health, outbox, prompt retention) plus setup status and connectors. */
export function connectionItems({ statusRows, health, retention, connectors }: ConnectionSources, tr: Tr, isKo: boolean): CatalogItem[] {
  const data = health.data;
  const limited = publicOnlyHealthNote(data, tr);
  const limitedFacts: Array<[string, string]> = limited ? [[tr("자세한 지표", "Details"), limited]] : [];
  const healthFacts: Array<[string, string]> = data && !limited
    ? buildSignalCards(data, isKo).map((signal) => [signal.label, signal.value])
    : limitedFacts;
  if (data?.uptime_secs) healthFacts.push([tr("가동 시간", "Uptime"), formatDurationCompact(data.uptime_secs, isKo)]);
  const items = [statusItem("health.status", undefined, pendingText(health, tr) ?? translateStatus(data?.status ?? "", isKo), {
    tone: !data ? "unknown" : data.status === "healthy" ? "ok" : "warn",
    facts: healthFacts,
    notes: data ? buildBottlenecks(data, isKo).map((row) => ({ title: row.label, detail: row.detail, tone: row.severity === "danger" ? "danger" as const : "warn" as const, tooltip: row.kind })) : [],
  }, isKo)];

  for (const row of statusRows) {
    const providers = row.key === "bot" && data && !limited ? data.providers ?? [] : [];
    const facts: Array<[string, string]> = providers.length > 0
      ? providers.map((provider) => [providerDisplayName(provider.name), !provider.connected
        ? tr("끊김", "Disconnected")
        : provider.restart_pending ? tr("연결됨 · 재시작 대기", "Connected · restart pending") : tr("연결됨", "Connected")])
      : row.detail ? [[tr("자세히", "Detail"), row.detail]] : [];
    items.push(statusItem(ROW_KEYS[row.key], row.label, row.value, { tone: row.tone, facts }, isKo));
  }

  const outbox = data?.dispatch_outbox;
  const pending = formatNumber(outbox?.pending ?? 0);
  items.push(statusItem("health.dispatch_outbox", undefined, pendingText(health, tr) ?? (limited ? "—" : tr(`${pending}건`, `${pending} pending`)), {
    tone: !data || limited ? "unknown" : (outbox?.permanent_failures ?? 0) > 0 ? "warn" : "ok",
    facts: !data || limited ? limitedFacts : [
      [tr("재시도 중", "Retrying"), formatNumber(outbox?.retrying ?? 0)],
      [tr("실패", "Failed"), formatNumber(outbox?.permanent_failures ?? 0)],
      [tr("가장 오래 기다린 항목", "Oldest pending"), formatDurationCompact(outbox?.oldest_pending_age ?? 0, isKo)],
    ],
  }, isKo));

  const kept = retention.data;
  const restartNeeded = kept?.restart_required_for_config_changes ?? true;
  items.push({
    ...statusItem("prompt_manifest_retention", undefined, pendingText(retention, tr)
      ?? (kept?.enabled ? tr(`${formatNumber(kept.retention_days)}일`, `${formatNumber(kept.retention_days)} days`) : tr("꺼짐", "Off")), {
      tone: kept ? "ok" : "unknown",
      facts: kept ? [
        [tr("저장 크기", "Stored"), formatBytes(kept.total_stored_bytes)],
        [tr("조각", "Layers"), formatNumber(kept.layer_count)],
        [tr("잘린 조각", "Truncated"), formatNumber(kept.truncated_count)],
      ] : [],
    }, isKo),
    input: "readonly",
    applies: restartNeeded ? "config-only" : "config-file",
    restartNeeded: Boolean(kept) && restartNeeded,
  });

  for (const connector of connectors.data?.connectors ?? []) items.push(connectorItem(connector, tr, isKo));
  return items;
}
