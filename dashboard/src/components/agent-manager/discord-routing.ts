import type { AutoQueueThreadLink, DiscordChannelInfo } from "../../api";
import type { DiscordBinding } from "../../api/client";
import type { DispatchedSession } from "../../types";

const PROVIDER_PREFIXES = [
  "claude",
  "codex",
  "gemini",
  "qwen",
  "copilot",
  "opencode",
  "antigravity",
  "api",
] as const;

export interface DiscordTargetSummary {
  title: string;
  subtitle: string | null;
  webUrl: string | null;
  deepLink: string | null;
}

export function isDiscordSnowflake(value: string | null | undefined): value is string {
  return Boolean(value && /^\d{15,}$/.test(value));
}

export function buildDiscordChannelLinks(
  channelId: string | null | undefined,
  guildId: string | null | undefined,
): Pick<DiscordTargetSummary, "webUrl" | "deepLink"> {
  if (!channelId || !guildId) {
    return {
      webUrl: null,
      deepLink: null,
    };
  }
  return {
    webUrl: `https://discord.com/channels/${guildId}/${channelId}`,
    deepLink: `discord://discord.com/channels/${guildId}/${channelId}`,
  };
}

export function buildDiscordThreadLinks(
  link: Pick<AutoQueueThreadLink, "url">,
): Pick<DiscordTargetSummary, "webUrl" | "deepLink"> {
  if (!link.url) {
    return {
      webUrl: null,
      deepLink: null,
    };
  }

  const match = link.url.match(/^https:\/\/discord\.com\/channels\/([^/]+)\/([^/]+)$/);
  return {
    webUrl: link.url,
    deepLink: match ? `discord://discord.com/channels/${match[1]}/${match[2]}` : null,
  };
}

export function parseChannelNameFromSessionKey(
  sessionKey: string | null | undefined,
): string | null {
  if (!sessionKey) return null;
  const tmuxName = sessionKey.includes(":")
    ? sessionKey.slice(sessionKey.indexOf(":") + 1)
    : sessionKey;
  const withoutAgentDeskPrefix = tmuxName.startsWith("AgentDesk-")
    ? tmuxName.slice("AgentDesk-".length)
    : tmuxName;

  for (const provider of PROVIDER_PREFIXES) {
    const prefix = `${provider}-`;
    if (withoutAgentDeskPrefix.startsWith(prefix)) {
      return withoutAgentDeskPrefix.slice(prefix.length) || null;
    }
  }

  return withoutAgentDeskPrefix !== tmuxName ? withoutAgentDeskPrefix : null;
}

function formatFallbackDiscordName(value: string | null | undefined): string {
  if (!value) return "Discord";
  if (value.startsWith("dm:")) return value;
  if (/^\d{15,}$/.test(value)) return value;
  return value.startsWith("#") ? value : `#${value}`;
}

export function describeDiscordTarget(
  rawValue: string | null | undefined,
  channelInfo?: DiscordChannelInfo | null,
  parentInfo?: DiscordChannelInfo | null,
  fallbackName?: string | null,
): DiscordTargetSummary {
  if (channelInfo?.id) {
    const isThread = Boolean(channelInfo.parent_id);
    const title = channelInfo.name
      ? (isThread ? channelInfo.name : `#${channelInfo.name}`)
      : formatFallbackDiscordName(fallbackName ?? rawValue);
    const subtitle = parentInfo?.name ? `#${parentInfo.name}` : null;
    return {
      title,
      subtitle,
      ...buildDiscordChannelLinks(channelInfo.id, channelInfo.guild_id),
    };
  }

  return {
    title: formatFallbackDiscordName(fallbackName ?? rawValue),
    subtitle: null,
    webUrl: null,
    deepLink: null,
  };
}

export function describeDiscordBinding(
  binding: Pick<DiscordBinding, "channelId">,
  channelInfo?: DiscordChannelInfo | null,
  parentInfo?: DiscordChannelInfo | null,
): DiscordTargetSummary {
  return describeDiscordTarget(binding.channelId, channelInfo, parentInfo);
}

export function describeDispatchedSession(
  session: Pick<DispatchedSession, "thread_channel_id" | "session_key" | "name">,
  channelInfo?: DiscordChannelInfo | null,
  parentInfo?: DiscordChannelInfo | null,
): DiscordTargetSummary {
  const fallbackName =
    parseChannelNameFromSessionKey(session.session_key)
    ?? session.name
    ?? session.session_key;
  return describeDiscordTarget(
    session.thread_channel_id ?? null,
    channelInfo,
    parentInfo,
    fallbackName,
  );
}

export function formatDiscordSummary(summary: Pick<DiscordTargetSummary, "title" | "subtitle">): string {
  return summary.subtitle ? `${summary.title} · ${summary.subtitle}` : summary.title;
}
