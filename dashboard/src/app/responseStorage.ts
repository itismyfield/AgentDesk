import { STORAGE_KEYS } from "../lib/storageKeys";

// Clear persisted server responses at the same boundary as HTTP/Query caches.
// User-authored drafts and visual preferences have separate ownership.
export function clearDashboardResponseStorage() {
  try {
    for (const key of [STORAGE_KEYS.settingsPipelineRepoCache, STORAGE_KEYS.settingsPipelineAgentCache, STORAGE_KEYS.settingsPipelineVisualCache]) {
      window.localStorage.removeItem(key);
    }
    for (let index = window.sessionStorage.length - 1; index >= 0; index--) {
      const key = window.sessionStorage.key(index);
      if (key?.startsWith("stats:token-analytics:") || key?.startsWith("stats:skill-ranking:")) {
        window.sessionStorage.removeItem(key);
      }
    }
  } catch { /* Storage restrictions do not prevent authentication. */ }
}
