// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, expect, it } from "vitest";
import AgentPersona from "./AgentPersona";
import { botAvatarFor } from "./agent-avatar/botAvatar";

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

const idleAgent = { id: "chef-goat", name: "Chef", name_ko: "최고의 셰프", status: "idle" as const };
const workingAgent = { id: "chef-goat", name: "Chef", name_ko: "최고의 셰프", status: "working" as const };

it("draws resting list avatars with lightweight CSS eye-blinking and state in the label", async () => {
  const target = await render(<AgentPersona agent={idleAgent} size={40} />);
  const persona = target.querySelector('[role="img"]');
  expect(persona?.getAttribute("aria-label")).toBe("최고의 셰프 · 대기");
  expect(persona?.getAttribute("data-persona-motion")).toBe("blink");
  expect(persona?.getAttribute("data-bot-avatar")).toBe(botAvatarFor("chef-goat").definition.id);
  const src = persona?.querySelector("img")?.getAttribute("src") ?? "";
  expect(src).toMatch(/^data:image\/svg\+xml/);
  expect(decodeURIComponent(src)).toContain("@keyframes oba-blink");
  expect(persona?.textContent).toBe("");
});

it("keeps offline and explicitly static avatars frozen without CSS keyframes", async () => {
  const offlineTarget = await render(<AgentPersona agent={idleAgent} state="offline" size={32} />);
  const offlinePersona = offlineTarget.querySelector('[role="img"]');
  expect(offlinePersona?.getAttribute("data-persona-motion")).toBe("still");
  expect(decodeURIComponent(offlinePersona?.querySelector("img")?.getAttribute("src") ?? "")).not.toContain("@keyframes");

  await act(async () => root?.unmount());
  container?.remove();

  const forcedStill = await render(<AgentPersona agent={workingAgent} size={32} animated={false} />);
  expect(forcedStill.querySelector('[role="img"]')?.getAttribute("data-persona-motion")).toBe("still");
  expect(forcedStill.querySelector("img")).not.toBeNull();
});

it("mounts the live 3D avatar automatically when working or animated=true", async () => {
  const target = await render(<AgentPersona agent={workingAgent} size={40} />);
  const persona = target.querySelector('[role="img"]');
  expect(persona?.getAttribute("aria-label")).toBe("최고의 셰프 · 작업 중");
  expect(persona?.getAttribute("data-persona-motion")).toBe("live");
  expect(persona?.querySelector("img")).toBeNull();
  expect(persona?.querySelector("span[aria-hidden] span")?.childElementCount).toBeGreaterThan(0);
});

it("upgrades resting row avatars to the live 3D scene on mouse hover and restores CSS blink on leave", async () => {
  const target = await render(
    <button type="button">
      <AgentPersona agent={idleAgent} size={32} />
      <span>최고의 셰프</span>
    </button>,
  );
  const button = target.querySelector("button")!;
  const persona = () => target.querySelector('[role="img"]');
  expect(persona()?.getAttribute("data-persona-motion")).toBe("blink");

  await act(async () => {
    button.dispatchEvent(new PointerEvent("pointerenter", { pointerType: "touch" }));
  });
  expect(persona()?.getAttribute("data-persona-motion")).toBe("blink");

  await act(async () => {
    button.dispatchEvent(new PointerEvent("pointerenter", { pointerType: "mouse" }));
  });
  expect(persona()?.getAttribute("data-persona-motion")).toBe("live");
  expect(persona()?.querySelector("img")).toBeNull();

  await act(async () => {
    button.dispatchEvent(new PointerEvent("pointerleave", { pointerType: "mouse" }));
  });
  expect(persona()?.getAttribute("data-persona-motion")).toBe("blink");
  expect(persona()?.querySelector("img")).not.toBeNull();
});
