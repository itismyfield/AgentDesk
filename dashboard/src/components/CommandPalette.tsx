import { useEffect, useMemo, useState, type CSSProperties, type FormEvent, type KeyboardEvent, type ReactNode } from "react";
import { CalendarClock, Folder, Hash, Layers, Play, Settings, Terminal } from "lucide-react";
import { getCampaigns, type Campaign } from "../api/campaigns";
import { request } from "../api/httpClient";
import type { RoutineRecord } from "../api/routines";
import { getServerCampaignSummaries, type ServerCampaignSummary } from "../api/serverCampaigns";
import { getSettingsConfig } from "../api/settingsRuntime";
import type { Agent, Department, DispatchedSession, KanbanCard } from "../types";
import { localeName } from "../i18n";
import { isTestAgent } from "../lib/agentHelpers";
import { CommandMenu, type CommandMenuCommand, type CommandMenuGroup, type CommandMenuLabels } from "../kit/pro/command-menu";
import AgentPersona from "./AgentPersona";
import { cardTitle, isOpenKanbanCard } from "./agent-manager/kanban-utils";
import { queueLocationQuery } from "./campaigns/campaignQueueModel";
import { campaignLocationQuery } from "./campaigns/serverCampaignModel";
import { filterCommandGroups } from "./commandPaletteSearch";
import { isOpenServerCampaign, unlinkedSessionTitle } from "./home/homeModel";
import type { Notification } from "./NotificationCenter";
import { SETTINGS_CATEGORIES, settingKeyMeta } from "./settings/SettingsCopy";
import type { ConfigEntry } from "./settings/SettingsModel";

function searchTerms(...values: Array<string | number | null | undefined>): string[] {
  return values
    .map((value) => (value == null ? "" : String(value).trim()))
    .filter((value) => value.length > 0);
}

interface PaletteRoute {
  id: string;
  labelKo: string;
  labelEn: string;
  icon: ReactNode;
}

export interface CommandPaletteProps {
  agents: Agent[];
  departments: Department[];
  sessions?: DispatchedSession[];
  kanbanCards?: KanbanCard[];
  campaigns?: Campaign[];
  serverCampaigns?: ServerCampaignSummary[];
  routines?: RoutineRecord[];
  configEntries?: ConfigEntry[];
  isKo: boolean;
  onSelectAgent: (agent: Agent) => void;
  onNavigate: (view: string) => void;
  onNotify?: (message: string, type?: Notification["type"]) => void;
  onClose: () => void;
  routes: PaletteRoute[];
  departmentRouteId?: string;
}

const OVERLAY_STYLE = { "--aui-overlay": "var(--th-modal-overlay)" } as CSSProperties;

const PALETTE_LIST_MASK_CSS = `
.adk-command-palette .aui-command-menu-list {
  scroll-padding-bottom: 20px;
  -webkit-mask-image: linear-gradient(to bottom, #000 0%, #000 calc(100% - 18px), transparent 100%);
  mask-image: linear-gradient(to bottom, #000 0%, #000 calc(100% - 18px), transparent 100%);
}
.adk-command-palette .aui-command-menu-list-content {
  padding-bottom: 18px;
}
`;

const INDEXED_SETTING_KEYS = [
  "dispatchRateLimitGateEnabled",
  "dispatchRateLimitGateDangerPct",
  "rateLimitWarningPct",
  "rateLimitDangerPct",
  "rateLimitStaleSec",
  "staleDispatchedGraceMin",
  "staleDispatchedTerminalStatuses",
  "staleDispatchedRecoverNullDispatch",
  "staleDispatchedRecoverMissingDispatch",
  "requested_timeout_min",
  "in_progress_stale_min",
  "maxRetries",
  "maxEntryRetries",
  "review_enabled",
  "max_review_rounds",
  "context_compact_percent",
  "context_compact_percent_claude",
  "context_compact_percent_codex",
  "context_compact_lower_bound_tokens",
  "agent_quality_monitoring_channel_id",
  "language",
  "theme",
  "server_port",
];

const IDLE_PREVIEW_LIMIT = 8;

export default function CommandPalette({
  agents,
  departments,
  sessions = [],
  kanbanCards: suppliedCards,
  campaigns: suppliedCampaigns,
  serverCampaigns: suppliedServerCampaigns,
  routines: suppliedRoutines,
  configEntries: suppliedConfigEntries,
  isKo,
  onSelectAgent,
  onNavigate,
  onNotify,
  onClose,
  routes,
  departmentRouteId = "settings_organization",
}: CommandPaletteProps) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const [query, setQuery] = useState("");
  const [opener] = useState(() => (document.activeElement instanceof HTMLElement ? document.activeElement : null));
  useEffect(() => () => {
    if (opener?.isConnected && opener !== document.body) opener.focus({ preventScroll: true });
  }, [opener]);

  const [fetchedCards, setFetchedCards] = useState<KanbanCard[]>([]);
  const [fetchedCampaigns, setFetchedCampaigns] = useState<Campaign[]>([]);
  const [fetchedServerCampaigns, setFetchedServerCampaigns] = useState<ServerCampaignSummary[]>([]);
  const [fetchedRoutines, setFetchedRoutines] = useState<RoutineRecord[]>([]);
  const [fetchedConfig, setFetchedConfig] = useState<ConfigEntry[]>([]);

  useEffect(() => {
    let active = true;
    if (!suppliedCards) {
      request<{ cards?: KanbanCard[] }>("/api/kanban-cards", { suppressErrorToast: true })
        .then((res) => { if (active) setFetchedCards(res.cards ?? []); })
        .catch(() => {});
    }
    if (!suppliedCampaigns) {
      getCampaigns()
        .then((res) => { if (active) setFetchedCampaigns(res.campaigns ?? []); })
        .catch(() => {});
    }
    if (!suppliedServerCampaigns) {
      getServerCampaignSummaries()
        .then((res) => { if (active) setFetchedServerCampaigns(res ?? []); })
        .catch(() => {});
    }
    if (!suppliedRoutines) {
      request<{ routines?: RoutineRecord[] }>("/api/routines", { suppressErrorToast: true })
        .then((res) => { if (active) setFetchedRoutines(res.routines ?? []); })
        .catch(() => {});
    }
    if (!suppliedConfigEntries) {
      getSettingsConfig()
        .then((res) => { if (active) setFetchedConfig(res.entries ?? []); })
        .catch(() => {});
    }
    return () => { active = false; };
  }, [suppliedCards, suppliedCampaigns, suppliedServerCampaigns, suppliedRoutines, suppliedConfigEntries]);

  const cards = suppliedCards ?? fetchedCards;
  const legacyCampaigns = suppliedCampaigns ?? fetchedCampaigns;
  const serverCampaigns = suppliedServerCampaigns ?? fetchedServerCampaigns;
  const routines = suppliedRoutines ?? fetchedRoutines;
  const configEntries = suppliedConfigEntries ?? fetchedConfig;

  const pick = (run: () => void) => () => {
    run();
    onClose();
  };

  const runRoutineNow = (routine: RoutineRecord) => () => {
    onClose();
    void request(`/api/routines/${encodeURIComponent(routine.id)}/run-now`, {
      method: "POST",
      maxRetries: 0,
      suppressErrorToast: true,
    })
      .then(() => {
        onNotify?.(tr(`${routine.name} 루틴을 지금 실행했어요.`, `Started routine ${routine.name}.`), "success");
      })
      .catch((error) => {
        onNotify?.(
          error instanceof Error ? error.message : tr(`${routine.name} 실행에 실패했어요.`, `Failed to run ${routine.name}.`),
          "error",
        );
      });
  };

  const hasQuery = query.trim().length > 0;

  const groups = useMemo<CommandMenuGroup[]>(() => {
    const realAgents = agents.filter(
      (agent) => !agent.id.startsWith("dispatched:") && (hasQuery || !isTestAgent(agent) || agent.status === "working"),
    );

    const openServer = serverCampaigns.filter(isOpenServerCampaign).map((summary): CommandMenuCommand => ({
      id: `server-campaign:${summary.view.campaign_id}`,
      label: summary.definition.title,
      hint: tr("서버 캠페인", "Server campaign"),
      icon: <Layers />,
      keywords: searchTerms(summary.view.campaign_id, summary.definition.title, summary.definition.goal, summary.definition.repository),
      onSelect: pick(() => onNavigate(`/campaigns${campaignLocationQuery({ mode: "server", campaign: summary.view.campaign_id, node: null })}`)),
    }));

    const openLegacy = legacyCampaigns
      .filter((campaign) => campaign.status === "active" || campaign.status === "paused" || campaign.status === "planned")
      .map((campaign): CommandMenuCommand => ({
        id: `legacy-campaign:${campaign.id}`,
        label: campaign.title,
        hint: tr(`원장 캠페인 · 작업 ${campaign.nodes?.length ?? 0}개`, `Ledger campaign · ${campaign.nodes?.length ?? 0} tasks`),
        icon: <Layers />,
        keywords: searchTerms(campaign.id, campaign.title, campaign.status),
        onSelect: pick(() => onNavigate(`/campaigns${campaignLocationQuery({ mode: "legacy", campaign: campaign.id, node: null })}`)),
      }));

    const taskCommands: CommandMenuCommand[] = [];
    const seenIssues = new Set<string>();
    for (const card of cards.filter(isOpenKanbanCard)) {
      const issueTag = card.github_issue_number ? `#${card.github_issue_number}` : null;
      if (issueTag) seenIssues.add(issueTag);
      const cleanTitle = cardTitle(card.title);
      taskCommands.push({
        id: `card:${card.id}`,
        label: issueTag ? `${issueTag} ${cleanTitle}` : cleanTitle,
        hint: searchTerms(card.status, card.github_repo?.split("/").pop()).join(" · ") || undefined,
        icon: <Hash />,
        keywords: searchTerms(card.id, issueTag, card.github_issue_number, cleanTitle, card.github_repo, card.status, card.blocked_reason),
        onSelect: pick(() => onNavigate(`/campaigns${queueLocationQuery(card.id)}`)),
      });
    }
    for (const campaign of legacyCampaigns) {
      if (campaign.status !== "active" && campaign.status !== "paused" && campaign.status !== "planned") continue;
      for (const node of campaign.nodes ?? []) {
        if (node.status === "completed" || node.status === "skipped") continue;
        const issueNum = (node as { github_issue_number?: number | null }).github_issue_number;
        const issueTag = issueNum ? `#${issueNum}` : null;
        if (issueTag && seenIssues.has(issueTag)) continue;
        taskCommands.push({
          id: `node:${campaign.id}:${node.id}`,
          label: issueTag ? `${issueTag} ${node.title}` : node.title,
          hint: searchTerms(campaign.title, node.status).join(" · ") || undefined,
          icon: <Hash />,
          keywords: searchTerms(node.id, issueTag, issueNum, node.title, campaign.title, node.status),
          onSelect: pick(() => onNavigate(`/campaigns${campaignLocationQuery({ mode: "legacy", campaign: campaign.id, node: node.id })}`)),
        });
      }
    }

    const sessionCommands: CommandMenuCommand[] = sessions
      .filter((session) => !session.linked_agent_id && session.status !== "disconnected")
      .map((session) => {
        const title = unlinkedSessionTitle(session);
        return {
          id: `session:${session.id}`,
          label: title,
          hint: searchTerms(session.provider, session.status, session.session_info?.split("\n")[0]?.trim()).join(" · ") || undefined,
          icon: <Terminal />,
          keywords: searchTerms(title, session.session_key, session.name, session.provider, session.model, session.status, session.session_info),
          onSelect: pick(() => onNavigate("/agents")),
        };
      });

    const routineCommands: CommandMenuCommand[] = routines
      .filter((routine) => routine.status !== "detached")
      .flatMap((routine): CommandMenuCommand[] => [
        {
          id: `routine:${routine.id}`,
          label: routine.name,
          hint: searchTerms(routine.schedule, routine.status).join(" · ") || undefined,
          icon: <CalendarClock />,
          keywords: searchTerms(routine.id, routine.name, routine.script_ref, routine.schedule, routine.status),
          onSelect: pick(() => onNavigate("/routines")),
        },
        {
          id: `routine-run:${routine.id}`,
          label: tr(`지금 실행: ${routine.name}`, `Run now: ${routine.name}`),
          hint: tr("루틴 즉시 실행", "Trigger routine immediately"),
          icon: <Play />,
          keywords: searchTerms("지금 실행", "run now", routine.id, routine.name, routine.script_ref),
          onSelect: runRoutineNow(routine),
        },
      ]);

    const configMap = new Map(configEntries.map((entry) => [entry.key, entry]));
    const allSettingKeys = [...new Set([...INDEXED_SETTING_KEYS, ...configEntries.map((entry) => entry.key)])];
    const categoryName = (id: string) => {
      const cat = SETTINGS_CATEGORIES.find((entry) => entry.id === id);
      return cat ? (isKo ? cat.ko : cat.en) : id;
    };
    const settingCommands: CommandMenuCommand[] = allSettingKeys.map((key) => {
      const meta = settingKeyMeta(key);
      const entry = configMap.get(key);
      const category = meta?.category ?? (entry?.category as typeof SETTINGS_CATEGORIES[number]["id"] | undefined) ?? "other";
      const label = (isKo ? meta?.labelKo ?? entry?.label_ko : meta?.labelEn ?? entry?.label_en) || key;
      return {
        id: `setting:${key}`,
        label: label === key ? key : `${label} (${key})`,
        hint: categoryName(category),
        icon: <Settings />,
        keywords: searchTerms(key, label, meta?.descKo, meta?.descEn, category, categoryName(category)),
        onSelect: pick(() => onNavigate(`/settings?category=${encodeURIComponent(category)}`)),
      };
    });

    const limitIdle = <T,>(items: T[]) => (hasQuery ? items : items.slice(0, IDLE_PREVIEW_LIMIT));

    return [
      {
        id: "navigate",
        label: tr("이동", "Navigate"),
        recommended: false,
        commands: routes.map((route) => ({
          id: route.id,
          label: tr(route.labelKo, route.labelEn),
          icon: route.icon,
          keywords: [route.id],
          onSelect: pick(() => onNavigate(route.id)),
        })),
      },
      {
        id: "agents",
        label: tr("에이전트", "Agents"),
        recommended: false,
        commands: realAgents.map((agent) => {
          const dept = departments.find((d) => d.id === agent.department_id);
          const deptHint = dept
            ? localeName(isKo ? "ko" : "en", dept)
            : (isKo ? agent.department_name_ko : agent.department_name || agent.department_name_ko);
          return {
            id: agent.id,
            label: agent.alias || localeName(isKo ? "ko" : "en", agent),
            hint: searchTerms(deptHint, agent.status).join(" · ") || undefined,
            icon: <AgentPersona agent={agent} size={24} />,
            keywords: searchTerms(agent.id, agent.name, agent.name_ko, agent.alias, agent.avatar_emoji, agent.department_name, agent.department_name_ko, dept?.name, dept?.name_ko, agent.status),
            onSelect: pick(() => onSelectAgent(agent)),
          };
        }),
      },
      {
        id: "sessions",
        label: tr("미연결 세션", "Unlinked sessions"),
        recommended: false,
        commands: limitIdle(sessionCommands),
      },
      {
        id: "campaigns",
        label: tr("캠페인", "Campaigns"),
        recommended: false,
        commands: limitIdle([...openServer, ...openLegacy]),
      },
      {
        id: "tasks",
        label: tr("작업 · 이슈", "Tasks & issues"),
        recommended: false,
        commands: limitIdle(taskCommands),
      },
      {
        id: "routines",
        label: tr("루틴", "Routines"),
        recommended: false,
        commands: limitIdle(routineCommands),
      },
      {
        id: "settings",
        label: tr("설정", "Settings"),
        recommended: false,
        commands: limitIdle(settingCommands),
      },
      {
        id: "departments",
        label: tr("부서", "Departments"),
        recommended: false,
        commands: departments.map((dept) => ({
          id: dept.id,
          label: localeName(isKo ? "ko" : "en", dept),
          hint: typeof dept.agent_count === "number" ? tr(`에이전트 ${dept.agent_count}명`, `${dept.agent_count} agents`) : undefined,
          icon: <Folder />,
          keywords: searchTerms(dept.id, dept.name, dept.name_ko, dept.icon),
          onSelect: pick(() => onNavigate(departmentRouteId)),
        })),
      },
    ].filter((group) => group.commands.length > 0);
  }, [agents, cards, configEntries, departmentRouteId, departments, hasQuery, isKo, legacyCampaigns, routes, routines, serverCampaigns, sessions]); // eslint-disable-line react-hooks/exhaustive-deps

  const labels: Partial<CommandMenuLabels> = {
    launch: tr("명령 팔레트 열기", "Open command palette"),
    search: tr("에이전트, 캠페인, #이슈, 세션, 루틴, 설정 검색", "Search agents, campaigns, #issues, sessions, routines, settings"),
    detailsShow: tr("설명 보기 (F2)", "Show details (F2)"),
    detailsHide: tr("설명 숨기기 (F2)", "Hide details (F2)"),
    close: tr("명령 팔레트 닫기", "Close command palette"),
    details: tr("항목 설명", "Details"),
    list: tr("검색 결과", "Search results"),
    empty: tr("보여 줄 항목이 없어요.", "Nothing to show."),
    noMatch: tr("맞는 항목이 없어요.", "No results."),
    groupMove: tr("다음 묶음", "Next group"),
    searchKey: tr("찾기", "Search"),
  };

  const closeOnEscape = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.key !== "Escape") return;
    event.preventDefault();
    event.stopPropagation();
    onClose();
  };

  return (
    <div
      className="fixed inset-0 z-[100] flex items-start justify-center px-4 pt-[8dvh]"
      style={OVERLAY_STYLE}
      onKeyDownCapture={closeOnEscape}
      onChange={(event: FormEvent<HTMLDivElement>) => {
        if (event.target instanceof HTMLInputElement) setQuery(event.target.value);
      }}
    >
      <style>{PALETTE_LIST_MASK_CSS}</style>
      <CommandMenu
        open
        modal
        autoFocus
        className="adk-command-palette"
        groups={filterCommandGroups(groups, query)}
        onClose={onClose}
        label={tr("명령 팔레트", "Command Palette")}
        placeholder={tr("검색... (에이전트, 캠페인, #이슈번호, 세션, 루틴, 설정 키)", "Search... (agents, campaigns, #issue, sessions, routines, settings)")}
        labels={labels}
      />
    </div>
  );
}
