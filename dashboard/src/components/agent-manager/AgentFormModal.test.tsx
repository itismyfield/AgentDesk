// @vitest-environment happy-dom

import { readdirSync } from "node:fs";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import AgentFormModal from "./AgentFormModal";
import { BLANK } from "./constants";

vi.mock("../../api/providers", () => ({
  useProviderCatalog: () => ({ loading: false, error: null, selectableIds: ["claude"], entries: [] }),
  catalogLabel: (_entries: unknown, provider: string) => provider,
}));
vi.mock("./AgentPromptEditor", () => ({ default: () => null }));

let root: Root | undefined;
let container: HTMLDivElement;

beforeEach(() => vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true));

afterEach(async () => {
  await act(async () => root?.unmount());
  container?.remove();
  vi.unstubAllGlobals();
});

it.each([null, 20, 30, 39, 40])("keeps button and keyboard portrait selection in the shipped range from %s", async (initial) => {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => {
    root!.render(<AgentFormModal isKo={false} locale="en" tr={(_ko, en) => en}
      form={{ ...BLANK, sprite_number: initial }} departments={[]} isEdit={false}
      saving={false} onSave={() => {}} onClose={() => {}} />);
  });
  const picker = container.querySelector<HTMLElement>('[role="spinbutton"]')!;
  const shipped = readdirSync("public/sprites")
    .filter((name) => /^\d+-D-1\.png$/.test(name)).map((name) => Number(name.split("-")[0]));
  expect(picker.getAttribute("aria-valuemax")).toBe(String(Math.max(...shipped)));
  await act(async () => {
    picker.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowUp", bubbles: true }));
  });
  const afterKey = Math.min(40, (initial ?? 0) + 1);
  expect(picker.getAttribute("aria-valuenow")).toBe(String(afterKey));
  await act(async () => {
    container.querySelector<HTMLButtonElement>('button[aria-label="Next Sprite"]')!.click();
  });
  expect(picker.getAttribute("aria-valuenow")).toBe(String(Math.min(40, afterKey + 1)));
});
