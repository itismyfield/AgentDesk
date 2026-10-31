import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import type { KanbanCard, TaskDispatch, WSEvent } from "../types";
import * as api from "../api/client";

export type KanbanLoadState = "idle" | "loading" | "ready" | "error";

interface KanbanContextValue {
  /** Open cards as summaries (no description); read one card with getKanbanCard for its full body. */
  kanbanCards: KanbanCard[];
  taskDispatches: TaskDispatch[];
  upsertKanbanCard: (card: KanbanCard) => void;
  upsertTaskDispatch: (dispatch: TaskDispatch) => void;
  setKanbanCards: React.Dispatch<React.SetStateAction<KanbanCard[]>>;
  deleteKanbanCard: (id: string) => void;
  kanbanCardsState: KanbanLoadState;
  reloadKanbanCards: () => void;
  requestKanbanCards: () => void;
  requestTaskDispatches: () => void;
}

const KanbanContext = createContext<KanbanContextValue | null>(null);

interface KanbanProviderProps {
  /** Cards already read elsewhere; without them the provider reads its own on first use. */
  initialCards?: KanbanCard[];
  initialDispatches?: TaskDispatch[];
  children: ReactNode;
}

const DISPATCH_LIMIT = 200;

/** A fetched list overlaid with what WS events changed while it was in flight (null: deleted). */
function overlay(fetched: KanbanCard[], touched: Map<string, KanbanCard | null>): KanbanCard[] {
  const merged = fetched.flatMap((card) => {
    if (!touched.has(card.id)) return [card];
    const latest = touched.get(card.id);
    return latest ? [latest] : [];
  });
  const listed = new Set(fetched.map((card) => card.id));
  const added = [...touched.values()].filter((card): card is KanbanCard => card !== null && !listed.has(card.id));
  return [...added, ...merged];
}

export function KanbanProvider({ initialCards, initialDispatches, children }: KanbanProviderProps) {
  const [kanbanCards, setKanbanCards] = useState<KanbanCard[]>(initialCards ?? []);
  const [taskDispatches, setTaskDispatches] = useState<TaskDispatch[]>(initialDispatches ?? []);
  const [kanbanCardsState, setKanbanCardsState] = useState<KanbanLoadState>(initialCards ? "ready" : "idle");
  const cardsRequested = useRef(initialCards !== undefined);
  const dispatchesRequested = useRef(initialDispatches !== undefined);
  // Non-null while a card read is in flight.
  const touchedDuringRead = useRef<Map<string, KanbanCard | null> | null>(null);
  const cardRead = useRef(0);

  // Replace in place so a busy WS stream does not reorder the list under the pointer.
  const upsertKanbanCard = useCallback((card: KanbanCard) => {
    touchedDuringRead.current?.set(card.id, card);
    setKanbanCards((prev) => {
      const idx = prev.findIndex((p) => p.id === card.id);
      if (idx === -1) return [card, ...prev];
      const next = prev.slice();
      next[idx] = card;
      return next;
    });
  }, []);

  const upsertTaskDispatch = useCallback((dispatch: TaskDispatch) => {
    setTaskDispatches((prev) => {
      const idx = prev.findIndex((p) => p.id === dispatch.id);
      if (idx === -1) return [dispatch, ...prev].slice(0, DISPATCH_LIMIT);
      const next = prev.slice();
      next[idx] = dispatch;
      return next;
    });
  }, []);

  const deleteKanbanCard = useCallback((id: string) => {
    touchedDuringRead.current?.set(id, null);
    setKanbanCards((prev) => prev.filter((card) => card.id !== id));
  }, []);

  const reloadKanbanCards = useCallback(() => {
    const read = ++cardRead.current;
    cardsRequested.current = true;
    touchedDuringRead.current = new Map();
    setKanbanCardsState("loading");
    api.getKanbanCards({ view: "summary", excludeDone: true }).then(
      (cards) => {
        if (read !== cardRead.current) return;
        const touched = touchedDuringRead.current ?? new Map();
        touchedDuringRead.current = null;
        setKanbanCards(overlay(cards, touched));
        setKanbanCardsState("ready");
      },
      () => {
        if (read !== cardRead.current) return;
        touchedDuringRead.current = null;
        setKanbanCardsState("error");
      },
    );
  }, []);

  const requestKanbanCards = useCallback(() => {
    if (!cardsRequested.current) reloadKanbanCards();
  }, [reloadKanbanCards]);

  const requestTaskDispatches = useCallback(() => {
    if (dispatchesRequested.current) return;
    dispatchesRequested.current = true;
    api.getTaskDispatches({ limit: DISPATCH_LIMIT }).then(
      (dispatches) => setTaskDispatches((prev) => {
        // Dispatches a WS event delivered meanwhile are newer than the read.
        const known = new Set(prev.map((dispatch) => dispatch.id));
        return [...prev, ...dispatches.filter((dispatch) => !known.has(dispatch.id))].slice(0, DISPATCH_LIMIT);
      }),
      () => { dispatchesRequested.current = false; },
    );
  }, []);

  useEffect(() => {
    function handleWs(e: Event) {
      const event = (e as CustomEvent<WSEvent>).detail;
      switch (event.type) {
        case "kanban_card_created":
        case "kanban_card_updated":
          upsertKanbanCard(event.payload as KanbanCard);
          break;
        case "kanban_card_deleted":
          deleteKanbanCard((event.payload as { id: string }).id);
          break;
        case "task_dispatch_created":
        case "task_dispatch_updated":
          upsertTaskDispatch(event.payload as TaskDispatch);
          break;
      }
    }
    window.addEventListener("pcd-ws-event", handleWs);
    return () => window.removeEventListener("pcd-ws-event", handleWs);
  }, [upsertKanbanCard, upsertTaskDispatch, deleteKanbanCard]);

  return (
    <KanbanContext.Provider value={{
      kanbanCards, taskDispatches, upsertKanbanCard, upsertTaskDispatch, setKanbanCards, deleteKanbanCard,
      kanbanCardsState, reloadKanbanCards, requestKanbanCards, requestTaskDispatches,
    }}>
      {children}
    </KanbanContext.Provider>
  );
}

function useKanbanContext(): KanbanContextValue {
  const ctx = useContext(KanbanContext);
  if (!ctx) throw new Error("useKanban must be used within KanbanProvider");
  return ctx;
}

/** Cards and their mutators; the first caller starts the card read. */
export function useKanban(): KanbanContextValue {
  const ctx = useKanbanContext();
  const { requestKanbanCards } = ctx;
  useEffect(() => requestKanbanCards(), [requestKanbanCards]);
  return ctx;
}

/** Recent dispatches (up to 200), read once on first use and kept current by WS events. */
export function useTaskDispatches(): TaskDispatch[] {
  const { taskDispatches, requestTaskDispatches } = useKanbanContext();
  useEffect(() => requestTaskDispatches(), [requestTaskDispatches]);
  return taskDispatches;
}
