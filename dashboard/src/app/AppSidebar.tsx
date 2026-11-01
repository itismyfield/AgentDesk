import type { Dispatch, SetStateAction } from "react";
import type { LucideIcon } from "lucide-react";
import type { AgentManagerTab } from "../components/agent-manager/useAgentManagerController";
import type { AppRouteEntry, AppRouteId, AppSectionId } from "./routes";

interface AppSidebarProps {
  currentRouteId: AppRouteId | null;
  iconForRoute: (routeId: AppRouteId) => LucideIcon;
  isKo: boolean;
  navigateToRoute: (path: string) => void;
  routeBadge: (routeId: AppRouteId) => number | undefined;
  routes: AppRouteEntry[];
  sections: AppSectionId[];
  setAgentsPageTab: Dispatch<SetStateAction<AgentManagerTab>>;
  wsConnected: boolean;
}

function SidebarRouteButton({
  route,
  currentRouteId,
  iconForRoute,
  isKo,
  badge,
  onNavigate,
}: {
  route: AppRouteEntry;
  currentRouteId: AppRouteId | null;
  iconForRoute: (routeId: AppRouteId) => LucideIcon;
  isKo: boolean;
  badge?: number;
  onNavigate: () => void;
}) {
  const active = currentRouteId === route.id;
  const Icon = iconForRoute(route.id);

  return (
    <button
      type="button"
      onClick={onNavigate}
      className="group relative flex min-h-11 w-full items-center gap-3 rounded-md px-3 py-2 text-left transition-colors"
      style={{
        background: active
          ? "var(--th-overlay-medium)"
          : "transparent",
        color: active ? "var(--th-text-primary)" : "var(--th-text-secondary)",
      }}
      title={isKo ? route.descriptionKo : route.descriptionEn}
      aria-current={active ? "page" : undefined}
    >
      <span
        className="flex h-6 w-6 shrink-0 items-center justify-center"
        style={{
          color: active ? "var(--th-text-primary)" : "var(--th-text-muted)",
        }}
      >
        <Icon size={18} />
      </span>
      <span className="min-w-0 flex-1">
        <span className="block truncate text-sm font-medium">
          {isKo ? route.labelKo : route.labelEn}
        </span>
      </span>
      {badge !== undefined && badge > 0 && (
        <span className="absolute right-3 top-3 flex h-5 min-w-5 items-center justify-center rounded-full bg-emerald-500 px-1 text-[10px] font-semibold text-white">
          {badge > 9 ? "9+" : badge}
        </span>
      )}
    </button>
  );
}

export function AppSidebar({
  currentRouteId,
  iconForRoute,
  isKo,
  navigateToRoute,
  routeBadge,
  routes,
  sections,
  setAgentsPageTab,
  wsConnected,
}: AppSidebarProps) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);

  return (
    <aside
      data-testid="app-sidebar-nav"
      className="flex w-[216px] shrink-0 flex-col border-r"
      style={{
        borderColor: "var(--th-border-subtle)",
        background: "var(--th-bg-surface)",
      }}
    >
      <div
        className="flex h-16 shrink-0 items-center border-b px-4"
        style={{ borderColor: "var(--th-border-subtle)" }}
      >
        <div className="flex items-center gap-2.5">
          <div
            className="flex h-9 w-9 items-center justify-center rounded-xl text-[13px] font-semibold"
            style={{
              background: "var(--th-accent-primary-soft)",
              color: "var(--th-accent-primary)",
            }}
          >
            AD
          </div>
          <div className="min-w-0 leading-tight">
            <div
              className="truncate text-sm font-semibold"
              style={{ color: "var(--th-text-heading)" }}
            >
              AgentDesk
            </div>
            <div
              className="truncate text-[11px]"
              style={{ color: "var(--th-text-muted)" }}
            >
              v2.4.1
            </div>
          </div>
        </div>
      </div>

      <div className="flex-1 overflow-y-auto px-3 py-4">
        {sections.map((sectionId, index) => {
          const sectionRoutes = routes.filter((route) => route.section === sectionId);
          return (
            <div
              key={sectionId}
              className={index > 0 ? "mt-4 border-t border-th-border-subtle pt-4" : undefined}
            >
              <div className="space-y-0.5">
                {sectionRoutes.map((route) => (
                  <SidebarRouteButton
                    key={route.id}
                    route={route}
                    currentRouteId={currentRouteId}
                    iconForRoute={iconForRoute}
                    isKo={isKo}
                    badge={routeBadge(route.id)}
                    onNavigate={() => {
                      if (route.id === "agents") {
                        setAgentsPageTab("agents");
                      }
                      navigateToRoute(route.path);
                    }}
                  />
                ))}
              </div>
            </div>
          );
        })}
      </div>

      <div
        className="border-t px-3 py-4"
        style={{ borderColor: "var(--th-border-subtle)" }}
      >
        <div
          data-testid="sidebar-connection-status"
          className="flex items-center gap-2 rounded-md border px-3 py-2"
          style={{
            borderColor: "var(--th-border-subtle)",
            background: "var(--th-overlay-subtle)",
            color: "var(--th-text-muted)",
          }}
        >
          <span
            className={`h-2 w-2 rounded-full ${wsConnected ? "animate-pulse motion-reduce:animate-none" : ""}`}
            style={{
              background: wsConnected
                ? "var(--th-accent-success)"
                : "var(--th-accent-danger)",
            }}
          />
          <span className="text-xs">
            {wsConnected
              ? tr("실시간 연결됨", "Live connection")
              : tr("연결 끊김", "Disconnected")}
          </span>
        </div>
      </div>
    </aside>
  );
}
