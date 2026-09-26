// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { CampaignNode } from "../../api/campaigns";
import CampaignGlance from "./CampaignGlance";
import { makeCampaignNode } from "./campaignTestFixtures";

let container: HTMLDivElement;
let root: Root;
beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); });
async function render(nodes: CampaignNode[], onOpen = vi.fn()) {
  await act(async () => root.render(<CampaignGlance nodes={nodes} tr={(_, en) => en} onOpen={onOpen} />));
  return onOpen;
}
const card = (id: string) => container.querySelector<HTMLButtonElement>(`[data-glance-id="${id}"]`)!;

it("emphasizes a blocker only on the cards that record one and falls back when gist or benefit is missing", async () => {
  await render([
    makeCampaignNode("described", { status: "running", title: "#6270 dashboard glance", issue_url: "https://github.com/o/r/issues/6270", stage: "review: draft",
      summary: "See running work at a glance", benefit: "Know what is stuck without opening logs", blocker: "Waiting for the relay freeze to lift", head_sha: "abc1234def", evidence: ["cargo test passed"] }),
    makeCampaignNode("bare", { status: "running", title: "#6264 shard script checks", issue_url: "https://github.com/o/r/issues/6264", stage: "부분 완료 · 잔여 확인" }),
  ]);
  expect(card("described").textContent).toContain("See running work at a glance");
  expect(card("described").textContent).toContain("Know what is stuck without opening logs");
  expect(card("described").querySelector(".campaign-glance-blocker")?.textContent).toContain("Waiting for the relay freeze to lift");
  expect(card("described").textContent).toContain("Review");
  // Technical detail stays in the task inspector.
  expect(container.textContent).not.toContain("abc1234def");
  expect(container.textContent).not.toContain("cargo test passed");
  expect(card("bare").querySelector(".campaign-glance-blocker")).toBeNull();
  expect(card("bare").textContent).toContain("shard script checks");
  expect(card("bare").textContent).toContain("No benefit recorded");
  expect(card("bare").querySelector(".campaign-glance-raw")?.textContent).toBe("부분 완료 · 잔여 확인");
});

it("counts waiting, done and skipped work and lists each status only when opened", async () => {
  const onOpen = await render([
    makeCampaignNode("run", { status: "running" }),
    makeCampaignNode("wait-1"), makeCampaignNode("wait-2", { summary: "Second waiting gist" }),
    makeCampaignNode("done", { status: "completed" }), makeCampaignNode("skip", { status: "skipped" }),
  ]);
  const chips = Array.from(container.querySelectorAll<HTMLButtonElement>(".campaign-glance-buckets button"));
  expect(chips.map((chip) => chip.textContent)).toEqual(["Pending 2", "Completed 1", "Skipped 1"]);
  expect(container.textContent).toContain("Running now 1");
  expect(container.querySelector(".campaign-glance-list")).toBeNull();
  await act(async () => chips[0].click());
  const items = Array.from(container.querySelectorAll<HTMLButtonElement>(".campaign-glance-list button"));
  expect(items).toHaveLength(2);
  expect(items[1].textContent).toContain("Second waiting gist");
  await act(async () => items[1].click());
  expect(onOpen).toHaveBeenCalledWith("wait-2");
  await act(async () => chips[0].click());
  expect(container.querySelector(".campaign-glance-list")).toBeNull();
});
