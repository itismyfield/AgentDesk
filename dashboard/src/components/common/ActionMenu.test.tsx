// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ActionMenu } from "./ActionMenu";

describe("ActionMenu", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
  });

  it("closes on Escape without propagating to ancestors and uses --accent-ink on confirm buttons", async () => {
    const onAncestorKeyDown = vi.fn();
    const onConfirmAction = vi.fn();

    await act(async () => {
      root.render(
        <div onKeyDown={onAncestorKeyDown}>
          <ActionMenu
            label="Agent actions"
            items={[
              {
                key: "sync",
                label: "Sync agent",
                onSelect: onConfirmAction,
                confirm: {
                  title: "Sync agent?",
                  confirmLabel: "Run sync",
                  cancelLabel: "Cancel",
                },
              },
            ]}
          />
        </div>,
      );
    });

    const trigger = container.querySelector<HTMLButtonElement>('button[aria-label="Agent actions"]')!;
    await act(async () => {
      trigger.click();
    });
    expect(container.querySelector('[role="menu"]')).not.toBeNull();

    await act(async () => {
      trigger.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
    });
    expect(container.querySelector('[role="menu"]')).toBeNull();
    expect(onAncestorKeyDown).not.toHaveBeenCalled();
    expect(document.activeElement).toBe(trigger);

    await act(async () => {
      trigger.click();
    });
    const menuItem = container.querySelector<HTMLButtonElement>('[role="menuitem"]')!;
    await act(async () => {
      menuItem.click();
    });

    const confirmBtn = Array.from(document.body.querySelectorAll("button")).find(
      (btn) => btn.textContent === "Run sync",
    );
    expect(confirmBtn).toBeDefined();
    expect(confirmBtn?.style.background).toBe("var(--accent)");
    expect(confirmBtn?.style.color).toBe("var(--accent-ink)");
  });
});
