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

function normalizeDiscordSnowflake(value: string | null | undefined): string | null {
  const normalized = value?.trim();
  return isDiscordSnowflake(normalized) ? normalized : null;
}

function parseDiscordChannelUrl(
  value: string | null | undefined,
  pattern: RegExp,
): { guildId: string; channelId: string } | null {
  const match = value?.trim().match(pattern);
  if (!match) return null;
  const guildId = normalizeDiscordSnowflake(match[1]);
  const channelId = normalizeDiscordSnowflake(match[2]);
  return guildId && channelId ? { guildId, channelId } : null;
}

function formatDiscordWebUrl(guildId: string, channelId: string): string {
  return `https://discord.com/channels/${guildId}/${channelId}`;
}

function formatDiscordDeepLink(guildId: string, channelId: string): string {
  return `discord://discord.com/channels/${guildId}/${channelId}`;
}

function normalizeDiscordWebUrl(value: string | null | undefined): string | null {
  const parsed = parseDiscordChannelUrl(
    value,
    /^https:\/\/discord\.com\/channels\/([^/]+)\/([^/]+)$/,
  );
  return parsed ? formatDiscordWebUrl(parsed.guildId, parsed.channelId) : null;
}

function normalizeDiscordDeepLink(value: string | null | undefined): string | null {
  const parsed = parseDiscordChannelUrl(
    value,
    /^discord:\/\/discord\.com\/channels\/([^/]+)\/([^/]+)$/,
  );
  return parsed ? formatDiscordDeepLink(parsed.guildId, parsed.channelId) : null;
}

export function buildDiscordChannelLinks(
  channelId: string | null | undefined,
  guildId: string | null | undefined,
): Pick<DiscordTargetSummary, "webUrl" | "deepLink"> {
  const normalizedChannelId = normalizeDiscordSnowflake(channelId);
  const normalizedGuildId = normalizeDiscordSnowflake(guildId);
  if (!normalizedChannelId || !normalizedGuildId) {
    return {
      webUrl: null,
      deepLink: null,
    };
  }
  return {
    webUrl: formatDiscordWebUrl(normalizedGuildId, normalizedChannelId),
    deepLink: formatDiscordDeepLink(normalizedGuildId, normalizedChannelId),
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

  const parsed = parseDiscordChannelUrl(
    link.url,
    /^https:\/\/discord\.com\/channels\/([^/]+)\/([^/]+)$/,
  );
  return {
    webUrl: parsed ? formatDiscordWebUrl(parsed.guildId, parsed.channelId) : null,
    deepLink: parsed ? formatDiscordDeepLink(parsed.guildId, parsed.channelId) : null,
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

  // #1241 contract: agents.rs returns canonical `deeplink_url` and
  // `thread_deeplink_url` (with legacy `channel_web_url` / `channel_deeplink_url`
  // kept for older server builds). The dashboard MUST paste these canonical
  // values into anchor `href` instead of rebuilding URLs from `channelInfo`.
  // Codex P2 on #1295: previously these guards were `if (!summary.webUrl)`,
  // which skipped the canonical fields whenever `describeDiscordTarget` had
  // already populated a rebuilt URL — defeating the contract. Prefer
  // canonical fields first, fall back to the rebuilt summary only when the
  // backend didn't supply a value.
  const canonicalWebUrl = normalizeDiscordWebUrl(
    session.deeplink_url ?? session.channel_web_url ?? null,
  );
  const canonicalDeepLink = normalizeDiscordDeepLink(
    session.thread_deeplink_url ?? session.channel_deeplink_url ?? null,
  );
  if (canonicalWebUrl) {
    summary.webUrl = canonicalWebUrl;
  }
  if (canonicalDeepLink) {
    summary.deepLink = canonicalDeepLink;
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
