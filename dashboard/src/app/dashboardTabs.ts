export type DashboardTab = "operations" | "tokens" | "automation" | "achievements" | "meetings";

export const DASHBOARD_TAB_QUERY_KEY = "dashboardTab";
export const DASHBOARD_TAB_STORAGE_KEY = "agentdesk.dashboard.active-tab";
export const DASHBOARD_TABS: DashboardTab[] = ["operations", "tokens", "automation", "achievements", "meetings"];

export function isDashboardTab(value: string | null | undefined): value is DashboardTab {
  return value != null && DASHBOARD_TABS.includes(value as DashboardTab);
}

export function readDashboardTabFromUrl(): DashboardTab {
  if (typeof window === "undefined") return "operations";
  const params = new URLSearchParams(window.location.search);
  const fromUrl = params.get(DASHBOARD_TAB_QUERY_KEY);
  if (isDashboardTab(fromUrl)) return fromUrl;

  const stored = window.localStorage.getItem(DASHBOARD_TAB_STORAGE_KEY);
  return isDashboardTab(stored) ? stored : "operations";
}

export function syncDashboardTabToUrl(tab: DashboardTab) {
  if (typeof window === "undefined") return;
  const url = new URL(window.location.href);
  url.searchParams.set(DASHBOARD_TAB_QUERY_KEY, tab);
  window.localStorage.setItem(DASHBOARD_TAB_STORAGE_KEY, tab);
  window.history.replaceState(null, "", `${url.pathname}${url.search}${url.hash}`);
}
