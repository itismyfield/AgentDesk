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
  expect(card("described").querySelector("strong")?.textContent).toBe("dashboard glance");
  expect(card("described").querySelector(".campaign-glance-summary")?.textContent).toBe("See running work at a glance");
  expect(card("described").textContent).toContain("Know what is stuck without opening logs");
  expect(card("described").querySelector(".campaign-glance-blocker")?.textContent).toContain("Waiting for the relay freeze to lift");
  expect(card("described").textContent).toContain("Review");
  // Technical detail stays in the task inspector.
  expect(container.textContent).not.toContain("abc1234def");
  expect(container.textContent).not.toContain("cargo test passed");
  expect(card("bare").querySelector(".campaign-glance-blocker")).toBeNull();
  expect(card("bare").textContent).toContain("shard script checks");
  expect(card("bare").querySelector(".campaign-glance-benefit")).toBeNull();
  expect(card("bare").querySelector(".campaign-glance-raw")?.textContent).toBe("부분 완료 · 잔여 확인");
});

it("counts waiting, done and skipped work and lists each status only when opened", async () => {
  const onOpen = await render([
    makeCampaignNode("run", { status: "running" }),
    makeCampaignNode("wait-1"), makeCampaignNode("wait-2", { title: "#7331 Mailbox lease settlement", summary: "Inherits #5951, #5996 4-consumer tests." }),
    makeCampaignNode("done", { status: "completed" }), makeCampaignNode("skip", { status: "skipped" }),
  ]);
  const chips = Array.from(container.querySelectorAll<HTMLButtonElement>(".campaign-glance-buckets button"));
  expect(chips.map((chip) => chip.textContent)).toEqual(["Pending 2", "Completed 1", "Skipped 1"]);
  expect(container.textContent).toContain("Running now 1");
  expect(container.querySelector(".campaign-glance-list")).toBeNull();
  await act(async () => chips[0].click());
  const items = Array.from(container.querySelectorAll<HTMLButtonElement>(".campaign-glance-list button"));
  expect(items).toHaveLength(2);
  // The compact list names the task; lineage notes in the summary stay in the details.
  expect(items[1].textContent).toContain("#7331Mailbox lease settlement");
  expect(items[1].textContent).not.toContain("Inherits");
  await act(async () => items[1].click());
  expect(onOpen).toHaveBeenCalledWith("wait-2");
  await act(async () => chips[0].click());
  expect(container.querySelector(".campaign-glance-list")).toBeNull();
});


it("keeps a short initial overview while every active task remains reachable", async () => {
  const onOpen = await render(Array.from({ length: 8 }, (_, index) => makeCampaignNode(`active-${index}`, { status: "running" })));
  expect(container.querySelectorAll("[data-glance-id]")).toHaveLength(3);
  const toggle = Array.from(container.querySelectorAll<HTMLButtonElement>("button")).find((button) => button.textContent === "Show all 8 active tasks")!;
  await act(async () => toggle.click());
  expect(container.querySelectorAll("[data-glance-id]")).toHaveLength(8);
  await act(async () => card("active-7").click());
  expect(onOpen).toHaveBeenCalledWith("active-7");
  await act(async () => toggle.click());
  expect(container.querySelectorAll("[data-glance-id]")).toHaveLength(3);
});


it("puts a blocked task in the short overview even when many other tasks are running", async () => {
  await render([...Array.from({ length: 8 }, (_, index) => makeCampaignNode(`running-${index}`, { status: "running" })), makeCampaignNode("blocked", { status: "blocked", blocker: "A dependency needs attention" })]);
  expect(container.querySelector("[data-glance-id]")?.getAttribute("data-glance-id")).toBe("blocked");
  expect(container.textContent).toContain("Blocked 1");
});


it.each([2, 3])("offers the mobile disclosure for %i active tasks", async (count) => {
  const onOpen = await render(Array.from({ length: count }, (_, index) => makeCampaignNode(`active-${index}`, { status: "running" })));
  const toggle = Array.from(container.querySelectorAll<HTMLButtonElement>("button")).find((button) => button.textContent === `Show all ${count} active tasks`)!;
  expect(toggle).toBeDefined();
  expect(toggle.getAttribute("aria-expanded")).toBe("false");
  await act(async () => toggle.click());
  expect(toggle.getAttribute("aria-expanded")).toBe("true");
  await act(async () => card(`active-${count - 1}`).click());
  expect(onOpen).toHaveBeenCalledWith(`active-${count - 1}`);
});


it("previews a long status list and reveals the rest inline instead of a nested scroll box", async () => {
  await render(Array.from({ length: 12 }, (_, index) => makeCampaignNode(`done-${index}`, { status: "completed" })));
  await act(async () => container.querySelector<HTMLButtonElement>(".campaign-glance-buckets button")!.click());
  const rows = () => container.querySelectorAll(".campaign-glance-list button");
  expect(rows()).toHaveLength(8);
  const more = Array.from(container.querySelectorAll<HTMLButtonElement>("button")).find((button) => button.textContent === "Show 4 more")!;
  await act(async () => more.click());
  expect(rows()).toHaveLength(12);
  expect(more.getAttribute("aria-expanded")).toBe("true");
});
