// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LANGUAGE_STORAGE_KEY, useI18n } from "../i18n";
import { readLocalStorageValue } from "../lib/useLocalStorage";
import type { CompanySettings, DashboardStats } from "../types";
import { SettingsProvider, useSettings } from "./SettingsContext";

vi.mock("../api/client", () => ({ getStats: vi.fn() }));

import { getStats } from "../api/client";

vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((res) => { resolve = res; });
  return { promise, resolve };
}

describe("settings stats refresh", () => {
  let container: HTMLDivElement;
  let root: Root;
  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    window.localStorage.clear();
  });

  it("keeps the newer stats when an older refresh answers last", async () => {
    const first = deferred<DashboardStats>();
    const second = deferred<DashboardStats>();
    vi.mocked(getStats).mockReset();
    vi.mocked(getStats).mockReturnValueOnce(first.promise).mockReturnValueOnce(second.promise);
    let refreshing = false;
    let total = 0;
    const Probe = () => {
      const stats = useSettings();
      refreshing = stats.refreshingStats;
      total = (stats.stats as { total?: number } | null)?.total ?? 0;
      return <button type="button" onClick={() => stats.refreshStats()}>refresh</button>;
    };
    container = document.createElement("div");
    root = createRoot(container);
    await act(async () => {
      root.render(
        <SettingsProvider initialSettings={{ language: "en" } as CompanySettings} initialStats={{ total: 1 } as unknown as DashboardStats}>
          <Probe />
        </SettingsProvider>,
      );
    });
    await act(async () => { container.querySelector("button")!.click(); });
    await act(async () => { container.querySelector("button")!.click(); });
    await act(async () => second.resolve({ total: 2 } as unknown as DashboardStats));
    expect(total).toBe(2);
    expect(refreshing).toBe(false);
    await act(async () => first.resolve({ total: 9 } as unknown as DashboardStats));
    expect(total).toBe(2);
    expect(refreshing).toBe(false);
  });

  it("syncs settings.language into I18nProvider and localStorage", async () => {
    let observedLang = "";
    let observedText = "";
    const Probe = () => {
      const { setSettings } = useSettings();
      const { language, t } = useI18n();
      observedLang = language;
      observedText = t({ ko: "한국어", en: "English", ja: "日本語" });
      return (
        <button type="button" onClick={() => setSettings((prev) => ({ ...prev, language: "ja" }))}>
          switch
        </button>
      );
    };
    container = document.createElement("div");
    root = createRoot(container);
    await act(async () => {
      root.render(
        <SettingsProvider initialSettings={{ language: "en" } as CompanySettings} initialStats={null}>
          <Probe />
        </SettingsProvider>,
      );
    });
    expect(observedLang).toBe("en");
    expect(observedText).toBe("English");
    expect(readLocalStorageValue<string | null>(LANGUAGE_STORAGE_KEY, null)).toBe("en");

    await act(async () => { container.querySelector("button")!.click(); });
    expect(observedLang).toBe("ja");
    expect(observedText).toBe("日本語");
    expect(readLocalStorageValue<string | null>(LANGUAGE_STORAGE_KEY, null)).toBe("ja");
  });
});
