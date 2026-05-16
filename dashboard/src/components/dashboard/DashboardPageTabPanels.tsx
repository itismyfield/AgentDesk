import { lazy, Suspense } from "react";
import type { SkillRankingResponse } from "../../api";
import type { DashboardTab } from "../../app/dashboardTabs";
import type { Agent, CompanySettings, DashboardStats, RoundTableMeeting } from "../../types";
import { SurfaceEmptyState, SurfaceSubsection } from "../common/SurfacePrimitives";
import { DashboardRankingBoard, type RankedAgent } from "./HeroSections";
import {
  DashboardTabPanel,
  MeetingTimelineCard,
  PulseSectionShell,
  PulseSignalCard,
  SkillRankingSection,
} from "./DashboardHomeRenderers";
import {
  AchievementWidget,
  AgentQualityWidget,
  AutoQueueHistoryWidget,
  BottleneckWidget,
  CronTimelineWidget,
  DashboardDeptAndSquad,
  GitHubIssuesWidget,
  HeatmapWidget,
  SkillTrendWidget,
  buildDepartmentPerformanceRows,
} from "./ExtraWidgets";
import HealthWidget from "./HealthWidget";
import RateLimitWidget from "./RateLimitWidget";
import TokenAnalyticsSection from "./TokenAnalyticsSection";
import ReceiptWidget from "./ReceiptWidget";
import type { TFunction } from "./model";

const SkillCatalogView = lazy(() => import("../SkillCatalogView"));
const MeetingMinutesView = lazy(() => import("../MeetingMinutesView"));

type PulseKanbanSignal = "review" | "blocked" | "requested" | "stalled";

interface DashboardPageTabPanelsProps {
  activeTab: DashboardTab;
  t: TFunction;
  localeTag: string;
  staleLinkedSessions: Array<{ id: string }>;
  reconnectingSessions: Array<{ id: string }>;
  dashboardStats: DashboardStats;
  onOpenDispatchSessions?: () => void;
  onOpenKanbanSignal?: (signal: PulseKanbanSignal) => void;
  meetingSummary: { activeCount: number; unresolvedCount: number };
  meetings: RoundTableMeeting[];
  recentMeetings: RoundTableMeeting[];
  onOpenSettings?: () => void;
  agents: Agent[];
  deptPerformanceRows: ReturnType<typeof buildDepartmentPerformanceRows>;
  workingAgents: Agent[];
  idleAgentsList: Agent[];
  language: CompanySettings["language"];
  numberFormatter: Intl.NumberFormat;
  onSelectAgent?: (agent: Agent) => void;
  topGithubRepo?: string;
  skillRanking: SkillRankingResponse | null;
  skillWindow: "7d" | "30d" | "all";
  onChangeSkillWindow: (value: "7d" | "30d" | "all") => void;
  skillRankingUpdatedAt: number | null;
  skillRankingRefreshFailed: boolean;
  topAgents: RankedAgent[];
  podiumOrder: RankedAgent[];
  agentMap: Map<string, Agent>;
  maxXp: number;
  onRefreshMeetings?: () => void;
  onSelectTab: (tab: DashboardTab) => void;
}

export function DashboardPageTabPanels({
  activeTab,
  t,
  localeTag,
  staleLinkedSessions,
  reconnectingSessions,
  dashboardStats,
  onOpenDispatchSessions,
  onOpenKanbanSignal,
  meetingSummary,
  meetings,
  recentMeetings,
  onOpenSettings,
  agents,
  deptPerformanceRows,
  workingAgents,
  idleAgentsList,
  language,
  numberFormatter,
  onSelectAgent,
  topGithubRepo,
  skillRanking,
  skillWindow,
  onChangeSkillWindow,
  skillRankingUpdatedAt,
  skillRankingRefreshFailed,
  topAgents,
  podiumOrder,
  agentMap,
  maxXp,
  onRefreshMeetings,
  onSelectTab,
}: DashboardPageTabPanelsProps) {
  return (
    <>
<DashboardTabPanel tab="operations" activeTab={activeTab} t={t}>
          <div className="grid gap-4 xl:grid-cols-[minmax(0,1.1fr)_minmax(0,0.9fr)]">
            <SurfaceSubsection
              title={t({ ko: "운영 시그널", en: "Ops Signals", ja: "運用シグナル", zh: "运营信号" })}
              description={t({
                ko: "세션 이상, 칸반 병목, 회의 후속 정리를 현재 탭에서 바로 점검합니다.",
                en: "Inspect session anomalies, kanban bottlenecks, and meeting follow-ups from this tab.",
                ja: "セッション異常、カンバンの詰まり、会議後続整理をこのタブで直接確認します。",
                zh: "在当前标签页直接检查会话异常、看板瓶颈和会议后续整理。",
              })}
              style={{
                borderColor: "color-mix(in srgb, var(--th-accent-info) 22%, var(--th-border) 78%)",
                background:
                  "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-info) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
              }}
            >
              <div className="mt-4 grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
                <PulseSignalCard
                  label={t({ ko: "세션 신호", en: "Session Signal", ja: "セッション信号", zh: "会话信号" })}
                  value={staleLinkedSessions.length + reconnectingSessions.length}
                  accent="#f97316"
                  sublabel={t({
                    ko: `${staleLinkedSessions.length} stale / ${reconnectingSessions.length} reconnecting`,
                    en: `${staleLinkedSessions.length} stale / ${reconnectingSessions.length} reconnecting`,
                    ja: `${staleLinkedSessions.length} stale / ${reconnectingSessions.length} reconnecting`,
                    zh: `${staleLinkedSessions.length} stale / ${reconnectingSessions.length} reconnecting`,
                  })}
                  actionLabel={t({ ko: "Dispatch 보기", en: "Open Dispatch", ja: "Dispatch を開く", zh: "打开 Dispatch" })}
                  onAction={onOpenDispatchSessions}
                />
                <PulseSignalCard
                  label={t({ ko: "리뷰 대기", en: "Review Queue", ja: "レビュー待ち", zh: "待审查" })}
                  value={dashboardStats.kanban.review_queue}
                  accent="#14b8a6"
                  sublabel={t({
                    ko: "검토/판정이 필요한 카드",
                    en: "Cards waiting for review or decision",
                    ja: "レビューまたは判断待ちカード",
                    zh: "等待审查或决策的卡片",
                  })}
                  actionLabel={t({ ko: "칸반 열기", en: "Open Kanban", ja: "カンバンを開く", zh: "打开看板" })}
                  onAction={onOpenKanbanSignal ? () => onOpenKanbanSignal("review") : undefined}
                />
                <PulseSignalCard
                  label={t({ ko: "블록됨", en: "Blocked", ja: "ブロック", zh: "阻塞" })}
                  value={dashboardStats.kanban.blocked}
                  accent="#ef4444"
                  sublabel={t({
                    ko: "수동 판단이나 해소를 기다리는 카드",
                    en: "Cards waiting on unblock or manual intervention",
                    ja: "解除や手動判断待ちのカード",
                    zh: "等待解除阻塞或人工判断的卡片",
                  })}
                  actionLabel={t({ ko: "막힘 카드 보기", en: "Open Blocked", ja: "Blocked を開く", zh: "打开阻塞卡片" })}
                  onAction={onOpenKanbanSignal ? () => onOpenKanbanSignal("blocked") : undefined}
                />
                <PulseSignalCard
                  label={t({ ko: "수락 지연", en: "Waiting Acceptance", ja: "受諾遅延", zh: "接收延迟" })}
                  value={dashboardStats.kanban.waiting_acceptance}
                  accent="#10b981"
                  sublabel={t({
                    ko: "requested 상태에 머문 카드",
                    en: "Cards stalled in requested",
                    ja: "requested に留まるカード",
                    zh: "停留在 requested 的卡片",
                  })}
                  actionLabel={t({ ko: "requested 보기", en: "Open Requested", ja: "requested を開く", zh: "打开 requested" })}
                  onAction={onOpenKanbanSignal ? () => onOpenKanbanSignal("requested") : undefined}
                />
                <PulseSignalCard
                  label={t({ ko: "진행 정체", en: "Stale In Progress", ja: "進行停滞", zh: "进行停滞" })}
                  value={dashboardStats.kanban.stale_in_progress}
                  accent="#f59e0b"
                  sublabel={t({
                    ko: "오래 머무는 in_progress 카드",
                    en: "Cards stuck in progress",
                    ja: "進行が長引く in_progress カード",
                    zh: "长时间停留在 in_progress 的卡片",
                  })}
                  actionLabel={t({ ko: "정체 카드 보기", en: "Open Stale", ja: "停滞カードを開く", zh: "打开停滞卡片" })}
                  onAction={onOpenKanbanSignal ? () => onOpenKanbanSignal("stalled") : undefined}
                />
                <PulseSignalCard
                  label={t({ ko: "회의 후속", en: "Meeting Follow-up", ja: "会議フォローアップ", zh: "会议后续" })}
                  value={meetingSummary.unresolvedCount}
                  accent="#22c55e"
                  sublabel={t({
                    ko: `${meetingSummary.activeCount} active / ${meetings.length} total`,
                    en: `${meetingSummary.activeCount} active / ${meetings.length} total`,
                    ja: `${meetingSummary.activeCount} active / ${meetings.length} total`,
                    zh: `${meetingSummary.activeCount} active / ${meetings.length} total`,
                  })}
                  actionLabel={t({ ko: "회의록 열기", en: "Open Meetings", ja: "会議録を開く", zh: "打开会议记录" })}
                  onAction={() => onSelectTab("meetings")}
                />
              </div>
            </SurfaceSubsection>

            <MeetingTimelineCard
              meetings={recentMeetings}
              activeCount={meetingSummary.activeCount}
              followUpCount={meetingSummary.unresolvedCount}
              localeTag={localeTag}
              t={t}
              onOpenMeetings={() => onSelectTab("meetings")}
            />
          </div>
          <div className="grid gap-4 xl:grid-cols-[minmax(0,1.1fr)_minmax(0,0.9fr)]">
            <HealthWidget t={t} localeTag={localeTag} />
            <RateLimitWidget t={t} onOpenSettings={onOpenSettings} />
          </div>
          <AgentQualityWidget agents={agents} t={t} localeTag={localeTag} />
          <DashboardDeptAndSquad
            deptRows={deptPerformanceRows}
            workingAgents={workingAgents}
            idleAgentsList={idleAgentsList}
            agents={agents}
            language={language}
            numberFormatter={numberFormatter}
            t={t}
            onSelectAgent={onSelectAgent}
          />
          <GitHubIssuesWidget t={t} repo={topGithubRepo} />
          <BottleneckWidget t={t} />
      </DashboardTabPanel>

      <DashboardTabPanel tab="tokens" activeTab={activeTab} t={t}>
          <ReceiptWidget t={t} />
          <HeatmapWidget t={t} />
          <TokenAnalyticsSection
            agents={agents}
            t={t}
            numberFormatter={numberFormatter}
          />
      </DashboardTabPanel>

      <DashboardTabPanel tab="automation" activeTab={activeTab} t={t}>
          <PulseSectionShell
            eyebrow={t({ ko: "Automation", en: "Automation", ja: "Automation", zh: "Automation" })}
            title={t({ ko: "자동화 / 스킬", en: "Automation / Skills", ja: "自動化 / スキル", zh: "自动化 / 技能" })}
            subtitle=""
            badge=""
            style={{
              borderColor: "color-mix(in srgb, var(--th-accent-warn) 20%, var(--th-border) 80%)",
              background:
                "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-warn) 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
            }}
          >
            <CronTimelineWidget t={t} localeTag={localeTag} />

            <AutoQueueHistoryWidget t={t} />

            <SkillRankingSection
              skillRanking={skillRanking}
              skillWindow={skillWindow}
              onChangeWindow={onChangeSkillWindow}
              numberFormatter={numberFormatter}
              localeTag={localeTag}
              lastUpdatedAt={skillRankingUpdatedAt}
              refreshFailed={skillRankingRefreshFailed}
              t={t}
            />

            <SkillTrendWidget t={t} />

            <Suspense
              fallback={(
                <div className="py-8 text-center text-sm" style={{ color: "var(--th-text-muted)" }}>
                  {t({ ko: "스킬 카탈로그를 불러오는 중입니다", en: "Loading skill catalog", ja: "スキルカタログを読み込み中", zh: "正在加载技能目录" })}
                </div>
              )}
            >
              <SkillCatalogView embedded />
            </Suspense>
          </PulseSectionShell>
      </DashboardTabPanel>

      <DashboardTabPanel tab="achievements" activeTab={activeTab} t={t}>
          <PulseSectionShell
            eyebrow={t({ ko: "Achievement", en: "Achievement", ja: "Achievement", zh: "Achievement" })}
            title={t({ ko: "업적 / XP", en: "Achievements / XP", ja: "実績 / XP", zh: "成就 / XP" })}
            subtitle=""
            badge=""
            style={{
              borderColor: "color-mix(in srgb, var(--th-accent-primary) 18%, var(--th-border) 82%)",
              background:
                "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-primary) 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
            }}
          >
            <DashboardRankingBoard
              topAgents={topAgents}
              podiumOrder={podiumOrder}
              agentMap={agentMap}
              agents={agents}
              maxXp={maxXp}
              numberFormatter={numberFormatter}
              t={t}
              onSelectAgent={onSelectAgent}
            />

            <div className="grid gap-4 lg:grid-cols-2">
              <AchievementWidget t={t} agents={agents} />
              <SurfaceSubsection
                title={t({ ko: "XP 스냅샷", en: "XP Snapshot", ja: "XP スナップショット", zh: "XP 快照" })}
                description={t({
                  ko: "최상위 랭커의 XP 규모를 간단히 확인합니다.",
                  en: "Quick read on the scale of top-ranked XP.",
                  ja: "上位ランカーの XP 規模を簡単に確認します。",
                  zh: "快速查看头部 XP 规模。",
                })}
                style={{
                  borderColor: "color-mix(in srgb, var(--th-accent-primary) 22%, var(--th-border) 78%)",
                  background:
                    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-primary) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
                }}
              >
                {topAgents.length === 0 ? (
                  <SurfaceEmptyState className="mt-4 px-4 py-6 text-center text-sm">
                    {t({ ko: "아직 XP 집계 대상이 없습니다.", en: "No XP snapshot is available yet.", ja: "まだ XP スナップショット対象がありません。", zh: "尚无 XP 快照数据。" })}
                  </SurfaceEmptyState>
                ) : (
                  <div className="mt-4 grid gap-3 sm:grid-cols-3">
                    {topAgents.slice(0, 3).map((agent, index) => (
                      <div
                        key={agent.id}
                        className="rounded-2xl border px-4 py-3"
                        style={{
                          borderColor: "rgba(148,163,184,0.16)",
                          background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
                        }}
                      >
                        <div className="text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
                          {t({ ko: `${index + 1}위`, en: `Rank ${index + 1}`, ja: `${index + 1}位`, zh: `第 ${index + 1} 名` })}
                        </div>
                        <div className="mt-2 truncate text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
                          {agent.name}
                        </div>
                        <div className="mt-1 text-lg font-black tracking-tight" style={{ color: "var(--th-accent-primary)" }}>
                          {numberFormatter.format(agent.xp)} XP
                        </div>
                        <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                          {t({ ko: `${numberFormatter.format(agent.tasksDone)}개 완료`, en: `${numberFormatter.format(agent.tasksDone)} completed`, ja: `${numberFormatter.format(agent.tasksDone)} 完了`, zh: `完成 ${numberFormatter.format(agent.tasksDone)} 项` })}
                        </div>
                      </div>
                    ))}
                  </div>
                )}
              </SurfaceSubsection>
            </div>
          </PulseSectionShell>
      </DashboardTabPanel>

      <DashboardTabPanel tab="meetings" activeTab={activeTab} t={t}>
          <PulseSectionShell
            eyebrow={t({ ko: "Meetings", en: "Meetings", ja: "Meetings", zh: "Meetings" })}
            title={t({ ko: "회의 기록 / 후속 일감", en: "Meeting Records / Follow-ups", ja: "会議記録 / フォローアップ", zh: "会议记录 / 后续事项" })}
            subtitle=""
            badge=""
            style={{
              borderColor: "color-mix(in srgb, var(--th-accent-success) 18%, var(--th-border) 82%)",
              background:
                "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-success) 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
            }}
          >
            <Suspense
              fallback={(
                <div className="py-8 text-center text-sm" style={{ color: "var(--th-text-muted)" }}>
                  {t({ ko: "회의 기록을 불러오는 중입니다", en: "Loading meeting records", ja: "会議記録を読み込み中", zh: "正在加载会议记录" })}
                </div>
              )}
            >
              <MeetingMinutesView meetings={meetings} onRefresh={() => onRefreshMeetings?.()} embedded />
            </Suspense>
          </PulseSectionShell>
      </DashboardTabPanel>
    </>
  );
}
