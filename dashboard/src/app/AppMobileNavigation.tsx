import { MoreHorizontal, Moon, Palette, Sun, X, type LucideIcon } from "lucide-react";
import type { CSSProperties, Dispatch, RefObject, SetStateAction } from "react";
import type { AgentManagerTab } from "../components/agent-manager/useAgentManagerController";
import { CountBadge, Desc, RowButton } from "../components/common/KitPrimitives";
import { IconButton } from "../kit/free/icon-button-clean";
import { Separator } from "../kit/free/separator-clean";
import type { AppRouteEntry, AppRouteId } from "./routes";
import type { ThemePreference } from "./themePreferences";

type MobileRouteId = AppRouteId | "more";

interface AppMobileNavigationProps {
  activeRouteId: MobileRouteId;
  bottomSheetZIndex: number;
  currentRouteId: AppRouteId | null;
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
  "relative flex min-h-11 min-w-0 flex-col items-center justify-center gap-[3px] px-1 text-[var(--fg-muted)] transition-colors data-[active]:text-[var(--accent-fg)] focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--accent)]";

const MOBILE_TAB_BADGE_STYLE = {
  "--h": "16px",
  padding: "0 4px",
  fontSize: "10px",
  fontWeight: 600,
  boxShadow: "0 0 0 2px var(--bg-1)",
} as CSSProperties;

const MORE_BADGE_STYLE = {
  fontSize: "11px",
  fontWeight: 600,
  paddingInline: "6px",
} as CSSProperties;

const SHEET_SEPARATOR_STYLE = {
  "--ui-sepc-line": "var(--line-soft)",
  "--ui-sepc-weight": "1px",
} as CSSProperties;

const CLOSE_ICON_BUTTON_STYLE = {
  "--size": "44px",
  "--icon": "16px",
  "--radius": "var(--r-sm)",
  "--ui-icbc-icon": "var(--fg-dim)",
  "--ui-icbc-ink": "var(--fg)",
  "--ui-icbc-wash": "color-mix(in oklch, var(--fg) 5%, transparent)",
  "--ui-icbc-pressed": "var(--bg-3)",
  "--ui-icbc-focus": "var(--accent)",
} as CSSProperties;

function badgeAriaSuffix(isKo: boolean, badge: number | undefined): string {
  if (!badge || badge <= 0) return "";
  return isKo ? `, 확인할 일 ${badge}건` : `, ${badge} need attention`;
}

export function AppMobileNavigation({
  activeRouteId,
  bottomSheetZIndex,
  currentRouteId,
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
  const autoFocusRouteId = moreRoutes.some((route) => route.id === currentRouteId)
    ? currentRouteId
    : moreRoutes[0]?.id;
  const themeLabel =
    resolvedTheme === "dark" ? tr("밝은 화면으로", "Light theme") : tr("어두운 화면으로", "Dark theme");

  return (
    <>
      <nav
        data-testid="app-mobile-tabbar"
        className="fixed inset-x-0 bottom-0 grid grid-cols-5 border-t border-[var(--line-soft)] bg-[var(--bg-1)] pb-[max(6px,env(safe-area-inset-bottom))] pl-[env(safe-area-inset-left)] pr-[env(safe-area-inset-right)]"
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
              onClick={() => navigateToRoute(route.path, route.id === "agents" ? { agentsTab: "agents" } : undefined)}
              className={TAB_CLASS}
            >
              <Icon size={20} aria-hidden />
              <span className="max-w-full truncate text-[11px] leading-4">{label}</span>
              {badge !== undefined && badge > 0 && (
                <span className="pointer-events-none absolute top-1 left-[calc(50%+2px)] flex" aria-hidden>
                  <CountBadge count={badge} max={9} tone="warn" style={MOBILE_TAB_BADGE_STYLE} />
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
          aria-label={tr("더 보기", "More") + badgeAriaSuffix(isKo, moreBadge)}
          onClick={() => setMoreOpen((prev) => !prev)}
          className={TAB_CLASS}
        >
          <MoreHorizontal size={20} aria-hidden />
          <span className="max-w-full truncate text-[11px] leading-4">{tr("더 보기", "More")}</span>
          {moreBadge > 0 && (
            <span className="pointer-events-none absolute top-1 left-[calc(50%+2px)] flex" aria-hidden>
              <CountBadge count={moreBadge} max={9} tone="warn" style={MOBILE_TAB_BADGE_STYLE} />
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
            aria-label={tr("더 보기", "More")}
            tabIndex={-1}
            className="relative max-h-[84dvh] w-full overflow-y-auto rounded-t-[20px] border border-b-0 border-[var(--line-soft)] bg-[var(--bg-2)] pt-2 pb-[max(1rem,env(safe-area-inset-bottom))] text-[var(--fg)] shadow-[var(--shadow-md)]"
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                event.preventDefault();
                setMoreOpen(false);
              }
            }}
            onClick={(event) => event.stopPropagation()}
          >
            <div className="mx-auto mb-2 h-1 w-9 rounded-full bg-[var(--line)]" aria-hidden />
            <div className="flex items-center justify-between gap-3 pr-2 pb-2 pl-4">
              <div className="text-base leading-6 font-semibold text-[var(--fg)]">{tr("더 보기", "More")}</div>
              <IconButton
                variant="ghost"
                label={tr("더 보기 닫기", "Close")}
                icon={<X />}
                style={CLOSE_ICON_BUTTON_STYLE}
                onClick={() => setMoreOpen(false)}
              />
            </div>

            <Separator style={SHEET_SEPARATOR_STYLE} />

            <div>
              {moreRoutes.map((route) => {
                const Icon = iconForRoute(route.id);
                const badge = routeBadge(route.id);
                const label = isKo ? route.labelKo : route.labelEn;
                const isCurrent = currentRouteId === route.id;
                return (
                  <RowButton
                    key={route.id}
                    data-testid={`app-mobile-more-${route.id}`}
                    data-autofocus={route.id === autoFocusRouteId || undefined}
                    selected={isCurrent}
                    aria-label={label + badgeAriaSuffix(isKo, badge)}
                    aria-current={isCurrent ? "page" : undefined}
                    onClick={() =>
                      navigateToRoute(route.path, route.id === "agents" ? { agentsTab: "agents" } : undefined)
                    }
                    className="border-t border-[var(--line-soft)] px-4 first:border-t-0"
                  >
                    <span className="flex size-6 shrink-0 items-center justify-center text-[var(--fg-muted)]" aria-hidden>
                      <Icon size={18} />
                    </span>
                    <span className="min-w-0 flex-1">
                      <span className="flex items-center gap-2 text-sm leading-5 font-medium text-[var(--fg)]">
                        {label}
                        {badge !== undefined && badge > 0 && (
                          <CountBadge count={badge} tone="warn" style={MORE_BADGE_STYLE} />
                        )}
                      </span>
                      <Desc className="mt-0.5 block">
                        {isKo ? route.descriptionKo : route.descriptionEn}
                      </Desc>
                    </span>
                  </RowButton>
                );
              })}
            </div>

            <Separator style={SHEET_SEPARATOR_STYLE} />

            <div>
              <RowButton
                data-testid="app-mobile-more-theme"
                onClick={toggleShellTheme}
                className="px-4"
              >
                <span className="flex size-6 shrink-0 items-center justify-center text-[var(--fg-muted)]" aria-hidden>
                  {resolvedTheme === "dark" ? <Sun size={18} /> : <Moon size={18} />}
                </span>
                <span className="text-sm leading-5 font-medium text-[var(--fg)]">{themeLabel}</span>
              </RowButton>
              <RowButton
                data-testid="app-mobile-more-tweaks"
                onClick={() => {
                  setMoreOpen(false);
                  setShowTweaksPanel(true);
                }}
                className="border-t border-[var(--line-soft)] px-4"
              >
                <span className="flex size-6 shrink-0 items-center justify-center text-[var(--fg-muted)]" aria-hidden>
                  <Palette size={18} />
                </span>
                <span className="text-sm leading-5 font-medium text-[var(--fg)]">{tr("화면 꾸미기", "Appearance")}</span>
              </RowButton>
            </div>
          </div>
        </div>
      )}
    </>
  );
}
