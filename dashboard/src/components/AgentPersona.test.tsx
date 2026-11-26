// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import AgentPersona from "./AgentPersona";
import { botAvatarFor } from "./agent-avatar/botAvatar";

let root: Root | null = null;
let container: HTMLDivElement | null = null;
afterEach(async () => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
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
const secondAgent = { id: "game-manager", name: "GM", name_ko: "게임 매니저", status: "idle" as const };
const thirdAgent = { id: "story-writer", name: "SW", name_ko: "스토리 라이터", status: "idle" as const };
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

it("mounts the live 3D avatar automatically when working and pauses it while scrolled offscreen", async () => {
  let ioCallback: IntersectionObserverCallback | null = null;
  vi.stubGlobal(
    "IntersectionObserver",
    class {
      constructor(cb: IntersectionObserverCallback) {
        ioCallback = cb;
      }
      observe() {}
      disconnect() {}
    },
  );

  const target = await render(<AgentPersona agent={workingAgent} size={40} />);
  const persona = () => target.querySelector('[role="img"]');
  expect(persona()?.getAttribute("aria-label")).toBe("최고의 셰프 · 작업 중");
  expect(persona()?.getAttribute("data-persona-motion")).toBe("live");
  expect(persona()?.querySelector("img")).toBeNull();
  expect(persona()?.querySelector("span[aria-hidden] span")?.childElementCount).toBeGreaterThan(0);

  await act(async () => {
    ioCallback?.([{ isIntersecting: false } as IntersectionObserverEntry], {} as IntersectionObserver);
  });
  expect(persona()?.getAttribute("data-persona-motion")).toBe("blink");
  expect(persona()?.querySelector("img")).not.toBeNull();

  await act(async () => {
    ioCallback?.([{ isIntersecting: true } as IntersectionObserverEntry], {} as IntersectionObserver);
  });
  expect(persona()?.getAttribute("data-persona-motion")).toBe("live");
});

it("upgrades single-avatar rows on mouse hover and clears hover on leave, blur, or state change", async () => {
  const target = await render(
    <button type="button">
      <AgentPersona agent={idleAgent} size={32} />
      <span data-testid="name">최고의 셰프</span>
    </button>,
  );
  const button = target.querySelector("button")!;
  const nameEl = target.querySelector('[data-testid="name"]')!;
  const persona = () => target.querySelector('[role="img"]')!;
  expect(persona().getAttribute("data-persona-motion")).toBe("blink");

  await act(async () => {
    button.dispatchEvent(new PointerEvent("pointerenter", { pointerType: "touch" }));
  });
  expect(persona().getAttribute("data-persona-motion")).toBe("blink");

  await act(async () => {
    button.dispatchEvent(new PointerEvent("pointerenter", { pointerType: "mouse" }));
  });
  expect(persona().getAttribute("data-persona-motion")).toBe("live");

  // Moving off the avatar onto the sibling text inside the same 1-avatar button keeps it live.
  await act(async () => {
    persona().dispatchEvent(new PointerEvent("pointerleave", { pointerType: "mouse", relatedTarget: nameEl }));
  });
  expect(persona().getAttribute("data-persona-motion")).toBe("live");

  // Window blur (e.g. modal/tab switch) clears sticky hover.
  await act(async () => {
    window.dispatchEvent(new Event("blur"));
  });
  expect(persona().getAttribute("data-persona-motion")).toBe("blink");

  // Re-hover then transition working -> idle cleans up hover without stuck live state.
  await act(async () => {
    button.dispatchEvent(new PointerEvent("pointerenter", { pointerType: "mouse" }));
  });
  expect(persona().getAttribute("data-persona-motion")).toBe("live");
  await act(async () => {
    root?.render(
      <button type="button">
        <AgentPersona agent={workingAgent} size={32} />
        <span data-testid="name">최고의 셰프</span>
      </button>,
    );
  });
  expect(persona().getAttribute("data-persona-motion")).toBe("live");
  await act(async () => {
    root?.render(
      <button type="button">
        <AgentPersona agent={idleAgent} size={32} />
        <span data-testid="name">최고의 셰프</span>
      </button>,
    );
  });
  expect(persona().getAttribute("data-persona-motion")).toBe("blink");
});

it("scopes hover to individual avatars inside multi-avatar buttons (department/fold stacks)", async () => {
  const target = await render(
    <button type="button">
      <AgentPersona agent={idleAgent} size={24} />
      <AgentPersona agent={secondAgent} size={24} />
      <AgentPersona agent={thirdAgent} size={24} />
      <span>나머지 3명</span>
    </button>,
  );
  const button = target.querySelector("button")!;
  const personas = () => [...target.querySelectorAll('[role="img"]')];

  await act(async () => {
    button.dispatchEvent(new PointerEvent("pointerenter", { pointerType: "mouse" }));
  });
  expect(personas().map((el) => el.getAttribute("data-persona-motion"))).toEqual(["blink", "blink", "blink"]);

  await act(async () => {
    personas()[1].dispatchEvent(new PointerEvent("pointerenter", { pointerType: "mouse" }));
  });
  expect(personas().map((el) => el.getAttribute("data-persona-motion"))).toEqual(["blink", "live", "blink"]);

  await act(async () => {
    personas()[1].dispatchEvent(new PointerEvent("pointerleave", { pointerType: "mouse", relatedTarget: button }));
  });
  expect(personas().map((el) => el.getAttribute("data-persona-motion"))).toEqual(["blink", "blink", "blink"]);
});

it("falls back to static stills and skips hover upgrades when prefers-reduced-motion is active", async () => {
  vi.spyOn(window, "matchMedia").mockImplementation(
    (query: string) =>
      ({
        matches: query === "(prefers-reduced-motion: reduce)",
        media: query,
        addEventListener: vi.fn(),
        removeEventListener: vi.fn(),
      }) as unknown as MediaQueryList,
  );

  const target = await render(
    <button type="button">
      <AgentPersona agent={idleAgent} size={32} />
      <AgentPersona agent={workingAgent} size={32} />
    </button>,
  );
  const personas = () => [...target.querySelectorAll('[role="img"]')];
  expect(personas().map((el) => el.getAttribute("data-persona-motion"))).toEqual(["still", "still"]);
  expect(decodeURIComponent(personas()[0].querySelector("img")?.getAttribute("src") ?? "")).not.toContain("@keyframes");

  await act(async () => {
    personas()[0].dispatchEvent(new PointerEvent("pointerenter", { pointerType: "mouse" }));
  });
  expect(personas()[0].getAttribute("data-persona-motion")).toBe("still");
});

it("localizes displayName and state labels when wrapped in I18nProvider language='en'", async () => {
  const { I18nProvider } = await import("../i18n");
  const target = await render(
    <I18nProvider language="en">
      <AgentPersona agent={idleAgent} size={32} animated={false} />
      <AgentPersona agent={workingAgent} size={32} animated={false} />
      <AgentPersona agent={idleAgent} state="waiting" size={32} animated={false} />
    </I18nProvider>,
  );
  const labels = [...target.querySelectorAll('[role="img"]')].map((el) => el.getAttribute("aria-label"));
  expect(labels).toEqual(["Chef · Idle", "Chef · Working", "Chef · Needs attention"]);
});
