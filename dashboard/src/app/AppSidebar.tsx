import type { Dispatch, SetStateAction } from "react";
import type { LucideIcon } from "lucide-react";
import { LevelRing } from "../components/gamification/GamificationShared";
import type { AppRouteEntry, AppRouteId } from "./routes";

type AgentsPageTab = "agents" | "departments" | "backlog" | "dispatch";

interface SidebarSection {
  id: "workspace" | "extensions" | "me";
  labelKo: string;
  labelEn: string;
}

interface AppSidebarProps {
  currentRouteId: AppRouteId | null;
  currentUserDetail: string;
  currentUserLabel: string;
  currentUserProgress: number;
  iconForRoute: (routeId: AppRouteId) => LucideIcon;
  isKo: boolean;
  navigateToRoute: (path: string) => void;
  routeBadge: (routeId: AppRouteId) => number | undefined;
  routes: AppRouteEntry[];
  sections: SidebarSection[];
  setAgentsPageTab: Dispatch<SetStateAction<AgentsPageTab>>;
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
      className="group relative flex w-full items-center gap-3 rounded-xl px-3 py-2.5 text-left transition-colors"
      style={{
        background: active
          ? "color-mix(in srgb, var(--th-accent-primary-soft) 80%, transparent)"
          : "transparent",
        color: active ? "var(--th-text-primary)" : "var(--th-text-secondary)",
      }}
      title={isKo ? route.labelKo : route.labelEn}
    >
      <span
        className="flex h-9 w-9 shrink-0 items-center justify-center rounded-xl"
        style={{
          background: active
            ? "color-mix(in srgb, var(--th-accent-primary) 18%, transparent)"
            : "var(--th-overlay-subtle)",
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
  currentUserDetail,
  currentUserLabel,
  currentUserProgress,
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
      className="flex w-[236px] shrink-0 flex-col border-r"
      style={{
        borderColor: "var(--th-border-subtle)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-nav-bg) 98%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 92%, transparent) 100%)",
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
        {sections.map((section) => {
          const sectionRoutes = routes.filter((route) => route.section === section.id);
          return (
            <div key={section.id} className="mb-5">
              <div
                className="px-3 pb-2 text-[11px] font-semibold uppercase tracking-[0.18em]"
                style={{ color: "var(--th-text-muted)" }}
              >
                {isKo ? section.labelKo : section.labelEn}
              </div>
              <div className="space-y-1">
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
        <div className="space-y-2">
          <div
            className="flex items-center gap-2 rounded-2xl border px-3 py-2"
            style={{
              borderColor: "var(--th-border-subtle)",
              background: "var(--th-overlay-subtle)",
              color: "var(--th-text-muted)",
            }}
          >
            <span
              className={`h-2 w-2 rounded-full ${wsConnected ? "animate-pulse" : ""}`}
              style={{
                background: wsConnected
                  ? "var(--th-accent-success)"
                  : "var(--th-accent-danger)",
              }}
            />
            <span className="font-mono text-[11px]">
              {wsConnected
                ? tr("2/2 providers", "2/2 providers")
                : tr("0/2 providers", "0/2 providers")}
            </span>
          </div>

          <div
            className="flex items-center gap-3 rounded-2xl border px-3 py-3"
            style={{
              borderColor: "var(--th-border-subtle)",
              background:
                "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
            }}
          >
            <div className="shrink-0">
              <LevelRing
                dataTestId="sidebar-user-level-ring"
                value={Math.round(currentUserProgress * 100)}
                size={40}
                stroke={3}
                color="var(--th-accent-primary)"
                trackColor="color-mix(in srgb, var(--th-overlay-medium) 86%, transparent)"
              >
                <div
                  className="flex h-8 w-8 items-center justify-center rounded-full text-[11px] font-semibold"
                  style={{
                    background: "var(--th-overlay-subtle)",
                    color: "var(--th-text-primary)",
                  }}
                >
                  AD
                </div>
              </LevelRing>
            </div>
            <div className="min-w-0">
              <div
                className="truncate text-sm font-medium"
                style={{ color: "var(--th-text-heading)" }}
              >
                {currentUserLabel}
              </div>
              <div
                className="truncate text-[11px]"
                style={{ color: "var(--th-text-muted)" }}
              >
                {currentUserDetail}
              </div>
            </div>
          </div>
        </div>
      </div>
    </aside>
  );
}
