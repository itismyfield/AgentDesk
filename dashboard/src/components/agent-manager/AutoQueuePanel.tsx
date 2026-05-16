import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import * as api from "../../api";
import type {
  AutoQueueStatus,
  DispatchQueueEntry as DispatchQueueEntryType,
  AutoQueueRun,
  PhaseGateInfo,
} from "../../api";

import type { Agent, UiLanguage } from "../../types";
import { localeName } from "../../i18n";
import { useLocalStorage } from "../../lib/useLocalStorage";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import {
  createEmptyAutoQueueStatus,
  getAutoQueuePrimaryAction,
  normalizeAutoQueueStatus,
  shouldClearSuppressedAutoQueueRun,
} from "./auto-queue-panel-state";
import { buildRequestGenerateGroups } from "./auto-queue-actions";
import AutoQueuePanelView from "./AutoQueuePanelView";
import { useSortableReorder } from "./AutoQueueSortableRows";
import { formatRequestGroupKey, isCompletedEntry, requestGroupKey, sortEntriesForDisplay, type ViewMode } from "./auto-queue-panel-utils";
import type { ReadyAutoQueueEntry } from "./auto-queue-actions";

interface Props {
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  agents: Agent[];
  selectedRepo: string;
  selectedAgentId?: string | null;
  /**
   * #2128: ready 카드(requested 컬럼) 중 assignee와 GH 이슈 번호가 있는 항목들.
   * "큐 생성" 버튼이 이 목록을 (agentId)로 group by 해서 agent별 별도 요청을 보냄.
   */
  readyEntries?: ReadyAutoQueueEntry[];
}

interface RequestProgress {
  startedAt: number;
  baselineEntryIds: Set<string>;
  pendingGroups: Set<string>;
  satisfiedGroups: Set<string>;
  errors: { groupKey: string; message: string }[];
}

const REQUEST_GENERATE_TIMEOUT_MS = 5 * 60 * 1000;
const REQUEST_GENERATE_POLL_MS = 30 * 1000;
export default function AutoQueuePanel({
  tr,
  locale,
  agents,
  selectedRepo,
  selectedAgentId,
  readyEntries = [],
}: Props) {
  const [status, setStatus] = useState<AutoQueueStatus | null>(null);
  const [expanded, setExpanded] = useLocalStorage<boolean>(STORAGE_KEYS.kanbanAutoQueueOpen, true);
  const [generating, setGenerating] = useState(false);
  const [activating, setActivating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [noReadyCards, setNoReadyCards] = useState(false);
  const [viewMode, setViewMode] = useState<ViewMode>("thread");
  const [requestProgress, setRequestProgress] = useState<RequestProgress | null>(null);
  const requestTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const agentMap = new Map(agents.map((a) => [a.id, a]));
  const suppressedRunIdRef = useRef<string | null>(null);

  const resetPanelState = useCallback(() => {
    setStatus(createEmptyAutoQueueStatus());
    setError(null);
    setNoReadyCards(false);
    setViewMode("thread");
    setGenerating(false);
    setActivating(false);
  }, []);

  const fetchStatus = useCallback(async () => {
    try {
      const s = await api.getAutoQueueStatus(selectedRepo || null, selectedAgentId);
      const normalized = normalizeAutoQueueStatus(s, suppressedRunIdRef.current);
      if (shouldClearSuppressedAutoQueueRun(s, suppressedRunIdRef.current)) {
        suppressedRunIdRef.current = null;
      }
      setStatus(normalized);
      // Only reset noReadyCards when a run with entries exists
      if (!normalized.run || normalized.entries.length > 0) setNoReadyCards(false);
    } catch {
      // silent
    }
  }, [selectedRepo, selectedAgentId]);

  useEffect(() => {
    void fetchStatus();
    const timer = setInterval(() => void fetchStatus(), 30_000);
    return () => clearInterval(timer);
  }, [fetchStatus]);

  const getAgentLabel = (agentId: string) => {
    const agent = agentMap.get(agentId);
    return agent ? localeName(locale, agent) : agentId.slice(0, 8);
  };

  // #2128: 결정론 smart-planner 대신 ready 카드를 (repo × agent)로 그룹핑해서 각
  // agent에게 /api/queue/request-generate로 위임. agent가 자체 판단으로 /generate
  // 호출하면 dashboard는 5분 polling으로 새 entries 감지.
  const stopRequestPolling = () => {
    if (requestTimerRef.current) {
      clearInterval(requestTimerRef.current);
      requestTimerRef.current = null;
    }
  };

  useEffect(() => () => stopRequestPolling(), []);

  const handleGenerate = async () => {
    if (!selectedRepo) return;
    if (generating || requestProgress) return;
    const groups = buildRequestGenerateGroups(readyEntries, selectedRepo);
    if (groups.length === 0) {
      setError(
        tr(
          "준비됨 카드가 없습니다 (assignee + GitHub 이슈 필요).",
          "No ready cards (need assignee + GitHub issue).",
        ),
      );
      setNoReadyCards(true);
      return;
    }

    setGenerating(true);
    setError(null);
    setNoReadyCards(false);
    suppressedRunIdRef.current = null;

    const baselineEntryIds = new Set(
      (status?.entries ?? []).map((entry) => entry.id),
    );

    const pendingGroups = new Set<string>();
    const errors: { groupKey: string; message: string }[] = [];
    await Promise.all(
      groups.map(async ({ repo, agentId, issueNumbers }) => {
        const groupKey = requestGroupKey(repo, agentId);
        try {
          await api.requestGenerateAutoQueue({
            repo,
            agentId,
            issueNumbers,
          });
          pendingGroups.add(groupKey);
        } catch (e) {
          errors.push({
            groupKey,
            message: e instanceof Error ? e.message : String(e),
          });
        }
      }),
    );

    setGenerating(false);

    if (pendingGroups.size === 0) {
      setError(
        tr(
          `큐 생성 요청이 모두 거부됐습니다 (${errors.length}건).`,
          `All queue requests rejected (${errors.length}).`,
        ),
      );
      return;
    }
    if (errors.length > 0) {
      const failed = errors
        .map((error) => `${formatRequestGroupKey(error.groupKey)}: ${error.message}`)
        .join(", ");
      setError(
        tr(
          `일부 큐 생성 요청이 실패했습니다: ${failed}`,
          `Some queue requests failed: ${failed}`,
        ),
      );
    }

    setRequestProgress({
      startedAt: Date.now(),
      baselineEntryIds,
      pendingGroups,
      satisfiedGroups: new Set<string>(),
      errors,
    });
  };

  // request-generate polling: 30초마다 status 새로고침. 새 entry로 잡힌 agent는
  // satisfied로 이동. 모두 만족하거나 5분 경과 시 종료.
  useEffect(() => {
    if (!requestProgress) {
      stopRequestPolling();
      return;
    }
    void fetchStatus();
    requestTimerRef.current = setInterval(() => void fetchStatus(), REQUEST_GENERATE_POLL_MS);
    return () => stopRequestPolling();
  }, [requestProgress?.startedAt]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (!requestProgress) return;
    const newlyAppearedGroups = new Set<string>();
    for (const entry of status?.entries ?? []) {
      if (requestProgress.baselineEntryIds.has(entry.id)) continue;
      if (!entry.agent_id) continue;
      const repo = entry.github_repo || selectedRepo || "";
      newlyAppearedGroups.add(requestGroupKey(repo, entry.agent_id));
    }

    let changed = false;
    const nextPending = new Set(requestProgress.pendingGroups);
    const nextSatisfied = new Set(requestProgress.satisfiedGroups);
    for (const groupKey of requestProgress.pendingGroups) {
      if (newlyAppearedGroups.has(groupKey)) {
        nextPending.delete(groupKey);
        nextSatisfied.add(groupKey);
        changed = true;
      }
    }

    const elapsed = Date.now() - requestProgress.startedAt;
    const timedOut = elapsed >= REQUEST_GENERATE_TIMEOUT_MS;

    if (nextPending.size === 0 || timedOut) {
      stopRequestPolling();
      if (timedOut && nextPending.size > 0) {
        const missing = [...nextPending].map(formatRequestGroupKey).join(", ");
        setError(
          tr(
            `5분 안에 응답하지 않은 에이전트: ${missing}`,
            `Agents did not respond within 5 min: ${missing}`,
          ),
        );
      }
      setRequestProgress(null);
      return;
    }

    if (changed) {
      setRequestProgress({ ...requestProgress, pendingGroups: nextPending, satisfiedGroups: nextSatisfied });
    }
  }, [selectedRepo, status, requestProgress, tr]);

  const handleReset = async () => {
    setError(null);
    setNoReadyCards(false);
    suppressedRunIdRef.current = status?.run?.id ?? null;
    try {
      const targets = resolveResetAgentTargets();
      if (targets.length === 0) {
        throw new Error(
          tr(
            "초기화할 에이전트를 찾지 못했습니다. 상단 필터에서 에이전트를 선택하세요.",
            "No agent to reset. Select an agent from the filter above.",
          ),
        );
      }
      for (const agentId of targets) {
        await api.resetAutoQueue({
          runId: status?.run?.id ?? null,
          repo: selectedRepo || null,
          agentId,
        });
      }
      resetPanelState();
    } catch (e) {
      suppressedRunIdRef.current = null;
      setError(e instanceof Error ? e.message : tr("초기화 실패", "Reset failed"));
    }
  };

  const handleActivate = async () => {
    setActivating(true);
    setError(null);
    try {
      await api.activateAutoQueue(selectedRepo || null, selectedAgentId);
      await fetchStatus();
    } catch (e) {
      setError(
        e instanceof Error ? e.message : tr("활성화 실패", "Activation failed"),
      );
    } finally {
      setActivating(false);
    }
  };

  /** Pending run → activate immediately with default order, then dispatch first entry */
  const handleFallbackActivate = async (runId: string) => {
    setActivating(true);
    setError(null);
    try {
      await api.updateAutoQueueRun(runId, "active");
      await api.activateAutoQueue(selectedRepo || null, selectedAgentId);
      await fetchStatus();
    } catch (e) {
      setError(
        e instanceof Error
          ? e.message
          : tr("기본 순서 시작 실패", "Default order start failed"),
      );
    } finally {
      setActivating(false);
    }
  };

  const handleEntryStatusUpdate = async (
    entryId: string,
    status: "pending" | "skipped",
  ) => {
    try {
      await api.updateAutoQueueEntry(entryId, { status });
      await fetchStatus();
    } catch (e) {
      setError(
        e instanceof Error
          ? e.message
          : status === "pending"
            ? tr("재시도 실패", "Retry failed")
            : tr("상태 변경 실패", "Status change failed"),
      );
    }
  };

  const handleRunAction = async (
    run: AutoQueueRun,
    action: "paused" | "active" | "completed",
  ) => {
    try {
      await api.updateAutoQueueRun(run.id, action);
      await fetchStatus();
    } catch (e) {
      setError(
        e instanceof Error
          ? e.message
          : tr("상태 변경 실패", "Status change failed"),
      );
    }
  };

  const handleReorder = async (
    orderedIds: string[],
    agentId?: string | null,
  ) => {
    try {
      await api.reorderAutoQueueEntries(orderedIds, agentId);
      await fetchStatus();
    } catch (e) {
      setError(
        e instanceof Error ? e.message : tr("순서 변경 실패", "Reorder failed"),
      );
    }
  };

  const run = status?.run ?? null;
  const entries = status?.entries ?? [];
  const phaseGates = status?.phase_gates ?? [];
  const deployPhases = new Set(run?.deploy_phases ?? []);
  const resetAgentId = selectedAgentId ?? run?.agent_id ?? null;
  const resolveResetAgentTargets = (): string[] => {
    if (resetAgentId) return [resetAgentId];
    const fromEntries = Array.from(
      new Set(entries.map((e) => e.agent_id).filter((id): id is string => Boolean(id))),
    );
    if (fromEntries.length > 0) return fromEntries;
    const fromStats = Object.keys(status?.agents ?? {});
    return fromStats;
  };
  const gatesByPhase = new Map<number, PhaseGateInfo[]>();
  for (const gate of phaseGates) {
    const list = gatesByPhase.get(gate.phase) ?? [];
    list.push(gate);
    gatesByPhase.set(gate.phase, list);
  }
  const agentStats: Record<
    string,
    { pending: number; dispatched: number; done: number; skipped: number; failed: number }
  > = status?.agents ?? {};

  const pendingCount = entries.filter((e) => e.status === "pending").length;
  const dispatchedCount = entries.filter(
    (e) => e.status === "dispatched",
  ).length;
  const doneCount = entries.filter((e) => e.status === "done").length;
  const failedCount = entries.filter((e) => e.status === "failed").length;
  const skippedCount = entries.filter((e) => e.status === "skipped").length;
  const completedCount = entries.filter(isCompletedEntry).length;
  const totalCount = entries.length;
  const primaryAction = getAutoQueuePrimaryAction(run, pendingCount);
  const showRunStartControls = !!run && (run.status === "generated" || run.status === "active") && pendingCount > 0;
  const startActionLabel = run?.status === "generated" ? tr("시작", "Start") : tr("디스패치", "Dispatch");

  // Group entries by agent
  const entriesByAgent = new Map<string, DispatchQueueEntryType[]>();
  for (const entry of entries) {
    const list = entriesByAgent.get(entry.agent_id) ?? [];
    list.push(entry);
    entriesByAgent.set(entry.agent_id, list);
  }

  // Thread group info
  const threadGroups = status?.thread_groups ?? {};
  const threadGroupCount = run?.thread_group_count ?? 0;
  const hasThreadGroups =
    threadGroupCount > 1 || Object.keys(threadGroups).length > 1;
  const maxConcurrent = run?.max_concurrent_threads ?? 1;
  const hasBatchPhases = entries.some((entry) => (entry.batch_phase ?? 0) > 0);
  // Earliest phase that still has work to do (pending or in-flight).
  // Previously this excluded phase 0 ("if (phase <= 0)"), so a queue with
  // P0 entries still pending and P1 entries also pending was reported as
  // "currently P1". Phase 0 is a real phase — include it.
  const currentBatchPhase = entries.reduce<number | null>((minPhase, entry) => {
    const phase = entry.batch_phase ?? 0;
    if (entry.status !== "pending" && entry.status !== "dispatched") return minPhase;
    return minPhase == null ? phase : Math.min(minPhase, phase);
  }, null);

  // Group entries by thread_group
  const entriesByThreadGroup = new Map<number, DispatchQueueEntryType[]>();
  for (const entry of entries) {
    const g = entry.thread_group ?? 0;
    const list = entriesByThreadGroup.get(g) ?? [];
    list.push(entry);
    entriesByThreadGroup.set(g, list);
  }

  const entriesByBatchPhase = new Map<number, DispatchQueueEntryType[]>();
  for (const entry of entries) {
    const phase = entry.batch_phase ?? 0;
    const list = entriesByBatchPhase.get(phase) ?? [];
    list.push(entry);
    entriesByBatchPhase.set(phase, list);
  }
  const phaseSections = Array.from(entriesByBatchPhase.entries()).sort(
    ([left], [right]) => left - right,
  );

  // All-queue view: merge all entries sorted by status then rank
  const allEntriesSorted = sortEntriesForDisplay(entries);

  // Drag & drop for "all" view (pending only, no agent scope)
  const allDrag = useSortableReorder(allEntriesSorted, handleReorder);

  return (
    <AutoQueuePanelView
      ctx={{
        activating,
        agentStats,
        allDrag,
        allEntriesSorted,
        completedCount,
        currentBatchPhase,
        deployPhases,
        dispatchedCount,
        doneCount,
        entries,
        entriesByAgent,
        entriesByThreadGroup,
        error,
        expanded,
        failedCount,
        gatesByPhase,
        generating,
        getAgentLabel,
        handleActivate,
        handleEntryStatusUpdate,
        handleFallbackActivate,
        handleGenerate,
        handleReorder,
        handleReset,
        handleRunAction,
        hasBatchPhases,
        hasThreadGroups,
        locale,
        maxConcurrent,
        pendingCount,
        phaseSections,
        primaryAction,
        readyEntries,
        requestProgress,
        run,
        selectedRepo,
        setExpanded,
        setViewMode,
        showRunStartControls,
        skippedCount,
        startActionLabel,
        threadGroups,
        totalCount,
        tr,
        viewMode,
      }}
    />
  );
}
