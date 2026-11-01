import { useMemo } from "react";
import {
  bulkKanbanAction, deleteKanbanCard, getKanbanCard, redispatchKanbanCard, retryKanbanCard, updateKanbanCard,
  type KanbanDispatchMutationResponse,
} from "../../api/kanban";
import { useKanban } from "../../contexts/KanbanContext";
import type { KanbanCard } from "../../types";

/** A dispatch write is complete only when the server started a new dispatch and asks for nothing more. */
function assertDispatched(result: KanbanDispatchMutationResponse, label: string) {
  if (result.next_action !== "none_required") throw new Error(`${label} requires follow-up: ${result.next_action}`);
  if (!result.new_dispatch_id) throw new Error(`${label} response did not include a new dispatch id.`);
}

/** Card writes for the queue; each one puts the server's answer into the shared card list. */
export function useQueueCardActions() {
  const { upsertKanbanCard, deleteKanbanCard: dropCard } = useKanban();
  return useMemo(() => ({
    async update(id: string, patch: Partial<KanbanCard>) {
      upsertKanbanCard(await updateKanbanCard(id, patch));
    },
    /** Ready needs an assignee; both are saved in order so a refused status keeps the new assignee visible. */
    async moveToReady(card: KanbanCard, assignee: string | null) {
      if (assignee && assignee !== card.assignee_agent_id) upsertKanbanCard(await updateKanbanCard(card.id, { assignee_agent_id: assignee }));
      upsertKanbanCard(await updateKanbanCard(card.id, { status: "ready" }));
    },
    async retry(id: string, payload?: { assignee_agent_id?: string | null; request_now?: boolean }) {
      const result = await retryKanbanCard(id, payload);
      upsertKanbanCard(result.card);
      assertDispatched(result, "Retry");
    },
    async redispatch(id: string, payload?: { reason?: string | null }) {
      const result = await redispatchKanbanCard(id, payload);
      upsertKanbanCard(result.card);
      assertDispatched(result, "Redispatch");
    },
    async remove(id: string) {
      await deleteKanbanCard(id);
      dropCard(id);
    },
    /** Closes the card as done; the server closes its GitHub issue with it. */
    async cancel(id: string) {
      const [result] = (await bulkKanbanAction("cancel", [id])).results;
      if (!result?.ok) throw new Error(result?.error ?? "Cancel failed");
      upsertKanbanCard(await getKanbanCard(id));
    },
    async load(id: string) {
      const card = await getKanbanCard(id);
      upsertKanbanCard(card);
      return card;
    },
  }), [upsertKanbanCard, dropCard]);
}
