// @vitest-environment happy-dom

import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import { request } from "../../api/httpClient";
import { getServerCampaignSummaries } from "../../api/serverCampaigns";
import { todoStatus, type HomeTurn } from "./homeModel";
import { AgentTurnsProvider, campaignTodoSource, loadHomeCampaigns, useAgentTurns, usePolled, type Loaded } from "./useHomeData";

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
  // A failed assertion must not leave fake timers or a never-answering request to the tests after it.
  afterEach(() => {
    vi.useRealTimers();
    vi.mocked(request).mockReset().mockImplementation(async () => ({ status: "working" }));
  });

  it("leaves the campaign to-dos partial, never empty, when a campaign detail fails to load", async () => {
    const data = await loadHomeCampaigns();
    expect([data.documents.length, data.detailFailed, data.detailsCut]).toEqual([1, 1, 0]);
    expect(todoStatus(0, ["ok", campaignTodoSource({ data, failed: false, at: 0 })])).toBe("partial");
  });

  it("leaves the to-dos partial when campaigns past the detail limit go unread", async () => {
    const open = Array.from({ length: 10 }, (_, index) => ({ definition: { title: `c${index}` }, view: { campaign_id: `c${index}`, display: "running" } }));
    vi.mocked(getServerCampaignSummaries).mockResolvedValueOnce(open as never);
    const data = await loadHomeCampaigns();
    expect([data.detailFailed, data.detailsCut, campaignTodoSource({ data, failed: false, at: 0 })]).toEqual([0, 2, "partial"]);
  });

  let latest = new Map<string, HomeTurn>();
  const Probe = ({ ids }: { ids: string[] }) => { latest = useAgentTurns(ids); return null; };
  const reads = () => vi.mocked(request).mock.calls.map(([path]) => String(path).split("/")[3]);

  it("reads only the agent that just started working and drops one that stopped", async () => {
    const root = createRoot(document.createElement("div"));
    await act(async () => root.render(<Probe ids={["a", "b"]} />));
    expect(reads()).toEqual(["a", "b"]);
    vi.mocked(request).mockClear();
    await act(async () => root.render(<Probe ids={["a", "b", "c"]} />));
    expect(reads()).toEqual(["c"]);
    await act(async () => root.render(<Probe ids={["a"]} />));
    expect([...latest.keys()]).toEqual(["a"]);
    await act(async () => root.unmount());
  });

  it("keeps rereading agents already working while others join faster than the cycle", async () => {
    vi.useFakeTimers();
    vi.mocked(request).mockClear();
    const root = createRoot(document.createElement("div"));
    for (const ids of [["a"], ["a", "b"], ["a", "b", "c"]]) {
      await act(async () => root.render(<Probe ids={ids} />));
      await act(async () => { vi.advanceTimersByTime(20_000); });
    }
    expect(reads().filter((id) => id === "a").length).toBeGreaterThanOrEqual(2);
    await act(async () => root.unmount());
  });

  it("keeps the newer turn when an older read of the same agent answers last", async () => {
    const answers: Array<(turn: unknown) => void> = [];
    vi.mocked(request).mockImplementation(() => new Promise((resolve) => answers.push(resolve)));
    const root = createRoot(document.createElement("div"));
    for (const ids of [["a", "b"], ["a"], ["a", "b"]]) await act(async () => root.render(<Probe ids={ids} />));
    await act(async () => answers[2]({ status: "new" }));
    await act(async () => { answers[0]({ status: "a" }); answers[1]({ status: "old" }); });
    expect(latest.get("b")).toEqual({ status: "new" });
    await act(async () => root.unmount());
  });

  it("keeps the newer poll when an older read fails afterwards", async () => {
    vi.useFakeTimers();
    const answers: Array<(value: string) => void> = [];
    const rejects: Array<(error: Error) => void> = [];
    const load = vi.fn(() => new Promise<string>((resolve, reject) => { answers.push(resolve); rejects.push(reject); }));
    let polled: Loaded<string> = { data: null, failed: false, at: null };
    const Poll = () => { polled = usePolled(load, 60_000); return null; };
    const root = createRoot(document.createElement("div"));
    await act(async () => root.render(<Poll />));
    await act(async () => { vi.advanceTimersByTime(60_000); });
    expect(load).toHaveBeenCalledTimes(2);
    await act(async () => answers[1]("new"));
    await act(async () => rejects[0](new Error("stale")));
    expect(polled).toMatchObject({ data: "new", failed: false });
    await act(async () => root.unmount());
  });

  it("does not read while polling is disabled", async () => {
    const load = vi.fn(async () => "ready");
    const Poll = ({ enabled }: { enabled: boolean }) => { usePolled(load, 60_000, enabled); return null; };
    const root = createRoot(document.createElement("div"));
    await act(async () => root.render(<Poll enabled={false} />));
    expect(load).not.toHaveBeenCalled();
    await act(async () => root.render(<Poll enabled />));
    expect(load).toHaveBeenCalledTimes(1);
    await act(async () => root.unmount());
  });

  it("does not read an agent the surrounding shell is already watching", async () => {
    vi.mocked(request).mockClear();
    const root = createRoot(document.createElement("div"));
    const Shell = ({ ids, extra }: { ids: string[]; extra: string[] }) => {
      const turns = useAgentTurns(ids);
      return <AgentTurnsProvider turns={turns} watched={ids}><Probe ids={[...ids, ...extra]} /></AgentTurnsProvider>;
    };
    await act(async () => root.render(<Shell ids={["a"]} extra={[]} />));
    expect(reads()).toEqual(["a"]);
    vi.mocked(request).mockClear();
    await act(async () => root.render(<Shell ids={["a"]} extra={["b"]} />));
    expect(reads()).toEqual(["b"]);
    expect(latest.get("a")).toEqual({ status: "working" });
    await act(async () => root.unmount());
  });
});
