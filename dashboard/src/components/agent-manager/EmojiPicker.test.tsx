// @vitest-environment happy-dom

import React, { act, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import EmojiPicker from "./EmojiPicker";

vi.mock("../../i18n", () => ({
  useI18n: () => ({
    t: (input: { en?: string; ko?: string } | string) => {
      if (typeof input === "string") return input;
      return input.en || input.ko || "";
    },
  }),
}));

describe("EmojiPicker", () => {
  let container: HTMLDivElement | null = null;
  let root: Root | null = null;

  async function render(element: ReactNode) {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    await act(async () => {
      root?.render(element);
    });
    return container;
  }

  afterEach(async () => {
    if (root) {
      await act(async () => {
        root?.unmount();
      });
      root = null;
    }
    container?.remove();
    container = null;
    vi.clearAllMocks();
  });

  it("renders with aria-expanded and aria-label", async () => {
    const target = await render(<EmojiPicker value="🤖" onChange={() => {}} />);
    const button = target.querySelector("button");
    expect(button).not.toBeNull();
    expect(button?.getAttribute("aria-expanded")).toBe("false");
    expect(button?.getAttribute("aria-label")).toBe("Selected emoji: 🤖, change emoji");
    expect(button?.getAttribute("aria-haspopup")).toBe("dialog");
  });

  it("renders the dialog with a translated accessible name", async () => {
    const target = await render(<EmojiPicker value="🤖" onChange={() => {}} />);
    const button = target.querySelector("button");

    await act(async () => {
      button?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    const dialog = target.querySelector('div[role="dialog"]');
    expect(dialog).not.toBeNull();
    expect(dialog?.getAttribute("aria-label")).toBe("Choose an emoji");
  });

  it("returns focus to the trigger when dismissed by an outside click", async () => {
    const target = await render(<EmojiPicker value="🤖" onChange={() => {}} />);
    const button = target.querySelector("button") as HTMLButtonElement;
    const outside = document.createElement("div");
    document.body.appendChild(outside);

    await act(async () => {
      button.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    expect(target.querySelector('div[role="dialog"]')).not.toBeNull();

    await act(async () => {
      outside.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    });

    expect(target.querySelector('div[role="dialog"]')).toBeNull();
    expect(document.activeElement).toBe(button);

    outside.remove();
  });
});
