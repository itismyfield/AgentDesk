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
  session: Pick<
    DispatchedSession,
    | "thread_channel_id"
    | "session_key"
    | "name"
    | "guild_id"
    | "channel_web_url"
    | "channel_deeplink_url"
    | "channel_id"
    | "thread_id"
    | "deeplink_url"
    | "thread_deeplink_url"
  >,
  channelInfo?: DiscordChannelInfo | null,
  parentInfo?: DiscordChannelInfo | null,
): DiscordTargetSummary {
  const fallbackName =
    parseChannelNameFromSessionKey(session.session_key)
    ?? session.name
    ?? session.session_key;
  // Issue #1241: prefer the canonical channel_id / thread_id aliases the
  // backend now returns. Fall back to thread_channel_id so older server
  // builds (or fixtures that only set the legacy field) still resolve.
  const channelId =
    session.channel_id
    ?? session.thread_id
    ?? session.thread_channel_id
    ?? null;
  const summary = describeDiscordTarget(
    channelId,
    channelInfo,
    parentInfo,
    fallbackName,
  );

  // Backend agents.rs now returns canonical deeplink_url + thread_deeplink_url
  // alongside the legacy channel_web_url / channel_deeplink_url pair. Use the
  // new names first (issue #1241 contract: dashboard pastes the value into
  // anchor `href` without rebuilding URLs); fall back to the legacy fields so
  // older server builds keep working.
  if (!summary.webUrl) {
    summary.webUrl = session.deeplink_url ?? session.channel_web_url ?? null;
  }
  if (!summary.deepLink) {
    summary.deepLink =
      session.thread_deeplink_url ?? session.channel_deeplink_url ?? null;
  }
  if (!summary.webUrl && !summary.deepLink && channelId && session.guild_id) {
    const links = buildDiscordChannelLinks(channelId, session.guild_id);
    summary.webUrl = links.webUrl;
    summary.deepLink = links.deepLink;
  }

  return summary;
}

export function formatDiscordSummary(summary: Pick<DiscordTargetSummary, "title" | "subtitle">): string {
  return summary.subtitle ? `${summary.title} · ${summary.subtitle}` : summary.title;
}
