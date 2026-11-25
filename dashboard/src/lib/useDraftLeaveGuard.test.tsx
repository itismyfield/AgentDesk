// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useDraftLeaveGuard, type DraftLeaveGuard } from "./useDraftLeaveGuard";

let latestGuard: DraftLeaveGuard | null = null;

function GuardProbe({ dirty }: { dirty: boolean }) {
  latestGuard = useDraftLeaveGuard(dirty);
  return <span data-open={String(latestGuard.confirmOpen)} />;
}

describe("useDraftLeaveGuard", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    latestGuard = null;
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    container.remove();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("attaches beforeunload only while dirty is true", async () => {
    await act(async () => {
      root.render(<GuardProbe dirty={false} />);
    });

    const cleanEvent = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(cleanEvent);
    expect(cleanEvent.defaultPrevented).toBe(false);

    await act(async () => {
      root.render(<GuardProbe dirty={true} />);
    });

    const dirtyEvent = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(dirtyEvent);
    expect(dirtyEvent.defaultPrevented).toBe(true);
  });

  it("runs proceed immediately when clean and gates behind confirmOpen when dirty", async () => {
    const proceed = vi.fn();

    await act(async () => {
      root.render(<GuardProbe dirty={false} />);
    });
    act(() => {
      latestGuard!.confirmLeave(proceed);
    });
    expect(proceed).toHaveBeenCalledTimes(1);
    expect(latestGuard!.confirmOpen).toBe(false);

    proceed.mockClear();
    await act(async () => {
      root.render(<GuardProbe dirty={true} />);
    });
    act(() => {
      latestGuard!.confirmLeave(proceed);
    });
    expect(proceed).not.toHaveBeenCalled();
    expect(latestGuard!.confirmOpen).toBe(true);

    act(() => {
      latestGuard!.cancelLeave();
    });
    expect(proceed).not.toHaveBeenCalled();
    expect(latestGuard!.confirmOpen).toBe(false);

    act(() => {
      latestGuard!.confirmLeave(proceed);
    });
    act(() => {
      latestGuard!.acceptLeave();
    });
    expect(proceed).toHaveBeenCalledTimes(1);
    expect(latestGuard!.confirmOpen).toBe(false);
  });
});
