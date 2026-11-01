import { MoreHorizontal, Moon, Palette, Sun, X, type LucideIcon } from "lucide-react";
import type { Dispatch, RefObject, SetStateAction } from "react";
import type { AgentManagerTab } from "../components/agent-manager/useAgentManagerController";
import type { AppRouteEntry, AppRouteId } from "./routes";
import type { ThemePreference } from "./themePreferences";

type MobileRouteId = AppRouteId | "more";

interface AppMobileNavigationProps {
  activeRouteId: MobileRouteId;
  bottomSheetZIndex: number;
  iconForRoute: (routeId: AppRouteId) => LucideIcon;
  isKo: boolean;
  moreMenuRef: RefObject<HTMLDivElement | null>;
  moreOpen: boolean;
  moreRoutes: AppRouteEntry[];
  navigateToRoute: (path: string, options?: { agentsTab?: AgentManagerTab }) => void;
  primaryRoutes: AppRouteEntry[];
  resolvedTheme: Exclude<ThemePreference, "auto">;
  routeBadge: (routeId: AppRouteId) => number | undefined;
  setMoreOpen: Dispatch<SetStateAction<boolean>>;
  setShowTweaksPanel: Dispatch<SetStateAction<boolean>>;
  toggleShellTheme: () => void;
  tabbarHeight: string;
  tabbarZIndex: number;
}

const TAB_CLASS =
  "relative flex h-full min-h-11 min-w-0 flex-1 flex-col items-center justify-center gap-1 px-1 text-th-text-muted transition-colors data-[active]:text-[var(--th-accent-primary)] focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--th-accent-primary)]";
const SHEET_ROW_CLASS =
  "flex min-h-12 w-full items-center gap-3 border-b border-th-border-subtle py-3 text-left hover:bg-surface-subtle focus-visible:outline-2 focus-visible:outline-[var(--th-accent-primary)]";

function formatBadge(value: number): string {
  return value > 9 ? "9+" : String(value);
}

function badgeAriaSuffix(isKo: boolean, badge: number | undefined): string {
  if (!badge || badge <= 0) return "";
  return isKo ? `, 확인할 일 ${badge}건` : `, ${badge} need attention`;
}

export function AppMobileNavigation({
  activeRouteId,
  bottomSheetZIndex,
  iconForRoute,
  isKo,
  moreMenuRef,
  moreOpen,
  moreRoutes,
  navigateToRoute,
  primaryRoutes,
  resolvedTheme,
  routeBadge,
  setMoreOpen,
  setShowTweaksPanel,
  toggleShellTheme,
  tabbarHeight,
  tabbarZIndex,
}: AppMobileNavigationProps) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const moreBadge = moreRoutes.reduce((sum, route) => sum + (routeBadge(route.id) ?? 0), 0);
  const themeLabel =
    resolvedTheme === "dark" ? tr("밝은 화면으로", "Light theme") : tr("어두운 화면으로", "Dark theme");

  return (
    <>
      <nav
        data-testid="app-mobile-tabbar"
        className="fixed inset-x-0 bottom-0 flex items-start justify-around border-t border-th-border-subtle bg-th-bg-surface pb-[env(safe-area-inset-bottom)] pl-[env(safe-area-inset-left)] pr-[env(safe-area-inset-right)]"
        style={{ height: tabbarHeight, zIndex: tabbarZIndex }}
        aria-label={tr("주요 탭", "Primary tabs")}
      >
        {primaryRoutes.map((route) => {
          const Icon = iconForRoute(route.id);
          const isActive = activeRouteId === route.id;
          const badge = routeBadge(route.id);
          const label = isKo ? route.labelKo : route.labelEn;
          return (
            <button
              key={route.id}
              type="button"
              data-testid={`app-mobile-tab-${route.id}`}
              data-active={isActive || undefined}
              aria-label={label + badgeAriaSuffix(isKo, badge)}
              aria-current={isActive ? "page" : undefined}
              onClick={() => navigateToRoute(route.path)}
              className={TAB_CLASS}
            >
              <Icon size={20} aria-hidden />
              <span className="max-w-full truncate text-[11px] font-medium leading-4">{label}</span>
              {badge !== undefined && badge > 0 && (
                <span className="pointer-events-none absolute top-1 right-1/4 flex h-4 min-w-4 items-center justify-center rounded-full bg-[var(--th-accent-primary)] px-1 text-[9px] font-semibold text-white" aria-hidden>
                  {formatBadge(badge)}
                </span>
              )}
            </button>
          );
        })}
        <button
          type="button"
          data-testid="app-mobile-more-button"
          data-active={activeRouteId === "more" || undefined}
          aria-haspopup="dialog"
          aria-expanded={moreOpen}
          aria-controls={moreOpen ? "app-mobile-more-menu" : undefined}
          aria-label={tr("더보기", "More") + badgeAriaSuffix(isKo, moreBadge)}
          onClick={() => setMoreOpen((prev) => !prev)}
          className={TAB_CLASS}
        >
          <MoreHorizontal size={20} aria-hidden />
          <span className="max-w-full truncate text-[11px] font-medium leading-4">{tr("더보기", "More")}</span>
          {moreBadge > 0 && (
            <span className="pointer-events-none absolute top-1 right-1/4 flex h-4 min-w-4 items-center justify-center rounded-full bg-[var(--th-accent-primary)] px-1 text-[9px] font-semibold text-white" aria-hidden>
              {formatBadge(moreBadge)}
            </span>
          )}
        </button>
      </nav>

      {moreOpen && (
        <div
          className="fixed inset-0 flex items-end justify-center"
          style={{ zIndex: bottomSheetZIndex }}
          onClick={() => setMoreOpen(false)}
        >
          <div className="absolute inset-0 bg-black/50" />
          <div
            ref={moreMenuRef}
            id="app-mobile-more-menu"
            data-testid="app-mobile-more-menu"
            role="dialog"
            aria-modal="true"
            aria-label={tr("더보기", "More")}
            tabIndex={-1}
            className="relative max-h-[84dvh] w-full overflow-y-auto rounded-t-xl border-t border-th-border-subtle bg-th-bg-surface px-4 pt-3 pb-[max(1rem,env(safe-area-inset-bottom))] text-th-text-primary"
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                event.preventDefault();
                setMoreOpen(false);
              }
            }}
            onClick={(event) => event.stopPropagation()}
          >
            <div className="mx-auto mb-4 h-1 w-10 rounded-full bg-th-border" aria-hidden />
            <div className="mb-2 flex items-center justify-between gap-3">
              <div className="text-base font-semibold text-th-text-heading">{tr("더보기", "More")}</div>
              <button
                type="button"
                onClick={() => setMoreOpen(false)}
                className="flex h-11 w-11 shrink-0 items-center justify-center rounded-md border border-th-border-subtle text-th-text-secondary hover:bg-surface-hover focus-visible:outline-2 focus-visible:outline-[var(--th-accent-primary)]"
                aria-label={tr("더보기 닫기", "Close")}
              >
                <X size={16} aria-hidden />
              </button>
            </div>

            <div>
              {moreRoutes.map((route) => {
                const Icon = iconForRoute(route.id);
                const badge = routeBadge(route.id);
                const label = isKo ? route.labelKo : route.labelEn;
                return (
                  <button
                    key={route.id}
                    type="button"
                    data-testid={`app-mobile-more-${route.id}`}
                    aria-label={label + badgeAriaSuffix(isKo, badge)}
                    onClick={() =>
                      navigateToRoute(route.path, route.id === "agents" ? { agentsTab: "agents" } : undefined)
                    }
                    className={SHEET_ROW_CLASS}
                  >
                    <span className="flex h-6 w-6 shrink-0 items-center justify-center text-th-text-muted" aria-hidden>
                      <Icon size={18} />
                    </span>
                    <span className="min-w-0 flex-1">
                      <span className="flex items-center gap-2 text-sm font-medium text-th-text-heading">
                        {label}
                        {badge !== undefined && badge > 0 && (
                          <span className="rounded-md bg-surface-medium px-1.5 text-xs tabular-nums text-th-text-muted" aria-hidden>
                            {formatBadge(badge)}
                          </span>
                        )}
                      </span>
                      <span className="mt-0.5 block text-xs leading-5 text-th-text-muted">
                        {isKo ? route.descriptionKo : route.descriptionEn}
                      </span>
                    </span>
                  </button>
                );
              })}
              <button type="button" data-testid="app-mobile-more-theme" onClick={toggleShellTheme} className={SHEET_ROW_CLASS}>
                <span className="flex h-6 w-6 shrink-0 items-center justify-center text-th-text-muted" aria-hidden>
                  {resolvedTheme === "dark" ? <Sun size={18} /> : <Moon size={18} />}
                </span>
                <span className="text-sm font-medium text-th-text-heading">{themeLabel}</span>
              </button>
              <button
                type="button"
                data-testid="app-mobile-more-tweaks"
                onClick={() => {
                  setMoreOpen(false);
                  setShowTweaksPanel(true);
                }}
                className={SHEET_ROW_CLASS}
              >
                <span className="flex h-6 w-6 shrink-0 items-center justify-center text-th-text-muted" aria-hidden>
                  <Palette size={18} />
                </span>
                <span className="text-sm font-medium text-th-text-heading">{tr("화면 꾸미기", "Appearance")}</span>
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}
