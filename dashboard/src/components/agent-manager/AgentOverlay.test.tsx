// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { AgentOverlay } from "./AgentOverlay";

let root: Root;
let container: HTMLDivElement;

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  document.body.innerHTML = "";
  vi.unstubAllGlobals();
});

function renderOverlay(onClose: () => void, open = true) {
  act(() => {
    root.render(
      <>
        <button type="button" id="opener">open</button>
        {open ? (
          <AgentOverlay variant="sheet" ariaLabel="Edit agent" onClose={onClose} header={<button type="button">close</button>}>
            <input aria-label="name" data-autofocus />
            <input
              aria-label="inner"
              onKeyDown={(event) => {
                if (event.key === "Escape") event.preventDefault();
              }}
            />
          </AgentOverlay>
        ) : null}
      </>,
    );
  });
}

it("focuses the marked control, makes the page inert and closes on Escape", () => {
  const onClose = vi.fn();
  renderOverlay(onClose);
  const dialog = document.querySelector<HTMLElement>('[role="dialog"]')!;
  expect(dialog.getAttribute("aria-modal")).toBe("true");
  expect(dialog.getAttribute("aria-label")).toBe("Edit agent");
  expect(document.activeElement?.getAttribute("aria-label")).toBe("name");
  expect(container.inert).toBe(true);

  act(() => {
    dialog.querySelector("input")!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  });
  expect(onClose).toHaveBeenCalledTimes(1);
});

it("leaves Escape to a nested control that already handled it", () => {
  const onClose = vi.fn();
  renderOverlay(onClose);
  const inner = document.querySelector<HTMLInputElement>('input[aria-label="inner"]')!;
  act(() => {
    inner.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
  });
  expect(onClose).not.toHaveBeenCalled();
});

it("closes on a backdrop press and restores the page and the opener focus", async () => {
  const onClose = vi.fn();
  act(() => {
    root.render(<button type="button" id="opener">open</button>);
  });
  const opener = document.getElementById("opener")!;
  opener.focus();
  renderOverlay(onClose);
  const backdrop = document.querySelector<HTMLElement>('[role="dialog"]')!.parentElement!;
  act(() => {
    backdrop.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
  });
  expect(onClose).toHaveBeenCalledTimes(1);

  renderOverlay(onClose, false);
  expect(container.inert).toBe(false);
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(document.activeElement).toBe(document.getElementById("opener"));
});
