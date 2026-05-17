import {
  getCachedSkillCatalog,
  getCachedSkillRanking,
  getCachedTokenAnalytics,
  getSkillCatalog,
  getSkillRanking,
  getTokenAnalytics,
} from "../api/client";

type IdleCallbackHandle = number;
type IdleDeadlineLike = { didTimeout: boolean; timeRemaining: () => number };
type WindowWithIdle = Window & {
  requestIdleCallback?: (
    callback: (deadline: IdleDeadlineLike) => void,
    options?: { timeout?: number },
  ) => IdleCallbackHandle;
  cancelIdleCallback?: (handle: IdleCallbackHandle) => void;
};

let warmupScheduled = false;

export function warmStatsEntryCache(): () => void {
  if (warmupScheduled || typeof window === "undefined") return () => {};
  warmupScheduled = true;

  const run = () => {
    void import("../components/StatsPageView");
    if (!getCachedTokenAnalytics("30d")) {
      void getTokenAnalytics("30d").catch(() => {});
    }
    if (!getCachedSkillRanking("30d", 16)) {
      void getSkillRanking("30d", 16).catch(() => {});
    }
    if (!getCachedSkillCatalog()) {
      void getSkillCatalog().catch(() => {});
    }
  };

  const idleWindow = window as WindowWithIdle;
  if (idleWindow.requestIdleCallback) {
    const handle = idleWindow.requestIdleCallback(run, { timeout: 2_000 });
    return () => idleWindow.cancelIdleCallback?.(handle);
  }

  const timer = window.setTimeout(run, 800);
  return () => window.clearTimeout(timer);
}
