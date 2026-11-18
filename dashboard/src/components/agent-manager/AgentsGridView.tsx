import { useRef, type KeyboardEvent } from "react";
import type { Agent } from "../../types";
import AgentCard, { type AgentCardViewMode } from "./AgentCard";
import type { Translator } from "./types";

interface AgentsGridViewProps {
  agents: Agent[];
  locale: string;
  tr: Translator;
  viewMode: AgentCardViewMode;
  selectedAgentId: string | null;
  onSelectAgent: (agent: Agent) => void;
  topSkillsByAgent: Map<string, string[]>;
}

/** The agent collection in either layout: a card grid or dense rows. Arrow keys move between agents. */
export function AgentsGridView({
  agents,
  locale,
  tr,
  viewMode,
  selectedAgentId,
  onSelectAgent,
  topSkillsByAgent,
}: AgentsGridViewProps) {
  const listRef = useRef<HTMLUListElement>(null);

  const move = (from: HTMLElement, delta: number) => {
    const nodes = Array.from(listRef.current?.querySelectorAll<HTMLButtonElement>("[data-agent-item]") ?? []);
    const index = nodes.indexOf(from as HTMLButtonElement);
    const next = nodes[Math.max(0, Math.min(nodes.length - 1, index + delta))];
    next?.focus();
    next?.scrollIntoView?.({ block: "nearest" });
  };
  const onKeyDown = (event: KeyboardEvent<HTMLButtonElement>) => {
    if (event.altKey || event.metaKey || event.ctrlKey) return;
    if (event.key === "ArrowDown" || event.key === "ArrowRight") {
      event.preventDefault();
      move(event.currentTarget, 1);
    } else if (event.key === "ArrowUp" || event.key === "ArrowLeft") {
      event.preventDefault();
      move(event.currentTarget, -1);
    } else if (event.key === "Home" || event.key === "End") {
      event.preventDefault();
      move(event.currentTarget, event.key === "Home" ? -agents.length : agents.length);
    }
  };

  return (
    <ul
      ref={listRef}
      data-testid={`agents-view-${viewMode}`}
      className={`m-0 min-w-0 list-none p-0 ${
        viewMode === "grid" ? "grid grid-cols-1 gap-2.5 @xl:grid-cols-2 @5xl:grid-cols-3" : "flex flex-col gap-0.5"
      }`}
    >
      {agents.map((agent, index) => (
        <li
          key={agent.id}
          className="@container min-w-0 motion-safe:animate-[agent-rise_360ms_cubic-bezier(.22,1,.36,1)_both]"
          style={{ animationDelay: `${Math.min(index, 12) * 30}ms` }}
        >
          <AgentCard
            agent={agent}
            locale={locale}
            tr={tr}
            topSkills={topSkillsByAgent.get(agent.role_id || agent.id) ?? []}
            viewMode={viewMode}
            selected={agent.id === selectedAgentId}
            onSelect={() => onSelectAgent(agent)}
            onKeyDown={onKeyDown}
          />
        </li>
      ))}
    </ul>
  );
}
