// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import BotAvatarPicker from "./BotAvatarPicker";

let root: Root | null = null;
let container: HTMLDivElement | null = null;
afterEach(async () => {
  await act(async () => root?.unmount());
  container?.remove();
  root = null;
  container = null;
});

async function render(value: string, isKo = false, onChange = vi.fn()) {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => root?.render(<BotAvatarPicker agentId="ch-sw" value={value} color="#8b5cf6" isKo={isKo} tr={(ko, en) => (isKo ? ko : en)} onChange={onChange} />));
  return { trigger: container.querySelector("button") as HTMLButtonElement, onChange };
}

it("marks an aliased emoji's shape as selected and saves the picked shape's emoji", async () => {
  const { trigger, onChange } = await render("✍️");
  expect(trigger.getAttribute("aria-label")).toBe("Selected avatar: Book, change avatar");

  await act(async () => trigger.click());
  const dialog = container?.querySelector('[role="dialog"]');
  expect(dialog?.getAttribute("aria-label")).toBe("Choose an avatar");
  expect(dialog?.querySelector('[aria-pressed="true"]')?.getAttribute("aria-label")).toBe("Book");

  await act(async () => (dialog?.querySelector('[aria-label="Flask"]') as HTMLButtonElement).click());
  expect(onChange).toHaveBeenCalledWith("🧪");
  expect(container?.querySelector('[role="dialog"]')).toBeNull();
  expect(document.activeElement).toBe(trigger);
});

it("labels shapes in Korean when the form is in Korean", async () => {
  const { trigger } = await render("🏗️", true);
  expect(trigger.getAttribute("aria-label")).toBe("선택된 아바타: 안전모, 아바타 변경");
  await act(async () => trigger.click());
  expect(container?.querySelector('[role="dialog"]')?.getAttribute("aria-label")).toBe("아바타 선택");
});
