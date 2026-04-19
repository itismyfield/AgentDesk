import { describe, expect, it } from "vitest";
import {
  DEFAULT_ACCENT_PRESET,
  applyThemeAccentDataset,
  readStoredAccentPreset,
  readThemePreferenceFromPatch,
  readStoredThemePreference,
  resolveThemePreference,
} from "./themePreferences";

function createStorage(seed: Record<string, string> = {}): Storage {
  const data = new Map(Object.entries(seed));
  return {
    length: data.size,
    clear() {
      data.clear();
      this.length = 0;
    },
    getItem(key) {
      return data.get(key) ?? null;
    },
    key(index) {
      return Array.from(data.keys())[index] ?? null;
    },
    removeItem(key) {
      data.delete(key);
      this.length = data.size;
    },
    setItem(key, value) {
      data.set(key, value);
      this.length = data.size;
    },
  };
}

describe("themePreferences", () => {
  it("reads stored values when valid", () => {
    const storage = createStorage({
      "agentdesk.theme": "light",
      "agentdesk.accent": "rose",
    });

    expect(readStoredThemePreference(storage, "dark")).toBe("light");
    expect(readStoredAccentPreset(storage, DEFAULT_ACCENT_PRESET)).toBe("rose");
  });

  it("falls back when stored values are invalid", () => {
    const storage = createStorage({
      "agentdesk.theme": "sepia",
      "agentdesk.accent": "mint",
    });

    expect(readStoredThemePreference(storage, "auto")).toBe("auto");
    expect(readStoredAccentPreset(storage, DEFAULT_ACCENT_PRESET)).toBe(DEFAULT_ACCENT_PRESET);
  });

  it("resolves auto theme against system preference", () => {
    expect(resolveThemePreference("auto", true)).toBe("dark");
    expect(resolveThemePreference("auto", false)).toBe("light");
    expect(resolveThemePreference("light", true)).toBe("light");
  });

  it("applies theme and accent to the document dataset", () => {
    const element = { dataset: {} } as HTMLElement;

    applyThemeAccentDataset(element, "light", "cyan");

    expect(element.dataset.theme).toBe("light");
    expect(element.dataset.accent).toBe("cyan");
  });

  it("reads a valid theme preference from settings patches", () => {
    expect(readThemePreferenceFromPatch({ theme: "light" })).toBe("light");
    expect(readThemePreferenceFromPatch({ theme: "auto" })).toBe("auto");
    expect(readThemePreferenceFromPatch({ theme: "sepia" })).toBeNull();
    expect(readThemePreferenceFromPatch({ theme: 1 })).toBeNull();
    expect(readThemePreferenceFromPatch({})).toBeNull();
  });
});
