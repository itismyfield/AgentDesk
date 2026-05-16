import {
  Component,
  type CSSProperties,
  type ErrorInfo,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
} from "react";
import { useSortable } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { GripVertical } from "lucide-react";
import type { TFunction } from "./model";
import type { DashboardTab } from "../../app/dashboardTabs";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceListItem,
  SurfaceMetaBadge,
  SurfaceSection,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";
import type { HomeWidgetId } from "./homeWidgetOrder";

interface DashboardTabDefinition {
  id: DashboardTab;
  label: string;
  detail: string;
}

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

export { MeetingTimelineCard, SkillRankingList, SkillRankingSection } from "./DashboardHomePulseSections";

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

export {
  DashboardHomeActivityWidget,
  DashboardHomeMetricTile,
  DashboardHomeOfficeWidget,
  DashboardHomeRosterWidget,
  DashboardHomeSignalsWidget,
} from "./DashboardHomeSnapshotWidgets";

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
