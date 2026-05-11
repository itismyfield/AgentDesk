import { createContext, useCallback, useContext, useEffect, useState, type ReactNode } from "react";
import type { KanbanCard, TaskDispatch, WSEvent } from "../types";
import * as api from "../api/client";

// ── Context value ──

interface KanbanContextValue {
  kanbanCards: KanbanCard[];
  taskDispatches: TaskDispatch[];
  upsertKanbanCard: (card: KanbanCard) => void;
  upsertTaskDispatch: (dispatch: TaskDispatch) => void;
  setKanbanCards: React.Dispatch<React.SetStateAction<KanbanCard[]>>;
  deleteKanbanCard: (id: string) => void;
}

const KanbanContext = createContext<KanbanContextValue | null>(null);

// ── Provider ──

interface KanbanProviderProps {
  initialCards: KanbanCard[];
  initialDispatches: TaskDispatch[];
  children: ReactNode;
}

export function KanbanProvider({ initialCards, initialDispatches, children }: KanbanProviderProps) {
  const [kanbanCards, setKanbanCards] = useState<KanbanCard[]>(initialCards);
  const [taskDispatches, setTaskDispatches] = useState<TaskDispatch[]>(initialDispatches);

  // #2050 P2 finding 3 — preserve existing order on update. Only prepend when
  // the id is genuinely new; otherwise replace in place. The previous version
  // moved every updated card to the top, causing kanban-board flicker and
  // misclicks under high-traffic emit storms (auto-queue, GitHub sync, etc).
  const upsertKanbanCard = useCallback((card: KanbanCard) => {
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
      if (idx === -1) return [dispatch, ...prev].slice(0, 200);
      const next = prev.slice();
      next[idx] = dispatch;
      return next;
    });
  }, []);

  const deleteKanbanCard = useCallback((id: string) => {
    setKanbanCards((prev) => prev.filter((card) => card.id !== id));
  }, []);

  // ── WS event handling ──
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
    <KanbanContext.Provider value={{ kanbanCards, taskDispatches, upsertKanbanCard, upsertTaskDispatch, setKanbanCards, deleteKanbanCard }}>
      {children}
    </KanbanContext.Provider>
  );
}

// ── Hook ──

export function useKanban(): KanbanContextValue {
  const ctx = useContext(KanbanContext);
  if (!ctx) throw new Error("useKanban must be used within KanbanProvider");
  return ctx;
}
