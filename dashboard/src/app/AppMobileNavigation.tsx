import { Settings, X, type LucideIcon } from "lucide-react";
import type { Dispatch, RefObject, SetStateAction } from "react";
import type { AppRouteEntry, AppRouteId } from "./routes";
import "./AppMobileNavigation.css";

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
        className="adk-mobile-tabbar"
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
              aria-label={tr(`${isKo ? route.labelKo : route.labelEn} 열기`, `Open ${route.labelEn}`)}
              aria-current={isActive ? "page" : undefined}
              onClick={() => navigateToRoute(route.path)}
              className="adk-mobile-tab"
            >
              <Icon size={20} aria-hidden />
              <span className="adk-mobile-tab-label">
                {isKo ? route.labelKo : route.labelEn}
              </span>
              {badge !== undefined && badge > 0 && (
                <span className="adk-mobile-tab-badge" aria-hidden>
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
          onClick={() => setMoreOpen((prev) => !prev)}
          className="adk-mobile-tab"
        >
          <Settings size={20} aria-hidden />
          <span className="adk-mobile-tab-label">{tr("설정", "Settings")}</span>
          {moreBadge > 0 && (
            <span className="adk-mobile-tab-badge" aria-hidden>
              {formatBadge(moreBadge)}
            </span>
          )}
        </button>
      </nav>

      {moreOpen && (
        <div
          className="adk-mobile-sheet-backdrop"
          style={{ zIndex: bottomSheetZIndex }}
          onClick={() => setMoreOpen(false)}
        >
          <div className="adk-mobile-sheet-scrim" />
          <div
            ref={moreMenuRef}
            id="app-mobile-more-menu"
            data-testid="app-mobile-more-menu"
            role="dialog"
            aria-modal="true"
            aria-label={tr("확장 메뉴", "Extensions menu")}
            tabIndex={-1}
            className="adk-mobile-sheet"
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                event.preventDefault();
                setMoreOpen(false);
              }
            }}
            onClick={(event) => event.stopPropagation()}
          >
            <div className="adk-mobile-sheet-grip" aria-hidden />
            <div className="mb-3 flex items-center justify-between gap-3">
              <div>
                <div className="text-[11px] font-semibold uppercase tracking-[0.2em]" style={{ color: "var(--th-text-muted)" }}>
                  {tr("확장", "Extensions")}
                </div>
                <div className="mt-1 text-base font-semibold" style={{ color: "var(--th-text-heading)" }}>
                  {tr("숨겨진 페이지 바로가기", "Jump to secondary pages")}
                </div>
              </div>
              <button
                type="button"
                onClick={() => setMoreOpen(false)}
                className="adk-mobile-sheet-close"
                aria-label={tr("더보기 닫기", "Close more menu")}
              >
                <X size={16} aria-hidden />
              </button>
            </div>

            <div className="space-y-4">
              {moreSections.map((section) => (
                <div key={section.id} className="space-y-2">
                  <div className="adk-mobile-sheet-section-title">
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
                          aria-label={isKo ? route.labelKo : route.labelEn}
                          onClick={() =>
                            navigateToRoute(
                              route.path,
                              route.id === "agents" ? { agentsTab: "agents" } : undefined,
                            )
                          }
                          className="adk-mobile-sheet-item"
                        >
                          <span className="adk-mobile-sheet-item-icon" aria-hidden>
                            <Icon size={18} />
                          </span>
                          <span className="min-w-0 flex-1">
                            <span className="adk-mobile-sheet-item-title">
                              {isKo ? route.labelKo : route.labelEn}
                              {badge !== undefined && badge > 0 && (
                                <span className="adk-mobile-sheet-item-badge" aria-hidden>
                                  {formatBadge(badge)}
                                </span>
                              )}
                            </span>
                            <span className="adk-mobile-sheet-item-description">
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
