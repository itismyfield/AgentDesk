import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("../api/client", () => ({
  getSession: vi.fn(),
  getAgents: vi.fn(),
  getSettings: vi.fn(),
  getDispatchedSessions: vi.fn(),
  getStats: vi.fn(),
}));

import * as api from "../api/client";
import {
  fetchBootstrapSessions,
  fetchBootstrapStats,
  fetchDashboardBootstrap,
} from "./bootstrapQuery";

describe("fetchDashboardBootstrap", () => {
  afterEach(() => {
    vi.resetAllMocks();
  });

  it("loads shell essentials without a duplicate getSession round-trip or blocking on stats/sessions", async () => {
    vi.mocked(api.getAgents).mockResolvedValue([]);
    vi.mocked(api.getSettings).mockResolvedValue({ language: "ko", theme: "dark" } as never);

    const data = await fetchDashboardBootstrap();

    expect(api.getSession).not.toHaveBeenCalled();
    expect(api.getDispatchedSessions).not.toHaveBeenCalled();
    expect(api.getStats).not.toHaveBeenCalled();
    expect(data.agents).toEqual([]);
    expect(data.settings.language).toBe("ko");
  });

  it("keeps includeMerged=true on parallel session bootstrap and falls back to null on failure", async () => {
    vi.mocked(api.getDispatchedSessions).mockRejectedValueOnce(new Error("boom"));
    vi.mocked(api.getStats).mockRejectedValueOnce(new Error("boom"));

    await expect(fetchBootstrapSessions()).resolves.toBeNull();
    await expect(fetchBootstrapStats()).resolves.toBeNull();
    expect(api.getDispatchedSessions).toHaveBeenCalledWith(true);
  });
});
