import { useEffect, useState, type ReactNode } from "react";
import {
  getAgentWarnings,
  getAgentWorkElapsedMs,
  getAgentWorkSummary,
} from "../../agent-insights";
import type { Agent, Department, DispatchedSession } from "../../types";
import { localeName } from "../../i18n";
import { getProviderMeta } from "../../app/providerTheme";
import AgentPersona from "../AgentPersona";
import * as api from "../../api";
import type { CronJob, AgentSkill, DiscordBinding } from "../../api/client";
import { catalogLabel, getProviderAuthProfiles, useProviderCatalog } from "../../api/providers";
import type { ProviderAuthProvider } from "../settings/SettingsProvidersModel";
import { Button } from "../../kit/free/button-clean";
import { TextField } from "../../kit/free/text-field-clean";
import type { Translator } from "./types";
import { AgentInfoOperationsSections } from "./AgentInfoOperationsSections";
import { AgentInfoProfileSections } from "./AgentInfoProfileSections";
import { AgentInfoRoutingSections } from "./AgentInfoRoutingSections";
import { AgentExecutionNodeSelect } from "./AgentExecutionNodeSelect";
import { inferBindingSource, isTurnSession, splitLinkedSessions } from "./AgentInfoCardModel";
import { isDiscordSnowflake } from "./discord-routing";
import { AgentStatusBadge, agentSecondaryLine } from "./AgentCard";

interface AgentInfoCardContentProps {
  agent: Agent;
  isKo: boolean;
  locale: string;
  tr: Translator;
  departments: Department[];
  onAgentUpdated?: () => void;
  /** Buttons on the right of the title row (edit, menu, close). */
  actions?: ReactNode;
  titleId?: string;
}

/** Agent detail body: identity, status, run settings, routing and operations, with its own data loading. */
export function AgentInfoCardContent({
  agent,
  isKo,
  locale,
  tr,
  departments,
  onAgentUpdated,
  actions,
  titleId,
}: AgentInfoCardContentProps) {
  const [cronJobs, setCronJobs] = useState<CronJob[]>([]);
  const [agentSkills, setAgentSkills] = useState<AgentSkill[]>([]);
  const [sharedSkills, setSharedSkills] = useState<AgentSkill[]>([]);
  const [loadingCron, setLoadingCron] = useState(true);
  const [loadingSkills, setLoadingSkills] = useState(true);
  const [loadingClaudeSessions, setLoadingClaudeSessions] = useState(true);
  const [claudeSessions, setClaudeSessions] = useState<DispatchedSession[]>([]);
  const [closedSessionCount, setClosedSessionCount] = useState(0);
  const [showSharedSkills, setShowSharedSkills] = useState(false);
  const [discordBindings, setDiscordBindings] = useState<DiscordBinding[]>([]);
  const [discordChannelInfoById, setDiscordChannelInfoById] = useState<Record<string, api.DiscordChannelInfo>>({});
  const [loadingBindings, setLoadingBindings] = useState(true);
  const [auditLogs, setAuditLogs] = useState<Array<{ id: string; action: string; ts: number; detail?: string; summary?: string; created_at?: number }>>([]);
  const [loadingAudit, setLoadingAudit] = useState(true);
  const [auditError, setAuditError] = useState(false);
  const [editingAlias, setEditingAlias] = useState(false);
  const [aliasValue, setAliasValue] = useState(agent.alias ?? "");
  const [savingAlias, setSavingAlias] = useState(false);
  const [selectedDeptId, setSelectedDeptId] = useState(agent.department_id ?? "");
  const [savingDept, setSavingDept] = useState(false);
  const [selectedProvider, setSelectedProvider] = useState<string>(agent.cli_provider ?? "claude");
  const providerCatalog = useProviderCatalog(selectedProvider);
  const [savingProvider, setSavingProvider] = useState(false);
  const [providerAuthProfiles, setProviderAuthProfiles] = useState<ProviderAuthProvider[]>([]);
  const [selectedAuthProfile, setSelectedAuthProfile] = useState("__primary__");
  const [savingAuthProfile, setSavingAuthProfile] = useState(false);
  const [timeline, setTimeline] = useState<api.TimelineEvent[]>([]);
  const [loadingTimeline, setLoadingTimeline] = useState(true);
  const [timelineOpen, setTimelineOpen] = useState(false);

  const saveAlias = async () => {
    const trimmed = aliasValue.trim();
    const newAlias = trimmed || null;
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

  useEffect(() => {
    setAliasValue(agent.alias ?? "");
    setEditingAlias(false);
    setSelectedDeptId(agent.department_id ?? "");
    setSelectedProvider(agent.cli_provider ?? "claude");
    setSelectedAuthProfile("__primary__");
  }, [agent.alias, agent.department_id, agent.cli_provider, agent.id]);

  useEffect(() => {
    let cancelled = false;
    void getProviderAuthProfiles()
      .then((response) => {
        if (cancelled) return;
        const providers = Array.isArray(response.providers) ? response.providers : [];
        setProviderAuthProfiles(providers);
        const override = response.agent_profile_overrides?.find((entry) => entry.agent_id === agent.id);
        setSelectedAuthProfile(override?.profile_id ?? "__primary__");
      })
      .catch(() => {
        if (!cancelled) setProviderAuthProfiles([]);
      });
    return () => {
      cancelled = true;
    };
  }, [agent.id]);

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
      await api.updateAgent(agent.id, {
        cli_provider: nextProvider as Agent["cli_provider"],
        auth_profile: null,
      });
      setSelectedAuthProfile("__primary__");
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
    setSelectedAuthProfile(nextProfile);
    setSavingAuthProfile(true);
    try {
      await api.updateAgent(agent.id, { auth_profile: nextProfile === "__primary__" ? null : nextProfile });
      onAgentUpdated?.();
    } catch (error) {
      setSelectedAuthProfile(previousProfile);
      console.error("Auth profile save failed:", error);
    } finally {
      setSavingAuthProfile(false);
    }
  };

  const authProfileOptions = [
    { id: "__primary__", home: "" },
    ...(providerAuthProfiles.find((provider) => provider.id === selectedProvider)?.accounts ?? [{ id: "default", home: "" }]),
  ];

  useEffect(() => {
    setLoadingCron(true);
    api
      .getAgentCron(agent.id)
      .then((jobs) => {
        setCronJobs(jobs);
        setLoadingCron(false);
      })
      .catch(() => setLoadingCron(false));

    setLoadingSkills(true);
    api
      .getAgentSkills(agent.id)
      .then((data) => {
        setAgentSkills(data.skills);
        setSharedSkills(data.sharedSkills);
        setLoadingSkills(false);
      })
      .catch(() => setLoadingSkills(false));

    setLoadingClaudeSessions(true);
    api
      .getAgentDispatchedSessions(agent.id)
      .then((rows) => {
        // Finished sessions run into the hundreds, so only live ones are listed (and looked up on Discord).
        const { live, closed } = splitLinkedSessions(rows);
        setClaudeSessions(live);
        setClosedSessionCount(closed);
        setLoadingClaudeSessions(false);
      })
      .catch(() => setLoadingClaudeSessions(false));

    setLoadingBindings(true);
    api
      .getDiscordBindings()
      .then((bindings) => {
        setDiscordBindings(bindings.filter((b) => b.agentId === agent.id));
        setLoadingBindings(false);
      })
      .catch(() => {
        setDiscordBindings([]);
        setLoadingBindings(false);
      });

    setLoadingTimeline(true);
    api
      .getAgentTimeline(agent.id, 30)
      .then((events) => {
        setTimeline(events);
        setLoadingTimeline(false);
      })
      .catch(() => {
        setTimeline([]);
        setLoadingTimeline(false);
      });

    setLoadingAudit(true);
    setAuditError(false);
    api
      .getAuditLogs(20, { agentId: agent.id })
      .then((logs) => setAuditLogs(logs.map((log) => ({ ...log, ts: log.created_at }))))
      .catch(() => {
        setAuditLogs([]);
        setAuditError(true);
      })
      .finally(() => setLoadingAudit(false));
  }, [agent.id]);

  useEffect(() => {
    const seedIds = Array.from(
      new Set(
        [
          ...discordBindings.flatMap((binding) => [binding.channelId, binding.counterModelChannelId ?? null]),
          ...claudeSessions.map((session) => session.thread_channel_id ?? null),
        ].filter((value): value is string => isDiscordSnowflake(value)),
      ),
    );

    if (seedIds.length === 0) {
      setDiscordChannelInfoById({});
      return;
    }

    let cancelled = false;

    const loadChannelInfo = async (ids: string[]) => {
      const entries = await Promise.all(
        ids.map(async (id) => {
          try {
            const info = await api.getDiscordChannelInfo(id);
            return info?.id ? ([id, info] as const) : null;
          } catch {
            return null;
          }
        }),
      );
      return Object.fromEntries(
        entries.filter((entry): entry is readonly [string, api.DiscordChannelInfo] => entry !== null),
      );
    };

    void (async () => {
      const initialInfo = await loadChannelInfo(seedIds);
      const parentIds = Array.from(
        new Set(
          Object.values(initialInfo)
            .map((info) => info.parent_id ?? null)
            .filter((value): value is string => isDiscordSnowflake(value) && !(value in initialInfo)),
        ),
      );
      const parentInfo = parentIds.length > 0 ? await loadChannelInfo(parentIds) : {};
      if (!cancelled) setDiscordChannelInfoById({ ...initialInfo, ...parentInfo });
    })();

    return () => {
      cancelled = true;
    };
  }, [claudeSessions, discordBindings]);

  const sourceLabel = agent.activity_source === "agentdesk" ? tr("AgentDesk 작업", "AgentDesk") : null;
  const workingLinkedSessions = claudeSessions.filter(isTurnSession);
  const currentWorkSummary = getAgentWorkSummary(agent, { linkedSessions: workingLinkedSessions });
  const currentWorkElapsedMs = getAgentWorkElapsedMs(agent, workingLinkedSessions);
  const warnings = getAgentWarnings(agent, {
    hasDiscordBindings: loadingBindings ? undefined : discordBindings.length > 0,
    linkedSessions: workingLinkedSessions,
  });
  const currentWorkDetails = Array.from(
    new Set(
      [
        agent.session_info,
        ...workingLinkedSessions.flatMap((session) => [session.session_info, session.name]),
      ].filter((value): value is string => Boolean(value && value.trim())),
    ),
  ).slice(0, 3);
  const roleMapBindings = discordBindings.filter((binding) => inferBindingSource(binding) === "role-map");
  const resolveDiscordChannelInfo = (channelId: string | null | undefined): api.DiscordChannelInfo | null =>
    channelId && isDiscordSnowflake(channelId) ? discordChannelInfoById[channelId] ?? null : null;
  const resolveDiscordParentInfo = (channelInfo: api.DiscordChannelInfo | null | undefined): api.DiscordChannelInfo | null =>
    channelInfo?.parent_id && isDiscordSnowflake(channelInfo.parent_id)
      ? discordChannelInfoById[channelInfo.parent_id] ?? null
      : null;
  const sourceOfTruthRows: Array<{ label: string; value: string; tone?: string }> = [
    { label: tr("에이전트 ID", "Agent ID"), value: agent.id },
    { label: tr("이름", "Name"), value: agent.name },
  ];
  const primaryName = localeName(locale, agent);
  const secondaryName = agentSecondaryLine(agent, locale);
  const providerMeta = getProviderMeta(selectedProvider);

  return (
    <div className="flex min-w-0 flex-col gap-7 [overflow-wrap:anywhere] [word-break:keep-all]">
      <div className="flex min-w-0 items-start gap-3">
        <AgentPersona agent={agent} size={44} animated />
        <div className="min-w-0 flex-1">
          <h2 id={titleId} tabIndex={-1} className="m-0 text-base font-semibold leading-6 text-(--fg) outline-none">
            {primaryName}
          </h2>
          {secondaryName ? (
            <p className="m-0 text-xs leading-5 text-(--fg-muted)">{secondaryName}</p>
          ) : null}
        </div>
        {actions ? <div className="flex shrink-0 items-center gap-1.5">{actions}</div> : null}
      </div>

      <dl className="m-0 -mt-2 flex min-w-0 flex-col gap-1 text-[13px]">
        <div className="flex min-h-7 items-center gap-3">
          <dt className="w-24 shrink-0 text-(--fg-muted)">{tr("상태", "Status")}</dt>
          <dd className="m-0 flex min-w-0 flex-1 flex-wrap items-center gap-1.5">
            <AgentStatusBadge status={agent.status} tr={tr} />
            {agent.status === "working" && sourceLabel ? <span className="text-xs text-(--fg-muted)">{sourceLabel}</span> : null}
          </dd>
        </div>
        <div className="flex min-h-7 items-center gap-3">
          <dt className="w-24 shrink-0 text-(--fg-muted)">Provider</dt>
          <dd className="m-0 min-w-0 flex-1 text-(--fg)">{providerMeta.label}</dd>
        </div>
        <div className="flex min-h-7 items-center gap-3">
          <dt className="w-24 shrink-0 text-(--fg-muted)">{tr("별명", "Alias")}</dt>
          <dd className="m-0 min-w-0 flex-1">
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
                className="-ml-2"
                title={tr("별명 편집", "Edit alias")}
                onClick={() => {
                  setAliasValue(agent.alias ?? "");
                  setEditingAlias(true);
                }}
              >
                {agent.alias ? `aka ${agent.alias}` : `+ ${tr("별명", "alias")}`}
              </Button>
            )}
          </dd>
        </div>
        <div className="flex min-h-7 items-center gap-3">
          <dt className="w-24 shrink-0 text-(--fg-muted)">{tr("완료", "Done")}</dt>
          <dd className="m-0 min-w-0 flex-1 tabular-nums text-(--fg)">{`${agent.stats_tasks_done.toLocaleString()} ${tr("건", "tasks")}`}</dd>
        </div>
        <div className="flex min-h-7 items-center gap-3">
          <dt className="w-24 shrink-0 text-(--fg-muted)">{tr("작업 중 세션", "Working sessions")}</dt>
          <dd className="m-0 min-w-0 flex-1 tabular-nums text-(--fg)">{`${workingLinkedSessions.length}/${claudeSessions.length}`}</dd>
        </div>
        {agent.role_id ? (
          <div className="flex min-h-7 items-center gap-3">
            <dt className="w-24 shrink-0 text-(--fg-muted)">{tr("역할 ID", "Role ID")}</dt>
            <dd className="m-0 min-w-0 flex-1 break-all font-mono text-xs text-(--fg-dim)">{agent.role_id}</dd>
          </div>
        ) : null}
      </dl>

      <AgentInfoProfileSections
        agent={agent}
        departments={departments}
        locale={locale}
        isKo={isKo}
        tr={tr}
        selectedDeptId={selectedDeptId}
        savingDept={savingDept}
        onSaveDepartment={(nextDeptId) => void saveDepartment(nextDeptId)}
        selectedProvider={selectedProvider}
        providerOptions={providerCatalog.selectableIds}
        providerLabels={Object.fromEntries(
          providerCatalog.selectableIds.map((id) => [id, catalogLabel(providerCatalog.entries, id)]),
        )}
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
        authProfileOptions={authProfileOptions.map((account) => account.id)}
        savingAuthProfile={savingAuthProfile}
        onSaveAuthProfile={(profileId) => void saveAuthProfile(profileId)}
        currentWorkSummary={currentWorkSummary}
        currentWorkElapsedMs={currentWorkElapsedMs}
        currentWorkDetails={currentWorkDetails}
        discordBindings={discordBindings}
        warnings={warnings}
      />

      <AgentExecutionNodeSelect key={agent.id} agentId={agent.id} provider={selectedProvider} tr={tr} onSaved={onAgentUpdated} />

      <AgentInfoRoutingSections
        tr={tr}
        discordBindings={discordBindings}
        roleMapBindings={roleMapBindings}
        claudeSessions={claudeSessions}
        closedSessionCount={closedSessionCount}
        loadingClaudeSessions={loadingClaudeSessions}
        sourceOfTruthRows={sourceOfTruthRows}
        resolveDiscordChannelInfo={resolveDiscordChannelInfo}
        resolveDiscordParentInfo={resolveDiscordParentInfo}
      />

      <AgentInfoOperationsSections
        isKo={isKo}
        tr={tr}
        cronJobs={cronJobs}
        loadingCron={loadingCron}
        auditLogs={auditLogs}
        loadingAudit={loadingAudit}
        auditError={auditError}
        agentSkills={agentSkills}
        sharedSkills={sharedSkills}
        loadingSkills={loadingSkills}
        showSharedSkills={showSharedSkills}
        onToggleSharedSkills={() => setShowSharedSkills((value) => !value)}
        timeline={timeline}
        loadingTimeline={loadingTimeline}
        timelineOpen={timelineOpen}
        onToggleTimeline={() => setTimelineOpen((value) => !value)}
      />
    </div>
  );
}
