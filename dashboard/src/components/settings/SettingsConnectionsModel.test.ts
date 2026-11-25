import { describe, expect, it } from "vitest";
import type { HealthResponse, OnboardingConnectionStatus } from "../../api/settingsRuntime";
import { buildConnectionSignalRows, connectionItems, connectionStatusRows, describeDegradedReason } from "./SettingsConnectionsModel";

const tr = (ko: string) => ko;
const ready = <T,>(data: T) => ({ data, isError: false, isPending: false });
const bot = (name: string, connected: boolean) => ({
  name, connected, active_turns: 0, queue_depth: 0, sessions: 0, restart_pending: false, last_turn_at: null,
});

describe("connection status rows", () => {
  it("reports onboarding setup or live runtime health without false disconnected warnings", () => {
    const onboarded = connectionStatusRows(
      ready<OnboardingConnectionStatus>({ guild_id: "1", owner_id: "2", bot_providers: { command: "claude", command2: "codex" } }),
      ready<HealthResponse>({ status: "healthy", providers: [bot("claude", true), bot("codex", true)] }),
      tr,
    );
    expect(onboarded.map((row) => [row.key, row.value, row.tone])).toEqual([
      ["bot", "연결됨", "ok"], ["server", "연결됨", "ok"], ["owner", "설정됨", "ok"], ["ai", "Claude, Codex 사용 중", "ok"],
    ]);

    const runtimeLive = connectionStatusRows(
      ready<OnboardingConnectionStatus>({ guild_id: null, owner_id: null, bot_providers: {} }),
      ready<HealthResponse>({ status: "healthy", providers: [bot("claude", true), bot("codex", true)] }),
      tr,
    );
    expect(runtimeLive.map((row) => [row.key, row.value, row.tone])).toEqual([
      ["bot", "연결됨", "ok"], ["server", "연결됨 · 봇 2개 가동", "ok"], ["owner", "서버 설정 기준 운영 중", "ok"], ["ai", "Claude, Codex 사용 중", "ok"],
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

    const unknown = connectionStatusRows({ data: undefined, isError: true, isPending: false }, { data: undefined, isError: false, isPending: true }, tr);
    const limited = connectionStatusRows(ready<OnboardingConnectionStatus>({ guild_id: "1", owner_id: "2", bot_providers: {} }), ready<HealthResponse>({ status: "healthy", publicOnly: "forbidden" }), tr);
    expect([limited[0].value, limited[0].tone]).toEqual(["이 기기에서는 확인할 수 없어요", "unknown"]);
    expect(unknown.map((row) => [row.value, row.tone])).toEqual([
      ["확인 중…", "unknown"], ["확인하지 못했어요", "unknown"], ["확인하지 못했어요", "unknown"], ["확인하지 못했어요", "unknown"],
    ]);
  });

  it("aggregates identical degraded labels into a single signal row with joined rawKey tooltip", () => {
    expect(describeDegradedReason("db_slow", true)).toBe("데이터베이스 응답 지연");
    const health: HealthResponse = {
      status: "degraded",
      degraded_reasons: [
        "relay_verdict_unknown_antigravity_1490141479707086938",
        "relay_verdict_unknown_antigravity_1490141479707086939",
      ],
    };
    const signalRows = buildConnectionSignalRows(health, true);
    expect(signalRows).toEqual([
      expect.objectContaining({
        label: "antigravity 응답 전달 확인 안 됨",
        count: 2,
        rawKey: "relay_verdict_unknown_antigravity_1490141479707086938, relay_verdict_unknown_antigravity_1490141479707086939",
      }),
    ]);
    const items = connectionItems(
      {
        statusRows: [],
        health: ready(health),
        retention: { data: undefined, isError: false, isPending: true },
        connectors: { data: undefined, isError: false, isPending: true },
      },
      tr,
      true,
    );
    expect(items[0].status?.notes).toEqual([
      expect.objectContaining({
        title: "antigravity 응답 전달 확인 안 됨",
        count: 2,
        tooltip: "relay_verdict_unknown_antigravity_1490141479707086938, relay_verdict_unknown_antigravity_1490141479707086939",
      }),
    ]);
  });
});
