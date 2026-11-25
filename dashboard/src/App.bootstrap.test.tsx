// @vitest-environment happy-dom

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { MemoryRouter } from "react-router";
import { afterEach, describe, expect, it, vi } from "vitest";
import { DEFAULT_SETTINGS } from "./types";

const fetchDashboardBootstrap = vi.hoisted(() => vi.fn());
const fetchBootstrapSessions = vi.hoisted(() => vi.fn());
const fetchBootstrapStats = vi.hoisted(() => vi.fn());
vi.mock("./app/useDashboardSocket", () => ({ useDashboardSocket: () => ({ wsConnected: false, lastEventTs: null }) }));
vi.mock("./app/bootstrapQuery", () => ({
  dashboardBootstrapQueryKey: ["dashboard", "bootstrap"],
  dashboardBootstrapSessionsQueryKey: ["dashboard", "bootstrap", "sessions"],
  dashboardBootstrapStatsQueryKey: ["dashboard", "bootstrap", "stats"],
  fetchDashboardBootstrap,
  fetchBootstrapSessions,
  fetchBootstrapStats,
}));

import App from "./App";
import { OverlayProvider } from "./components/common/overlay";

async function settle() {
  for (let i = 0; i < 10; i += 1) {
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 0)); });
  }
}

describe("dashboard bootstrap", () => {
  let container: HTMLDivElement;
  let root: Root;
  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    fetchDashboardBootstrap.mockReset();
    fetchBootstrapSessions.mockReset();
    fetchBootstrapStats.mockReset();
  });

  it("shows a retry when the first load fails instead of spinning forever", async () => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    fetchDashboardBootstrap.mockRejectedValue(new Error("offline"));
    fetchBootstrapSessions.mockResolvedValue([]);
    fetchBootstrapStats.mockResolvedValue(null);
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    await act(async () => {
      root.render(<QueryClientProvider client={client}><App /></QueryClientProvider>);
    });
    await settle();
    expect(container.querySelector("[data-testid='bootstrap-loading']")).toBeNull();
    expect(container.querySelector("[data-testid='bootstrap-error']")?.textContent).toMatch(/Could not load|불러오지 못/);
    await act(async () => { container.querySelector<HTMLButtonElement>("[data-testid='bootstrap-retry']")!.click(); });
    await settle();
    expect(fetchDashboardBootstrap).toHaveBeenCalledTimes(2);
    expect(container.querySelector("[data-testid='bootstrap-error']")).toBeTruthy();
  });

  it("mounts the shell as soon as core bootstrap resolves while stats and sessions are still in flight", async () => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    fetchDashboardBootstrap.mockResolvedValue({
      agents: [],
      departments: [],
      settings: DEFAULT_SETTINGS,
    });
    // Never resolve sessions or stats during initial assertion
    fetchBootstrapSessions.mockReturnValue(new Promise(() => {}));
    fetchBootstrapStats.mockReturnValue(new Promise(() => {}));

    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    await act(async () => {
      root.render(
        <QueryClientProvider client={client}>
          <MemoryRouter initialEntries={["/"]}>
            <OverlayProvider>
              <App />
            </OverlayProvider>
          </MemoryRouter>
        </QueryClientProvider>,
      );
    });
    await settle();
    expect(container.querySelector("[data-testid='bootstrap-loading']")).toBeNull();
    expect(container.querySelector("[data-testid='app-shell-frame']")).toBeTruthy();
    expect(container.querySelector("[data-testid='home-page']")).toBeTruthy();
  });
});
