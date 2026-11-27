import { formatRelativeTime } from "../../app/shellFormatting";
import type { Agent, TaskDispatch } from "../../types";
import { queueLocationQuery } from "../campaigns/campaignQueueModel";
import { detectTurnLoop, formatLoopSummary, stepsFromLiveTurnEvents } from "../../lib/agentObservability";
import type { HomeTone, HomeTurn, TodoItem, Tr } from "../home/homeModel";
import { coerceTimestampMs } from "./kanban-utils";

/** One reason an agent needs a person, with the queue link of the card behind it (none for a stuck turn). */
export interface AttentionReason {
  key: string;
  tone: HomeTone;
  chip: string;
  detail: string;
  to: string | null;
}

export interface AttentionEntry {
  agent: Agent;
  reasons: AttentionReason[];
}

const RECENT_FAILURE_MS = 86_400_000;
const join = (...parts: Array<string | null | undefined>) => parts.filter(Boolean).join(" · ");

/** Agents a person should look at, grouped per agent: a turn the server suspects is stuck, the agent's cards on
 * home's to-do list (decisions and failures), and failed dispatches from the last day. A failed dispatch with a card
 * counts only while the cards are unread: once read, the card is on the to-do list if it still needs a person, and the
 * open-card list (done cards excluded) shows it moved on or closed otherwise. */
export function buildAttention({ agents, todo, failed, turns, cardsRead, tr, isKo, now = Date.now() }: {
  agents: Agent[];
  todo: TodoItem[];
  failed: TaskDispatch[];
  turns?: ReadonlyMap<string, HomeTurn>;
  /** The open-card list loaded (or is loading); false after it failed. */
  cardsRead: boolean;
  tr: Tr;
  isKo: boolean;
  now?: number;
}): AttentionEntry[] {
  const known = new Map(agents.map((agent) => [agent.id, agent]));
  const byAgent = new Map<string, AttentionReason[]>();
  const add = (agentId: string | null | undefined, reason: AttentionReason) => {
    if (agentId && known.has(agentId)) byAgent.set(agentId, [...(byAgent.get(agentId) ?? []), reason]);
  };
  for (const agent of agents) {
    const loop = detectTurnLoop(stepsFromLiveTurnEvents(turns?.get(agent.id)?.tool_events));
    if (loop) {
      add(agent.id, {
        key: `loop:${agent.id}:${loop.key}`,
        tone: loop.pattern === "contract" ? "err" : "warn",
        chip: tr("루프 의심", "Looping"),
        detail: formatLoopSummary(loop, tr),
        to: null,
      });
      continue;
    }
    if ((agent as Agent & { visual_status_code?: string }).visual_status_code !== "stuck-suspect") continue;
    add(agent.id, {
      key: `stuck:${agent.id}`,
      tone: "warn",
      chip: tr("막힘 의심", "Maybe stuck"),
      detail: tr("도구를 실행한 뒤 5분 넘게 응답이 없어요", "No reply for 5+ minutes after a tool"),
      to: null,
    });
  }
  const cards = new Set<string>();
  // Cards on the to-do list without an owner go under the agent their latest failed dispatch went to.
  const unowned = new Map<string, TodoItem>();
  const fromTodo = (item: TodoItem): AttentionReason => ({ key: item.key, tone: item.tone, chip: item.chip, detail: join(item.title, item.detail), to: item.to });
  for (const item of todo) {
    if (!item.agentId) {
      unowned.set(item.key, item);
      continue;
    }
    cards.add(item.key);
    add(item.agentId, fromTodo(item));
  }
  for (const dispatch of failed) {
    const at = coerceTimestampMs((dispatch as TaskDispatch & { updated_at?: string | number | null }).updated_at ?? dispatch.created_at);
    const card = dispatch.kanban_card_id ? `card:${dispatch.kanban_card_id}` : null;
    if (at === null || now - at > RECENT_FAILURE_MS) continue;
    const orphan = card ? unowned.get(card) : undefined;
    if (orphan) {
      unowned.delete(orphan.key);
      add(dispatch.to_agent_id, fromTodo(orphan));
      continue;
    }
    if (card && (cardsRead || cards.has(card))) continue;
    if (card) cards.add(card);
    add(dispatch.to_agent_id, {
      key: `dispatch:${dispatch.id}`,
      tone: "err",
      chip: tr("실패", "Failed"),
      detail: join(dispatch.title, formatRelativeTime(at, isKo)),
      to: dispatch.kanban_card_id ? `/campaigns${queueLocationQuery(dispatch.kanban_card_id)}` : null,
    });
  }
  return [...byAgent].map(([id, reasons]) => ({ agent: known.get(id)!, reasons }));
}
