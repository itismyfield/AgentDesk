import { lazy, Suspense, type ReactNode, useCallback, useEffect, useMemo, useState } from "react";
import { getSkillRanking, type SkillRankingResponse } from "../api";
import type {
  Agent,
  CompanySettings,
  DashboardStats,
  DispatchedSession,
  RoundTableMeeting,
} from "../types";
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
  CronTimelineWidget,
  MvpWidget,
  SkillTrendWidget,
} from "./dashboard/ExtraWidgets";
import HealthWidget from "./dashboard/HealthWidget";
import { type TFunction } from "./dashboard/model";
import RateLimitWidget from "./dashboard/RateLimitWidget";
import TokenAnalyticsSection from "./dashboard/TokenAnalyticsSection";

const SkillCatalogView = lazy(() => import("./SkillCatalogView"));

type DashboardTab = "operations" | "tokens" | "automation" | "achievements";
type DashboardKanbanSignal = "review" | "blocked" | "requested" | "stalled";
const DASHBOARD_TAB_QUERY_KEY = "dashboardTab";

function parseDashboardTab(value: string | null): DashboardTab {
  switch (value) {
    case "tokens":
    case "automation":
    case "achievements":
      return value;
    case "operations":
    default:
      return "operations";
  }
}

function readDashboardTabFromUrl(): DashboardTab {
  if (typeof window === "undefined") return "operations";

  const url = new URL(window.location.href);
  const fromQuery = url.searchParams.get(DASHBOARD_TAB_QUERY_KEY);
  if (fromQuery) return parseDashboardTab(fromQuery);

  const fromHash = new URLSearchParams(url.hash.replace(/^#/, "")).get(DASHBOARD_TAB_QUERY_KEY);
  return parseDashboardTab(fromHash);
}

function writeDashboardTabToUrl(tab: DashboardTab) {
  if (typeof window === "undefined") return;

  const url = new URL(window.location.href);
  url.searchParams.set(DASHBOARD_TAB_QUERY_KEY, tab);
  window.history.replaceState(window.history.state, "", `${url.pathname}${url.search}${url.hash}`);
}

interface DashboardPageViewProps {
  stats: DashboardStats | null;
  agents: Agent[];
  sessions: DispatchedSession[];
  meetings: RoundTableMeeting[];
  settings: CompanySettings;
  onSelectAgent?: (agent: Agent) => void;
  onOpenKanbanSignal?: (signal: DashboardKanbanSignal) => void;
  onOpenDispatchSessions?: () => void;
  onOpenSettings?: () => void;
  onOpenMeetings?: () => void;
}

export default function DashboardPageView({
  stats,
  agents,
  settings,
  onSelectAgent,
  onOpenSettings,
}: DashboardPageViewProps) {
  const language = settings.language;
  const localeTag = language === "ko" ? "ko-KR" : language === "ja" ? "ja-JP" : language === "zh" ? "zh-CN" : "en-US";
  const numberFormatter = useMemo(() => new Intl.NumberFormat(localeTag), [localeTag]);

  const t: TFunction = useCallback(
    (messages) => messages[language] ?? messages.ko,
    [language],
  );

  const [skillRanking, setSkillRanking] = useState<SkillRankingResponse | null>(null);
  const [skillWindow, setSkillWindow] = useState<"7d" | "30d" | "all">("7d");
  const [activeTab, setActiveTab] = useState<DashboardTab>(() => readDashboardTabFromUrl());

  useEffect(() => {
    let mounted = true;
    const load = async () => {
      try {
        const data = await getSkillRanking(skillWindow, 10);
        if (mounted) setSkillRanking(data);
      } catch {
        // Ignore auth/network errors in dashboard widgets.
      }
    };

    void load();
    const timer = setInterval(() => void load(), 60_000);
    return () => {
      mounted = false;
      clearInterval(timer);
    };
  }, [skillWindow]);

  useEffect(() => {
    writeDashboardTabToUrl(activeTab);
  }, [activeTab]);

  useEffect(() => {
    const syncFromUrl = () => setActiveTab(readDashboardTabFromUrl());
    window.addEventListener("popstate", syncFromUrl);
    window.addEventListener("hashchange", syncFromUrl);
    return () => {
      window.removeEventListener("popstate", syncFromUrl);
      window.removeEventListener("hashchange", syncFromUrl);
    };
  }, []);

  if (!stats) {
    return (
      <div className="flex items-center justify-center h-full" style={{ color: "var(--th-text-muted)" }}>
        <div className="text-center">
          <div className="text-4xl mb-4 opacity-30">📊</div>
          <div>{t({ ko: "대시보드 로딩 중...", en: "Loading dashboard...", ja: "ダッシュボード読み込み中...", zh: "仪表板加载中..." })}</div>
        </div>
      </div>
    );
  }

  const hudStats: HudStat[] = [
    {
      id: "total",
      label: t({ ko: "전체 직원", en: "Total Agents", ja: "全エージェント", zh: "全部代理" }),
      value: stats.agents.total,
      sub: t({ ko: "등록된 에이전트", en: "Registered agents", ja: "登録エージェント", zh: "已注册代理" }),
      color: "#60a5fa",
      icon: "👥",
    },
    {
      id: "working",
      label: t({ ko: "근무 중", en: "Working", ja: "作業中", zh: "工作中" }),
      value: stats.agents.working,
      sub: t({ ko: "실시간 활동", en: "Active now", ja: "リアルタイム活動", zh: "当前活跃" }),
      color: "#34d399",
      icon: "💼",
    },
    {
      id: "idle",
      label: t({ ko: "대기", en: "Idle", ja: "待機", zh: "空闲" }),
      value: stats.agents.idle,
      sub: t({ ko: "배치 대기", en: "Awaiting assignment", ja: "配置待ち", zh: "等待分配" }),
      color: "#94a3b8",
      icon: "⏸️",
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
  const tabs: Array<{ id: DashboardTab; label: string; summary: string }> = [
    {
      id: "operations",
      label: t({ ko: "운영", en: "Operations", ja: "運用", zh: "运营" }),
      summary: t({
        ko: "요약 카드, Health, Weather",
        en: "Summary cards, health, weather",
        ja: "要約カード、Health、Weather",
        zh: "摘要卡、Health、Weather",
      }),
    },
    {
      id: "tokens",
      label: t({ ko: "토큰", en: "Tokens", ja: "トークン", zh: "Token" }),
      summary: t({
        ko: "히트맵, 비용, 모델, 추이",
        en: "Heatmap, spend, models, trend",
        ja: "ヒートマップ、コスト、モデル、推移",
        zh: "热力图、成本、模型、趋势",
      }),
    },
    {
      id: "automation",
      label: t({ ko: "자동화", en: "Automation", ja: "自動化", zh: "自动化" }),
      summary: t({
        ko: "크론 타임라인, 스킬 허브",
        en: "Cron timeline, skill hub",
        ja: "Cron タイムライン、スキルハブ",
        zh: "Cron 时间线、技能中心",
      }),
    },
    {
      id: "achievements",
      label: t({ ko: "업적", en: "Achievements", ja: "業績", zh: "成就" }),
      summary: t({
        ko: "랭킹, 업적, MVP",
        en: "Ranking, achievements, MVP",
        ja: "ランキング、業績、MVP",
        zh: "排行、成就、MVP",
      }),
    },
  ];

  return (
    <div
      className="mx-auto h-full max-w-6xl min-w-0 space-y-5 overflow-x-hidden overflow-y-auto p-4 pb-40 sm:p-6"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <DashboardHeroHeader
        companyName={settings.companyName}
        t={t}
      />

      <DashboardTabBar tabs={tabs} activeTab={activeTab} onChange={setActiveTab} />

      <DashboardTabPanel active={activeTab === "operations"}>
        <section className="space-y-4">
          <SectionHeader
            title={t({ ko: "운영", en: "Operations", ja: "運用", zh: "运营" })}
            subtitle={t({
              ko: "상단 요약 카드와 런타임 상태만 빠르게 확인합니다",
              en: "Focus on summary cards and live runtime status only",
              ja: "上部サマリーカードとランタイム状態だけを素早く確認します",
              zh: "只快速查看摘要卡与运行时状态",
            })}
          />
          <DashboardHudStats hudStats={hudStats} numberFormatter={numberFormatter} />
          <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
            <HealthWidget t={t} />
            <RateLimitWidget t={t} onOpenSettings={onOpenSettings} />
          </div>
        </section>
      </DashboardTabPanel>

      <DashboardTabPanel active={activeTab === "tokens"}>
        <TokenAnalyticsSection
          t={t}
          numberFormatter={numberFormatter}
        />
      </DashboardTabPanel>

      <DashboardTabPanel active={activeTab === "automation"}>
        <section className="space-y-4">
          <SectionHeader
            title={t({ ko: "자동화", en: "Automation", ja: "自動化", zh: "自动化" })}
            subtitle={t({
              ko: "스케줄 실행 흐름과 스킬 호출 허브를 함께 봅니다",
              en: "Keep scheduled runs and the skill hub together",
              ja: "スケジュール実行フローとスキルハブをまとめて確認します",
              zh: "将定时执行流与技能中心放在一起查看",
            })}
          />
          <CronTimelineWidget t={t} />

          <section className="space-y-4">
            <SectionHeader
              title={t({ ko: "스킬 허브", en: "Skill Hub", ja: "スキルハブ", zh: "技能中心" })}
              subtitle={t({
                ko: "호출 랭킹, 추세, 카탈로그를 한 곳에 모읍니다",
                en: "Keep skill ranking, trends, and catalog together in one place",
                ja: "呼び出しランキング、推移、カタログを一か所に集約",
                zh: "将技能排行、趋势和目录集中到一处",
              })}
            />

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
                  {t({ ko: "카탈로그 로딩 중...", en: "Loading catalog...", ja: "カタログ読み込み中...", zh: "加载目录中..." })}
                </div>
              )}
            >
              <SkillCatalogView embedded />
            </Suspense>
          </section>
        </section>
      </DashboardTabPanel>

      <DashboardTabPanel active={activeTab === "achievements"}>
        <section className="space-y-4">
          <SectionHeader
            title={t({ ko: "업적", en: "Achievements", ja: "業績", zh: "成就" })}
            subtitle={t({
              ko: "핵심 랭킹과 남겨둘 가치가 있는 보상 정보만 표시합니다",
              en: "Keep only the core ranking and reward signals worth surfacing",
              ja: "残す価値のある主要ランキングと報酬情報だけを表示します",
              zh: "仅保留值得展示的核心排行与奖励信息",
            })}
          />

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

          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
            <AchievementWidget t={t} />
            <MvpWidget agents={agents} t={t} isKo={language === "ko"} />
          </div>
        </section>
      </DashboardTabPanel>
    </div>
  );
}

function DashboardTabBar({
  tabs,
  activeTab,
  onChange,
}: {
  tabs: Array<{ id: DashboardTab; label: string; summary: string }>;
  activeTab: DashboardTab;
  onChange: (tab: DashboardTab) => void;
}) {
  return (
    <div className="game-panel p-2">
      <div className="grid grid-cols-2 gap-2 xl:grid-cols-4">
        {tabs.map((tab) => {
          const active = tab.id === activeTab;
          return (
            <button
              key={tab.id}
              type="button"
              onClick={() => onChange(tab.id)}
              className="rounded-2xl px-3 py-3 text-left transition-all"
              style={{
                background: active ? "linear-gradient(135deg, rgba(245,158,11,0.22), rgba(59,130,246,0.18))" : "rgba(15,23,42,0.18)",
                border: active ? "1px solid rgba(251,191,36,0.28)" : "1px solid rgba(148,163,184,0.14)",
                boxShadow: active ? "0 0 0 1px rgba(245,158,11,0.12) inset" : "none",
              }}
            >
              <div
                className="text-sm font-semibold"
                style={{ color: active ? "var(--th-text-heading)" : "var(--th-text)" }}
              >
                {tab.label}
              </div>
              <div className="mt-1 text-[11px] leading-4" style={{ color: "var(--th-text-muted)" }}>
                {tab.summary}
              </div>
            </button>
          );
        })}
      </div>
    </div>
  );
}

function DashboardTabPanel({ active, children }: { active: boolean; children: ReactNode }) {
  return (
    <div hidden={!active} aria-hidden={!active}>
      {children}
    </div>
  );
}

function SectionHeader({ title, subtitle }: { title: string; subtitle: string }) {
  return (
    <div className="flex items-end justify-between gap-3">
      <div className="min-w-0">
        <h2 className="text-lg font-semibold" style={{ color: "var(--th-text-heading)" }}>
          {title}
        </h2>
        <p className="text-sm" style={{ color: "var(--th-text-muted)" }}>
          {subtitle}
        </p>
      </div>
    </div>
  );
}

function SkillRankingSnapshot({
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
    <section
      className="rounded-2xl border p-4 sm:p-5"
      style={{
        borderColor: "var(--th-border)",
        background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 92%, #f59e0b 8%), var(--th-surface))",
      }}
    >
      <div className="flex min-w-0 flex-wrap items-center justify-between gap-3">
        <div className="min-w-0 flex-1">
          <h3 className="text-base font-semibold" style={{ color: "var(--th-text-heading)" }}>
            {t({ ko: "스킬 랭킹", en: "Skill Ranking", ja: "スキルランキング", zh: "技能排行" })}
          </h3>
          <p className="text-sm" style={{ color: "var(--th-text-muted)" }}>
            {t({ ko: "호출량 기준 상위 스킬과 에이전트", en: "Top skills and agents by call volume", ja: "呼び出し量ベースの上位スキルとエージェント", zh: "按调用量统计的热门技能与代理" })}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          {(["7d", "30d", "all"] as const).map((windowId) => (
            <button
              key={windowId}
              type="button"
              onClick={() => onChangeWindow(windowId)}
              className="text-[11px] px-2 py-1 rounded-md border"
              style={{
                borderColor: skillWindow === windowId ? "#f59e0b" : "var(--th-border)",
                color: skillWindow === windowId ? "#f59e0b" : "var(--th-text-muted)",
                background: skillWindow === windowId ? "rgba(245,158,11,0.12)" : "transparent",
              }}
            >
              {windowId}
            </button>
          ))}
        </div>
      </div>

      {!skillRanking || skillRanking.overall.length === 0 ? (
        <div className="mt-4 text-sm" style={{ color: "var(--th-text-muted)" }}>
          {t({ ko: "아직 집계된 스킬 호출이 없습니다.", en: "No skill usage aggregated yet.", ja: "まだ集計されたスキル呼び出しがありません。", zh: "尚无技能调用统计。" })}
        </div>
      ) : (
        <div className="mt-4 grid min-w-0 grid-cols-1 gap-4 xl:grid-cols-2">
          <div className="min-w-0">
            <div className="text-sm font-medium mb-2" style={{ color: "var(--th-text-muted)" }}>
              {t({ ko: "전체 TOP 5", en: "Overall TOP 5", ja: "全体 TOP 5", zh: "全体 TOP 5" })}
            </div>
            <ol className="space-y-2">
              {skillRanking.overall.slice(0, 5).map((row, index) => (
                <li key={`${row.skill_name}-${index}`} className="flex items-start justify-between gap-3 text-sm">
                  <div className="min-w-0 flex flex-1 items-start gap-2" style={{ color: "var(--th-text)" }}>
                    <span className="inline-flex w-6 shrink-0" style={{ color: "var(--th-text-muted)" }}>
                      {index + 1}.
                    </span>
                    <TooltipLabel text={row.skill_desc_ko} tooltip={row.skill_name} className="flex-1" />
                  </div>
                  <span className="font-semibold shrink-0" style={{ color: "#f59e0b" }}>
                    {numberFormatter.format(row.calls)}
                  </span>
                </li>
              ))}
            </ol>
          </div>

          <div className="min-w-0">
            <div className="text-sm font-medium mb-2" style={{ color: "var(--th-text-muted)" }}>
              {t({ ko: "에이전트별 TOP 5", en: "Top by Agent", ja: "エージェント別 TOP 5", zh: "按代理 TOP 5" })}
            </div>
            <ul className="space-y-2">
              {skillRanking.byAgent.slice(0, 5).map((row, index) => (
                <li key={`${row.agent_role_id}-${row.skill_name}-${index}`} className="flex items-start justify-between gap-3 text-sm">
                  <div className="min-w-0 flex flex-1 items-start gap-2" style={{ color: "var(--th-text)" }}>
                    <span className="inline-flex w-6 shrink-0" style={{ color: "var(--th-text-muted)" }}>
                      {index + 1}.
                    </span>
                    <div className="min-w-0 flex flex-1 items-center gap-1">
                      <span className="truncate" title={row.agent_name}>
                        {row.agent_name}
                      </span>
                      <span className="shrink-0" style={{ color: "var(--th-text-muted)" }}>
                        ·
                      </span>
                      <TooltipLabel text={row.skill_desc_ko} tooltip={row.skill_name} className="flex-1" />
                    </div>
                  </div>
                  <span className="font-semibold shrink-0" style={{ color: "#f59e0b" }}>
                    {numberFormatter.format(row.calls)}
                  </span>
                </li>
              ))}
            </ul>
          </div>
        </div>
      )}
    </section>
  );
}

function SkillRankingSection(props: {
  skillRanking: SkillRankingResponse | null;
  skillWindow: "7d" | "30d" | "all";
  onChangeWindow: (value: "7d" | "30d" | "all") => void;
  numberFormatter: Intl.NumberFormat;
  t: TFunction;
}) {
  return <SkillRankingSnapshot {...props} />;
}
