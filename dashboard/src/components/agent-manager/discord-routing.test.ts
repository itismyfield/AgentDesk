import { describe, expect, it } from "vitest";

import {
  buildDiscordChannelLinks,
  buildDiscordThreadLinks,
  describeDiscordTarget,
  describeDispatchedSession,
  formatDiscordSummary,
  parseChannelNameFromSessionKey,
} from "./discord-routing";

describe("discord-routing", () => {
  it("builds Discord web and app links", () => {
    expect(buildDiscordChannelLinks("456", "123")).toEqual({
      webUrl: "https://discord.com/channels/123/456",
      deepLink: "discord://discord.com/channels/123/456",
    });
  });

  it("formats a resolved thread with parent channel context", () => {
    const summary = describeDiscordTarget(
      "456",
      {
        id: "456",
        guild_id: "123",
        name: "리뷰 스레드",
        parent_id: "789",
      },
      {
        id: "789",
        guild_id: "123",
        name: "adk-cdx",
      },
    );

    expect(summary.title).toBe("리뷰 스레드");
    expect(summary.subtitle).toBe("#adk-cdx");
    expect(summary.deepLink).toBe("discord://discord.com/channels/123/456");
  });

  it("builds Discord thread web and app links from auto-queue thread URLs", () => {
    expect(
      buildDiscordThreadLinks({
        url: "https://discord.com/channels/123/456",
      }),
    ).toEqual({
      webUrl: "https://discord.com/channels/123/456",
      deepLink: "discord://discord.com/channels/123/456",
    });
  });

  it("parses tmux-backed session keys into channel names", () => {
    expect(
      parseChannelNameFromSessionKey("host:AgentDesk-codex-adk-cdx-t1485400795435372796"),
    ).toBe("adk-cdx-t1485400795435372796");
  });

  it("uses parsed channel names as session fallback text", () => {
    const summary = describeDispatchedSession({
      thread_channel_id: null,
      session_key: "host:AgentDesk-claude-agentdesk-cc",
      name: null,
    } as any);

    expect(formatDiscordSummary(summary)).toBe("#agentdesk-cc");
    expect(summary.webUrl).toBeNull();
  });

  /* Issue #1241: the dashboard must consume the canonical deeplink_url +
     thread_deeplink_url fields the backend now ships and paste them straight
     into anchor `href`s, without rebuilding URLs client-side. */
  it("prefers canonical deeplink_url + thread_deeplink_url over legacy aliases", () => {
    const summary = describeDispatchedSession({
      thread_channel_id: "456",
      channel_id: "456",
      thread_id: "456",
      session_key: "host:AgentDesk-claude-channel-x",
      name: null,
      guild_id: "123",
      deeplink_url: "https://discord.com/channels/123/456",
      thread_deeplink_url: "discord://discord.com/channels/123/456",
      // Legacy fields intentionally point at a different (stale) URL pair so
      // we can prove the canonical fields win.
      channel_web_url: "https://discord.com/channels/999/legacy",
      channel_deeplink_url: "discord://discord.com/channels/999/legacy",
    } as any);

    expect(summary.webUrl).toBe("https://discord.com/channels/123/456");
    expect(summary.deepLink).toBe("discord://discord.com/channels/123/456");
  });

  it("falls back to legacy channel_web_url / channel_deeplink_url when canonical fields are absent", () => {
    const summary = describeDispatchedSession({
      thread_channel_id: "456",
      session_key: "host:AgentDesk-claude-channel-x",
      name: null,
      guild_id: "123",
      channel_web_url: "https://discord.com/channels/123/456",
      channel_deeplink_url: "discord://discord.com/channels/123/456",
    } as any);

    expect(summary.webUrl).toBe("https://discord.com/channels/123/456");
    expect(summary.deepLink).toBe("discord://discord.com/channels/123/456");
  });
});
