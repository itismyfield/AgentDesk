import { useEffect, useState, type CSSProperties, type Dispatch, type SetStateAction } from "react";
import { Link } from "react-router";
import { useQuery } from "@tanstack/react-query";
import { PanelLeftClose, PanelLeftOpen, type LucideIcon } from "lucide-react";
import { getServerVersion } from "../api/settingsRuntime";
import type { AgentManagerTab } from "../components/agent-manager/useAgentManagerController";
import { Cap, CountBadge, ROW_HOVER_CLASS, StatusDot, cn } from "../components/common/KitPrimitives";
import { Separator } from "../kit/free/separator-clean";
import type { AppRouteEntry, AppRouteId, AppSectionId } from "./routes";
import { formatRelativeTime } from "./shellFormatting";

interface AppSidebarProps {
  collapsed?: boolean;
  onToggleCollapsed?: () => void;
  currentRouteId: AppRouteId | null;
  iconForRoute: (routeId: AppRouteId) => LucideIcon;
  isKo: boolean;
  routeBadge: (routeId: AppRouteId) => number | undefined;
  routes: AppRouteEntry[];
  sections: AppSectionId[];
  setAgentsPageTab: Dispatch<SetStateAction<AgentManagerTab>>;
  wsConnected: boolean;
  wsLastEventTs: number | null;
}

const EXPANDED_NAV_CLASS = `flex h-10 w-full items-center gap-2.5 rounded-[var(--r-sm)] px-2.5 text-sm leading-5 font-medium text-[var(--fg-dim)] ${ROW_HOVER_CLASS} aria-[current=page]:bg-[var(--bg-3)] aria-[current=page]:text-[var(--fg)] pointer-coarse:h-11`;
const COLLAPSED_NAV_CLASS = `relative flex h-10 w-full items-center justify-center rounded-[var(--r-sm)] px-0 text-[var(--fg-dim)] ${ROW_HOVER_CLASS} aria-[current=page]:bg-[var(--bg-3)] aria-[current=page]:text-[var(--fg)] pointer-coarse:h-11`;

const RAIL_SEPARATOR_STYLE = {
  "--ui-sepc-line": "var(--line-soft)",
  "--ui-sepc-weight": "1px",
  margin: "8px 4px",
  width: "calc(100% - 8px)",
} as CSSProperties;

const RAIL_COUNT_BADGE_STYLE = {
  fontSize: "11px",
  fontWeight: 600,
  paddingInline: "6px",
} as CSSProperties;

const COLLAPSED_BADGE_STYLE = {
  "--h": "15px",
  padding: "0 4px",
  fontSize: "10px",
  fontWeight: 600,
  boxShadow: "0 0 0 2px var(--bg-1)",
} as CSSProperties;

function useNow(active: boolean, tickMs = 5_000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    const id = window.setInterval(() => setNow(Date.now()), tickMs);
    return () => window.clearInterval(id);
  }, [active, tickMs]);
  return now;
}

export function AppSidebar({
  collapsed = false,
  onToggleCollapsed,
  currentRouteId,
  iconForRoute,
  isKo,
  routeBadge,
  routes,
  sections,
  setAgentsPageTab,
  wsConnected,
  wsLastEventTs,
}: AppSidebarProps) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const version = useQuery({ queryKey: ["shell", "server-version"], queryFn: getServerVersion, staleTime: Infinity, retry: false }).data;
  useNow(wsConnected && wsLastEventTs != null);
  const lastEvent = wsConnected && wsLastEventTs != null ? formatRelativeTime(wsLastEventTs, isKo) : null;
  const statusText = wsConnected
    ? lastEvent
      ? tr(`실시간 연결 · 서버 ${lastEvent}`, `Live · server ${lastEvent}`)
      : tr("실시간 연결", "Live")
    : tr("연결 끊김", "Disconnected");
  const toggleLabel = collapsed
    ? tr("사이드바 펼치기", "Expand sidebar")
    : tr("사이드바 접기", "Collapse sidebar");

  return (
    <nav
      data-testid="app-sidebar-nav"
      data-collapsed={collapsed || undefined}
      aria-label={tr("주 메뉴", "Main menu")}
      className={cn(
        "flex shrink-0 flex-col gap-1 border-r border-[var(--line-soft)] bg-[var(--bg-1)] transition-[width] duration-150 motion-reduce:transition-none",
        collapsed ? "w-16 px-2 py-3" : "w-[232px] p-3",
      )}
    >
      <div
        className={cn(
          "flex shrink-0 items-center pb-3",
          collapsed ? "flex-col gap-2 pt-0.5" : "gap-2.5 px-2 pt-1.5 pb-3.5",
        )}
      >
        <span
          aria-hidden
          className="grid size-8 shrink-0 place-items-center rounded-[var(--r-sm)] bg-[var(--accent-soft)] text-xs leading-none font-bold text-[var(--accent-fg)]"
        >
          AD
        </span>
        {!collapsed ? (
          <div className="min-w-0 flex-1">
            <div className="truncate text-sm leading-5 font-semibold text-[var(--fg)]">AgentDesk</div>
            {version ? (
              <Cap className="block truncate">{version.startsWith("v") ? version : `v${version}`}</Cap>
            ) : null}
          </div>
        ) : null}
        {onToggleCollapsed ? (
          <button
            type="button"
            data-testid="app-sidebar-toggle"
            aria-expanded={!collapsed}
            aria-label={toggleLabel}
            title={toggleLabel}
            onClick={onToggleCollapsed}
            className={cn(
              "grid size-8 shrink-0 place-items-center rounded-[var(--r-sm)] border-0 bg-transparent text-[var(--fg-muted)] pointer-coarse:size-11",
              ROW_HOVER_CLASS,
              "hover:text-[var(--fg)]",
            )}
          >
            {collapsed ? <PanelLeftOpen size={16} aria-hidden /> : <PanelLeftClose size={16} aria-hidden />}
          </button>
        ) : null}
      </div>

      <div data-testid="app-sidebar-menu" className="-m-1 flex min-h-0 flex-1 flex-col gap-1 overflow-y-auto p-1">
        {sections.map((sectionId, index) => (
          <div key={sectionId} className="flex flex-col gap-1">
            {index > 0 ? <Separator style={RAIL_SEPARATOR_STYLE} /> : null}
            {routes
              .filter((route) => route.section === sectionId)
              .map((route) => {
                const Icon = iconForRoute(route.id);
                const badge = routeBadge(route.id);
                const label = isKo ? route.labelKo : route.labelEn;
                const ariaLabel = badge
                  ? `${label}, ${tr(`확인할 일 ${badge}건`, `${badge} need attention`)}`
                  : collapsed
                    ? label
                    : undefined;
                return (
                  <Link
                    key={route.id}
                    to={route.path}
                    onClick={() => {
                      if (route.id === "agents") setAgentsPageTab("agents");
                    }}
                    className={collapsed ? COLLAPSED_NAV_CLASS : EXPANDED_NAV_CLASS}
                    title={collapsed ? label : (isKo ? route.descriptionKo : route.descriptionEn)}
                    aria-current={currentRouteId === route.id ? "page" : undefined}
                    aria-label={ariaLabel}
                  >
                    <Icon size={18} aria-hidden className="shrink-0" />
                    {collapsed ? (
                      badge ? (
                        <span aria-hidden className="pointer-events-none absolute top-0.5 right-0.5 flex">
                          <CountBadge count={badge} max={9} tone="warn" style={COLLAPSED_BADGE_STYLE} />
                        </span>
                      ) : null
                    ) : (
                      <>
                        <span className="min-w-0 flex-1 truncate">{label}</span>
                        {badge ? (
                          <span aria-hidden className="ml-auto flex shrink-0">
                            <CountBadge count={badge} tone="warn" style={RAIL_COUNT_BADGE_STYLE} />
                          </span>
                        ) : null}
                      </>
                    )}
                  </Link>
                );
              })}
          </div>
        ))}
      </div>

      <div
        data-testid="sidebar-connection-status"
        title={statusText}
        className={cn(
          "mt-2 flex shrink-0 items-center rounded-[var(--r-sm)] border border-[var(--line-soft)] text-xs leading-4 text-[var(--fg-muted)]",
          collapsed ? "justify-center p-2.5" : "gap-2 p-2.5",
        )}
      >
        <StatusDot tone={wsConnected ? "ok" : "err"} />
        {collapsed ? (
          <span className="sr-only">{statusText}</span>
        ) : (
          <span className="min-w-0 truncate">{statusText}</span>
        )}
      </div>
    </nav>
  );
}
