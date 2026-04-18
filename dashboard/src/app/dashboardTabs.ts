import { STORAGE_KEYS } from "../lib/storageKeys";
import {
  readLocalStorageValue,
  writeLocalStorageValue,
} from "../lib/useLocalStorage";

export type DashboardTab = "operations" | "tokens" | "automation" | "achievements" | "meetings";

export const DASHBOARD_TAB_QUERY_KEY = "dashboardTab";
export const DASHBOARD_TAB_STORAGE_KEY = STORAGE_KEYS.dashboardActiveTab;
export const DASHBOARD_TABS: DashboardTab[] = ["operations", "tokens", "automation", "achievements", "meetings"];

export function isDashboardTab(value: string | null | undefined): value is DashboardTab {
  return value != null && DASHBOARD_TABS.includes(value as DashboardTab);
}

export function readDashboardTabFromStorage(): DashboardTab | null {
  return readLocalStorageValue<DashboardTab | null>(DASHBOARD_TAB_STORAGE_KEY, null, {
    validate: (value): value is DashboardTab => typeof value === "string" && isDashboardTab(value),
    legacy: (raw) => (isDashboardTab(raw) ? raw : null),
  });
}

export function readDashboardTabFromUrl(): DashboardTab {
  if (typeof window === "undefined") return "operations";
  const params = new URLSearchParams(window.location.search);
  const fromUrl = params.get(DASHBOARD_TAB_QUERY_KEY);
  if (isDashboardTab(fromUrl)) return fromUrl;

  return readDashboardTabFromStorage() ?? "operations";
}

export function syncDashboardTabToUrl(
  tab: DashboardTab,
  options: { replace?: boolean } = {},
) {
  if (typeof window === "undefined") return;
  const url = new URL(window.location.href);
  const currentTab = url.searchParams.get(DASHBOARD_TAB_QUERY_KEY);
  url.searchParams.set(DASHBOARD_TAB_QUERY_KEY, tab);
  writeLocalStorageValue(DASHBOARD_TAB_STORAGE_KEY, tab);
  if (currentTab === tab) return;

  const nextUrl = `${url.pathname}${url.search}${url.hash}`;
  if (options.replace) {
    window.history.replaceState(null, "", nextUrl);
    return;
  }
  window.history.pushState(null, "", nextUrl);
}
