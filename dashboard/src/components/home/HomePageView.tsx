import { useCallback, useMemo } from "react";
import { PageFrame } from "../../app/PageFrame";
import { useKanban } from "../../contexts/KanbanContext";
import { useWorkspace } from "../../contexts/WorkspaceContext";
import { isTestAgent } from "../../lib/agentHelpers";
import type { Agent } from "../../types";
import { HomeCampaignsCard, HomeRoutinesCard, HomeUsageCard } from "./HomeActivityCards";
import { HomeAgentsCard, HomeSystemCard, HomeTodoCard } from "./HomeSummaryCards";
import { buildTodoItems, classifyAgents, type SourceState } from "./homeModel";
import { loadHomeCampaigns, usePolled } from "./useHomeData";

/** v3 home in a fixed order: to-dos, system, agents, campaigns, today's routines, usage. */
export default function HomePageView({ isKo, isMobileViewport, agents, onOpenAgent }: {
  isKo: boolean;
  isMobileViewport: boolean;
  agents: Agent[];
  onOpenAgent: (agent: Agent) => void;
}) {
  const tr = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);
  const { sessions, sessionsFailed } = useWorkspace();
  // Same open-card list the campaign queue uses, so counts and rows agree.
  const { kanbanCards, kanbanCardsState } = useKanban();
  const campaigns = usePolled(loadHomeCampaigns, 60_000);
  // E2E fixture agents stay out of the roster unless they are actually busy.
  const entries = useMemo(
    () => classifyAgents(agents.filter((agent) => !isTestAgent(agent) || agent.status === "working"), sessions, sessionsFailed),
    [agents, sessions, sessionsFailed],
  );
  const todo = useMemo(
    () => buildTodoItems({ cards: kanbanCards, campaigns: campaigns.data?.summaries ?? null, tr, isKo }),
    [kanbanCards, campaigns.data, tr, isKo],
  );
  // Each to-do source keeps its own read state, so a slow or failed one never reads as nothing to do.
  const cardsSource: SourceState = kanbanCardsState === "error" ? "failed" : kanbanCardsState === "ready" ? "ok" : "loading";
  const campaignsSource: SourceState = campaigns.data ? (campaigns.data.summaries ? "ok" : "failed") : campaigns.failed ? "failed" : "loading";

  return (
    <PageFrame
      testId="home-page"
      title={tr("홈", "Home")}
    >
      <div className="flex min-w-0 flex-col gap-3 sm:gap-4">
        <div className="flex min-w-0 flex-wrap items-stretch gap-3 sm:gap-4">
          <HomeTodoCard
            items={todo}
            cards={cardsSource}
            campaigns={campaignsSource}
            tr={tr}
            className="flex-[2_1_32.5rem]"
          />
          <HomeSystemCard campaigns={campaigns} tr={tr} className="flex-[1_1_18.75rem]" />
        </div>
        <HomeAgentsCard entries={entries} isMobile={isMobileViewport} tr={tr} isKo={isKo} onOpenAgent={onOpenAgent} />
        <div className="flex min-w-0 flex-wrap items-stretch gap-3 sm:gap-4">
          <HomeCampaignsCard campaigns={campaigns} tr={tr} className="flex-[3_1_26.25rem]" />
          <HomeRoutinesCard tr={tr} isKo={isKo} className="flex-[2_1_18.75rem]" />
          <HomeUsageCard tr={tr} isKo={isKo} className="flex-[2_1_16.25rem]" />
        </div>
      </div>
    </PageFrame>
  );
}
