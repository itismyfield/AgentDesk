import { useEffect, useState, type CSSProperties, type Dispatch, type SetStateAction } from "react";
import { Link } from "react-router";
import { useQuery } from "@tanstack/react-query";
import type { LucideIcon } from "lucide-react";
import { getServerVersion } from "../api/settingsRuntime";
import type { AgentManagerTab } from "../components/agent-manager/useAgentManagerController";
import { Cap, CountBadge, ROW_HOVER_CLASS, StatusDot } from "../components/common/KitPrimitives";
import { Separator } from "../kit/free/separator-clean";
import type { AppRouteEntry, AppRouteId, AppSectionId } from "./routes";
import { formatRelativeTime } from "./shellFormatting";

interface AppSidebarProps {
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

const NAV_CLASS = `flex h-10 w-full items-center gap-2.5 rounded-[var(--r-sm)] px-2.5 text-sm leading-5 font-medium text-[var(--fg-dim)] ${ROW_HOVER_CLASS} aria-[current=page]:bg-[var(--bg-3)] aria-[current=page]:text-[var(--fg)] pointer-coarse:h-11`;

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

/** Re-renders every few seconds so the "n seconds ago" text stays honest. */
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

  return (
    <nav
      data-testid="app-sidebar-nav"
      aria-label={tr("주 메뉴", "Main menu")}
      className="flex w-[232px] shrink-0 flex-col gap-1 border-r border-[var(--line-soft)] bg-[var(--bg-1)] p-3"
    >
      <div className="flex shrink-0 items-center gap-2.5 px-2 pt-1.5 pb-3.5">
        <span
          aria-hidden
          className="grid size-8 shrink-0 place-items-center rounded-[var(--r-sm)] bg-[var(--accent-soft)] text-xs leading-none font-bold text-[var(--accent-fg)]"
        >
          AD
        </span>
        <div className="min-w-0">
          <div className="truncate text-sm leading-5 font-semibold text-[var(--fg)]">AgentDesk</div>
          {version ? (
            <Cap className="block truncate">{version.startsWith("v") ? version : `v${version}`}</Cap>
          ) : null}
        </div>
      </div>

      {/* Only the menu scrolls, so brand and connection status stay in view on short windows. */}
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
                return (
                  <Link
                    key={route.id}
                    to={route.path}
                    onClick={() => {
                      if (route.id === "agents") setAgentsPageTab("agents");
                    }}
                    className={NAV_CLASS}
                    title={isKo ? route.descriptionKo : route.descriptionEn}
                    aria-current={currentRouteId === route.id ? "page" : undefined}
                    aria-label={badge ? `${label}, ${tr(`확인할 일 ${badge}건`, `${badge} need attention`)}` : undefined}
                  >
                    <Icon size={18} aria-hidden className="shrink-0" />
                    <span className="min-w-0 flex-1 truncate">{label}</span>
                    {badge ? (
                      <span aria-hidden className="ml-auto flex shrink-0">
                        <CountBadge count={badge} tone="warn" style={RAIL_COUNT_BADGE_STYLE} />
                      </span>
                    ) : null}
                  </Link>
                );
              })}
          </div>
        ))}
      </div>

      <div
        data-testid="sidebar-connection-status"
        className="mt-2 flex shrink-0 items-center gap-2 rounded-[var(--r-sm)] border border-[var(--line-soft)] p-2.5 text-xs leading-4 text-[var(--fg-muted)]"
      >
        <StatusDot tone={wsConnected ? "ok" : "err"} />
        <span className="min-w-0 truncate">
          {wsConnected
            ? lastEvent
              ? tr(`실시간 연결 · 서버 ${lastEvent}`, `Live · server ${lastEvent}`)
              : tr("실시간 연결", "Live")
            : tr("연결 끊김", "Disconnected")}
        </span>
      </div>
    </nav>
  );
}
