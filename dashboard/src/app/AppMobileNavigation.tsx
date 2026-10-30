import { Settings, X, type LucideIcon } from "lucide-react";
import type { Dispatch, RefObject, SetStateAction } from "react";
import type { AppRouteEntry, AppRouteId } from "./routes";

type MobileRouteId = AppRouteId | "more";
type NavigateOptions = {
  agentsTab?: "agents" | "departments" | "backlog" | "dispatch";
};

interface MobileOverflowSection {
  id: string;
  labelKo: string;
  labelEn: string;
  routes: AppRouteEntry[];
}

interface AppMobileNavigationProps {
  activeRouteId: MobileRouteId;
  bottomSheetZIndex: number;
  iconForRoute: (routeId: AppRouteId) => LucideIcon;
  isKo: boolean;
  moreMenuRef: RefObject<HTMLDivElement | null>;
  moreOpen: boolean;
  moreSections: MobileOverflowSection[];
  navigateToRoute: (path: string, options?: NavigateOptions) => void;
  primaryRoutes: AppRouteEntry[];
  routeBadge: (routeId: AppRouteId) => number | undefined;
  setMoreOpen: Dispatch<SetStateAction<boolean>>;
  tabbarHeight: string;
  tabbarZIndex: number;
}

function formatBadge(value: number): string {
  return value > 9 ? "9+" : String(value);
}

function badgeAriaSuffix(isKo: boolean, badge: number | undefined): string {
  if (!badge || badge <= 0) return "";
  return isKo ? `, 알림 ${badge}건` : `, ${badge} pending`;
}

export function AppMobileNavigation({
  activeRouteId,
  bottomSheetZIndex,
  iconForRoute,
  isKo,
  moreMenuRef,
  moreOpen,
  moreSections,
  navigateToRoute,
  primaryRoutes,
  routeBadge,
  setMoreOpen,
  tabbarHeight,
  tabbarZIndex,
}: AppMobileNavigationProps) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const moreBadge = (routeBadge("meetings") ?? 0) + (routeBadge("settings") ?? 0);

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
          return (
            <button
              key={route.id}
              type="button"
              data-testid={`app-mobile-tab-${route.id}`}
              data-active={isActive || undefined}
              aria-label={
                tr(`${isKo ? route.labelKo : route.labelEn} 열기`, `Open ${route.labelEn}`) +
                badgeAriaSuffix(isKo, badge)
              }
              aria-current={isActive ? "page" : undefined}
              onClick={() => navigateToRoute(route.path)}
              className="relative flex h-full min-h-11 min-w-0 flex-1 flex-col items-center justify-center gap-1 px-1 text-th-text-muted transition-colors data-[active]:text-[var(--th-accent-primary)] focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--th-accent-primary)]"
            >
              <Icon size={20} aria-hidden />
              <span className="max-w-full truncate text-[11px] font-medium leading-4">
                {isKo ? route.labelKo : route.labelEn}
              </span>
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
          aria-label={tr("설정", "Settings") + badgeAriaSuffix(isKo, moreBadge)}
          onClick={() => setMoreOpen((prev) => !prev)}
          className="relative flex h-full min-h-11 min-w-0 flex-1 flex-col items-center justify-center gap-1 px-1 text-th-text-muted transition-colors data-[active]:text-[var(--th-accent-primary)] focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--th-accent-primary)]"
        >
          <Settings size={20} aria-hidden />
          <span className="max-w-full truncate text-[11px] font-medium leading-4">{tr("설정", "Settings")}</span>
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
            aria-label={tr("확장 메뉴", "Extensions menu")}
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
            <div className="mb-3 flex items-center justify-between gap-3">
              <div>
                <div className="text-xs font-medium" style={{ color: "var(--th-text-muted)" }}>
                  {tr("확장", "Extensions")}
                </div>
                <div className="mt-1 text-base font-semibold" style={{ color: "var(--th-text-heading)" }}>
                  {tr("메뉴", "More pages")}
                </div>
              </div>
              <button
                type="button"
                onClick={() => setMoreOpen(false)}
                className="flex h-11 w-11 shrink-0 items-center justify-center rounded-md border border-th-border-subtle text-th-text-secondary hover:bg-surface-hover focus-visible:outline-2 focus-visible:outline-[var(--th-accent-primary)]"
                aria-label={tr("더보기 닫기", "Close more menu")}
              >
                <X size={16} aria-hidden />
              </button>
            </div>

            <div className="space-y-4">
              {moreSections.map((section) => (
                <div key={section.id} className="space-y-2">
                  <div className="text-xs font-medium text-th-text-muted">
                    {isKo ? section.labelKo : section.labelEn}
                  </div>
                  <div className="grid gap-2">
                    {section.routes.map((route) => {
                      const Icon = iconForRoute(route.id);
                      const badge = routeBadge(route.id);
                      return (
                        <button
                          key={route.id}
                          type="button"
                          aria-label={
                            (isKo ? route.labelKo : route.labelEn) +
                            badgeAriaSuffix(isKo, badge)
                          }
                          onClick={() =>
                            navigateToRoute(
                              route.path,
                              route.id === "agents" ? { agentsTab: "agents" } : undefined,
                            )
                          }
                          className="flex min-h-11 w-full items-start gap-3 border-b border-th-border-subtle py-3 text-left hover:bg-surface-subtle focus-visible:outline-2 focus-visible:outline-[var(--th-accent-primary)]"
                        >
                          <span className="flex h-6 w-6 shrink-0 items-center justify-center text-th-text-muted" aria-hidden>
                            <Icon size={18} />
                          </span>
                          <span className="min-w-0 flex-1">
                            <span className="flex flex-wrap items-center gap-2 text-sm font-medium text-th-text-heading">
                              {isKo ? route.labelKo : route.labelEn}
                              {badge !== undefined && badge > 0 && (
                                <span className="rounded-md bg-surface-medium px-1.5 text-xs tabular-nums text-th-text-muted" aria-hidden>
                                  {formatBadge(badge)}
                                </span>
                              )}
                            </span>
                            <span className="mt-1 block text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">
                              {isKo ? route.descriptionKo : route.descriptionEn}
                            </span>
                          </span>
                        </button>
                      );
                    })}
                  </div>
                </div>
              ))}
            </div>
          </div>
        </div>
      )}
    </>
  );
}
