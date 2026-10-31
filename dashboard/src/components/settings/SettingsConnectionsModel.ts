import type { HealthResponse, OnboardingConnectionStatus, OperatorConnectorsResponse } from "../../api/settingsRuntime";
import { providerDisplayName } from "./SettingsCopy";

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

function unknownRow(key: ConnectionStatusRow["key"], label: string, source: Source<unknown>, tr: Tr): ConnectionStatusRow | null {
  if (source.isPending) return { key, label, value: tr("확인 중…", "Checking…"), tone: "unknown" };
  if (source.isError || !source.data) return { key, label, value: tr("확인하지 못했어요", "Could not check"), tone: "unknown" };
  return null;
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
