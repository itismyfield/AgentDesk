// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { RoutineRecord } from "../../api";
import { ToastProvider } from "../common/overlay/ToastProvider";
import type { TFunction } from "./model";

const api = vi.hoisted(() => ({
  getRoutines: vi.fn(),
  getRoutineRuns: vi.fn(),
  getDiscordChannelInfo: vi.fn(),
  pauseRoutine: vi.fn(),
  resumeRoutine: vi.fn(),
}));
vi.mock("../../api", () => api);

import { RoutinesTimelineWidget } from "./RoutinesTimelineWidget";

const koT: TFunction = (translations) => translations.ko;

function routine(id: string, status: string): RoutineRecord {
  return {
    id,
    agent_id: null,
    script_ref: `${id}.js`,
    name: id,
    status,
    execution_strategy: "fresh",
    schedule: "0 9 * * *",
    next_due_at: null,
    last_run_at: null,
    last_result: null,
    checkpoint: null,
    discord_thread_id: null,
    timeout_secs: null,
    in_flight_run_id: null,
    created_at: "2026-11-01T00:00:00Z",
    updated_at: "2026-11-01T00:00:00Z",
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

const flush = () =>
  act(async () => {
    for (let index = 0; index < 5; index += 1) await Promise.resolve();
  });

describe("RoutinesTimelineWidget pause and resume", () => {
  let container: HTMLDivElement;
  let root: Root;
  const switchOf = (id: string) => container.querySelector<HTMLInputElement>(`[data-testid='routine-switch-${id}']`)!;

  beforeEach(async () => {
    api.getRoutines.mockReset();
    api.getRoutineRuns.mockReset().mockResolvedValue([]);
    api.getDiscordChannelInfo.mockReset().mockResolvedValue({});
    api.pauseRoutine.mockReset();
    api.resumeRoutine.mockReset();
    if (!("ResizeObserver" in window)) {
      vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} unobserve() {} });
    }
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    document.body.innerHTML = "";
    vi.unstubAllGlobals();
  });

  async function mount() {
    await act(async () => {
      root.render(
        <ToastProvider>
          <RoutinesTimelineWidget t={koT} localeTag="ko-KR" language="ko" />
        </ToastProvider>,
      );
    });
    await flush();
  }

  it("pauses at once, locks only that switch, then reloads the list and the detail follows", async () => {
    api.getRoutines
      .mockResolvedValueOnce([routine("rt-a", "enabled"), routine("rt-b", "enabled"), routine("rt-c", "detached")])
      .mockResolvedValue([routine("rt-a", "paused"), routine("rt-b", "enabled"), routine("rt-c", "detached")]);
    const request = deferred<{ ok: boolean; routine_id: string }>();
    api.pauseRoutine.mockReturnValue(request.promise);
    await mount();

    expect(switchOf("rt-c").disabled).toBe(true);
    expect(switchOf("rt-c").getAttribute("aria-label")).toContain("분리된 루틴이라 켜고 끌 수 없어요");
    await act(async () => container.querySelector<HTMLButtonElement>("[data-row='rt-a']")!.click());
    const pane = () => container.querySelector("[data-testid='routine-detail-pane']")!;
    expect(pane().textContent).toContain("활성");

    await act(async () => switchOf("rt-a").click());
    expect(api.pauseRoutine).toHaveBeenCalledWith("rt-a");
    expect(switchOf("rt-a").checked).toBe(false);
    expect(switchOf("rt-a").disabled).toBe(true);
    expect(switchOf("rt-b").disabled).toBe(false);
    expect(pane().textContent).toContain("일시정지");

    await act(async () => request.resolve({ ok: true, routine_id: "rt-a" }));
    await flush();
    expect(api.getRoutines).toHaveBeenCalledTimes(2);
    expect(switchOf("rt-a").checked).toBe(false);
    expect(switchOf("rt-a").disabled).toBe(false);
    expect(document.body.textContent).not.toContain("하지 못했어요");
  });

  it("puts the switch back and shows a toast when the server refuses", async () => {
    api.getRoutines.mockResolvedValue([routine("rt-b", "paused")]);
    api.resumeRoutine.mockRejectedValue(new Error("paused routine rt-b not found"));
    await mount();

    expect(switchOf("rt-b").checked).toBe(false);
    await act(async () => switchOf("rt-b").click());
    await flush();
    expect(api.resumeRoutine).toHaveBeenCalledWith("rt-b");
    expect(switchOf("rt-b").checked).toBe(false);
    expect(switchOf("rt-b").disabled).toBe(false);
    expect(document.body.textContent).toContain("rt-b 다시 켜지 못했어요: paused routine rt-b not found");
    expect(api.getRoutines).toHaveBeenCalledTimes(1);
  });

  it("keeps the newer list when an older read answers last and keeps the list when a refresh fails", async () => {
    vi.useFakeTimers();
    const first = deferred<RoutineRecord[]>();
    api.getRoutines.mockReturnValueOnce(first.promise).mockResolvedValueOnce([routine("fresh", "enabled")]).mockRejectedValue(new Error("down"));
    await act(async () => {
      root.render(<ToastProvider><RoutinesTimelineWidget t={koT} localeTag="ko-KR" language="ko" /></ToastProvider>);
    });
    await act(async () => { vi.advanceTimersByTime(60_000); });
    await act(async () => first.resolve([routine("stale", "enabled")]));
    await flush();
    expect(container.textContent).toContain("fresh");
    expect(container.textContent).not.toContain("stale");

    await act(async () => { vi.advanceTimersByTime(60_000); });
    await flush();
    expect(container.querySelector("[data-testid='routines-refresh-error']")).toBeTruthy();
    expect(container.textContent).toContain("fresh");
    vi.useRealTimers();
  });

  it("renders English ScheduledRuns labels without Korean leaks when language='en'", async () => {
    const enT: TFunction = (translations) => translations.en;
    api.getRoutines.mockResolvedValue([{ ...routine("rt-en", "enabled"), agent_id: "personal-obiseo" }]);
    await act(async () => {
      root.render(
        <ToastProvider>
          <RoutinesTimelineWidget t={enT} localeTag="en-US" language="en" />
        </ToastProvider>,
      );
    });
    await flush();

    const board = container.querySelector<HTMLElement>("[data-testid='routines-scheduled-runs']")!;
    expect(board.textContent).toContain("7-day routine board");
    expect(board.textContent).toContain("Unscheduled");
    expect(board.textContent).toContain("1 routines");
    expect(board.querySelector("[role='grid']")?.getAttribute("aria-label")).toBe("Sessions by day");
    expect(board.querySelector(".aui-scheduled-runs-agents")?.getAttribute("aria-label")).toBe("Highlight agent");
    expect(board.textContent).not.toMatch(/[가-힣]/);
  });
});
