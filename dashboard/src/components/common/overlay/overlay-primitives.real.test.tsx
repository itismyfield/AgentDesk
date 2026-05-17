// @vitest-environment happy-dom

import { act, useState, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import { BottomSheet } from "./BottomSheet";
import { Modal } from "./Modal";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

describe("overlay primitives with real dialog libraries", () => {
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

  async function click(element: HTMLElement) {
    await act(async () => {
      element.click();
    });
  }

  async function pressEscape() {
    await act(async () => {
      document.dispatchEvent(
        new KeyboardEvent("keydown", {
          bubbles: true,
          cancelable: true,
          key: "Escape",
        }),
      );
    });
  }

  async function waitForReturnFocus() {
    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 0));
    });
  }

  async function finishCloseAnimation(element: Element | null) {
    if (!element) return;
    await act(async () => {
      element.dispatchEvent(new Event("animationend", { bubbles: true }));
    });
  }

  function warningsMatching(spy: ReturnType<typeof vi.spyOn>, text: string) {
    return spy.mock.calls.filter((call: unknown[]) =>
      String(call[0]).includes(text),
    );
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
    vi.restoreAllMocks();
    document.body.removeAttribute("style");
  });

  it("closes a real Radix modal on Escape without description warnings and restores trigger focus", async () => {
    const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});

    function Harness() {
      const [open, setOpen] = useState(false);
      return (
        <>
          <button type="button" onClick={() => setOpen(true)}>
            Open modal
          </button>
          <Modal open={open} onClose={() => setOpen(false)} title="Real modal">
            Body
          </Modal>
        </>
      );
    }

    await render(<Harness />);
    const trigger = document.querySelector("button") as HTMLButtonElement;
    trigger.focus();
    await click(trigger);

    expect(document.querySelector("[role='dialog']")).not.toBeNull();

    await pressEscape();
    await waitForReturnFocus();

    expect(document.querySelector("[role='dialog']")).toBeNull();
    expect(document.activeElement).toBe(trigger);
    expect(warningsMatching(warnSpy, "Missing `Description`")).toHaveLength(0);
  });

  it("closes a real Vaul bottom sheet on Escape without description warnings and restores trigger focus", async () => {
    const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});

    function Harness() {
      const [open, setOpen] = useState(false);
      return (
        <>
          <button type="button" onClick={() => setOpen(true)}>
            Open sheet
          </button>
          <BottomSheet
            open={open}
            onClose={() => setOpen(false)}
            title="Real bottom sheet"
          >
            Body
          </BottomSheet>
        </>
      );
    }

    await render(<Harness />);
    const trigger = document.querySelector("button") as HTMLButtonElement;
    trigger.focus();
    await click(trigger);

    const dialog = document.querySelector("[role='dialog']");
    expect(dialog).not.toBeNull();

    await pressEscape();
    await finishCloseAnimation(dialog);
    await waitForReturnFocus();

    expect(document.querySelector("[role='dialog'][data-state='open']")).toBeNull();
    expect(document.activeElement).toBe(trigger);
    expect(warningsMatching(warnSpy, "Missing `Description`")).toHaveLength(0);
  });
});
