// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from "vitest";
import { pollWhileVisible } from "./pollWhileVisible";

function setHidden(hidden: boolean) {
  Object.defineProperty(document, "hidden", { configurable: true, get: () => hidden });
  document.dispatchEvent(new Event("visibilitychange"));
}

describe("pollWhileVisible", () => {
  afterEach(() => {
    vi.useRealTimers();
    setHidden(false);
  });

  it("skips hidden ticks, catches up on return and stops after cleanup", () => {
    vi.useFakeTimers();
    const run = vi.fn();
    const stop = pollWhileVisible(run, 1_000);
    vi.advanceTimersByTime(1_000);
    expect(run).toHaveBeenCalledTimes(1);

    setHidden(true);
    vi.advanceTimersByTime(3_000);
    expect(run).toHaveBeenCalledTimes(1);
    setHidden(false);
    expect(run).toHaveBeenCalledTimes(2);

    stop();
    vi.advanceTimersByTime(3_000);
    setHidden(true);
    setHidden(false);
    expect(run).toHaveBeenCalledTimes(2);
  });
});
