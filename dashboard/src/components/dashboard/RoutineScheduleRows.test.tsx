// @vitest-environment happy-dom

import { act, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import { RoutineScheduleList, type RoutineRowView } from "./RoutineScheduleRows";

const row = (id: string, patch: Partial<RoutineRowView> = {}): RoutineRowView => ({
  id,
  title: `${id} 목적`,
  slug: id,
  schedule: "매일 09:00",
  agent: null,
  tone: "success",
  paused: false,
  nextText: "오늘 14:00",
  relative: "2시간 후",
  latestStatus: "최근 성공",
  latestTone: "success",
  threadUrl: null,
  enabled: true,
  label: `${id} 목적, 매일 09:00`,
  threadLabel: `${id} 디스코드 스레드 열기`,
  switchLabel: `${id} 자동 실행`,
  switchLocked: false,
  switchPending: false,
  ...patch,
});

describe("RoutineScheduleList", () => {
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
  });

  it("switches routines on and off, locks detached and in-flight rows, and opens with the row as trigger", async () => {
    const onOpen = vi.fn();
    const onToggle = vi.fn();
    const view = await render(
      <RoutineScheduleList
        rows={[
          row("a"),
          row("b", { enabled: false, paused: true, nextText: "일시정지", relative: null, threadUrl: "https://discord.com/channels/1/2" }),
          row("c", { enabled: false, paused: true, switchLocked: true, switchLabel: "c 자동 실행 · 분리된 루틴이라 켜고 끌 수 없어요" }),
          row("d", { switchPending: true }),
        ]}
        selectedId="b"
        onOpen={onOpen}
        onToggle={onToggle}
      />,
    );
    const switches = Array.from(view.querySelectorAll<HTMLInputElement>("input[role='switch']"));
    expect(switches.map((input) => [input.checked, input.disabled, input.getAttribute("aria-label")])).toEqual([
      [true, false, "a 자동 실행"],
      [false, false, "b 자동 실행"],
      [false, true, "c 자동 실행 · 분리된 루틴이라 켜고 끌 수 없어요"],
      [true, true, "d 자동 실행"],
    ]);
    expect(switches[3].getAttribute("aria-busy")).toBe("true");
    await act(async () => {
      switches[1].click();
    });
    expect(onToggle).toHaveBeenCalledWith("b");
    expect(onOpen).not.toHaveBeenCalled();

    const second = view.querySelector<HTMLButtonElement>("[data-row='b']")!;
    expect(second.getAttribute("aria-current")).toBe("true");
    expect(view.querySelector("[data-row='a']")!.hasAttribute("aria-current")).toBe(false);
    expect(view.querySelector("[data-testid='routine-thread-b']")!.getAttribute("href")).toBe("https://discord.com/channels/1/2");
    await act(async () => {
      second.click();
    });
    expect(onOpen).toHaveBeenCalledWith("b", second);
  });

  it("moves focus between rows with arrow keys and J/K", async () => {
    const view = await render(<RoutineScheduleList rows={[row("a"), row("b"), row("c")]} onOpen={() => {}} />);
    const button = (id: string) => view.querySelector<HTMLButtonElement>(`[data-row='${id}']`)!;
    button("a").focus();
    const press = async (key: string) => {
      await act(async () => {
        document.activeElement!.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true }));
      });
    };
    await press("ArrowDown");
    expect(document.activeElement).toBe(button("b"));
    await press("j");
    expect(document.activeElement).toBe(button("c"));
    await press("ArrowDown");
    expect(document.activeElement).toBe(button("c"));
    await press("k");
    expect(document.activeElement).toBe(button("b"));
  });

  it("keeps the latest result under the name on the home card and drops the switch", async () => {
    const view = await render(<RoutineScheduleList compact rows={[row("a", { latestStatus: "최근 실패", latestTone: "danger" })]} onOpen={() => {}} />);
    expect(view.querySelector("input[role='switch']")).toBeNull();
    expect(view.querySelector("[data-row='a']")!.textContent).toContain("최근 실패");
  });
});
