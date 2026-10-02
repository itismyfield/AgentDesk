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
import { buildGenerateGroups, describeGenerateSkips, resetAutoQueueForSelection } from "./auto-queue-actions";
import AutoQueuePanelView from "./AutoQueuePanelView";
import { useSortableReorder } from "./AutoQueueSortableRows";
import { deriveGateKindByPhase, isCompletedEntry, sortEntriesForDisplay, type ViewMode } from "./auto-queue-panel-utils";
import type { ReadyAutoQueueEntry } from "./auto-queue-actions";

interface Props {
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  agents: Agent[];
  selectedRepo: string;
  selectedAgentId?: string | null;
  /**
   * #2128: ready 카드(requested 컬럼) 중 assignee와 GH 이슈 번호가 있는 항목들.
   * "큐 생성" 버튼이 이 목록을 (repo, agentId)로 묶어 묶음마다 큐를 하나씩 만든다.
   */
  readyEntries?: ReadyAutoQueueEntry[];
}

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

  const agentMap = new Map(agents.map((a) => [a.id, a]));
  const suppressedRunIdRef = useRef<string | null>(null);
  // Generate stays locked until a status read that started after generate settled
  // shows a run it made, or any new run when a request's outcome is unknown.
  const generateHoldRef = useRef<{
    runIds: Set<string>;
    uncertain: boolean;
    before: string | null;
    settledAt: number;
  } | null>(null);

  const resetPanelState = useCallback(() => {
    setStatus(createEmptyAutoQueueStatus());
    setError(null);
    setNoReadyCards(false);
    setViewMode("thread");
    setGenerating(false);
    setActivating(false);
  }, []);

  const fetchStatus = useCallback(async () => {
    const hold = generateHoldRef.current;
    const requestedAt = Date.now();
    try {
      const s = await api.getAutoQueueStatus(selectedRepo || null, selectedAgentId, { fresh: hold !== null });
      const normalized = normalizeAutoQueueStatus(s, suppressedRunIdRef.current);
      if (shouldClearSuppressedAutoQueueRun(s, suppressedRunIdRef.current)) {
        suppressedRunIdRef.current = null;
      }
      setStatus(normalized);
      // Only reset noReadyCards when a run with entries exists
      if (!normalized.run || normalized.entries.length > 0) setNoReadyCards(false);
      if (
        hold &&
        generateHoldRef.current === hold &&
        requestedAt >= hold.settledAt &&
        s.run &&
        (hold.runIds.has(s.run.id) || (hold.uncertain && s.run.id !== hold.before))
      ) {
        generateHoldRef.current = null;
        setGenerating(false);
      }
    } catch {
      // silent
    }
  }, [selectedRepo, selectedAgentId]);

  useEffect(() => {
    // A lock taken for another repo or agent cannot be confirmed from this one.
    if (generateHoldRef.current) {
      generateHoldRef.current = null;
      setGenerating(false);
    }
    void fetchStatus();
    const timer = setInterval(() => void fetchStatus(), 30_000);
    return () => clearInterval(timer);
  }, [fetchStatus]);

  const getAgentLabel = (agentId: string) => {
    const agent = agentMap.get(agentId);
    return agent ? localeName(locale, agent) : agentId.slice(0, 8);
  };

  // 준비된 카드를 (repo, agent)별로 묶어 묶음마다 /api/queue/generate로 큐를 만든다.
  const handleGenerate = async () => {
    if (!selectedRepo || generating) return;
    const groups = buildGenerateGroups(readyEntries, selectedRepo);
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

    const before = status?.run?.id ?? null;
    const failures: string[] = [];
    const partial: string[] = [];
    const runIds = new Set<string>();
    let uncertain = false;
    for (const { repo, agentId, issueNumbers } of groups) {
      const label = getAgentLabel(agentId);
      try {
        const result = await api.generateAutoQueue({ repo, agentId, issueNumbers });
        const skipped = describeGenerateSkips(result, tr);
        if (!result.run) failures.push(`${label}: ${result.message ?? "-"}${skipped ? ` (${skipped})` : ""}`);
        else if (skipped) partial.push(`${label}: ${skipped}`);
        // Only runs this panel's status scope can show are waited for.
        if (result.run && repo === selectedRepo && (!selectedAgentId || agentId === selectedAgentId)) {
          runIds.add(result.run.id);
        }
      } catch (e) {
        // Only a 4xx proves nothing was made; a timeout or 5xx may have made a run.
        if (!(e instanceof api.ApiRequestError && e.status < 500)) uncertain = true;
        failures.push(`${label}: ${e instanceof Error ? e.message : String(e)}`);
      }
    }
    const messages: string[] = [];
    if (failures.length > 0) {
      messages.push(tr(`큐를 만들지 못했습니다: ${failures.join(", ")}`, `Queue not created: ${failures.join(", ")}`));
    }
    if (partial.length > 0) {
      messages.push(tr(`큐에 넣지 않은 카드: ${partial.join(", ")}`, `Cards left out: ${partial.join(", ")}`));
    }
    if (uncertain) {
      messages.push(
        tr(
          "응답이 없던 요청이 큐를 만들었을 수 있어, 새 큐가 보일 때까지 생성을 잠급니다. 계속 잠겨 있으면 새로고침해 확인하세요.",
          "A request that got no answer may have created a queue, so Generate stays locked until a new queue shows. Reload to check if it stays locked.",
        ),
      );
    }
    if (messages.length > 0) setError(messages.join(" · "));
    if (runIds.size > 0 || uncertain) {
      generateHoldRef.current = { runIds, uncertain, before, settledAt: Date.now() };
    } else {
      setGenerating(false);
    }
    await fetchStatus();
  };

  const handleReset = async () => {
    setError(null);
    setNoReadyCards(false);
    suppressedRunIdRef.current = status?.run?.id ?? null;
    try {
      const reset = await resetAutoQueueForSelection(
        api,
        selectedRepo || null,
        selectedAgentId ?? status?.run?.agent_id,
        status?.run?.id,
      );
      if (!reset) {
        setError(tr("초기화할 run이 없습니다", "No run to reset"));
        return;
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
      await api.startAutoQueueRun(runId);
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
    action: "pause" | "resume" | "end",
  ) => {
    try {
      if (action === "pause") await api.pauseAutoQueueRun(run.id);
      if (action === "resume") await api.resumeAutoQueueRun(run.id);
      if (action === "end") await api.endAutoQueueRun(run.id);
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
  const gatesByPhase = new Map<number, PhaseGateInfo[]>();
  for (const gate of phaseGates) {
    const list = gatesByPhase.get(gate.phase) ?? [];
    list.push(gate);
    gatesByPhase.set(gate.phase, list);
  }
  const gateKindByPhase = deriveGateKindByPhase(entries);
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
        dispatchedCount,
        doneCount,
        entries,
        entriesByAgent,
        entriesByThreadGroup,
        error,
        expanded,
        failedCount,
        gateKindByPhase,
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
