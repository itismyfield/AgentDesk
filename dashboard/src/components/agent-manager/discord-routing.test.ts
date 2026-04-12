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
});
