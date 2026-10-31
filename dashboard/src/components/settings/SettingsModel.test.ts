import { describe, expect, it } from "vitest";
import { configEntryPlacement, metaFromConfigEntry, resolveSettingsPanel, type ConfigEntry } from "./SettingsModel";

const entry = (key: string, category: string, extra: Partial<ConfigEntry> = {}): ConfigEntry => ({
  key, category, value: "1", label_ko: key, label_en: key, ...extra,
});

describe("settings panels", () => {
  it("opens the tab that now holds settings from the old 8-tab ids", () => {
    expect(["pipeline", "runtime", "providers", "connectors", "onboarding", "voice"].map(resolveSettingsPanel))
      .toEqual(["workflow", "workflow", "ai", "connections", "connections", "general"]);
    expect(resolveSettingsPanel("machine")).toBe("machine");
    expect(resolveSettingsPanel("nope")).toBeNull();
  });

  it("places server config keys where people look for them", () => {
    expect(configEntryPlacement(entry("kanban_manager_channel_id", "pipeline"))).toEqual({ panel: "connections", section: "channels" });
    expect(configEntryPlacement(entry("server_port", "system"))).toEqual({ panel: "machine", section: "server" });
    expect(configEntryPlacement(entry("context_compact_percent", "context"))).toEqual({ panel: "ai", section: "context" });
    expect(configEntryPlacement(entry("requested_timeout_min", "timeout"))).toEqual({ panel: "workflow", section: "timeouts" });
    expect(configEntryPlacement(entry("review_enabled", "review"))).toEqual({ panel: "workflow", section: "review" });
    // The old quality category now only holds live alert settings, so they show like any other key.
    expect(configEntryPlacement(entry("agent_quality_monitoring_channel_id", "quality"))).toEqual({ panel: "connections", section: "channels" });
    expect(configEntryPlacement(entry("kanban_relay_alert_threshold", "quality"))).toEqual({ panel: "workflow", section: "other" });
  });

  it("replaces internal server labels and marks pending edits", () => {
    const meta = metaFromConfigEntry(
      entry("requested_timeout_min", "timeout", { label_ko: "요청됨 타임아웃 (분)", restart_behavior: "persist-live-override" }),
      { requested_timeout_min: "30" },
      { panel: "workflow", section: "timeouts" },
    );
    expect(meta.labelKo).toBe("접수 후 대기 경고 (분)");
    expect(meta.effectiveValue).toBe("30");
    expect(meta.changed).toBe(true);
    expect(meta.flags).toEqual([]);
    expect(meta.applyNoteKo).toBe("다시 시작해도 여기서 바꾼 값이 유지돼요.");
  });

  it("shows read-only server values without a restart badge", () => {
    const meta = metaFromConfigEntry(entry("server_port", "system", { editable: false, restart_behavior: "config-only" }), {}, { panel: "machine", section: "server" });
    expect(meta.editable).toBe(false);
    expect(meta.flags).toEqual(["read_only"]);
  });
});
