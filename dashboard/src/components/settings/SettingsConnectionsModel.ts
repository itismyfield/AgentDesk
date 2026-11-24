import type { HealthResponse, OnboardingConnectionStatus, OperatorConnectorStatus, OperatorConnectorsResponse, PromptManifestRetentionStatus } from "../../api/settingsRuntime";
import { buildBottlenecks, buildSignalCards, formatBytes, formatDurationCompact, formatNumber, publicOnlyHealthNote, translateStatus } from "../OpsPageModel";
import { connectorReason, connectorSetupSteps, providerDisplayName, settingKeyMeta } from "./SettingsCopy";
import type { CatalogItem, StatusDetail, StatusTone } from "./SettingsModel";

type Tr = (ko: string, en: string) => string;
export type ConnectionTone = "ok" | "warn" | "unknown";
export interface ConnectionStatusRow { key: "bot" | "server" | "owner" | "ai"; label: string; value: string; detail?: string; tone: ConnectionTone }
export interface IntegrationCardSpec { id: string; name: string; enabled: boolean; count?: number; meta: string; pausedLabel: string; disabledReason?: string }
type Source<T> = { data: T | undefined; isError: boolean; isPending: boolean; dataUpdatedAt?: number; isFetching?: boolean; refetch?: () => unknown };

function pendingText(source: Source<unknown>, tr: Tr): string | null {
  return source.data ? null : source.isPending ? tr("확인 중…", "Checking…") : tr("확인하지 못했어요", "Could not check");
}

function freshness(source: Source<unknown>): Pick<StatusDetail, "updatedAt" | "failed" | "refresh"> {
  const { refetch } = source;
  return { updatedAt: source.dataUpdatedAt || undefined, failed: source.isError && Boolean(source.data), refresh: refetch ? { run: () => void refetch(), busy: Boolean(source.isFetching) } : undefined };
}

function unknownRow(key: ConnectionStatusRow["key"], label: string, source: Source<unknown>, tr: Tr): ConnectionStatusRow | null {
  const value = pendingText(source, tr);
  return value ? { key, label, value, tone: "unknown" } : null;
}

export function connectionStatusRows(onboarding: Source<OnboardingConnectionStatus>, health: Source<HealthResponse>, tr: Tr): ConnectionStatusRow[] {
  const botLabel = tr("Discord 봇", "Discord bot"), serverLabel = tr("Discord 서버", "Discord server"), ownerLabel = tr("소유자", "Owner"), aiLabel = tr("AI 서비스", "AI services");
  const limited = publicOnlyHealthNote(health.data, tr);
  const runtimeBots = health.data && !limited ? health.data.providers ?? [] : [];
  const connectedBots = runtimeBots.filter((b) => b.connected), offlineBots = runtimeBots.filter((b) => !b.connected);

  const bot = unknownRow("bot", botLabel, health, tr) ?? (() => {
    if (limited) return { key: "bot" as const, label: botLabel, value: tr("이 기기에서는 확인할 수 없어요", "Not visible from this device"), detail: limited, tone: "unknown" as const };
    const names = runtimeBots.map((b) => providerDisplayName(b.name)).join(", "), offline = offlineBots.map((b) => providerDisplayName(b.name));
    if (!runtimeBots.length) return { key: "bot" as const, label: botLabel, value: tr("연결된 봇이 없어요", "No bot connected"), tone: "warn" as const };
    if (offline.length) return { key: "bot" as const, label: botLabel, value: tr(`연결 끊김: ${offline.join(", ")}`, `Disconnected: ${offline.join(", ")}`), tone: "warn" as const };
    return { key: "bot" as const, label: botLabel, value: tr("연결됨", "Connected"), detail: names, tone: "ok" as const };
  })();

  const resolve = (key: "server" | "owner" | "ai", label: string): ConnectionStatusRow => {
    const data = onboarding.data;
    if (key === "server") {
      if (data?.guild_id) return { key, label, value: tr("연결됨", "Connected"), detail: tr(`서버 ID ${data.guild_id}`, `Server ID ${data.guild_id}`), tone: "ok" };
      if (connectedBots.length && !offlineBots.length) return { key, label, value: tr(`연결됨 · 봇 ${connectedBots.length}개 가동`, `Connected · ${connectedBots.length} bots active`), detail: tr("서버 설정·런타임 기준으로 동작 중", "Active via runtime configuration"), tone: "ok" };
      const pending = unknownRow(key, label, onboarding, tr);
      if (pending) return pending;
      if (connectedBots.length) return { key, label, value: tr(`일부 연결됨 · 봇 ${connectedBots.length}/${runtimeBots.length}개`, `Partially connected · ${connectedBots.length}/${runtimeBots.length} bots`), tone: "warn" };
      return { key, label, value: tr("아직 연결하지 않았어요", "Not connected yet"), tone: "warn" };
    }
    if (key === "owner") {
      if (data?.owner_id) return { key, label, value: tr("설정됨", "Set"), detail: tr(`사용자 ID ${data.owner_id}`, `User ID ${data.owner_id}`), tone: "ok" };
      if (connectedBots.length && !offlineBots.length) return { key, label, value: tr("서버 설정 기준 운영 중", "Managed in server config"), tone: "ok" };
      return unknownRow(key, label, onboarding, tr) ?? { key, label, value: tr("아직 정하지 않았어요", "Not set yet"), tone: "warn" };
    }
    const configured = [data?.bot_providers?.command, data?.bot_providers?.command2].filter((x): x is string => Boolean(x)).map(providerDisplayName);
    if (configured.length) return { key, label, value: tr(`${configured.join(", ")} 사용 중`, `Using ${configured.join(", ")}`), tone: "ok" };
    if (connectedBots.length) {
      const activeNames = connectedBots.map((b) => providerDisplayName(b.name)).join(", ");
      if (offlineBots.length) return { key, label, value: tr(`${activeNames} 가동 · 끊김: ${offlineBots.map((b) => providerDisplayName(b.name)).join(", ")}`, `${activeNames} active · Disconnected: ${offlineBots.map((b) => providerDisplayName(b.name)).join(", ")}`), tone: "warn" };
      return { key, label, value: tr(`${activeNames} 사용 중`, `Using ${activeNames}`), detail: tr("실시간 런타임 연결 기준", "Live runtime health"), tone: "ok" };
    }
    return unknownRow(key, label, onboarding, tr) ?? { key, label, value: tr("아직 고르지 않았어요", "Not chosen yet"), tone: "warn" };
  };

  return [bot, resolve("server", serverLabel), resolve("owner", ownerLabel), resolve("ai", aiLabel)];
}

export function buildIntegrationCards(onboarding: Source<OnboardingConnectionStatus>, health: Source<HealthResponse>, connectors: Source<OperatorConnectorsResponse>, tr: Tr, isKo: boolean): IntegrationCardSpec[] {
  const limited = publicOnlyHealthNote(health.data, tr);
  const bots = health.data && !limited ? health.data.providers ?? [] : [];
  const connected = bots.filter((b) => b.connected), offline = bots.filter((b) => !b.connected);
  const activeNames = connected.map((b) => providerDisplayName(b.name)).join(", ");
  const cards: IntegrationCardSpec[] = [
    {
      id: "discord-runtime", name: tr("Discord 서버 · 게이트웨이", "Discord server & gateway"),
      enabled: connected.length > 0 || Boolean(onboarding.data?.guild_id), count: connected.length || undefined,
      meta: connected.length > 0 ? tr(`${activeNames} 봇 연결됨${onboarding.data?.guild_id ? ` · 서버 ${onboarding.data.guild_id}` : " · 런타임 활성"}`, `${activeNames} connected${onboarding.data?.guild_id ? ` · Guild ${onboarding.data.guild_id}` : " · Runtime active"}`) : (onboarding.data?.guild_id ? tr(`서버 ID ${onboarding.data.guild_id}`, `Server ID ${onboarding.data.guild_id}`) : ""),
      pausedLabel: pendingText(health, tr) ?? tr("연결된 Discord 봇이 없어요", "No Discord bots connected"),
      disabledReason: offline.length > 0 ? tr(`연결 끊김: ${offline.map((b) => providerDisplayName(b.name)).join(", ")}`, `Disconnected: ${offline.map((b) => providerDisplayName(b.name)).join(", ")}`) : undefined,
    },
    {
      id: "ai-providers", name: tr("AI 프로바이더 런타임", "AI provider runtime"),
      enabled: connected.length > 0 || Boolean(onboarding.data?.bot_providers?.command),
      count: connected.reduce((s, b) => s + (b.sessions ?? 0), 0) || connected.length || undefined,
      meta: connected.length > 0 ? tr(`${activeNames} 사용 중 · 활성 턴 ${connected.reduce((s, b) => s + b.active_turns, 0)}개`, `Using ${activeNames} · ${connected.reduce((s, b) => s + b.active_turns, 0)} active turns`) : tr("온보딩 설정 완료", "Configured via setup"),
      pausedLabel: pendingText(health, tr) ?? tr("연결된 AI 프로바이더가 없어요", "No AI providers active"),
    },
  ];
  for (const connector of connectors.data?.connectors ?? []) {
    const metaInfo = settingKeyMeta(connector.id), ready = connector.state === "ready", setup = connectorSetupSteps(connector, isKo);
    const why = connector.reason ? connectorReason(connector.reason, tr) : connectorState(connector.state, tr);
    cards.push({
      id: connector.id, name: (isKo ? metaInfo?.labelKo : metaInfo?.labelEn) ?? connector.name, enabled: ready,
      count: connector.capabilities.length || undefined,
      meta: ready ? `${tr("준비됨", "Ready")}${connector.source ? ` · ${connector.source}` : ` · ${connector.env_var}`}` : why,
      pausedLabel: `${connectorState(connector.state, tr)} · ${why}`,
      disabledReason: ready ? undefined : (setup.steps[0] || connector.env_var),
    });
  }
  return cards;
}

const ROW_KEYS: Record<ConnectionStatusRow["key"], string> = { bot: "health.providers", server: "onboarding.guild_id", owner: "onboarding.owner_id", ai: "onboarding.bot_providers" };

function statusItem(key: string, label: string | undefined, saved: string, status: StatusDetail, isKo: boolean): CatalogItem {
  const meta = settingKeyMeta(key);
  return { key, source: "status", category: meta?.category ?? "connections", label: label ?? (isKo ? meta?.labelKo : meta?.labelEn) ?? key, description: (isKo ? meta?.descKo : meta?.descEn) ?? "", input: "status", saved, draft: null, baseline: null, baselineSource: null, overridden: false, restartNeeded: false, applies: "status", related: [], dangerous: false, status };
}

const connectorTone = (s: string): StatusTone => (s === "ready" ? "ok" : s === "skipped" || s === "missing_config" ? "unknown" : "warn");
function connectorState(s: string, tr: Tr): string {
  return s === "ready" ? tr("준비됨", "Ready") : s === "skipped" ? tr("꺼짐", "Off") : s === "missing_config" ? tr("설정 안 함", "Not set up") : s === "missing_path" ? tr("폴더 없음", "Folder missing") : s === "missing_provider" ? tr("제공자 없음", "Provider missing") : tr("설정 오류", "Setup error");
}

function connectorItem(connector: OperatorConnectorStatus, tr: Tr, isKo: boolean): CatalogItem {
  const setup = connectorSetupSteps(connector, isKo), facts: Array<[string, string]> = [];
  if (connector.reason && connector.state !== "ready") facts.push([tr("이유", "Reason"), connectorReason(connector.reason, tr)]);
  facts.push([tr("환경 변수", "Environment variable"), connector.env_var]);
  if (connector.source) facts.push([tr("읽는 곳", "Reads from"), connector.source]);
  return statusItem(connector.id, settingKeyMeta(connector.id) ? undefined : connector.name, connectorState(connector.state, tr), { tone: connectorTone(connector.state), facts, steps: setup.steps.length > 0 ? { lines: setup.steps, translated: setup.translated } : undefined }, isKo);
}

export interface ConnectionSources { statusRows: ConnectionStatusRow[]; health: Source<HealthResponse>; retention: Source<PromptManifestRetentionStatus>; connectors: Source<OperatorConnectorsResponse> }

export function connectionItems({ statusRows, health, retention, connectors }: ConnectionSources, tr: Tr, isKo: boolean): CatalogItem[] {
  const data = health.data, limited = publicOnlyHealthNote(data, tr);
  const limitedFacts: Array<[string, string]> = limited ? [[tr("자세한 지표", "Details"), limited]] : [];
  const healthFacts: StatusDetail["facts"] = data && !limited ? buildSignalCards(data, isKo).map((s) => [s.label, s.value, s.note]) : limitedFacts;
  if (data?.uptime_secs) healthFacts.push([tr("가동 시간", "Uptime"), formatDurationCompact(data.uptime_secs, isKo)]);
  const items = [statusItem("health.status", undefined, pendingText(health, tr) ?? translateStatus(data?.status ?? "", isKo), {
    tone: !data ? "unknown" : data.status === "healthy" ? "ok" : "warn", facts: healthFacts,
    notes: data ? buildBottlenecks(data, isKo).map((r) => ({ title: r.label, detail: r.detail, tone: r.severity === "danger" ? "danger" as const : "warn" as const, tooltip: r.kind, count: r.count })) : [],
    ...freshness(health),
  }, isKo)];
  for (const row of statusRows) {
    const providers = row.key === "bot" && data && !limited ? data.providers ?? [] : [];
    const facts: Array<[string, string]> = providers.length > 0
      ? providers.map((p) => [providerDisplayName(p.name), !p.connected ? tr("끊김", "Disconnected") : p.restart_pending ? tr("연결됨 · 재시작 대기", "Connected · restart pending") : tr("연결됨", "Connected")])
      : row.detail ? [[tr("자세히", "Detail"), row.detail]] : [];
    items.push(statusItem(ROW_KEYS[row.key], row.label, row.value, { tone: row.tone, facts }, isKo));
  }
  const outbox = data?.dispatch_outbox, pending = formatNumber(outbox?.pending ?? 0);
  items.push(statusItem("health.dispatch_outbox", undefined, pendingText(health, tr) ?? (limited ? "—" : tr(`${pending}건`, `${pending} pending`)), {
    tone: !data || limited ? "unknown" : (outbox?.permanent_failures ?? 0) > 0 ? "warn" : "ok",
    facts: !data || limited ? limitedFacts : [[tr("재시도 중", "Retrying"), formatNumber(outbox?.retrying ?? 0)], [tr("실패", "Failed"), formatNumber(outbox?.permanent_failures ?? 0)], [tr("가장 오래 기다린 항목", "Oldest pending"), formatDurationCompact(outbox?.oldest_pending_age ?? 0, isKo)]],
  }, isKo));
  const kept = retention.data, restartNeeded = kept?.restart_required_for_config_changes ?? true;
  items.push({
    ...statusItem("prompt_manifest_retention", undefined, pendingText(retention, tr) ?? (kept?.enabled ? tr(`${formatNumber(kept.retention_days)}일`, `${formatNumber(kept.retention_days)} days`) : tr("꺼짐", "Off")), {
      tone: kept ? "ok" : "unknown",
      facts: kept ? [[tr("저장 크기", "Stored"), formatBytes(kept.total_stored_bytes)], [tr("조각", "Layers"), formatNumber(kept.layer_count)], [tr("잘린 조각", "Truncated"), formatNumber(kept.truncated_count)]] : [],
      ...freshness(retention),
    }, isKo),
    input: "readonly", applies: restartNeeded ? "config-only" : "config-file", restartNeeded: Boolean(kept) && restartNeeded,
  });
  for (const connector of connectors.data?.connectors ?? []) items.push(connectorItem(connector, tr, isKo));
  return items;
}
