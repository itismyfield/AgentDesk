import { lazy, Suspense, useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
import { getSkillRanking, type SkillRankingResponse } from "../api";
import type {
  Agent,
  CompanySettings,
  DashboardStats,
  DispatchedSession,
  RoundTableMeeting,
} from "../types";
import {
  SurfaceEmptyState,
  SurfaceListItem,
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

const SkillCatalogView = lazy(() => import("./SkillCatalogView"));
const MeetingMinutesView = lazy(() => import("./MeetingMinutesView"));

type PulseKanbanSignal = "review" | "blocked" | "requested" | "stalled";
type DashboardTab = "operations" | "tokens" | "automation" | "achievements" | "meetings";

const DASHBOARD_TAB_QUERY_KEY = "dashboardTab";
const DASHBOARD_TAB_STORAGE_KEY = "agentdesk.dashboard.active-tab";
const DASHBOARD_TABS: DashboardTab[] = ["operations", "tokens", "automation", "achievements", "meetings"];

function readDashboardTabFromUrl(): DashboardTab {
  if (typeof window === "undefined") return "operations";
  const params = new URLSearchParams(window.location.search);
  const value = params.get(DASHBOARD_TAB_QUERY_KEY);
  if (DASHBOARD_TABS.includes(value as DashboardTab)) return value as DashboardTab;
  const stored = window.localStorage.getItem(DASHBOARD_TAB_STORAGE_KEY);
  return DASHBOARD_TABS.includes(stored as DashboardTab) ? (stored as DashboardTab) : "operations";
}

function syncDashboardTabToUrl(tab: DashboardTab) {
  if (typeof window === "undefined") return;
  const url = new URL(window.location.href);
  url.searchParams.set(DASHBOARD_TAB_QUERY_KEY, tab);
  window.localStorage.setItem(DASHBOARD_TAB_STORAGE_KEY, tab);
  window.history.replaceState(null, "", `${url.pathname}${url.search}${url.hash}`);
}

interface DashboardPageViewProps {
  stats: DashboardStats | null;
  agents: Agent[];
  sessions: DispatchedSession[];
  meetings: RoundTableMeeting[];
  settings: CompanySettings;
  onSelectAgent?: (agent: Agent) => void;
  onOpenKanbanSignal?: (signal: PulseKanbanSignal) => void;
  onOpenDispatchSessions?: () => void;
  onOpenSettings?: () => void;
  onRefreshMeetings?: () => void;
}

export default function DashboardPageView({
  stats,
  agents,
  meetings,
  settings,
  onSelectAgent,
  onOpenSettings,
  onRefreshMeetings,
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
    const handlePopState = () => setActiveTab(readDashboardTabFromUrl());
    window.addEventListener("popstate", handlePopState);
    return () => window.removeEventListener("popstate", handlePopState);
  }, []);

  useEffect(() => {
    let mounted = true;

    const load = async () => {
      try {
        const next = await getSkillRanking(skillWindow, 10);
        if (mounted) setSkillRanking(next);
      } catch {
        if (mounted) setSkillRanking(null);
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
      id: "open-cards",
      label: t({ ko: "열린 카드", en: "Open Cards", ja: "オープンカード", zh: "开放卡片" }),
      value: stats.kanban.open_total,
      sub: t({ ko: "칸반 총량", en: "Kanban load", ja: "カンバン総量", zh: "看板总量" }),
      color: "#f59e0b",
      icon: "📋",
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
  children,
}: {
  eyebrow: string;
  title: string;
  subtitle: string;
  badge: string;
  children: ReactNode;
}) {
  return (
    <SurfaceSection
      eyebrow={eyebrow}
      title={title}
      description={subtitle}
      badge={badge}
      className="rounded-[28px] p-4 sm:p-5"
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 82%, transparent)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 97%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 99%, transparent) 100%)",
      }}
    >
      <div className="mt-4 space-y-4">{children}</div>
    </SurfaceSection>
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
