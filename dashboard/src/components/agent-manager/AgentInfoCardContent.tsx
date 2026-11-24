import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { getAgentWarnings, getAgentWorkSummary } from "../../agent-insights";
import type { Agent, Department } from "../../types";
import { localeName } from "../../i18n";
import { getProviderMeta } from "../../app/providerTheme";
import AgentPersona from "../AgentPersona";
import * as api from "../../api";
import { catalogLabel, useProviderCatalog } from "../../api/providers";
import { useKanban } from "../../contexts/KanbanContext";
import { Button } from "../../kit/free/button-clean";
import { TextField } from "../../kit/free/text-field-clean";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import { buildTodoItems, monitorView, type HomeAgentState, type HomeTurn } from "../home/homeModel";
import { useAgentTurns } from "../home/useHomeData";
import type { Translator } from "./types";
import { buildAttention, type AttentionReason } from "./agentsAttention";
import { AgentNowSection, AgentRunSettingsSection } from "./AgentInfoProfileSections";
import { AgentConnectionSection, AgentSourceSection } from "./AgentInfoRoutingSections";
import { AgentActivitySection, AgentCronSection, AgentSkillsSection } from "./AgentInfoOperationsSections";
import { AgentInfoTurnsSection } from "./AgentInfoTurnsSection";
import { AgentExecutionNodeSelect } from "./AgentExecutionNodeSelect";
import type { DetailFold } from "./AgentInfoCardSections";
import { agentSkillCalls, inferBindingSource, isTurnSession, stopAgentTurn, turnMonitorSteps } from "./AgentInfoCardModel";
import { isDiscordSnowflake } from "./discord-routing";
import { agentSecondaryLine } from "./AgentRow";
import { useAgentDetailData, useAgentDetailShared, useDiscordChannels, useReadWhenOpen, type AgentDetailShared } from "./useAgentDetailData";

type FoldKey = "run" | "connections" | "turns" | "skills" | "activity" | "cron" | "source";
// These fold on every screen, so their reads wait until someone opens them; phones fold everything but "now".
const ALWAYS_FOLDED = new Set<FoldKey>(["turns", "activity", "cron"]);

const readCron = (agentId: string) => api.getAgentCron(agentId);
const readTimeline = (agentId: string) => api.getAgentTimeline(agentId, 30);
const readAudit = (agentId: string) => api.getAuditLogs(20, { agentId });

interface AgentInfoCardContentProps {
  agent: Agent;
  /** Home's state for the agent, when the caller knows the sessions. */
  state?: HomeAgentState;
  isKo: boolean;
  locale: string;
  tr: Translator;
  departments: Department[];
  onAgentUpdated?: () => void;
  /** Buttons on the right of the title row (edit, menu, close). */
  actions?: ReactNode;
  titleId?: string;
  /** Reads shared by every detail on the page; without them (home, ⌘K) the detail reads its own. */
  shared?: AgentDetailShared;
  /** Live turns the caller already polls; without them a desktop detail polls this agent while it works. */
  turns?: Map<string, HomeTurn>;
  /** What waits on the agent; without it the detail takes the agent's cards from the shared card list. */
  reasons?: AttentionReason[];
}

/** Agent detail body, shared by the agents page, home and ⌘K: now, run settings, connections, recent turns, skills,
 * activity, cron and source of truth, in that order. */
export function AgentInfoCardContent({
  agent,
  state,
  isKo,
  locale,
  tr,
  departments,
  onAgentUpdated,
  actions,
  titleId,
  shared: pageShared,
  turns,
  reasons: pageReasons,
}: AgentInfoCardContentProps) {
  const isMobile = useIsMobile();
  const ownShared = useAgentDetailShared(!pageShared);
  const shared = pageShared ?? ownShared;
  const { sessions, skills, retrySessions, retrySkills } = useAgentDetailData(agent.id);
  const root = useRef<HTMLDivElement>(null);
  const [opened, setOpened] = useState<Set<FoldKey>>(() => new Set());
  const fold = (key: FoldKey): DetailFold => {
    const foldable = isMobile || ALWAYS_FOLDED.has(key);
    return {
      foldable,
      open: !foldable || opened.has(key),
      onToggle: () => setOpened((previous) => {
        const next = new Set(previous);
        if (!next.delete(key)) next.add(key);
        return next;
      }),
    };
  };
  const [editingAlias, setEditingAlias] = useState(false);
  const [aliasValue, setAliasValue] = useState(agent.alias ?? "");
  const [savingAlias, setSavingAlias] = useState(false);
  const [selectedDeptId, setSelectedDeptId] = useState(agent.department_id ?? "");
  const [savingDept, setSavingDept] = useState(false);
  const [selectedProvider, setSelectedProvider] = useState<string>(agent.cli_provider ?? "claude");
  const providerCatalog = useProviderCatalog(selectedProvider);
  const [savingProvider, setSavingProvider] = useState(false);
  const [selectedAuthProfile, setSelectedAuthProfile] = useState("__primary__");
  const [savingAuthProfile, setSavingAuthProfile] = useState(false);
  const profiles = shared.authProfiles;

  useEffect(() => {
    setAliasValue(agent.alias ?? "");
    setEditingAlias(false);
    setSelectedDeptId(agent.department_id ?? "");
    setSelectedProvider(agent.cli_provider ?? "claude");
  }, [agent.alias, agent.department_id, agent.cli_provider, agent.id]);

  // An override belongs to one Provider, so another Provider (switched here or in the edit form) shows the primary account.
  useEffect(() => {
    if (!profiles) return;
    const override = profiles.agent_profile_overrides?.find((entry) => entry.agent_id === agent.id && entry.provider === selectedProvider);
    setSelectedAuthProfile(override?.profile_id ?? "__primary__");
  }, [profiles, agent.id, selectedProvider]);

  const saveAlias = async () => {
    const newAlias = aliasValue.trim() || null;
    if (newAlias === (agent.alias ?? null)) {
      setEditingAlias(false);
      return;
    }
    setSavingAlias(true);
    try {
      await api.updateAgent(agent.id, { alias: newAlias });
      setEditingAlias(false);
      onAgentUpdated?.();
    } catch (e) {
      console.error("Alias save failed:", e);
    } finally {
      setSavingAlias(false);
    }
  };

  const saveDepartment = async (nextDeptId: string) => {
    const previousDeptId = selectedDeptId;
    if ((nextDeptId || "") === previousDeptId) return;
    setSelectedDeptId(nextDeptId);
    setSavingDept(true);
    try {
      await api.updateAgent(agent.id, { department_id: nextDeptId || null });
      onAgentUpdated?.();
    } catch (e) {
      setSelectedDeptId(previousDeptId);
      console.error("Department save failed:", e);
    } finally {
      setSavingDept(false);
    }
  };

  const saveProvider = async (nextProvider: string) => {
    if (nextProvider === selectedProvider) return;
    const previousProvider = selectedProvider;
    setSelectedProvider(nextProvider);
    setSavingProvider(true);
    try {
      await api.updateAgent(agent.id, { cli_provider: nextProvider as Agent["cli_provider"], auth_profile: null });
      setSelectedAuthProfile("__primary__");
      shared.rememberProfile(agent.id, nextProvider, null);
      onAgentUpdated?.();
    } catch (e) {
      setSelectedProvider(previousProvider);
      console.error("Provider save failed:", e);
    } finally {
      setSavingProvider(false);
    }
  };

  const saveAuthProfile = async (nextProfile: string) => {
    if (nextProfile === selectedAuthProfile) return;
    const previousProfile = selectedAuthProfile;
    const profileId = nextProfile === "__primary__" ? null : nextProfile;
    setSelectedAuthProfile(nextProfile);
    setSavingAuthProfile(true);
    try {
      await api.updateAgent(agent.id, { auth_profile: profileId });
      shared.rememberProfile(agent.id, selectedProvider, profileId);
      onAgentUpdated?.();
    } catch (error) {
      setSelectedAuthProfile(previousProfile);
      console.error("Auth profile save failed:", error);
    } finally {
      setSavingAuthProfile(false);
    }
  };

  const authProfileOptions = [
    "__primary__",
    ...(profiles?.providers?.find((provider) => provider.id === selectedProvider)?.accounts ?? [{ id: "default" }]).map((account) => account.id),
  ];

  const bindings = useMemo(() => (shared.bindings ?? []).filter((binding) => binding.agentId === agent.id), [shared.bindings, agent.id]);
  const live = sessions?.live ?? [];
  const workingSessions = live.filter(isTurnSession);
  const connections = fold("connections");
  // Routes and live session threads only, once both are read, and only while the section shows.
  const channelInfo = useDiscordChannels(
    connections.open && shared.bindings !== undefined && sessions !== undefined
      ? [...bindings.flatMap((binding) => [binding.channelId, binding.counterModelChannelId]), ...live.map((session) => session.thread_channel_id)]
      : null,
  );
  const resolveChannel = (channelId: string | null | undefined) => (isDiscordSnowflake(channelId) ? channelInfo[channelId] ?? null : null);
  const resolveParent = (info: api.DiscordChannelInfo | null | undefined) => (isDiscordSnowflake(info?.parent_id) ? channelInfo[info.parent_id] ?? null : null);

  const cronFold = fold("cron");
  const activityFold = fold("activity");
  const cronRead = useReadWhenOpen(cronFold.open, agent.id, readCron);
  const timelineRead = useReadWhenOpen(activityFold.open, agent.id, readTimeline);
  const auditRead = useReadWhenOpen(activityFold.open, agent.id, readAudit);
  const calls = useMemo(() => (shared.skillCalls ? agentSkillCalls(shared.skillCalls, agent.id) : shared.skillCalls), [shared.skillCalls, agent.id]);

  const { kanbanCards } = useKanban();
  const ownReasons = useMemo(
    () => (pageReasons ? [] : buildAttention({ agents: [agent], todo: buildTodoItems({ cards: kanbanCards, campaigns: null, tr, isKo }), failed: [], cardsRead: true, tr, isKo })[0]?.reasons ?? []),
    [pageReasons, agent, kanbanCards, tr, isKo],
  );

  const [stoppingTurn, setStoppingTurn] = useState(false);
  const [stopFeedback, setStopFeedback] = useState<{ ok: boolean; text: string } | null>(null);
  useEffect(() => {
    setStopFeedback(null);
  }, [agent.id, state, agent.status]);

  // Phones read no turns (as home); a caller that already polls the list's working agents hands its map over.
  const working = state ? state === "working" : agent.status === "working";
  const ownTurns = useAgentTurns(turns || isMobile || !working ? [] : [agent.id]);
  const liveTurn = (turns ?? ownTurns).get(agent.id);
  const summary = getAgentWorkSummary(agent, { linkedSessions: workingSessions });
  const monitor = isMobile ? null : monitorView(liveTurn, { agent, state: working ? "working" : "resting", info: summary, seenAt: null }, tr);
  const monitorSteps = useMemo(() => turnMonitorSteps(liveTurn, tr), [liveTurn, tr]);
  const handleStopTurn = async () => {
    if (window.confirm?.(tr("지금 진행 중인 턴을 중단할까요?", "Stop the running turn now?")) === false) return;
    setStoppingTurn(true);
    setStopFeedback(null);
    const res = await stopAgentTurn(agent.id);
    setStoppingTurn(false);
    if (res.ok) {
      setStopFeedback({ ok: true, text: tr("턴 중단 요청을 보냈어요.", "Turn stop requested.") });
      retrySessions();
      onAgentUpdated?.();
    } else {
      setStopFeedback({ ok: false, text: tr(`턴을 중단하지 못했어요 (${res.message})`, `Could not stop turn (${res.message})`) });
    }
  };
  const openTurns = () => {
    setOpened((previous) => new Set(previous).add("turns"));
    requestAnimationFrame(() => root.current?.querySelector('[data-testid="agent-detail-turns"]')?.scrollIntoView({ block: "nearest" }));
  };

  const department = departments.find((entry) => entry.id === selectedDeptId);
  // Counts show once read, so an unread list never reads as zero.
  const routeCount = shared.bindings === undefined ? null : bindings.length;
  const liveCount = sessions ? live.length : null;
  const connectionSummary = isMobile
    ? [routeCount === null ? null : tr(`경로 ${routeCount}`, `${routeCount} routes`), liveCount === null ? null : tr(`세션 ${liveCount}`, `${liveCount} sessions`)]
    : [routeCount === null ? null : tr(`Discord 경로 ${routeCount}`, `${routeCount} Discord routes`), liveCount === null ? null : tr(`살아 있는 세션 ${liveCount}`, `${liveCount} live sessions`)];
  const primaryName = localeName(locale, agent);
  const secondaryName = agentSecondaryLine(agent, locale);

  return (
    <div ref={root} className="flex min-w-0 flex-col [overflow-wrap:anywhere] [word-break:keep-all]">
      <div className="flex min-w-0 items-start gap-3 pb-4">
        <AgentPersona agent={agent} size={44} animated />
        <div className="min-w-0 flex-1">
          <h2 id={titleId} tabIndex={-1} className="m-0 text-base font-semibold leading-6 text-(--fg) outline-none">
            {primaryName}
          </h2>
          <div className="flex min-w-0 flex-wrap items-center gap-x-1 text-xs leading-5 text-(--fg-muted)">
            {secondaryName ? <span className="min-w-0">{secondaryName}</span> : null}
            {editingAlias ? (
              <TextField
                size="sm"
                autoFocus
                aria-label={tr("별명 입력", "Enter alias")}
                value={aliasValue}
                onChange={(event) => setAliasValue(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    void saveAlias();
                  }
                  if (event.key === "Escape") {
                    event.preventDefault();
                    setEditingAlias(false);
                    setAliasValue(agent.alias ?? "");
                  }
                }}
                onBlur={() => void saveAlias()}
                disabled={savingAlias}
                placeholder={tr("별명 입력", "Enter alias")}
                className="max-w-56"
              />
            ) : (
              <Button
                variant="ghost"
                size="sm"
                title={tr("별명 편집", "Edit alias")}
                onClick={() => {
                  setAliasValue(agent.alias ?? "");
                  setEditingAlias(true);
                }}
              >
                {agent.alias ? `aka ${agent.alias}` : `+ ${tr("별명", "alias")}`}
              </Button>
            )}
          </div>
        </div>
        {actions ? <div className="flex shrink-0 items-center gap-1.5">{actions}</div> : null}
      </div>

      <AgentNowSection
        agent={agent}
        state={state}
        tr={tr}
        isKo={isKo}
        summary={summary}
        monitor={monitor}
        steps={monitorSteps}
        onOpenLog={openTurns}
        onStopTurn={working ? () => void handleStopTurn() : undefined}
        stoppingTurn={stoppingTurn}
        stopFeedback={stopFeedback}
        reasons={pageReasons ?? ownReasons}
        warnings={getAgentWarnings(agent, { hasDiscordBindings: shared.bindings ? bindings.length > 0 : undefined, linkedSessions: workingSessions })}
      />

      <AgentRunSettingsSection
        tr={tr}
        locale={locale}
        fold={fold("run")}
        summary={[department ? localeName(locale, department) : tr("미배정", "Unassigned"), getProviderMeta(selectedProvider).label].join(" · ")}
        departments={departments}
        selectedDeptId={selectedDeptId}
        savingDept={savingDept}
        onSaveDepartment={(nextDeptId) => void saveDepartment(nextDeptId)}
        selectedProvider={selectedProvider}
        providerOptions={providerCatalog.selectableIds}
        providerLabels={Object.fromEntries(providerCatalog.selectableIds.map((id) => [id, catalogLabel(providerCatalog.entries, id)]))}
        providerSelectDisabled={providerCatalog.loading}
        providerNotice={
          providerCatalog.loading
            ? tr("프로바이더 목록을 불러오는 중...", "Loading providers...")
            : providerCatalog.error
              ? tr("카탈로그를 못 불러 로컬 목록을 씁니다.", "Catalog unavailable; using local list.")
              : null
        }
        savingProvider={savingProvider}
        onSaveProvider={(nextProvider) => void saveProvider(nextProvider)}
        selectedAuthProfile={selectedAuthProfile}
        authProfileOptions={authProfileOptions}
        savingAuthProfile={savingAuthProfile}
        onSaveAuthProfile={(profileId) => void saveAuthProfile(profileId)}
      >
        <AgentExecutionNodeSelect key={agent.id} agentId={agent.id} provider={selectedProvider} tr={tr} onSaved={onAgentUpdated} />
      </AgentRunSettingsSection>

      <AgentConnectionSection
        tr={tr}
        fold={connections}
        summary={connectionSummary.filter(Boolean).join(" · ")}
        bindings={bindings}
        sessions={sessions}
        resolveChannel={resolveChannel}
        resolveParent={resolveParent}
        onRetrySessions={retrySessions}
      />
      <AgentInfoTurnsSection agentId={agent.id} tr={tr} isKo={isKo} fold={fold("turns")} />
      <AgentSkillsSection
        tr={tr}
        fold={fold("skills")}
        calls={calls}
        skills={skills}
        onRetryCalls={shared.retrySkillCalls}
        onRetrySkills={retrySkills}
      />
      <AgentActivitySection
        tr={tr}
        isKo={isKo}
        fold={activityFold}
        timeline={timelineRead.value}
        audit={auditRead.value}
        onRetryTimeline={timelineRead.retry}
        onRetryAudit={auditRead.retry}
      />
      <AgentCronSection tr={tr} isKo={isKo} fold={cronFold} jobs={cronRead.value} onRetry={cronRead.retry} />
      <AgentSourceSection agent={agent} tr={tr} fold={fold("source")} roleMap={bindings.some((binding) => inferBindingSource(binding) === "role-map")} />
    </div>
  );
}
