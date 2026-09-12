// @vitest-environment happy-dom

import React, { act, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import EmojiPickerLibraryPanel from "./EmojiPickerLibraryPanel";

vi.mock("emoji-picker-react", () => ({
  default: ({ onEmojiClick }: { onEmojiClick: (data: { emoji: string }) => void }) => (
    <div>
      <button type="button" className="epr-emoji" onClick={() => onEmojiClick({ emoji: "😀" })}>
        😀
      </button>
      <button type="button" className="epr-emoji">
        😃
      </button>
    </div>
  ),
  EmojiStyle: { NATIVE: "native" },
  Theme: { DARK: "dark" },
}));

describe("EmojiPickerLibraryPanel accessibility", () => {
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
      await act(async () => root?.unmount());
      root = null;
    }
    container?.remove();
    container = null;
    vi.clearAllMocks();
  });

  it("exposes false for unselected emoji buttons and updates the selected state", async () => {
    const target = await render(
      <EmojiPickerLibraryPanel height={200} onSelect={() => {}} value="😀" width={200} />,
    );
    const buttons = target.querySelectorAll<HTMLButtonElement>("button.epr-emoji");

    expect(buttons[0]?.getAttribute("aria-pressed")).toBe("true");
    expect(buttons[1]?.getAttribute("aria-pressed")).toBe("false");

    await act(async () => {
      root?.render(
        <EmojiPickerLibraryPanel height={200} onSelect={() => {}} value="😃" width={200} />,
      );
    });

    expect(buttons[0]?.getAttribute("aria-pressed")).toBe("false");
    expect(buttons[1]?.getAttribute("aria-pressed")).toBe("true");
  });
});
