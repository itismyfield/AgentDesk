export type DashboardTab = "operations" | "tokens" | "automation" | "achievements" | "meetings";

export const DASHBOARD_TAB_QUERY_KEY = "dashboardTab";
export const DASHBOARD_TAB_STORAGE_KEY = "agentdesk.dashboard.active-tab";
export const DASHBOARD_TABS: DashboardTab[] = ["operations", "tokens", "automation", "achievements", "meetings"];

export function isDashboardTab(value: string | null | undefined): value is DashboardTab {
  return value != null && DASHBOARD_TABS.includes(value as DashboardTab);
}

function getDashboardTabStorage(): Storage | null {
  if (typeof window === "undefined") return null;
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

export function readDashboardTabFromStorage(): DashboardTab | null {
  const storage = getDashboardTabStorage();
  if (!storage) return null;

  let stored: string | null = null;
  try {
    stored = storage.getItem(DASHBOARD_TAB_STORAGE_KEY);
  } catch {
    return null;
  }

  if (stored === null) return null;
  if (isDashboardTab(stored)) return stored;

  try {
    storage.removeItem(DASHBOARD_TAB_STORAGE_KEY);
  } catch {
    // Ignore storage cleanup failures and fall back to the default tab.
  }
  return null;
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
  const storage = getDashboardTabStorage();
  if (storage) {
    try {
      storage.setItem(DASHBOARD_TAB_STORAGE_KEY, tab);
    } catch {
      // Ignore storage write failures and keep the URL as the source of truth.
    }
  }
  if (currentTab === tab) return;

  const nextUrl = `${url.pathname}${url.search}${url.hash}`;
  if (options.replace) {
    window.history.replaceState(null, "", nextUrl);
    return;
  }
  window.history.pushState(null, "", nextUrl);
}
