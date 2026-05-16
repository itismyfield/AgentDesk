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
        className="fixed bottom-0 left-0 right-0 flex items-start justify-around border-t"
        style={{
          height: tabbarHeight,
          zIndex: tabbarZIndex,
          borderColor: "var(--th-border-subtle)",
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 98%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
          backdropFilter: "blur(18px)",
          boxShadow: "0 -10px 30px -20px color-mix(in srgb, black 70%, transparent)",
          paddingBottom: "env(safe-area-inset-bottom)",
          paddingLeft: "env(safe-area-inset-left)",
          paddingRight: "env(safe-area-inset-right)",
        }}
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
              aria-label={tr(`${isKo ? route.labelKo : route.labelEn} 열기`, `Open ${route.labelEn}`)}
              aria-current={isActive ? "page" : undefined}
              onClick={() => navigateToRoute(route.path)}
              className="relative flex h-16 min-w-0 flex-1 flex-col items-center justify-center gap-1 text-[11.5px] font-medium leading-none"
              style={{
                color: isActive
                  ? "var(--th-accent-primary)"
                  : "var(--th-text-muted)",
              }}
            >
              <Icon size={20} />
              <span className="max-w-full truncate px-1">
                {isKo ? route.labelKo : route.labelEn}
              </span>
              {badge !== undefined && badge > 0 && (
                <span className="absolute right-[28%] top-1 flex h-4 min-w-4 items-center justify-center rounded-full bg-emerald-500 px-1 text-[8px] font-semibold text-white">
                  {badge > 9 ? "9+" : badge}
                </span>
              )}
            </button>
          );
        })}
        <button
          type="button"
          data-testid="app-mobile-more-button"
          aria-haspopup="dialog"
          aria-expanded={moreOpen}
          aria-controls={moreOpen ? "app-mobile-more-menu" : undefined}
          onClick={() => setMoreOpen((prev) => !prev)}
          className="relative flex h-16 min-w-0 flex-1 flex-col items-center justify-center gap-1 text-[11.5px] font-medium leading-none"
          style={{
            color:
              activeRouteId === "more"
                ? "var(--th-accent-primary)"
                : "var(--th-text-muted)",
          }}
        >
          <Settings size={20} />
          <span className="max-w-full truncate px-1">{tr("설정", "Settings")}</span>
          {moreBadge > 0 && (
            <span className="absolute right-[28%] top-1 flex h-4 min-w-4 items-center justify-center rounded-full bg-emerald-500 px-1 text-[8px] font-semibold text-white">
              {moreBadge > 9 ? "9+" : moreBadge}
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
          <div className="absolute inset-0 bg-black/55 backdrop-blur-sm" />
          <div
            ref={moreMenuRef}
            id="app-mobile-more-menu"
            data-testid="app-mobile-more-menu"
            role="dialog"
            aria-modal="true"
            aria-label={tr("확장 메뉴", "Extensions menu")}
            tabIndex={-1}
            className="relative w-full max-h-[80vh] overflow-y-auto rounded-t-[2rem] border px-4 pb-4 pt-3 shadow-2xl animate-in fade-in slide-in-from-bottom-4 duration-200"
            style={{
              borderColor: "var(--th-border-subtle)",
              background:
                "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 98%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 95%, transparent) 100%)",
              paddingBottom:
                "max(1rem, calc(1rem + env(safe-area-inset-bottom)))",
            }}
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                event.preventDefault();
                setMoreOpen(false);
              }
            }}
            onClick={(event) => event.stopPropagation()}
          >
            <div className="mx-auto mb-3 h-1.5 w-14 rounded-full bg-white/10" />
            <div className="mb-3 flex items-center justify-between gap-3">
              <div>
                <div
                  className="text-[11px] font-semibold uppercase tracking-[0.2em]"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {tr("확장", "Extensions")}
                </div>
                <div
                  className="mt-1 text-base font-semibold"
                  style={{ color: "var(--th-text-heading)" }}
                >
                  {tr("숨겨진 페이지 바로가기", "Jump to secondary pages")}
                </div>
              </div>
              <button
                type="button"
                onClick={() => setMoreOpen(false)}
                className="flex h-10 w-10 items-center justify-center rounded-xl border text-[var(--th-text-muted)]"
                style={{
                  borderColor:
                    "color-mix(in srgb, var(--th-border) 64%, transparent)",
                  background:
                    "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
                }}
                aria-label={tr("더보기 닫기", "Close more menu")}
              >
                <X size={16} />
              </button>
            </div>

            <div className="space-y-4">
              {moreSections.map((section) => (
                <div key={section.id} className="space-y-2">
                  <div
                    className="px-1 text-[11px] font-semibold uppercase tracking-[0.18em]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
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
                          className="flex items-start gap-3 rounded-xl border px-3 py-3 text-left"
                          style={{
                            borderColor: "var(--th-border-subtle)",
                            background: "var(--th-overlay-subtle)",
                          }}
                        >
                          <span className="flex h-10 w-10 items-center justify-center rounded-2xl bg-[var(--th-overlay-subtle)]">
                            <Icon size={18} />
                          </span>
                          <span className="min-w-0 flex-1">
                            <span
                              className="flex items-center gap-2 text-sm font-semibold"
                              style={{ color: "var(--th-text-heading)" }}
                            >
                              {isKo ? route.labelKo : route.labelEn}
                              {badge !== undefined && badge > 0 && (
                                <span className="inline-flex h-5 min-w-5 items-center justify-center rounded-full bg-emerald-500 px-1.5 text-[10px] text-white">
                                  {badge > 9 ? "9+" : badge}
                                </span>
                              )}
                            </span>
                            <span
                              className="mt-1 block text-xs leading-relaxed"
                              style={{ color: "var(--th-text-muted)" }}
                            >
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
