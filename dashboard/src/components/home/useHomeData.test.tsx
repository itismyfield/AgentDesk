// @vitest-environment happy-dom

import { act } from "react";
import { createRoot } from "react-dom/client";
import { describe, expect, it, vi } from "vitest";
import { request } from "../../api/httpClient";
import { todoStatus } from "./homeModel";
import { campaignTodoSource, loadHomeCampaigns, useAgentTurns } from "./useHomeData";

vi.mock("../../api/httpClient", () => ({ ApiRequestError: class extends Error {}, request: vi.fn(async () => ({ status: "working" })) }));
vi.mock("../../api/campaigns", () => ({ getCampaigns: vi.fn(async () => ({ campaigns: [] })) }));
vi.mock("../../api/serverCampaigns", () => ({
  getServerCampaignSummaries: vi.fn(async () => ["ok", "lost"].map((id) => ({ definition: { title: id }, view: { campaign_id: id, display: "running" } }))),
  getServerCampaign: vi.fn(async (id: string) => {
    if (id === "lost") throw new Error("detail failed");
    return { definition: { nodes: [] }, view: { campaign_id: id, nodes: [] } };
  }),
}));
vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);

describe("home data", () => {
  it("leaves the campaign to-dos partial, never empty, when a campaign detail fails to load", async () => {
    const data = await loadHomeCampaigns();
    expect([data.documents.length, data.detailFailed, data.detailsCut]).toEqual([1, 1, 0]);
    expect(todoStatus(0, ["ok", campaignTodoSource({ data, failed: false, at: 0 })])).toBe("partial");
  });

  it("reads only the agent that just started working", async () => {
    const Probe = ({ ids }: { ids: string[] }) => { useAgentTurns(ids); return null; };
    const reads = () => vi.mocked(request).mock.calls.map(([path]) => String(path).split("/")[3]);
    const root = createRoot(document.createElement("div"));
    await act(async () => root.render(<Probe ids={["a", "b"]} />));
    expect(reads()).toEqual(["a", "b"]);
    vi.mocked(request).mockClear();
    await act(async () => root.render(<Probe ids={["a", "b", "c"]} />));
    expect(reads()).toEqual(["c"]);
    await act(async () => root.unmount());
  });
});
