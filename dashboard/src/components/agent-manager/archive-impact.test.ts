import { describe, expect, it } from "vitest";

import {
  archiveBlockedByActiveTurn,
  resolveArchiveChannelImpact,
} from "./archive-impact";

describe("resolveArchiveChannelImpact", () => {
  it("returns empty list when no channel ids are bound", () => {
    expect(
      resolveArchiveChannelImpact({
        discord_channel_id: null,
        discord_channel_id_alt: null,
        discord_channel_id_codex: null,
      }),
    ).toEqual([]);
  });

  it("returns each populated channel with its role", () => {
    expect(
      resolveArchiveChannelImpact({
        discord_channel_id: "111",
        discord_channel_id_alt: "222",
        discord_channel_id_codex: "333",
      }),
    ).toEqual([
      { id: "111", role: "primary" },
      { id: "222", role: "alt" },
      { id: "333", role: "codex" },
    ]);
  });

  it("de-duplicates channels that appear in multiple slots", () => {
    expect(
      resolveArchiveChannelImpact({
        discord_channel_id: "111",
        discord_channel_id_alt: "111",
        discord_channel_id_codex: "222",
      }),
    ).toEqual([
      { id: "111", role: "primary" },
      { id: "222", role: "codex" },
    ]);
  });

  it("trims whitespace and skips blank entries", () => {
    expect(
      resolveArchiveChannelImpact({
        discord_channel_id: "  444  ",
        discord_channel_id_alt: "   ",
        discord_channel_id_codex: undefined,
      }),
    ).toEqual([{ id: "444", role: "primary" }]);
  });
});

describe("archiveBlockedByActiveTurn", () => {
  it("flags working agents as blocked", () => {
    expect(archiveBlockedByActiveTurn({ status: "working" })).toBe(true);
  });

  it("allows archiving for idle / break / offline / archived agents", () => {
    for (const status of ["idle", "break", "offline", "archived"] as const) {
      expect(archiveBlockedByActiveTurn({ status })).toBe(false);
    }
  });
});
