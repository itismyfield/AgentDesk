import type { ReactNode } from "react";
import type { Agent, CompanySettings, DashboardStats, DispatchedSession, RoundTableMeeting } from "../../types";
import { AgentQualityWidget } from "./ExtraWidgets";
import {
  DashboardHomeActivityWidget,
  DashboardHomeMetricTile,
  DashboardHomeOfficeWidget,
  DashboardHomeRosterWidget,
  DashboardHomeSignalsWidget,
} from "./DashboardHomeRenderers";
import type { TFunction } from "./model";
import type { HomeWidgetId } from "./homeWidgetOrder";

type HomeSignalTone = "info" | "warn" | "danger" | "success";

interface HomeSignalRow {
  id: string;
  label: string;
  value: number;
  description: string;
  accent: string;
  tone: HomeSignalTone;
  onAction?: () => void;
}

interface HomeActivityItem {
  id: string;
  title: string;
  detail: string;
  timestamp: number;
  tone: "success" | "warn";
}

interface HomeAgentRow {
  agent: Agent;
  displayName: string;
  workSummary: string | null;
  elapsedLabel: string | null;
  linkedSessions: DispatchedSession[];
}

interface BuildDashboardHomeWidgetSpecsArgs {
  t: TFunction;
  numberFormatter: Intl.NumberFormat;
  dashboardStats: DashboardStats;
  reconnectingSessions: DispatchedSession[];
  activeSessions: DispatchedSession[];
  meetingSummary: { activeCount: number; unresolvedCount: number };
  meetings: RoundTableMeeting[];
  homeAgents: HomeAgentRow[];
  language: CompanySettings["language"];
  onSelectAgent?: (agent: Agent) => void;
  focusSignals: HomeSignalRow[];
  agents: Agent[];
  localeTag: string;
  homeActivityItems: HomeActivityItem[];
  onSelectTab: (tab: "achievements" | "meetings") => void;
}

export function buildDashboardHomeWidgetSpecs({
  t,
  numberFormatter,
  dashboardStats,
  reconnectingSessions,
  activeSessions,
  meetingSummary,
  meetings,
  homeAgents,
  language,
  onSelectAgent,
  focusSignals,
  agents,
  localeTag,
  homeActivityItems,
  onSelectTab,
}: BuildDashboardHomeWidgetSpecsArgs): Record<HomeWidgetId, { className: string; render: () => ReactNode }> {
  return {
    metric_agents: {
      className: "col-span-12 sm:col-span-6 xl:col-span-3",
      render: () => (
        <DashboardHomeMetricTile
          title={t({ ko: "작업 중", en: "Working", ja: "作業中", zh: "工作中" })}
          value={numberFormatter.format(dashboardStats.agents.working)}
          badge={t({ ko: `${numberFormatter.format(dashboardStats.agents.idle)} 대기`, en: `${numberFormatter.format(dashboardStats.agents.idle)} idle`, ja: `${numberFormatter.format(dashboardStats.agents.idle)} 待機`, zh: `${numberFormatter.format(dashboardStats.agents.idle)} 空闲` })}
          sub={t({ ko: `${numberFormatter.format(dashboardStats.agents.total)}명 등록`, en: `${numberFormatter.format(dashboardStats.agents.total)} registered`, ja: `${numberFormatter.format(dashboardStats.agents.total)}人登録`, zh: `已注册 ${numberFormatter.format(dashboardStats.agents.total)} 名` })}
          accent="#60a5fa"
          spark={[dashboardStats.agents.working, dashboardStats.agents.idle, dashboardStats.agents.break, dashboardStats.agents.offline]}
        />
      ),
    },
    metric_dispatch: {
      className: "col-span-12 sm:col-span-6 xl:col-span-3",
      render: () => (
        <DashboardHomeMetricTile
          title={t({ ko: "파견 세션", en: "Dispatched", ja: "派遣セッション", zh: "派遣会话" })}
          value={numberFormatter.format(dashboardStats.dispatched_count)}
          badge={t({ ko: `${reconnectingSessions.length} reconnect`, en: `${reconnectingSessions.length} reconnect`, ja: `${reconnectingSessions.length} reconnect`, zh: `${reconnectingSessions.length} reconnect` })}
          sub={t({ ko: `${numberFormatter.format(activeSessions.length)}개 활성 연결`, en: `${numberFormatter.format(activeSessions.length)} live sessions`, ja: `${numberFormatter.format(activeSessions.length)}件 アクティブ`, zh: `${numberFormatter.format(activeSessions.length)} 个活跃连接` })}
          accent="#34d399"
          spark={[
            activeSessions.filter((session) => session.status === "working").length,
            activeSessions.filter((session) => session.status === "idle").length,
            reconnectingSessions.length,
          ]}
        />
      ),
    },
    metric_review: {
      className: "col-span-12 sm:col-span-6 xl:col-span-3",
      render: () => (
        <DashboardHomeMetricTile
          title={t({ ko: "리뷰 큐", en: "Review Queue", ja: "レビューキュー", zh: "审查队列" })}
          value={numberFormatter.format(dashboardStats.kanban.review_queue)}
          badge={t({ ko: `${dashboardStats.kanban.blocked} blocked`, en: `${dashboardStats.kanban.blocked} blocked`, ja: `${dashboardStats.kanban.blocked} blocked`, zh: `${dashboardStats.kanban.blocked} blocked` })}
          sub={t({ ko: `requested ${dashboardStats.kanban.waiting_acceptance} · stale ${dashboardStats.kanban.stale_in_progress}`, en: `requested ${dashboardStats.kanban.waiting_acceptance} · stale ${dashboardStats.kanban.stale_in_progress}`, ja: `requested ${dashboardStats.kanban.waiting_acceptance} · stale ${dashboardStats.kanban.stale_in_progress}`, zh: `requested ${dashboardStats.kanban.waiting_acceptance} · stale ${dashboardStats.kanban.stale_in_progress}` })}
          accent="#f59e0b"
          spark={[
            dashboardStats.kanban.review_queue,
            dashboardStats.kanban.blocked,
            dashboardStats.kanban.waiting_acceptance,
            dashboardStats.kanban.stale_in_progress,
          ]}
        />
      ),
    },
    metric_followups: {
      className: "col-span-12 sm:col-span-6 xl:col-span-3",
      render: () => (
        <DashboardHomeMetricTile
          title={t({ ko: "회의 후속", en: "Follow-ups", ja: "会議フォローアップ", zh: "会议后续" })}
          value={numberFormatter.format(meetingSummary.unresolvedCount)}
          badge={t({ ko: `${meetingSummary.activeCount} active`, en: `${meetingSummary.activeCount} active`, ja: `${meetingSummary.activeCount} active`, zh: `${meetingSummary.activeCount} active` })}
          sub={t({ ko: `회의 ${meetings.length}건 · GitHub 종료 ${numberFormatter.format(dashboardStats.github_closed_today ?? 0)}`, en: `${meetings.length} meetings · ${numberFormatter.format(dashboardStats.github_closed_today ?? 0)} GitHub closed`, ja: `会議 ${meetings.length}件 · GitHub 完了 ${numberFormatter.format(dashboardStats.github_closed_today ?? 0)}`, zh: `会议 ${meetings.length} 个 · GitHub 已关闭 ${numberFormatter.format(dashboardStats.github_closed_today ?? 0)}` })}
          accent="#a855f7"
          spark={[meetingSummary.activeCount, meetingSummary.unresolvedCount, dashboardStats.github_closed_today ?? 0, meetings.length]}
        />
      ),
    },
    office: {
      className: "col-span-12 xl:col-span-8",
      render: () => (
        <DashboardHomeOfficeWidget
          rows={homeAgents.slice(0, 8)}
          stats={dashboardStats}
          language={language}
          t={t}
          onSelectAgent={onSelectAgent}
        />
      ),
    },
    signals: {
      className: "col-span-12 xl:col-span-4",
      render: () => (
        <DashboardHomeSignalsWidget
          rows={focusSignals}
          maxValue={Math.max(1, ...focusSignals.map((item) => item.value))}
          t={t}
        />
      ),
    },
    quality: {
      className: "col-span-12 xl:col-span-6",
      render: () => (
        <AgentQualityWidget
          agents={agents}
          t={t}
          localeTag={localeTag}
          compact
        />
      ),
    },
    roster: {
      className: "col-span-12 xl:col-span-7",
      render: () => (
        <DashboardHomeRosterWidget
          rows={homeAgents.slice(0, 5)}
          t={t}
          numberFormatter={numberFormatter}
          onSelectAgent={onSelectAgent}
          onOpenAchievements={() => onSelectTab("achievements")}
        />
      ),
    },
    activity: {
      className: "col-span-12 xl:col-span-5",
      render: () => (
        <DashboardHomeActivityWidget
          items={homeActivityItems}
          localeTag={localeTag}
          t={t}
          onOpenMeetings={() => onSelectTab("meetings")}
        />
      ),
    },
  };
}
