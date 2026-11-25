// @vitest-environment happy-dom
import { act, createElement, Fragment } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useNow } from "./useNow";

function ClockProbe({
  id,
  intervalMs = 1_000,
  enabled = true,
}: {
  id: string;
  intervalMs?: number;
  enabled?: boolean;
}) {
  const now = useNow(intervalMs, enabled);
  return createElement("span", { "data-testid": id }, String(now));
}

describe("useNow", () => {
  let container: HTMLDivElement;
  let root: Root;
  let hidden = false;

  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-11-25T00:00:00Z"));
    hidden = false;
    Object.defineProperty(document, "hidden", {
      configurable: true,
      get: () => hidden,
    });
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    container.remove();
    vi.useRealTimers();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("shares a single setInterval across multiple subscribers with the same intervalMs", async () => {
    const setIntervalSpy = vi.spyOn(window, "setInterval");
    const clearIntervalSpy = vi.spyOn(window, "clearInterval");

    await act(async () => {
      root.render(
        createElement(
          Fragment,
          null,
          createElement(ClockProbe, { id: "a", intervalMs: 1_000 }),
          createElement(ClockProbe, { id: "b", intervalMs: 1_000 }),
          createElement(ClockProbe, { id: "c", intervalMs: 5_000 }),
        ),
      );
    });

    expect(setIntervalSpy).toHaveBeenCalledTimes(2);
    const startMs = Date.now();
    expect(container.querySelector("[data-testid='a']")?.textContent).toBe(String(startMs));
    expect(container.querySelector("[data-testid='b']")?.textContent).toBe(String(startMs));

    await act(async () => {
      vi.advanceTimersByTime(1_000);
    });
    expect(container.querySelector("[data-testid='a']")?.textContent).toBe(String(startMs + 1_000));
    expect(container.querySelector("[data-testid='b']")?.textContent).toBe(String(startMs + 1_000));
    expect(container.querySelector("[data-testid='c']")?.textContent).toBe(String(startMs));

    await act(async () => {
      root.render(createElement(ClockProbe, { id: "b", intervalMs: 1_000 }));
    });
    expect(clearIntervalSpy).toHaveBeenCalledTimes(1);

    await act(async () => {
      root.render(null);
    });
    expect(clearIntervalSpy).toHaveBeenCalledTimes(2);
  });

  it("does not allocate a timer when enabled is false", async () => {
    const setIntervalSpy = vi.spyOn(window, "setInterval");

    await act(async () => {
      root.render(createElement(ClockProbe, { id: "off", intervalMs: 1_000, enabled: false }));
    });

    expect(setIntervalSpy).not.toHaveBeenCalled();
  });

  it("pauses while document is hidden and refreshes immediately when visible again", async () => {
    const clearIntervalSpy = vi.spyOn(window, "clearInterval");

    await act(async () => {
      root.render(createElement(ClockProbe, { id: "clock", intervalMs: 1_000 }));
    });
    const startMs = Date.now();

    await act(async () => {
      hidden = true;
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(clearIntervalSpy).toHaveBeenCalledTimes(1);

    await act(async () => {
      vi.advanceTimersByTime(4_000);
    });
    expect(container.querySelector("[data-testid='clock']")?.textContent).toBe(String(startMs));

    await act(async () => {
      hidden = false;
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(container.querySelector("[data-testid='clock']")?.textContent).toBe(String(startMs + 4_000));

    await act(async () => {
      vi.advanceTimersByTime(1_000);
    });
    expect(container.querySelector("[data-testid='clock']")?.textContent).toBe(String(startMs + 5_000));
  });
});
