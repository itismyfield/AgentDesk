// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, expect, it } from "vitest";
import AgentPersona from "./AgentPersona";

let root: Root | null = null;
let container: HTMLDivElement | null = null;
afterEach(async () => {
  await act(async () => root?.unmount());
  container?.remove();
  root = null;
  container = null;
});

async function render(element: React.ReactNode) {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => root?.render(element));
  return container;
}

const agent = { id: "chef-goat", name: "Chef", name_ko: "최고의 셰프", avatar_emoji: "👨‍🍳", department_color: "#06b6d4", status: "working" as const };

it("draws the agent's bot avatar as a still image with its state in the label", async () => {
  const target = await render(<AgentPersona agent={agent} size={40} />);
  const persona = target.querySelector('[role="img"]');
  expect(persona?.getAttribute("aria-label")).toBe("최고의 셰프 · 작업 중");
  expect(persona?.getAttribute("data-bot-avatar")).toBe("chef-hat");
  expect(persona?.querySelector("img")?.getAttribute("src")).toMatch(/^data:image\/svg\+xml/);
  expect(persona?.textContent).toBe("");
});

it("mounts a live avatar for profile headers instead of the still", async () => {
  const target = await render(<AgentPersona agent={agent} size={56} animated />);
  const persona = target.querySelector('[role="img"]');
  expect(persona?.querySelector("img")).toBeNull();
  expect(persona?.querySelector("span[aria-hidden] span")?.childElementCount).toBeGreaterThan(0);
});
