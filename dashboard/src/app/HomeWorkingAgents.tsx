import type { Agent } from "../types";
import AgentPersona from "../components/AgentPersona";
import { HomeWidgetShell } from "./HomeOverviewWidgets";

/** Agents working right now, each with a one-line task summary. */
export function HomeWorkingAgents({
  agents,
  isKo,
  onOpenAgent,
}: {
  agents: Agent[];
  isKo: boolean;
  onOpenAgent: (agent: Agent) => void;
}) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const working = agents.filter((agent) => agent.status === "working");
  const idleCount = agents.length - working.length;

  return (
    <HomeWidgetShell
      title={tr("지금 일하는 에이전트", "Working now")}
      subtitle={
        working.length > 0
          ? tr(`${working.length}명 작업 중 · ${idleCount}명 대기`, `${working.length} working · ${idleCount} idle`)
          : tr("지금은 모두 대기 중이에요.", "Everyone is idle right now.")
      }
    >
      {working.length === 0 ? (
        <p className="py-2 text-sm text-th-text-muted">
          {tr("새 작업이 들어오면 여기에 바로 보여요.", "New work shows up here as soon as it starts.")}
        </p>
      ) : (
        <ul data-testid="home-working-agents" className="divide-y divide-th-border-subtle">
          {working.slice(0, 8).map((agent) => (
            <li key={agent.id}>
              <button
                type="button"
                onClick={() => onOpenAgent(agent)}
                className="flex min-h-12 w-full items-center gap-3 py-2 text-left hover:bg-surface-subtle focus-visible:outline-2 focus-visible:outline-[var(--th-accent-primary)]"
              >
                <AgentPersona agent={agent} size={32} />
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-sm font-medium text-th-text-heading">
                    {isKo ? agent.name_ko || agent.name : agent.name}
                  </span>
                  <span className="block truncate text-xs text-th-text-muted">
                    {agent.session_info?.split("\n")[0] || tr("작업 중", "Working")}
                  </span>
                </span>
                {(agent.agentdesk_working_count ?? 0) > 1 ? (
                  <span className="shrink-0 rounded-md bg-surface-medium px-1.5 py-0.5 text-xs tabular-nums text-th-text-muted">
                    {tr(`세션 ${agent.agentdesk_working_count}개`, `${agent.agentdesk_working_count} sessions`)}
                  </span>
                ) : null}
              </button>
            </li>
          ))}
        </ul>
      )}
    </HomeWidgetShell>
  );
}
