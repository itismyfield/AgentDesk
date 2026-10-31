import { describe, expect, it } from "vitest";
import type { HealthResponse, OnboardingConnectionStatus } from "../../api/settingsRuntime";
import { connectionStatusRows } from "./SettingsConnectionsModel";

const tr = (ko: string) => ko;
const ready = <T,>(data: T) => ({ data, isError: false, isPending: false });
const bot = (name: string, connected: boolean) => ({
  name, connected, active_turns: 0, queue_depth: 0, sessions: 0, restart_pending: false, last_turn_at: null,
});

describe("connection status rows", () => {
  it("reports the onboarding setup as read-only status", () => {
    const rows = connectionStatusRows(
      ready<OnboardingConnectionStatus>({ guild_id: "1", owner_id: "2", bot_providers: { command: "claude", command2: "codex" } }),
      ready<HealthResponse>({ status: "healthy", providers: [bot("claude", true), bot("codex", true)] }),
      tr,
    );
    expect(rows.map((row) => [row.key, row.value, row.tone])).toEqual([
      ["bot", "연결됨", "ok"],
      ["server", "연결됨", "ok"],
      ["owner", "설정됨", "ok"],
      ["ai", "Claude, Codex 사용 중", "ok"],
    ]);
  });

  it("flags missing setup and disconnected bots, and never claims success when unknown", () => {
    const rows = connectionStatusRows(
      ready<OnboardingConnectionStatus>({ guild_id: null, owner_id: null, bot_providers: {} }),
      ready<HealthResponse>({ status: "degraded", providers: [bot("claude", true), bot("codex", false)] }),
      tr,
    );
    expect(rows.map((row) => row.tone)).toEqual(["warn", "warn", "warn", "warn"]);
    expect(rows[0].value).toBe("연결 끊김: Codex");

    const unknown = connectionStatusRows(
      { data: undefined, isError: true, isPending: false },
      { data: undefined, isError: false, isPending: true },
      tr,
    );
    expect(unknown.map((row) => [row.value, row.tone])).toEqual([
      ["확인 중…", "unknown"],
      ["확인하지 못했어요", "unknown"],
      ["확인하지 못했어요", "unknown"],
      ["확인하지 못했어요", "unknown"],
    ]);
  });
});
