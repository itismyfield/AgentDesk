// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { CompanySettings, DashboardStats } from "../types";
import { SettingsProvider, useSettings } from "./SettingsContext";

vi.mock("../api/client", () => ({ getStats: vi.fn() }));

import { getStats } from "../api/client";

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
});
