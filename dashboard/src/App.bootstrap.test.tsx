// @vitest-environment happy-dom

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";

const fetchDashboardBootstrap = vi.hoisted(() => vi.fn());
vi.mock("./app/useDashboardSocket", () => ({ useDashboardSocket: () => ({ wsConnected: false, lastEventTs: null }) }));
vi.mock("./app/bootstrapQuery", () => ({
  dashboardBootstrapQueryKey: ["dashboard", "bootstrap"],
  fetchDashboardBootstrap,
}));

import App from "./App";

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
  });

  it("shows a retry when the first load fails instead of spinning forever", async () => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    fetchDashboardBootstrap.mockRejectedValue(new Error("offline"));
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
});
