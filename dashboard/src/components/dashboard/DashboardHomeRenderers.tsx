import {
  Component,
  useCallback,
  useMemo,
  type CSSProperties,
  type ErrorInfo,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
} from "react";
import { useSortable } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { GripVertical } from "lucide-react";
import type { SkillRankingResponse } from "../../api";
import { timeAgo, type TFunction } from "./model";
import {
  formatElapsedCompact,
} from "../../agent-insights";
import { countOpenMeetingIssues, summarizeMeetings } from "../../app/meetingSummary";
import type { DashboardTab } from "../../app/dashboardTabs";
import type {
  Agent,
  CompanySettings,
  DashboardStats,
  DispatchedSession,
  RoundTableMeeting,
} from "../../types";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceListItem,
  SurfaceMetaBadge,
  SurfaceSection,
  SurfaceSegmentButton,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";
import TooltipLabel from "../common/TooltipLabel";
import AgentAvatar from "../AgentAvatar";
import { DashboardRankingBoard, type RankedAgent } from "./HeroSections";
import { formatProviderFlow } from "../MeetingProviderFlow";
import type { HomeWidgetId } from "./homeWidgetOrder";

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

interface DashboardTabDefinition {
  id: DashboardTab;
  label: string;
  detail: string;
}

type HomeSignalTone = "info" | "warn" | "danger" | "success";

function dashboardTabButtonId(tab: DashboardTab): string {
  return `dashboard-tab-${tab}`;
}

function dashboardTabPanelId(tab: DashboardTab): string {
  return `dashboard-panel-${tab}`;
}

export function DashboardTabPanel({
  tab,
  activeTab,
  t,
  children,
}: {
  tab: DashboardTab;
  activeTab: DashboardTab;
  t: TFunction;
  children: ReactNode;
}) {
  if (activeTab !== tab) return null;

  return (
    <DashboardTabErrorBoundary tab={tab} t={t}>
      <div
        role="tabpanel"
        id={dashboardTabPanelId(tab)}
        aria-labelledby={dashboardTabButtonId(tab)}
        tabIndex={0}
        className="space-y-5"
      >
        {children}
      </div>
    </DashboardTabErrorBoundary>
  );
}

class DashboardTabErrorBoundary extends Component<
  { tab: DashboardTab; t: TFunction; children: ReactNode },
  { hasError: boolean }
> {
  state = { hasError: false };

  static getDerivedStateFromError(): { hasError: boolean } {
    return { hasError: true };
  }

  componentDidCatch(error: Error, errorInfo: ErrorInfo) {
    console.error(`Dashboard tab "${this.props.tab}" crashed`, error, errorInfo);
  }

  render() {
    if (!this.state.hasError) {
      return this.props.children;
    }

    return (
      <SurfaceEmptyState className="rounded-3xl border px-4 py-8 text-center text-sm">
        <div className="space-y-3">
          <div className="text-3xl opacity-40">⚠️</div>
          <div style={{ color: "var(--th-text-heading)" }}>
            {this.props.t({
              ko: "이 탭을 렌더링하는 중 오류가 발생했습니다.",
              en: "This tab failed while rendering.",
              ja: "このタブの描画中にエラーが発生しました。",
              zh: "该标签页渲染时发生错误。",
            })}
          </div>
          <div style={{ color: "var(--th-text-muted)" }}>
            {this.props.t({
              ko: "다른 탭으로 이동한 뒤 다시 돌아오거나 새로고침해 주세요.",
              en: "Switch away and come back, or refresh the page.",
              ja: "別のタブに移動して戻るか、ページを更新してください。",
              zh: "请切换到其他标签页后再返回，或刷新页面。",
            })}
          </div>
          <div className="flex justify-center">
            <SurfaceActionButton
              tone="neutral"
              onClick={() => this.setState({ hasError: false })}
            >
              {this.props.t({
                ko: "다시 시도",
                en: "Try Again",
                ja: "再試行",
                zh: "重试",
              })}
            </SurfaceActionButton>
          </div>
        </div>
      </SurfaceEmptyState>
    );
  }
}

export function PulseSectionShell({
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

export function PulseSignalCard({
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

export function MeetingTimelineCard({
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

export function SkillRankingSection({
  skillRanking,
  skillWindow,
  onChangeWindow,
  numberFormatter,
  localeTag,
  lastUpdatedAt,
  refreshFailed,
  t,
}: {
  skillRanking: SkillRankingResponse | null;
  skillWindow: "7d" | "30d" | "all";
  onChangeWindow: (value: "7d" | "30d" | "all") => void;
  numberFormatter: Intl.NumberFormat;
  localeTag: string;
  lastUpdatedAt: number | null;
  refreshFailed: boolean;
  t: TFunction;
}) {
  const updatedLabel = lastUpdatedAt
    ? new Intl.DateTimeFormat(localeTag, {
        month: "2-digit",
        day: "2-digit",
        hour: "2-digit",
        minute: "2-digit",
      }).format(lastUpdatedAt)
    : null;

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
          {updatedLabel ? (
            <SurfaceMetaBadge tone={refreshFailed ? "warn" : "neutral"}>
              {refreshFailed
                ? t({
                    ko: `새로고침 실패 · 마지막 ${updatedLabel}`,
                    en: `Refresh failed · last ${updatedLabel}`,
                    ja: `更新失敗 · 最終 ${updatedLabel}`,
                    zh: `刷新失败 · 最后 ${updatedLabel}`,
                  })
                : t({
                    ko: `마지막 갱신 ${updatedLabel}`,
                    en: `Last updated ${updatedLabel}`,
                    ja: `最終更新 ${updatedLabel}`,
                    zh: `最后更新 ${updatedLabel}`,
                  })}
            </SurfaceMetaBadge>
          ) : null}
          {(["7d", "30d", "all"] as const).map((windowId) => (
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

export function SkillRankingList({
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

export function DashboardTabButton({
  tab,
  active,
  label,
  detail,
  onClick,
  onKeyDown,
  buttonRef,
}: {
  tab: DashboardTab;
  active: boolean;
  label: string;
  detail: string;
  onClick: () => void;
  onKeyDown: (event: ReactKeyboardEvent<HTMLButtonElement>, tab: DashboardTab) => void;
  buttonRef: (node: HTMLButtonElement | null) => void;
}) {
  return (
    <button
      ref={buttonRef}
      type="button"
      id={dashboardTabButtonId(tab)}
      role="tab"
      aria-selected={active}
      aria-controls={dashboardTabPanelId(tab)}
      tabIndex={active ? 0 : -1}
      onClick={onClick}
      onKeyDown={(event) => onKeyDown(event, tab)}
      className="min-h-[5.25rem] w-full rounded-[22px] border px-4 py-3.5 text-left transition-all"
      style={{
        borderColor: active
          ? "color-mix(in srgb, var(--th-accent-primary) 32%, var(--th-border) 68%)"
          : "rgba(148,163,184,0.16)",
        background: active
          ? "color-mix(in srgb, var(--th-accent-primary-soft) 74%, transparent)"
          : "color-mix(in srgb, var(--th-card-bg) 94%, transparent)",
        boxShadow: active ? "0 14px 32px rgba(15, 23, 42, 0.12)" : "none",
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

export function DashboardSortableWidget({
  widgetId,
  className,
  editing,
  activeWidgetId,
  overWidgetId,
  handleLabel,
  children,
}: {
  widgetId: HomeWidgetId;
  className: string;
  editing: boolean;
  activeWidgetId: HomeWidgetId | null;
  overWidgetId: HomeWidgetId | null;
  handleLabel: string;
  children: ReactNode;
}) {
  const {
    attributes,
    isDragging,
    listeners,
    setActivatorNodeRef,
    setNodeRef,
    transform,
    transition,
  } = useSortable({ id: widgetId, disabled: !editing });
  const isOver = overWidgetId === widgetId && activeWidgetId !== widgetId;

  return (
    <div
      ref={setNodeRef}
      className={[
        className,
        isDragging ? "opacity-60" : "",
        isOver
          ? "rounded-[18px] ring-2 ring-[color:var(--th-accent-primary)] ring-offset-2 ring-offset-transparent"
          : "",
      ]
        .filter(Boolean)
        .join(" ")}
      style={{
        transform: CSS.Transform.toString(transform),
        transition: transition ?? "opacity 160ms ease, transform 160ms ease",
      }}
    >
      <div className="relative h-full">
        {editing ? (
          <button
            ref={setActivatorNodeRef}
            type="button"
            aria-label={handleLabel}
            className="absolute right-3 top-3 z-10 inline-flex h-8 w-8 cursor-grab items-center justify-center rounded-lg border transition-colors hover:bg-white/10 active:cursor-grabbing"
            style={{
              borderColor: "rgba(148,163,184,0.18)",
              background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
              color: "var(--th-text-muted)",
              touchAction: "none",
            }}
            {...attributes}
            {...listeners}
          >
            <GripVertical size={13} />
          </button>
        ) : null}
        {children}
      </div>
    </div>
  );
}

export function DashboardHomeMetricTile({
  title,
  value,
  badge,
  sub,
  accent,
  spark,
}: {
  title: string;
  value: string;
  badge: string;
  sub: string;
  accent: string;
  spark: number[];
}) {
  const maxValue = Math.max(1, ...spark);

  return (
    <SurfaceCard
      className="h-full rounded-[28px] p-4 sm:p-5"
      style={{
        borderColor: `color-mix(in srgb, ${accent} 22%, var(--th-border) 78%)`,
        background: `linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, ${accent} 5%) 0%, color-mix(in srgb, var(--th-bg-surface) 97%, transparent) 100%)`,
      }}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div
            className="text-[11px] font-semibold uppercase tracking-[0.16em]"
            style={{ color: "var(--th-text-muted)" }}
          >
            {title}
          </div>
          <div className="mt-3 text-[2rem] font-black leading-none tracking-tight" style={{ color: "var(--th-text-heading)" }}>
            {value}
          </div>
          <p className="mt-2 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
            {sub}
          </p>
        </div>
        <SurfaceMetaBadge
          tone="neutral"
          style={{
            borderColor: `color-mix(in srgb, ${accent} 22%, var(--th-border) 78%)`,
            background: `color-mix(in srgb, ${accent} 14%, var(--th-card-bg) 86%)`,
            color: accent,
          }}
        >
          {badge}
        </SurfaceMetaBadge>
      </div>

      <div className="mt-4 flex h-10 items-end gap-1">
        {spark.map((point, index) => (
          <div
            key={`${title}-${index}`}
            className="min-w-0 flex-1 rounded-full"
            style={{
              height: `${Math.max(18, (point / maxValue) * 100)}%`,
              background: `linear-gradient(180deg, color-mix(in srgb, ${accent} 78%, white 22%) 0%, ${accent} 100%)`,
              opacity: index === spark.length - 1 ? 1 : 0.72,
            }}
          />
        ))}
      </div>
    </SurfaceCard>
  );
}

export function DashboardHomeOfficeWidget({
  rows,
  stats,
  language,
  t,
  onSelectAgent,
}: {
  rows: HomeAgentRow[];
  stats: DashboardStats;
  language: CompanySettings["language"];
  t: TFunction;
  onSelectAgent?: (agent: Agent) => void;
}) {
  void language;
  const visibleRows = rows.slice(0, 8);

  return (
    <SurfaceSubsection
      title={t({ ko: "오피스 뷰", en: "Office View", ja: "オフィスビュー", zh: "办公室视图" })}
      description={t({
        ko: "지금 일하는 에이전트와 세션 상태를 한 화면에 압축해 보여줍니다.",
        en: "A compressed office snapshot of active agents and live sessions.",
        ja: "作業中エージェントとセッション状態を圧縮して見せます。",
        zh: "压缩展示当前工作中的代理与会话状态。",
      })}
      style={{
        minHeight: 320,
        borderColor: "color-mix(in srgb, var(--th-accent-info) 22%, var(--th-border) 78%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-info) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
      actions={(
        <div className="flex flex-wrap gap-2">
          <SurfaceMetaBadge tone="success">
            {t({ ko: `${stats.agents.working} working`, en: `${stats.agents.working} working`, ja: `${stats.agents.working} working`, zh: `${stats.agents.working} working` })}
          </SurfaceMetaBadge>
          <SurfaceMetaBadge tone="neutral">
            {t({ ko: `${stats.agents.idle} idle`, en: `${stats.agents.idle} idle`, ja: `${stats.agents.idle} idle`, zh: `${stats.agents.idle} idle` })}
          </SurfaceMetaBadge>
        </div>
      )}
    >
      {visibleRows.length === 0 ? (
        <SurfaceEmptyState className="px-4 py-6 text-center text-sm">
          {t({ ko: "표시할 에이전트가 없습니다.", en: "No agents available.", ja: "表示するエージェントがいません。", zh: "没有可显示的代理。" })}
        </SurfaceEmptyState>
      ) : (
        <>
          <div
            className="rounded-[24px] border p-4"
            style={{
              borderColor: "rgba(148,163,184,0.16)",
              background:
                "radial-gradient(circle at top, color-mix(in srgb, var(--th-accent-info) 12%, transparent), transparent 52%), linear-gradient(180deg, color-mix(in srgb, var(--th-bg-surface) 94%, transparent), color-mix(in srgb, var(--th-card-bg) 90%, transparent))",
            }}
          >
            <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
              {visibleRows.map((row) => {
                const statusTone = getAgentStatusTone(row.agent.status);
                const accent =
                  statusTone === "success"
                    ? "var(--th-accent-success)"
                    : statusTone === "warn"
                      ? "var(--th-accent-warn)"
                      : statusTone === "danger"
                        ? "var(--th-accent-danger)"
                        : "var(--th-text-muted)";
                return (
                  <button
                    key={row.agent.id}
                    type="button"
                    onClick={onSelectAgent ? () => onSelectAgent(row.agent) : undefined}
                    className="rounded-2xl border p-3 text-left transition-transform hover:-translate-y-0.5"
                    style={{
                      borderColor: `color-mix(in srgb, ${accent} 22%, var(--th-border) 78%)`,
                      background: "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
                    }}
                  >
                    <div className="flex items-start justify-between gap-2">
                      <div className="relative">
                        <AgentAvatar agent={row.agent} size={44} />
                        <span
                          className="absolute -right-0.5 -top-0.5 inline-flex h-3 w-3 rounded-full border-2"
                          style={{
                            borderColor: "var(--th-card-bg)",
                            background: accent,
                            boxShadow: `0 0 0 3px color-mix(in srgb, ${accent} 16%, transparent)`,
                          }}
                        />
                      </div>
                      <SurfaceMetaBadge tone={statusTone}>
                        {getAgentStatusLabel(row.agent.status, t)}
                      </SurfaceMetaBadge>
                    </div>
                    <div className="mt-3 truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                      {row.displayName}
                    </div>
                    <div className="mt-1 min-h-[2.5rem] text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                      {row.workSummary ?? t({ ko: "대기 중", en: "Idle", ja: "待機中", zh: "待机中" })}
                    </div>
                    <div className="mt-2 flex items-center justify-between text-[10px]" style={{ color: "var(--th-text-muted)" }}>
                      <span style={{ fontFamily: "var(--font-mono)" }}>
                        {row.elapsedLabel ?? "--"}
                      </span>
                      <span style={{ fontFamily: "var(--font-mono)" }}>
                        {row.linkedSessions.length} session
                      </span>
                    </div>
                  </button>
                );
              })}
            </div>
          </div>

          <div className="mt-4 flex flex-wrap gap-2">
            <SurfaceMetaBadge tone="success">
              {t({ ko: `${stats.agents.working} working`, en: `${stats.agents.working} working`, ja: `${stats.agents.working} working`, zh: `${stats.agents.working} working` })}
            </SurfaceMetaBadge>
            <SurfaceMetaBadge tone="neutral">
              {t({ ko: `${stats.agents.idle} idle`, en: `${stats.agents.idle} idle`, ja: `${stats.agents.idle} idle`, zh: `${stats.agents.idle} idle` })}
            </SurfaceMetaBadge>
            <SurfaceMetaBadge tone="warn">
              {t({ ko: `${stats.dispatched_count} dispatched`, en: `${stats.dispatched_count} dispatched`, ja: `${stats.dispatched_count} dispatched`, zh: `${stats.dispatched_count} dispatched` })}
            </SurfaceMetaBadge>
          </div>
        </>
      )}
    </SurfaceSubsection>
  );
}

export function DashboardHomeSignalsWidget({
  rows,
  maxValue,
  t,
}: {
  rows: HomeSignalRow[];
  maxValue: number;
  t: TFunction;
}) {
  return (
    <SurfaceSubsection
      title={t({ ko: "운영 미션", en: "Ops Missions", ja: "運用ミッション", zh: "运营任务" })}
      description={t({
        ko: "지금 바로 처리할 운영 압력을 우선순위 카드로 정리했습니다.",
        en: "Priority cards for the operational pressure points that need action now.",
        ja: "今すぐ処理すべき運用圧力を優先カードで整理しました。",
        zh: "将需要立即处理的运营压力整理成优先级卡片。",
      })}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-primary) 22%, var(--th-border) 78%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-primary) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      <div className="mb-4 flex items-center justify-between gap-2">
        <SurfaceMetaBadge tone="neutral">
          {t({
            ko: `${rows.length}개 트래킹`,
            en: `${rows.length} tracked`,
            ja: `${rows.length}件を追跡中`,
            zh: `跟踪 ${rows.length} 项`,
          })}
        </SurfaceMetaBadge>
        <span
          className="text-[11px]"
          style={{
            color: "var(--th-text-muted)",
            fontFamily: "var(--font-mono)",
          }}
        >
          {t({ ko: "priority live", en: "priority live", ja: "priority live", zh: "priority live" })}
        </span>
      </div>

      <div className="space-y-2.5">
        {rows.map((row) => {
          const accent = getSignalAccent(row.tone);
          const tone = row.tone === "info" ? "info" : row.tone;
          const ratio = Math.max(0, Math.min(100, (row.value / maxValue) * 100));
          const body = (
            <div
              className="rounded-[22px] border p-4 text-left transition-transform duration-150"
              style={{
                borderColor: `color-mix(in srgb, ${accent} 24%, var(--th-border) 76%)`,
                background: `linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 93%, ${accent} 7%) 0%, color-mix(in srgb, var(--th-bg-surface) 95%, transparent) 100%)`,
              }}
            >
              <div className="flex items-start gap-3">
                <div
                  className="mt-0.5 flex h-6 w-6 shrink-0 items-center justify-center rounded-lg border text-[11px] font-semibold"
                  style={{
                    borderColor: `color-mix(in srgb, ${accent} 26%, var(--th-border) 74%)`,
                    background: `color-mix(in srgb, ${accent} 12%, var(--th-card-bg) 88%)`,
                    color: accent,
                  }}
                >
                  {row.value > 0 ? "!" : "·"}
                </div>

                <div className="min-w-0 flex-1">
                  <div className="text-[10.5px] font-semibold uppercase tracking-[0.16em]" style={{ color: accent }}>
                    {row.label}
                  </div>
                  <div className="mt-2 flex items-end justify-between gap-3">
                    <div className="text-3xl font-black tracking-tight" style={{ color: "var(--th-text-heading)" }}>
                      {row.value}
                    </div>
                    <SurfaceMetaBadge tone={tone}>{row.description}</SurfaceMetaBadge>
                  </div>
                  <div className="mt-3 flex items-center justify-between gap-3 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                    <span>
                      {t({
                        ko: row.value > 0 ? "지금 확인 필요" : "현재 추가 조치 없음",
                        en: row.value > 0 ? "Needs attention now" : "No extra action right now",
                        ja: row.value > 0 ? "今すぐ確認が必要" : "追加アクションなし",
                        zh: row.value > 0 ? "需要立即确认" : "当前无需额外处理",
                      })}
                    </span>
                    <span style={{ fontFamily: "var(--font-mono)" }}>
                      {t({ ko: "압력", en: "pressure", ja: "pressure", zh: "pressure" })} {Math.round(ratio)}%
                    </span>
                  </div>

                  <div className="mt-3 flex items-center gap-3">
                    <div className="h-[5px] flex-1 overflow-hidden rounded-full" style={{ background: "color-mix(in srgb, var(--th-bg-surface) 82%, transparent)" }}>
                      <div
                        className="h-full rounded-full"
                        style={{
                          width: `${Math.max(8, ratio)}%`,
                          background: `linear-gradient(90deg, color-mix(in srgb, ${accent} 68%, white 32%), ${accent})`,
                        }}
                      />
                    </div>
                    <span className="text-[11px] font-medium" style={{ color: accent }}>
                      {row.onAction
                        ? t({ ko: "열기", en: "Open", ja: "開く", zh: "打开" })
                        : t({ ko: "모니터링", en: "Monitoring", ja: "監視中", zh: "监控中" })}
                    </span>
                  </div>
                </div>
              </div>
            </div>
          );

          return row.onAction ? (
            <button
              key={row.id}
              type="button"
              onClick={row.onAction}
              className="block w-full rounded-2xl text-left transition-transform hover:-translate-y-0.5"
            >
              {body}
            </button>
          ) : (
            <div key={row.id}>{body}</div>
          );
        })}
      </div>
    </SurfaceSubsection>
  );
}

export function DashboardHomeRosterWidget({
  rows,
  t,
  numberFormatter,
  onSelectAgent,
  onOpenAchievements,
}: {
  rows: HomeAgentRow[];
  t: TFunction;
  numberFormatter: Intl.NumberFormat;
  onSelectAgent?: (agent: Agent) => void;
  onOpenAchievements?: () => void;
}) {
  return (
    <SurfaceSubsection
      title={t({ ko: "에이전트 현황", en: "Agent Roster", ja: "エージェント現況", zh: "代理现况" })}
      description={t({
        ko: "활성 우선으로 상위 에이전트 상태를 요약합니다.",
        en: "A live-first roster summary of the top agents.",
        ja: "アクティブ優先で上位エージェントの状態を要約します。",
        zh: "按活跃优先总结头部代理状态。",
      })}
      actions={onOpenAchievements ? (
        <SurfaceActionButton tone="accent" onClick={onOpenAchievements}>
          {t({ ko: "업적 보기", en: "Open XP", ja: "XP を開く", zh: "查看 XP" })}
        </SurfaceActionButton>
      ) : undefined}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-success) 22%, var(--th-border) 78%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-success) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      {rows.length === 0 ? (
        <SurfaceEmptyState className="px-4 py-6 text-center text-sm">
          {t({ ko: "표시할 에이전트가 없습니다.", en: "No agents to show.", ja: "表示するエージェントがいません。", zh: "没有可显示的代理。" })}
        </SurfaceEmptyState>
      ) : (
        <div className="space-y-2">
          {rows.map((row) => (
            <SurfaceListItem
              key={row.agent.id}
              tone={getAgentStatusTone(row.agent.status)}
              trailing={(
                <div className="flex items-center gap-2">
                  <div className="text-right text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                    <div style={{ color: "var(--th-text-heading)" }}>
                      XP {numberFormatter.format(row.agent.stats_xp)}
                    </div>
                    <div>{numberFormatter.format(row.agent.stats_tasks_done)} done</div>
                  </div>
                  {onSelectAgent ? (
                    <SurfaceActionButton compact tone="neutral" onClick={() => onSelectAgent(row.agent)}>
                      {t({ ko: "열기", en: "Open", ja: "開く", zh: "打开" })}
                    </SurfaceActionButton>
                  ) : null}
                </div>
              )}
            >
              <div className="flex items-start gap-3">
                <AgentAvatar agent={row.agent} size={34} />
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-2">
                    <span className="truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                      {row.displayName}
                    </span>
                    <SurfaceMetaBadge tone={getAgentStatusTone(row.agent.status)}>
                      {getAgentStatusLabel(row.agent.status, t)}
                    </SurfaceMetaBadge>
                  </div>
                  <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                    {row.workSummary ?? t({ ko: "대기 중", en: "Idle", ja: "待機中", zh: "待机中" })}
                  </div>
                  <div className="mt-2 flex flex-wrap gap-2 text-[11px]">
                    {row.elapsedLabel ? <SurfaceMetaBadge>{row.elapsedLabel}</SurfaceMetaBadge> : null}
                    <SurfaceMetaBadge>{row.linkedSessions.length} session</SurfaceMetaBadge>
                  </div>
                </div>
              </div>
            </SurfaceListItem>
          ))}
        </div>
      )}
    </SurfaceSubsection>
  );
}

export function DashboardHomeActivityWidget({
  items,
  localeTag,
  t,
  onOpenMeetings,
}: {
  items: HomeActivityItem[];
  localeTag: string;
  t: TFunction;
  onOpenMeetings?: () => void;
}) {
  const formatter = useMemo(
    () =>
      new Intl.DateTimeFormat(localeTag, {
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
      }),
    [localeTag],
  );

  return (
    <SurfaceSubsection
      title={t({ ko: "최근 활동", en: "Recent Activity", ja: "最近の活動", zh: "最近活动" })}
      description={t({
        ko: "회의와 세션 전환을 시간순으로 압축해 보여줍니다.",
        en: "A compressed activity stream across meetings and sessions.",
        ja: "会議とセッション遷移を時間順で圧縮表示します。",
        zh: "按时间顺序压缩展示会议与会话活动。",
      })}
      actions={onOpenMeetings ? (
        <SurfaceActionButton tone="neutral" onClick={onOpenMeetings}>
          {t({ ko: "회의 보기", en: "Open Meetings", ja: "会議を開く", zh: "打开会议" })}
        </SurfaceActionButton>
      ) : undefined}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-warn) 22%, var(--th-border) 78%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-warn) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      {items.length === 0 ? (
        <SurfaceEmptyState className="px-4 py-6 text-center text-sm">
          {t({ ko: "최근 활동이 없습니다.", en: "No recent activity.", ja: "最近の活動はありません。", zh: "暂无最近活动。" })}
        </SurfaceEmptyState>
      ) : (
        <div className="space-y-2">
          {items.map((item) => (
            <SurfaceListItem
              key={item.id}
              tone={item.tone === "success" ? "success" : "warn"}
              trailing={(
                <div className="text-right text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                  <div>{timeAgo(item.timestamp, localeTag)}</div>
                  <div style={{ fontFamily: "var(--font-mono)" }}>{formatter.format(item.timestamp)}</div>
                </div>
              )}
            >
              <div className="min-w-0">
                <div className="truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                  {item.title}
                </div>
                <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {item.detail}
                </div>
              </div>
            </SurfaceListItem>
          ))}
        </div>
      )}
    </SurfaceSubsection>
  );
}

export function DashboardHomeSectionNavigatorWidget({
  tabDefinitions,
  activeTab,
  t,
  topRepos,
  openTotal,
  onClickTab,
  onKeyDown,
  buttonRefs,
}: {
  tabDefinitions: DashboardTabDefinition[];
  activeTab: DashboardTab;
  t: TFunction;
  topRepos: Array<{
    github_repo: string;
    open_count: number;
    pressure_count: number;
  }>;
  openTotal: number;
  onClickTab: (tab: DashboardTab) => void;
  onKeyDown: (event: ReactKeyboardEvent<HTMLButtonElement>, tab: DashboardTab) => void;
  buttonRefs: { current: Record<DashboardTab, HTMLButtonElement | null> };
}) {
  return (
    <SurfaceSubsection
      title={t({ ko: "빠른 이동", en: "Quick Navigation", ja: "クイック移動", zh: "快速导航" })}
      description={t({
        ko: "홈에서 각 운영 섹션과 칸반 압력을 바로 전환합니다.",
        en: "Jump directly into each operational section and kanban pressure lane from home.",
        ja: "ホームから各運用セクションとカンバン圧力レーンへ直接移動します。",
        zh: "从首页直接跳转到各运营分区与看板压力区。",
      })}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-info) 22%, var(--th-border) 78%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-info) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      <div className="grid gap-4 xl:grid-cols-[minmax(0,1.05fr)_minmax(0,0.95fr)]">
        <div
          role="tablist"
          aria-label={t({ ko: "대시보드 섹션", en: "Dashboard sections", ja: "ダッシュボードセクション", zh: "仪表盘分区" })}
          className="grid gap-2 sm:grid-cols-2 xl:grid-cols-3"
        >
          {tabDefinitions.map((definition) => (
            <DashboardTabButton
              key={definition.id}
              tab={definition.id}
              active={activeTab === definition.id}
              label={definition.label}
              detail={definition.detail}
              onClick={() => onClickTab(definition.id)}
              onKeyDown={onKeyDown}
              buttonRef={(node) => {
                buttonRefs.current[definition.id] = node;
              }}
            />
          ))}
        </div>

        <SurfaceCard
          className="rounded-[24px] p-4"
          style={{
            borderColor: "color-mix(in srgb, var(--th-accent-primary) 20%, var(--th-border) 80%)",
            background:
              "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, var(--th-accent-primary) 5%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
          }}
        >
          <div className="text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
            {t({ ko: "Kanban Snapshot", en: "Kanban Snapshot", ja: "Kanban Snapshot", zh: "Kanban Snapshot" })}
          </div>
          <div className="mt-3 text-3xl font-black tracking-tight" style={{ color: "var(--th-text-heading)" }}>
            {openTotal}
          </div>
          <p className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "현재 열려 있는 전체 카드 수와 압력이 높은 저장소입니다.",
              en: "Open card count and the repos with the heaviest pressure.",
              ja: "現在開いているカード総数と圧力の高いリポジトリです。",
              zh: "当前打开卡片总数与压力最高的仓库。",
            })}
          </p>

          <div className="mt-4 space-y-2">
            {topRepos.length === 0 ? (
              <SurfaceEmptyState className="px-4 py-6 text-center text-sm">
                {t({ ko: "추적 중인 저장소가 없습니다.", en: "No repo pressure tracked yet.", ja: "追跡中のリポジトリがありません。", zh: "暂无正在跟踪的仓库压力。" })}
              </SurfaceEmptyState>
            ) : (
              topRepos.slice(0, 3).map((repo) => (
                <SurfaceListItem
                  key={repo.github_repo}
                  tone={repo.pressure_count > 0 ? "warn" : "neutral"}
                  trailing={(
                    <div className="text-right text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                      <div style={{ color: "var(--th-text-heading)" }}>{repo.open_count}</div>
                      <div>{repo.pressure_count} pressure</div>
                    </div>
                  )}
                >
                  <div className="min-w-0">
                    <div className="truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                      {repo.github_repo}
                    </div>
                    <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                      {t({
                        ko: repo.pressure_count > 0 ? "리뷰/블록 압력 있음" : "오픈 카드 추적 중",
                        en: repo.pressure_count > 0 ? "Pressure in review/blocked" : "Tracking open cards",
                        ja: repo.pressure_count > 0 ? "レビュー/ブロック圧力あり" : "オープンカード追跡中",
                        zh: repo.pressure_count > 0 ? "存在 review/blocked 压力" : "正在跟踪打开卡片",
                      })}
                    </div>
                  </div>
                </SurfaceListItem>
              ))
            )}
          </div>
        </SurfaceCard>
      </div>
    </SurfaceSubsection>
  );
}

function getAgentStatusTone(status: Agent["status"]): "neutral" | "success" | "warn" | "danger" {
  switch (status) {
    case "working":
      return "success";
    case "break":
      return "warn";
    case "offline":
      return "danger";
    case "idle":
    default:
      return "neutral";
  }
}

function getAgentStatusLabel(status: Agent["status"], t: TFunction): string {
  switch (status) {
    case "working":
      return t({ ko: "작업 중", en: "Working", ja: "作業中", zh: "工作中" });
    case "break":
      return t({ ko: "휴식", en: "Break", ja: "休憩", zh: "休息" });
    case "offline":
      return t({ ko: "오프라인", en: "Offline", ja: "オフライン", zh: "离线" });
    case "idle":
    default:
      return t({ ko: "대기", en: "Idle", ja: "待機", zh: "待机" });
  }
}

function getSignalAccent(tone: HomeSignalTone): string {
  switch (tone) {
    case "success":
      return "#22c55e";
    case "warn":
      return "#f59e0b";
    case "danger":
      return "#ef4444";
    case "info":
    default:
      return "#14b8a6";
  }
}
