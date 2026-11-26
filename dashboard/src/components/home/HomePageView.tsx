import { useCallback, useMemo, useRef, useState } from "react";
import { getDispatchedSessions } from "../../api/agentActivity";
import { PageFrame } from "../../app/PageFrame";
import { AlertDialog } from "../../kit/free/dialog-clean";
import type { Notification } from "../NotificationCenter";
import { useKanban } from "../../contexts/KanbanContext";
import { useWorkspace } from "../../contexts/WorkspaceContext";
import type { Agent } from "../../types";
import { HomeCampaignsCard, HomeRoutinesCard, HomeUsageCard } from "./HomeActivityCards";
import { HomeAgentsCard, HomeSystemCard, HomeTodoCard } from "./HomeSummaryCards";
import {
  agentName,
  autoPauseRule,
  buildHomeAgents,
  buildTodoItems,
  buildTriageAlerts,
  buildUsageRows,
  type SourceState,
} from "./homeModel";
import {
  campaignTodoSource,
  cleanupStaleIdleSession,
  loadHomeCampaigns,
  loadHomeHealth,
  loadHomeMemento,
  loadHomeUsage,
  loadRuntimeConfig,
  reconcileStaleSessionTurn,
  stopAgentTurn,
  usePolled,
} from "./useHomeData";

/** v3 home: 3-second unified triage + collapsed empty banner, agents & unlinked lanes, campaigns, routines, usage. */
export default function HomePageView({
  isKo,
  isMobileViewport,
  agents,
  onOpenAgent,
  onNotify,
}: {
  isKo: boolean;
  isMobileViewport: boolean;
  agents: Agent[];
  onOpenAgent: (agent: Agent) => void;
  onNotify?: (message: string, type?: Notification["type"]) => void;
}) {
  const tr = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);
  const { sessions, setSessions, sessionsFailed, refreshAgents } = useWorkspace();
  const { kanbanCards, kanbanCardsState } = useKanban();
  const campaigns = usePolled(loadHomeCampaigns, 60_000);
  const health = usePolled(loadHomeHealth, 30_000);
  const usage = usePolled(loadHomeUsage, 60_000);
  const runtime = usePolled(loadRuntimeConfig, 300_000);

  const agentsRef = useRef(agents);
  agentsRef.current = agents;
  const mementoKey = useMemo(
    () =>
      agents
        .map((a) => `${a.id}:${a.discord_channel_cc ?? ""}:${a.discord_channel_cdx ?? ""}:${a.discord_channel_alt ?? ""}:${a.discord_channel_id ?? ""}`)
        .join("|"),
    [agents],
  );
  const readMemento = useCallback(() => loadHomeMemento(agentsRef.current), [mementoKey]);
  const memento = usePolled(readMemento, 60_000);

  const [reconciledKeys, setReconciledKeys] = useState<Set<string>>(() => new Set());
  const [busyKeys, setBusyKeys] = useState<Set<string>>(() => new Set());
  const [stopTarget, setStopTarget] = useState<Agent | null>(null);

  const entries = useMemo(
    () => buildHomeAgents(agents, sessions, sessionsFailed),
    [agents, sessions, sessionsFailed],
  );
  const todo = useMemo(
    () =>
      buildTodoItems({
        cards: kanbanCards,
        campaigns: campaigns.data?.summaries ?? null,
        documents: campaigns.data?.documents,
        legacy: campaigns.data?.legacy,
        tr,
        isKo,
      }),
    [kanbanCards, campaigns.data, tr, isKo],
  );
  const usageRows = useMemo(() => buildUsageRows(usage.data ?? [], isKo), [usage.data, isKo]);
  const rule = useMemo(() => autoPauseRule(runtime.data), [runtime.data]);
  const triageAlerts = useMemo(
    () =>
      buildTriageAlerts({
        usageRows,
        rule,
        sessions,
        agentEntries: entries,
        reconciledKeys,
        health: health.failed ? null : health.data,
        tr,
        isKo,
      }),
    [usageRows, rule, sessions, entries, reconciledKeys, health.failed, health.data, tr, isKo],
  );
  const hasHealthTriage = useMemo(() => triageAlerts.some((alert) => alert.kind === "health"), [triageAlerts]);

  const handleReconcileSession = useCallback(async (sessionKey: string) => {
    setBusyKeys((prev) => new Set(prev).add(sessionKey));
    try {
      const target = sessions.find((session) => session.session_key === sessionKey);
      if (target?.status === "idle") {
        const result = await cleanupStaleIdleSession(sessionKey);
        if (result.disconnected) {
          setSessions((prev) =>
            prev.map((session) =>
              session.session_key === sessionKey ? { ...session, status: "disconnected" } : session,
            ),
          );
          onNotify?.(tr("오래된 세션을 정리했어요.", "Cleaned up the stale session."), "success");
        } else {
          onNotify?.(
            result.reason
              ? tr(`세션 상태 확인 완료 (${result.reason})`, `Session checked (${result.reason})`)
              : tr("아직 활성 상태라 세션을 유지했어요.", "Session is still active; kept as-is."),
            "info",
          );
        }
      } else {
        const result = await reconcileStaleSessionTurn(sessionKey);
        if (result.reconciled) {
          setReconciledKeys((prev) => new Set(prev).add(sessionKey));
          setSessions((prev) =>
            prev.map((session) =>
              session.session_key === sessionKey
                ? { ...session, status: "idle", last_seen_at: Date.now() }
                : session,
            ),
          );
          onNotify?.(tr("멈춘 세션을 정리했어요.", "Reconciled the stale session."), "success");
        } else {
          onNotify?.(
            result.reason
              ? tr(`세션 상태 확인 완료 (${result.reason})`, `Session checked (${result.reason})`)
              : tr("멈춘 세션 상태를 확인했어요.", "Checked session state."),
            "info",
          );
        }
      }
      void getDispatchedSessions(true, { suppressErrorToast: true, maxRetries: 0 }).then(setSessions).catch(() => {});
    } catch (error) {
      onNotify?.(
        error instanceof Error ? error.message : tr("세션 정리에 실패했어요.", "Failed to reconcile session."),
        "error",
      );
    } finally {
      setBusyKeys((prev) => {
        const next = new Set(prev);
        next.delete(sessionKey);
        return next;
      });
    }
  }, [onNotify, sessions, setSessions, tr]);

  const confirmStopAgentTurn = useCallback(async (agent: Agent) => {
    const name = agentName(agent, isKo);
    try {
      const response = await stopAgentTurn(agent.id);
      if (response.session_key) {
        setSessions((prev) =>
          prev.map((session) =>
            session.session_key === response.session_key ? { ...session, status: "disconnected" } : session,
          ),
        );
      }
      refreshAgents();
      onNotify?.(tr(`${name} 턴을 중단했어요.`, `Stopped ${name}'s turn.`), "success");
    } catch (error) {
      onNotify?.(
        error instanceof Error ? error.message : tr(`${name} 턴 중단에 실패했어요.`, `Failed to stop ${name}'s turn.`),
        "error",
      );
    }
  }, [isKo, onNotify, refreshAgents, setSessions, tr]);

  const cardsSource: SourceState = kanbanCardsState === "error" ? "failed" : kanbanCardsState === "ready" ? "ok" : "loading";
  const campaignsSource = campaignTodoSource(campaigns);
  const details = campaigns.data && !campaigns.failed ? { failed: campaigns.data.detailFailed, cut: campaigns.data.detailsCut } : undefined;
  const todoCollapsed = todo.length === 0 && triageAlerts.length === 0 && cardsSource === "ok" && campaignsSource === "ok";
  const stopName = stopTarget ? agentName(stopTarget, isKo) : "";

  return (
    <PageFrame
      testId="home-page"
      title={tr("홈", "Home")}
      description={tr(
        "사람이 할 일이 맨 위예요. 그 아래로 에이전트·캠페인·루틴이 이어져요.",
        "What needs a person comes first, followed by agents, campaigns, and routines.",
      )}
    >
      <div className="flex min-w-0 flex-col gap-3 sm:gap-4">
        {todoCollapsed ? (
          <>
            <HomeTodoCard
              items={todo}
              alerts={triageAlerts}
              agents={agents}
              cards={cardsSource}
              campaigns={campaignsSource}
              details={details}
              isMobile={isMobileViewport}
              tr={tr}
              onReconcileSession={handleReconcileSession}
              busySessionKeys={busyKeys}
            />
            <HomeSystemCard campaigns={campaigns} healthState={health} mementoState={memento} agents={agents} onlyWhenOff={isMobileViewport} healthTriageShown={hasHealthTriage} tr={tr} />
          </>
        ) : (
          <div className="flex min-w-0 flex-wrap items-stretch gap-3 sm:gap-4">
            <HomeTodoCard
              items={todo}
              alerts={triageAlerts}
              agents={agents}
              cards={cardsSource}
              campaigns={campaignsSource}
              details={details}
              isMobile={isMobileViewport}
              tr={tr}
              onReconcileSession={handleReconcileSession}
              busySessionKeys={busyKeys}
              className="flex-[2_1_32.5rem]"
            />
            <HomeSystemCard campaigns={campaigns} healthState={health} mementoState={memento} agents={agents} onlyWhenOff={isMobileViewport} healthTriageShown={hasHealthTriage} tr={tr} className="flex-[1_1_18.75rem]" />
          </div>
        )}
        <HomeAgentsCard
          entries={entries}
          isMobile={isMobileViewport}
          tr={tr}
          isKo={isKo}
          onOpenAgent={onOpenAgent}
          onStopAgentTurn={setStopTarget}
        />
        <div className="flex min-w-0 flex-wrap items-stretch gap-3 sm:gap-4">
          <HomeCampaignsCard campaigns={campaigns} tr={tr} className="flex-[3_1_26.25rem]" />
          <HomeRoutinesCard tr={tr} isKo={isKo} className="flex-[2_1_18.75rem]" />
          <HomeUsageCard tr={tr} isKo={isKo} usageState={usage} runtimeState={runtime} className="flex-[2_1_16.25rem]" />
        </div>
      </div>

      <AlertDialog
        open={Boolean(stopTarget)}
        onOpenChange={(open) => { if (!open) setStopTarget(null); }}
        danger
        title={tr(`${stopName}의 실행 중인 턴을 중단할까요?`, `Stop ${stopName}'s running turn?`)}
        description={tr("진행 중인 응답만 멈추고 대기열에 남은 작업은 유지돼요.", "Only the running turn stops; queued items stay.")}
        cancelLabel={tr("취소", "Cancel")}
        confirmLabel={tr("턴 중단", "Stop turn")}
        onConfirm={() => (stopTarget ? confirmStopAgentTurn(stopTarget) : undefined)}
      />
    </PageFrame>
  );
}
