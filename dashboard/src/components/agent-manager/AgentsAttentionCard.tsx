import { ChevronRight } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router";
import { getTaskDispatches } from "../../api";
import { useKanban } from "../../contexts/KanbanContext";
import { localeName } from "../../i18n";
import { Button } from "../../kit/free/button-clean";
import type { Agent, TaskDispatch } from "../../types";
import AgentPersona from "../AgentPersona";
import { Cap, CardNote, Chip, Row, RowButton, RowLink, SectionCard } from "../common/KitPrimitives";
import { buildTodoItems, todoStatus, type SourceState, type Tr } from "../home/homeModel";
import { buildAttention, type AttentionEntry } from "./agentsAttention";

export interface AgentsAttention {
  entries: AttentionEntry[];
  cards: SourceState;
  dispatches: SourceState;
  status: ReturnType<typeof todoStatus>;
}

/** Reads the attention sources once: open cards from the shared context (as home) and the last 50 failed dispatches. */
export function useAgentsAttention(agents: Agent[], tr: Tr, isKo: boolean): AgentsAttention {
  const { kanbanCards, kanbanCardsState } = useKanban();
  const [failed, setFailed] = useState<{ rows: TaskDispatch[]; state: SourceState }>({ rows: [], state: "loading" });
  useEffect(() => {
    let active = true;
    getTaskDispatches({ status: "failed", limit: 50 }, { suppressErrorToast: true }).then(
      (rows) => { if (active) setFailed({ rows, state: "ok" }); },
      () => { if (active) setFailed({ rows: [], state: "failed" }); },
    );
    return () => { active = false; };
  }, []);
  const entries = useMemo(() => {
    const todo = buildTodoItems({ cards: kanbanCards, campaigns: null, tr, isKo });
    return buildAttention({ agents, todo, failed: failed.rows, cardsRead: kanbanCardsState !== "error", tr, isKo });
  }, [agents, kanbanCards, kanbanCardsState, failed.rows, tr, isKo]);
  const cards: SourceState = kanbanCardsState === "error" ? "failed" : kanbanCardsState === "ready" ? "ok" : "loading";
  return { entries, cards, dispatches: failed.state, status: todoStatus(entries.length, [cards, failed.state]) };
}

/** "Needs a look" above the roster; it stays away while nothing needs a person. Desktop rows end in a button to the
 * card's queue or the agent detail; phone rows open the same place. */
export function AgentsAttentionCard({ attention, locale, isMobile, tr, onOpenAgent }: {
  attention: AgentsAttention;
  locale: string;
  isMobile: boolean;
  tr: Tr;
  onOpenAgent: (agentId: string) => void;
}) {
  const navigate = useNavigate();
  const { entries, cards, dispatches, status } = attention;
  if (status !== "ready" && status !== "partial") return null;
  return (
    <SectionCard
      title={tr("살펴볼 것", "Needs a look")}
      count={entries.length || null}
      countTone="warn"
      testId="agents-attention"
    >
      {entries.length > 0 ? (
        <ul className="m-0 list-none border-t border-[var(--line-soft)] p-0">
          {entries.map(({ agent, reasons }) => {
            const [first] = reasons;
            const more = reasons.length - 1;
            const open = () => (first.to ? navigate(first.to) : onOpenAgent(agent.id));
            const body = (
              <>
                <span aria-hidden className="flex"><AgentPersona agent={agent} size={32} /></span>
                <span className="min-w-0 flex-1">
                  <span className="flex min-w-0 items-center gap-2">
                    <span className="truncate text-sm leading-5 font-medium">{localeName(locale, agent)}</span>
                    <Chip tone={first.tone}>{first.chip}</Chip>
                  </span>
                  <Cap className="mt-0.5 block truncate" title={first.detail}>
                    {more > 0 ? tr(`${first.detail} 외 ${more}건`, `${first.detail} +${more} more`) : first.detail}
                  </Cap>
                </span>
              </>
            );
            return (
              <li key={agent.id} className="flex min-w-0 items-center border-t border-[var(--line-soft)] first:border-t-0">
                {isMobile ? (
                  first.to ? (
                    <RowLink to={first.to} className="min-h-14">{body}<ChevronRight size={16} aria-hidden className="shrink-0 text-[var(--fg-muted)]" /></RowLink>
                  ) : (
                    <RowButton className="min-h-14" onClick={open}>{body}<ChevronRight size={16} aria-hidden className="shrink-0 text-[var(--fg-muted)]" /></RowButton>
                  )
                ) : (
                  <>
                    <Row divider={false} className="min-h-14 flex-1">{body}</Row>
                    <span className="shrink-0 pr-4">
                      <Button size="lg" variant="secondary" onClick={open}>{first.to ? tr("대기열", "Queue") : tr("상세", "Details")}</Button>
                    </span>
                  </>
                )}
              </li>
            );
          })}
        </ul>
      ) : null}
      {cards === "failed" ? <CardNote>{tr("작업 카드를 읽지 못해 일부는 확인 불가예요.", "Task cards did not load, so some are unknown.")}</CardNote> : null}
      {dispatches === "failed" ? <CardNote>{tr("실패 기록을 읽지 못해 일부는 확인 불가예요.", "Failed runs did not load, so some are unknown.")}</CardNote> : null}
    </SectionCard>
  );
}
