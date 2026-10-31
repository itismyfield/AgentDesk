// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import * as api from "../api/client";
import type { KanbanCard } from "../types";
import { KanbanProvider, useKanban, useTaskDispatches } from "./KanbanContext";

vi.mock("../api/client", () => ({ getKanbanCards: vi.fn(), getTaskDispatches: vi.fn() }));

const card = (id: string, title = id) => ({ id, title, status: "backlog" }) as KanbanCard;
let container: HTMLDivElement;
let root: Root;
beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.clearAllMocks(); });

function Cards() {
  const { kanbanCards, kanbanCardsState } = useKanban();
  return <p data-state={kanbanCardsState}>{kanbanCards.map((value) => value.title).join(",")}</p>;
}
function Dispatches() {
  return <i>{useTaskDispatches().length}</i>;
}
const emit = (type: string, payload: unknown) => window.dispatchEvent(new CustomEvent("pcd-ws-event", { detail: { type, payload } }));

it("reads nothing until a consumer asks, then reads open card summaries once", async () => {
  vi.mocked(api.getKanbanCards).mockResolvedValue([card("a")]);
  await act(async () => root.render(<KanbanProvider><span /></KanbanProvider>));
  expect(api.getKanbanCards).not.toHaveBeenCalled();
  await act(async () => root.render(<KanbanProvider><Cards /><Cards /></KanbanProvider>));
  expect(api.getKanbanCards).toHaveBeenCalledExactlyOnceWith({ view: "summary", excludeDone: true });
  expect(container.querySelector("p")?.textContent).toBe("a");
  expect(container.querySelector("p")?.getAttribute("data-state")).toBe("ready");
  expect(api.getTaskDispatches).not.toHaveBeenCalled();
});

it("keeps WS changes that arrive while the first read is in flight", async () => {
  let answer: (cards: KanbanCard[]) => void = () => {};
  vi.mocked(api.getKanbanCards).mockImplementation(() => new Promise((resolve) => { answer = resolve; }));
  await act(async () => root.render(<KanbanProvider><Cards /></KanbanProvider>));
  await act(async () => { emit("kanban_card_updated", card("a", "a-new")); emit("kanban_card_created", card("c")); emit("kanban_card_deleted", { id: "b" }); });
  await act(async () => answer([card("a"), card("b")]));
  expect(container.querySelector("p")?.textContent).toBe("c,a-new");
});

it("uses cards it is given without reading, and reads dispatches only for a dispatch consumer", async () => {
  vi.mocked(api.getTaskDispatches).mockResolvedValue([{ id: "d1" }] as never);
  await act(async () => root.render(<KanbanProvider initialCards={[card("given")]}><Cards /><Dispatches /></KanbanProvider>));
  expect(api.getKanbanCards).not.toHaveBeenCalled();
  expect(container.querySelector("p")?.textContent).toBe("given");
  expect(api.getTaskDispatches).toHaveBeenCalledExactlyOnceWith({ limit: 200 });
  expect(container.querySelector("i")?.textContent).toBe("1");
});

it("reads the cards again after a failed read and after the socket reconnects", async () => {
  vi.useFakeTimers();
  try {
    vi.mocked(api.getKanbanCards).mockRejectedValueOnce(new Error("down")).mockResolvedValue([card("a")]);
    await act(async () => root.render(<KanbanProvider><Cards /></KanbanProvider>));
    expect(container.querySelector("p")?.getAttribute("data-state")).toBe("error");
    await act(async () => { await vi.advanceTimersByTimeAsync(30_000); });
    expect(container.querySelector("p")?.getAttribute("data-state")).toBe("ready");
    expect(api.getKanbanCards).toHaveBeenCalledTimes(2);
    await act(async () => emit("connected", null));
    expect(api.getKanbanCards).toHaveBeenCalledTimes(3);
  } finally { vi.useRealTimers(); }
});
