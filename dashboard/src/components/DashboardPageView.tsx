import { lazy, Suspense, useCallback, useEffect, useMemo, useState, type CSSProperties, type ReactNode } from "react";
import { getSkillRanking, type SkillRankingResponse } from "../api";
import { getStaleLinkedSessions } from "../agent-insights";
import {
  readDashboardTabFromUrl,
  syncDashboardTabToUrl,
  type DashboardTab,
} from "../app/dashboardTabs";
import type {
  Agent,
  CompanySettings,
  DashboardStats,
  DispatchedSession,
  RoundTableMeeting,
} from "../types";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceListItem,
  SurfaceMetaBadge,
  SurfaceSection,
  SurfaceSegmentButton,
  SurfaceSubsection,
} from "./common/SurfacePrimitives";
import TooltipLabel from "./common/TooltipLabel";
import {
  DashboardHeroHeader,
  DashboardHudStats,
  DashboardRankingBoard,
  type HudStat,
  type RankedAgent,
} from "./dashboard/HeroSections";
import {
  AchievementWidget,
  AutoQueueHistoryWidget,
  BottleneckWidget,
  CronTimelineWidget,
  SkillTrendWidget,
} from "./dashboard/ExtraWidgets";
import HealthWidget from "./dashboard/HealthWidget";
import RateLimitWidget from "./dashboard/RateLimitWidget";
import TokenAnalyticsSection from "./dashboard/TokenAnalyticsSection";
import type { TFunction } from "./dashboard/model";
import { formatProviderFlow } from "./MeetingProviderFlow";

const SkillCatalogView = lazy(() => import("./SkillCatalogView"));
const MeetingMinutesView = lazy(() => import("./MeetingMinutesView"));

type PulseKanbanSignal = "review" | "blocked" | "requested" | "stalled";

interface DashboardPageViewProps {
  stats: DashboardStats | null;
  agents: Agent[];
  sessions: DispatchedSession[];
  meetings: RoundTableMeeting[];
  settings: CompanySettings;
  requestedTab?: DashboardTab | null;
  onSelectAgent?: (agent: Agent) => void;
  onOpenKanbanSignal?: (signal: PulseKanbanSignal) => void;
  onOpenDispatchSessions?: () => void;
  onOpenSettings?: () => void;
  onRefreshMeetings?: () => void;
  onRequestedTabHandled?: () => void;
}

export default function DashboardPageView({
  stats,
  agents,
  sessions,
  meetings,
  settings,
  requestedTab,
  onSelectAgent,
  onOpenKanbanSignal,
  onOpenDispatchSessions,
  onOpenSettings,
  onRefreshMeetings,
  onRequestedTabHandled,
}: DashboardPageViewProps) {
  const language = settings.language;
  const localeTag = language === "ko" ? "ko-KR" : language === "ja" ? "ja-JP" : language === "zh" ? "zh-CN" : "en-US";
  const numberFormatter = useMemo(() => new Intl.NumberFormat(localeTag), [localeTag]);
  const t: TFunction = useCallback((messages) => messages[language] ?? messages.ko, [language]);
  const [activeTab, setActiveTab] = useState<DashboardTab>(() => readDashboardTabFromUrl());
  const [skillRanking, setSkillRanking] = useState<SkillRankingResponse | null>(null);
  const [skillWindow, setSkillWindow] = useState<"7d" | "30d" | "all">("all");

  useEffect(() => {
    syncDashboardTabToUrl(activeTab);
  }, [activeTab]);

  useEffect(() => {
    if (!requestedTab) return;
    setActiveTab(requestedTab);
    onRequestedTabHandled?.();
  }, [requestedTab, onRequestedTabHandled]);

  useEffect(() => {
    let mounted = true;

    const load = async () => {
      try {
        const next = await getSkillRanking(skillWindow, 10);
        if (mounted) setSkillRanking(next);
      } catch {
        // Keep the last successful ranking during transient network failures.
      }
    };

    void load();
    const timer = setInterval(() => void load(), 60_000);
    return () => {
      mounted = false;
      clearInterval(timer);
    };
  }, [skillWindow]);

  if (!stats) {
    return (
      <div className="flex h-full items-center justify-center" style={{ color: "var(--th-text-muted)" }}>
        <div className="text-center">
          <div className="mb-4 text-4xl opacity-30">📊</div>
          <div>{t({ ko: "대시보드를 불러오는 중입니다", en: "Loading dashboard", ja: "ダッシュボードを読み込み中", zh: "正在加载仪表盘" })}</div>
        </div>
      </div>
    );
  }

  const hudStats: HudStat[] = [
    {
      id: "total",
      label: t({ ko: "전체 에이전트", en: "Total Agents", ja: "全エージェント", zh: "全部代理" }),
      value: stats.agents.total,
      sub: t({ ko: "등록 인원", en: "Registered", ja: "登録数", zh: "已注册" }),
      color: "#60a5fa",
      icon: "👥",
    },
    {
      id: "working",
      label: t({ ko: "작업 중", en: "Working", ja: "作業中", zh: "工作中" }),
      value: stats.agents.working,
      sub: t({ ko: "현재 가동", en: "Live now", ja: "稼働中", zh: "当前活跃" }),
      color: "#34d399",
      icon: "💼",
    },
    {
      id: "idle",
      label: t({ ko: "대기", en: "Idle", ja: "待機", zh: "空闲" }),
      value: stats.agents.idle,
      sub: t({ ko: "배정 가능", en: "Available", ja: "配置可能", zh: "可分配" }),
      color: "#94a3b8",
      icon: "⏸️",
    },
    {
      id: "dispatched",
      label: t({ ko: "파견 세션", en: "Dispatched", ja: "派遣セッション", zh: "派遣会话" }),
      value: stats.dispatched_count,
      sub: t({ ko: "외부 연결", en: "External sessions", ja: "外部接続", zh: "外部连接" }),
      color: "#f59e0b",
      icon: "🛰️",
    },
  ];

  const topAgents: RankedAgent[] = stats.top_agents.map((agent) => ({
    id: agent.id,
    name: agent.alias || agent.name_ko || agent.name,
    department: "",
    tasksDone: agent.stats_tasks_done,
    xp: agent.stats_xp,
  }));
  const podiumOrder: RankedAgent[] =
    topAgents.length >= 3
      ? [topAgents[1], topAgents[0], topAgents[2]]
      : topAgents.length === 2
        ? [topAgents[1], topAgents[0]]
        : [];
  const agentMap = new Map(agents.map((agent) => [agent.id, agent]));
  const maxXp = topAgents.reduce((max, agent) => Math.max(max, agent.xp), 1);
  const staleLinkedSessions = useMemo(() => getStaleLinkedSessions(sessions), [sessions]);
  const reconnectingSessions = useMemo(
    () => sessions.filter((session) => session.linked_agent_id && session.status === "disconnected"),
    [sessions],
  );
  const activeMeetings = useMemo(
    () => meetings.filter((meeting) => meeting.status === "in_progress"),
    [meetings],
  );
  const recentMeetings = useMemo(
    () =>
      [...meetings]
        .sort((left, right) => {
          const leftTime = left.started_at || left.created_at;
          const rightTime = right.started_at || right.created_at;
          return rightTime - leftTime;
        })
        .slice(0, 4),
    [meetings],
  );
  const openMeetingFollowUps = useMemo(
    () => meetings.reduce((sum, meeting) => sum + countOpenMeetingIssues(meeting), 0),
    [meetings],
  );

  return (
    <div
      className="mx-auto h-full w-full max-w-6xl min-w-0 space-y-5 overflow-x-hidden overflow-y-auto p-4 pb-40 sm:p-6"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <DashboardHeroHeader companyName={settings.companyName} t={t} />

      <SurfaceSection
        eyebrow={t({ ko: "Dashboard Layout", en: "Dashboard Layout", ja: "Dashboard Layout", zh: "Dashboard Layout" })}
        title={t({ ko: "운영 / 토큰 / 자동화 / 업적 / 회의", en: "Operations / Tokens / Automation / Achievements / Meetings", ja: "運用 / トークン / 自動化 / 実績 / 会議", zh: "运营 / Token / 自动化 / 成就 / 会议" })}
        description={t({
          ko: "단일 장문 스크롤 대신 탭 전환으로 필요한 표면만 집중해서 봅니다.",
          en: "Switch surfaces by tab instead of scrolling one long page.",
          ja: "長い単一ページではなくタブ切り替えで必要な面だけに集中します。",
          zh: "用标签切换代替超长滚动页面，只看当前需要的面板。",
        })}
        className="rounded-[28px] p-4 sm:p-5"
        style={{
          borderColor: "color-mix(in srgb, var(--th-accent-info) 18%, var(--th-border) 82%)",
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-info) 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
        }}
      >
        <div className="mt-4 flex flex-wrap gap-2">
          <DashboardTabButton
            active={activeTab === "operations"}
            label={t({ ko: "운영", en: "Operations", ja: "運用", zh: "运营" })}
            detail={t({ ko: "HEALTH + 프로바이더 상태", en: "HEALTH + provider status", ja: "HEALTH + provider 状態", zh: "HEALTH + provider 状态" })}
            onClick={() => setActiveTab("operations")}
          />
          <DashboardTabButton
            active={activeTab === "tokens"}
            label={t({ ko: "토큰", en: "Tokens", ja: "トークン", zh: "Token" })}
            detail={t({ ko: "히트맵 + 비용 + ROI", en: "Heatmap + spend + ROI", ja: "ヒートマップ + コスト + ROI", zh: "热力图 + 成本 + ROI" })}
            onClick={() => setActiveTab("tokens")}
          />
          <DashboardTabButton
            active={activeTab === "automation"}
            label={t({ ko: "자동화", en: "Automation", ja: "自動化", zh: "自动化" })}
            detail={t({ ko: "크론 + 스킬 허브", en: "Cron + skill hub", ja: "Cron + スキルハブ", zh: "Cron + 技能中心" })}
            onClick={() => setActiveTab("automation")}
          />
          <DashboardTabButton
            active={activeTab === "achievements"}
            label={t({ ko: "업적", en: "Achievements", ja: "実績", zh: "成就" })}
            detail={t({ ko: "랭킹 + 업적", en: "Ranking + achievements", ja: "ランキング + 実績", zh: "排行 + 成就" })}
            onClick={() => setActiveTab("achievements")}
          />
          <DashboardTabButton
            active={activeTab === "meetings"}
            label={t({ ko: "회의", en: "Meetings", ja: "会議", zh: "会议" })}
            detail={t({ ko: "기록 + 후속 일감", en: "Records + follow-ups", ja: "記録 + フォローアップ", zh: "记录 + 后续事项" })}
            onClick={() => setActiveTab("meetings")}
          />
        </div>
      </SurfaceSection>

      {activeTab === "operations" && (
        <div className="space-y-5">
          <DashboardHudStats hudStats={hudStats} numberFormatter={numberFormatter} />
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
                  value={stats.kanban.review_queue}
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
                  value={stats.kanban.blocked}
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
                  value={stats.kanban.waiting_acceptance}
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
                  value={stats.kanban.stale_in_progress}
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
                  value={openMeetingFollowUps}
                  accent="#22c55e"
                  sublabel={t({
                    ko: `${activeMeetings.length} active / ${meetings.length} total`,
                    en: `${activeMeetings.length} active / ${meetings.length} total`,
                    ja: `${activeMeetings.length} active / ${meetings.length} total`,
                    zh: `${activeMeetings.length} active / ${meetings.length} total`,
                  })}
                  actionLabel={t({ ko: "회의록 열기", en: "Open Meetings", ja: "会議録を開く", zh: "打开会议记录" })}
                  onAction={() => setActiveTab("meetings")}
                />
              </div>
            </SurfaceSubsection>

            <MeetingTimelineCard
              meetings={recentMeetings}
              activeCount={activeMeetings.length}
              followUpCount={openMeetingFollowUps}
              localeTag={localeTag}
              t={t}
              onOpenMeetings={() => setActiveTab("meetings")}
            />
          </div>
          <div className="grid gap-4 xl:grid-cols-[minmax(0,1.1fr)_minmax(0,0.9fr)]">
            <HealthWidget t={t} />
            <RateLimitWidget t={t} onOpenSettings={onOpenSettings} />
          </div>
          <BottleneckWidget t={t} />
        </div>
      )}

      {activeTab === "tokens" && (
        <div className="space-y-5">
          <TokenAnalyticsSection
            agents={agents}
            t={t}
            numberFormatter={numberFormatter}
          />
        </div>
      )}

      {activeTab === "automation" && (
        <div className="space-y-5">
          <PulseSectionShell
            eyebrow={t({ ko: "Automation", en: "Automation", ja: "Automation", zh: "Automation" })}
            title={t({ ko: "자동화 / 스킬", en: "Automation / Skills", ja: "自動化 / スキル", zh: "自动化 / 技能" })}
            subtitle={t({
              ko: "크론 실행 흐름과 스킬 호출 지형을 분리된 섹션으로 유지합니다.",
              en: "Keep cron execution flow and skill usage surfaces together.",
              ja: "Cron 実行フローとスキル利用面をまとめて保持します。",
              zh: "把 cron 执行流与技能使用面放在一起查看。",
            })}
            badge={t({ ko: "Automation", en: "Automation", ja: "Automation", zh: "Automation" })}
            style={{
              borderColor: "color-mix(in srgb, var(--th-accent-warn) 20%, var(--th-border) 80%)",
              background:
                "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-warn) 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
            }}
          >
            <CronTimelineWidget t={t} />

            <AutoQueueHistoryWidget t={t} />

            <SkillRankingSection
              skillRanking={skillRanking}
              skillWindow={skillWindow}
              onChangeWindow={setSkillWindow}
              numberFormatter={numberFormatter}
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
        </div>
      )}

      {activeTab === "achievements" && (
        <div className="space-y-5">
          <PulseSectionShell
            eyebrow={t({ ko: "Achievement", en: "Achievement", ja: "Achievement", zh: "Achievement" })}
            title={t({ ko: "업적 / XP", en: "Achievements / XP", ja: "実績 / XP", zh: "成就 / XP" })}
            subtitle={t({
              ko: "랭킹과 실업적만 남기고 보상성 잡음을 제거했습니다.",
              en: "Keep only ranking and concrete achievements while removing ornamental reward noise.",
              ja: "ランキングと実績だけを残し、装飾的な報酬ノイズを取り除きました。",
              zh: "只保留排行与真实成就，去掉装饰性奖励噪音。",
            })}
            badge={t({ ko: "Focused", en: "Focused", ja: "Focused", zh: "Focused" })}
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
        </div>
      )}

      {activeTab === "meetings" && (
        <div className="space-y-5">
          <PulseSectionShell
            eyebrow={t({ ko: "Meetings", en: "Meetings", ja: "Meetings", zh: "Meetings" })}
            title={t({ ko: "회의 기록 / 후속 일감", en: "Meeting Records / Follow-ups", ja: "会議記録 / フォローアップ", zh: "会议记录 / 后续事项" })}
            subtitle={t({
              ko: "라운드 테이블 결과와 후속 이슈 정리를 대시보드 안에서 바로 이어서 처리합니다.",
              en: "Continue round-table review and follow-up issue cleanup directly from the dashboard.",
              ja: "ラウンドテーブル結果と後続イシュー整理をダッシュボード内で続けて処理します。",
              zh: "在仪表盘内直接继续处理圆桌结果与后续 issue 整理。",
            })}
            badge={t({ ko: `${meetings.length}개 기록`, en: `${meetings.length} records`, ja: `${meetings.length}件`, zh: `${meetings.length} 条` })}
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
        </div>
      )}
    </div>
  );
}

function PulseSectionShell({
  eyebrow,
  title,
  subtitle,
  badge,
  style,
  children,
}: {
  eyebrow: string;
  title: string;
  subtitle: string;
  badge: string;
  style?: CSSProperties;
  children: ReactNode;
}) {
  return (
    <SurfaceSection
      eyebrow={eyebrow}
      title={title}
      description={subtitle}
      badge={badge}
      className="rounded-[28px] p-4 sm:p-5"
      style={style ?? {
        borderColor: "color-mix(in srgb, var(--th-border) 82%, transparent)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 97%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 99%, transparent) 100%)",
      }}
    >
      <div className="mt-4 space-y-4">{children}</div>
    </SurfaceSection>
  );
}

function countOpenMeetingIssues(meeting: RoundTableMeeting): number {
  const totalIssues = meeting.proposed_issues?.length ?? 0;
  if (meeting.status !== "completed" || totalIssues === 0) return 0;

  const results = meeting.issue_creation_results ?? [];
  if (results.length === 0) {
    return Math.max(totalIssues - meeting.issues_created, 0);
  }

  const created = results.filter((result) => result.ok && result.discarded !== true).length;
  const discarded = results.filter((result) => result.discarded === true).length;
  return Math.max(totalIssues - created - discarded, 0);
}

function PulseSignalCard({
  label,
  value,
  sublabel,
  accent,
  actionLabel,
  onAction,
}: {
  label: string;
  value: number;
  sublabel: string;
  accent: string;
  actionLabel: string;
  onAction?: () => void;
}) {
  return (
    <SurfaceCard
      className="min-w-0 rounded-2xl p-4"
      style={{
        borderColor: `color-mix(in srgb, ${accent} 24%, var(--th-border) 76%)`,
        background: `linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 93%, ${accent} 7%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)`,
      }}
    >
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div className="min-w-0 flex-1">
          <div className="text-[11px] font-semibold uppercase tracking-[0.14em]" style={{ color: accent }}>
            {label}
          </div>
          <div className="mt-2 text-3xl font-black tracking-tight" style={{ color: "var(--th-text-heading)" }}>
            {value}
          </div>
          <p className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
            {sublabel}
          </p>
        </div>
        {onAction ? (
          <SurfaceActionButton
            onClick={onAction}
            className="w-full shrink-0 sm:w-auto"
            style={{
              color: accent,
              border: `1px solid color-mix(in srgb, ${accent} 28%, var(--th-border) 72%)`,
              background: `color-mix(in srgb, ${accent} 14%, var(--th-card-bg) 86%)`,
            }}
          >
            {actionLabel}
          </SurfaceActionButton>
        ) : null}
      </div>
    </SurfaceCard>
  );
}

function MeetingTimelineCard({
  meetings,
  activeCount,
  followUpCount,
  localeTag,
  t,
  onOpenMeetings,
}: {
  meetings: RoundTableMeeting[];
  activeCount: number;
  followUpCount: number;
  localeTag: string;
  t: TFunction;
  onOpenMeetings?: () => void;
}) {
  const formatter = useMemo(
    () => new Intl.DateTimeFormat(localeTag, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }),
    [localeTag],
  );

  const getMeetingStatusLabel = useCallback(
    (status: RoundTableMeeting["status"]) =>
      t({
        ko: status === "in_progress" ? "진행 중" : status === "completed" ? "완료" : "초안",
        en: status === "in_progress" ? "In Progress" : status === "completed" ? "Completed" : "Draft",
        ja: status === "in_progress" ? "進行中" : status === "completed" ? "完了" : "下書き",
        zh: status === "in_progress" ? "进行中" : status === "completed" ? "已完成" : "草稿",
      }),
    [t],
  );

  return (
    <SurfaceSubsection
      title={t({ ko: "회의 타임라인", en: "Meeting Timeline", ja: "会議タイムライン", zh: "会议时间线" })}
      description={t({
        ko: `${activeCount}개 진행 중, 후속 이슈 ${followUpCount}개 미정리`,
        en: `${activeCount} active, ${followUpCount} follow-up issues still open`,
        ja: `${activeCount}件進行中、後続イシュー ${followUpCount}件 未整理`,
        zh: `${activeCount} 个进行中，${followUpCount} 个后续 issue 未整理`,
      })}
      actions={onOpenMeetings ? (
        <SurfaceActionButton tone="success" onClick={onOpenMeetings}>
          {t({ ko: "회의록 열기", en: "Open Meetings", ja: "会議録を開く", zh: "打开会议记录" })}
        </SurfaceActionButton>
      ) : undefined}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-primary) 24%, var(--th-border) 76%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-primary) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      <div className="space-y-2">
        {meetings.length === 0 ? (
          <SurfaceEmptyState className="px-4 py-6 text-center text-sm">
            {t({ ko: "최근 회의가 없습니다.", en: "No recent meetings yet.", ja: "最近の会議はありません。", zh: "暂无最近会议。" })}
          </SurfaceEmptyState>
        ) : (
          meetings.map((meeting) => {
            const statusTone = meeting.status === "in_progress" ? "success" : meeting.status === "completed" ? "info" : "neutral";
            const issueCount = countOpenMeetingIssues(meeting);
            return (
              <SurfaceListItem
                key={meeting.id}
                tone={statusTone}
                trailing={(
                  <div className="text-right">
                    <div className="text-xs font-semibold" style={{ color: "var(--th-text-heading)" }}>
                      {meeting.primary_provider || meeting.reviewer_provider
                        ? formatProviderFlow(meeting.primary_provider, meeting.reviewer_provider)
                        : "RT"}
                    </div>
                    <div className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                      {t({
                        ko: `${meeting.issues_created}개 생성`,
                        en: `${meeting.issues_created} created`,
                        ja: `${meeting.issues_created}件 作成`,
                        zh: `已创建 ${meeting.issues_created} 个`,
                      })}
                    </div>
                  </div>
                )}
              >
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-2">
                    <SurfaceMetaBadge tone={statusTone}>{getMeetingStatusLabel(meeting.status)}</SurfaceMetaBadge>
                    <span className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                      {formatter.format(meeting.started_at || meeting.created_at)}
                    </span>
                  </div>
                  <div className="mt-1 truncate font-medium" style={{ color: "var(--th-text)" }}>
                    {meeting.agenda}
                  </div>
                  <div className="mt-2 flex flex-wrap gap-2 text-[11px]">
                    <SurfaceMetaBadge>
                      {meeting.participant_names.length} {t({ ko: "참여자", en: "participants", ja: "参加者", zh: "参与者" })}
                    </SurfaceMetaBadge>
                    <SurfaceMetaBadge>
                      {meeting.total_rounds} {t({ ko: "라운드", en: "rounds", ja: "ラウンド", zh: "轮" })}
                    </SurfaceMetaBadge>
                    {issueCount > 0 ? (
                      <SurfaceMetaBadge tone="warn">
                        {issueCount} {t({ ko: "후속 대기", en: "follow-up pending", ja: "後続待ち", zh: "后续待处理" })}
                      </SurfaceMetaBadge>
                    ) : null}
                  </div>
                </div>
              </SurfaceListItem>
            );
          })
        )}
      </div>
    </SurfaceSubsection>
  );
}

function SkillRankingSection({
  skillRanking,
  skillWindow,
  onChangeWindow,
  numberFormatter,
  t,
}: {
  skillRanking: SkillRankingResponse | null;
  skillWindow: "7d" | "30d" | "all";
  onChangeWindow: (value: "7d" | "30d" | "all") => void;
  numberFormatter: Intl.NumberFormat;
  t: TFunction;
}) {
  return (
    <SurfaceSubsection
      title={t({ ko: "스킬 랭킹", en: "Skill Ranking", ja: "スキルランキング", zh: "技能排行" })}
      description={t({
        ko: "호출량 기준 상위 스킬과 에이전트를 같은 문법으로 정리합니다.",
        en: "Top skills and agents by call volume in the same grammar.",
        ja: "呼び出し量ベースの上位スキルとエージェントを同じ文法で整理します。",
        zh: "用统一语法整理按调用量统计的技能与代理排行。",
      })}
      actions={(
        <>
          {(["all", "30d", "7d"] as const).map((windowId) => (
            <SurfaceSegmentButton
              key={windowId}
              onClick={() => onChangeWindow(windowId)}
              active={skillWindow === windowId}
              tone="warn"
            >
              {windowId}
            </SurfaceSegmentButton>
          ))}
        </>
      )}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-warn) 24%, var(--th-border) 76%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-warn) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      {!skillRanking || skillRanking.overall.length === 0 ? (
        <SurfaceEmptyState className="mt-4 px-4 py-6 text-center text-sm">
          {t({ ko: "아직 집계된 스킬 호출이 없습니다.", en: "No skill usage aggregated yet.", ja: "まだ集計されたスキル呼び出しがありません。", zh: "尚无技能调用统计。" })}
        </SurfaceEmptyState>
      ) : (
        <div className="mt-4 grid gap-4 xl:grid-cols-2">
          <SkillRankingList
            title={t({ ko: "전체 TOP 5", en: "Overall TOP 5", ja: "全体 TOP 5", zh: "全体 TOP 5" })}
            emptyLabel={t({ ko: "표시할 스킬이 없습니다.", en: "No skills to show.", ja: "表示するスキルがありません。", zh: "没有可显示的技能。" })}
            t={t}
            items={skillRanking.overall.slice(0, 5).map((row, index) => ({
              id: `${row.skill_name}-${index}`,
              leading: `${index + 1}.`,
              title: row.skill_desc_ko,
              tooltip: row.skill_name,
              trailing: numberFormatter.format(row.calls),
            }))}
          />
          <SkillRankingList
            title={t({ ko: "에이전트별 TOP 5", en: "Top by Agent", ja: "エージェント別 TOP 5", zh: "按代理 TOP 5" })}
            emptyLabel={t({ ko: "표시할 에이전트 호출이 없습니다.", en: "No agent calls to show.", ja: "表示するエージェント呼び出しがありません。", zh: "没有可显示的代理调用。" })}
            t={t}
            items={skillRanking.byAgent.slice(0, 5).map((row, index) => ({
              id: `${row.agent_role_id}-${row.skill_name}-${index}`,
              leading: `${index + 1}.`,
              title: `${row.agent_name} · ${row.skill_desc_ko}`,
              tooltip: row.skill_name,
              trailing: numberFormatter.format(row.calls),
            }))}
          />
        </div>
      )}
    </SurfaceSubsection>
  );
}

function SkillRankingList({
  title,
  emptyLabel,
  items,
  t,
}: {
  title: string;
  emptyLabel: string;
  items: Array<{
    id: string;
    leading: string;
    title: string;
    tooltip: string;
    trailing: string;
  }>;
  t: TFunction;
}) {
  return (
    <div className="min-w-0">
      <div className="mb-2 text-sm font-medium" style={{ color: "var(--th-text-muted)" }}>
        {title}
      </div>
      {items.length === 0 ? (
        <SurfaceEmptyState className="px-4 py-6 text-center text-sm">
          {emptyLabel}
        </SurfaceEmptyState>
      ) : (
        <ul className="space-y-2">
          {items.map((item) => (
            <li key={item.id}>
              <SurfaceListItem
                tone="warn"
                trailing={(
                  <span className="text-sm font-semibold" style={{ color: "var(--th-accent-warn)" }}>
                    {item.trailing}
                  </span>
                )}
              >
                <div className="min-w-0 flex flex-1 items-start gap-2 text-sm" style={{ color: "var(--th-text)" }}>
                  <span className="inline-flex w-6 shrink-0" style={{ color: "var(--th-text-muted)" }}>
                    {item.leading}
                  </span>
                  <TooltipLabel text={item.title} tooltip={item.tooltip} className="flex-1" />
                </div>
              </SurfaceListItem>
            </li>
          ))}
        </ul>
      )}
      <div className="mt-2 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
        {t({ ko: "집계 창을 바꾸면 같은 카드 안에서 즉시 다시 계산됩니다.", en: "Changing the window recalculates in place.", ja: "ウィンドウを変えると同じカード内で再計算されます。", zh: "切换窗口后会在同一卡片内重新计算。" })}
      </div>
    </div>
  );
}

function DashboardTabButton({
  active,
  label,
  detail,
  onClick,
}: {
  active: boolean;
  label: string;
  detail: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="min-w-[10rem] flex-1 rounded-2xl border px-4 py-3 text-left transition-colors sm:flex-none"
      style={{
        borderColor: active
          ? "color-mix(in srgb, var(--th-accent-primary) 32%, var(--th-border) 68%)"
          : "rgba(148,163,184,0.16)",
        background: active
          ? "color-mix(in srgb, var(--th-accent-primary-soft) 74%, transparent)"
          : "color-mix(in srgb, var(--th-card-bg) 94%, transparent)",
      }}
    >
      <div className="text-sm font-semibold" style={{ color: active ? "var(--th-text-heading)" : "var(--th-text)" }}>
        {label}
      </div>
      <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
        {detail}
      </div>
    </button>
  );
}
