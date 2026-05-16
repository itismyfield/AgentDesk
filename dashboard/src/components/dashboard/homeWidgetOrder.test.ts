import { describe, expect, it } from "vitest";
import {
  DEFAULT_HOME_WIDGET_ORDER,
  HOME_WIDGET_STORAGE_KEY,
  normalizeHomeWidgetOrder,
  readStoredHomeWidgetOrder,
} from "./homeWidgetOrder";

function makeStorage(seed?: string) {
  const values = new Map<string, string>();
  if (seed !== undefined) values.set(HOME_WIDGET_STORAGE_KEY, seed);
  return {
    getItem: (key: string) => values.get(key) ?? null,
  } as Storage;
}

describe("homeWidgetOrder", () => {
  it("keeps known widgets in user order and appends missing defaults", () => {
    expect(normalizeHomeWidgetOrder(["signals", "metric_agents"])).toEqual([
      "signals",
      "metric_agents",
      ...DEFAULT_HOME_WIDGET_ORDER.filter((id) => id !== "signals" && id !== "metric_agents"),
    ]);
  });

  it("drops duplicates and unknown entries", () => {
    expect(normalizeHomeWidgetOrder(["quality", "unknown", "quality", 42])).toEqual([
      "quality",
      ...DEFAULT_HOME_WIDGET_ORDER.filter((id) => id !== "quality"),
    ]);
  });

  it("falls back to defaults for malformed storage", () => {
    expect(readStoredHomeWidgetOrder(makeStorage("{nope"))).toEqual(DEFAULT_HOME_WIDGET_ORDER);
  });

  it("reads and normalizes stored JSON", () => {
    expect(readStoredHomeWidgetOrder(makeStorage(JSON.stringify(["activity", "office"])))).toEqual([
      "activity",
      "office",
      ...DEFAULT_HOME_WIDGET_ORDER.filter((id) => id !== "activity" && id !== "office"),
    ]);
  });
});
