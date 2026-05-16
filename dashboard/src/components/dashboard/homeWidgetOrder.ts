export type HomeWidgetId =
  | "metric_agents"
  | "metric_dispatch"
  | "metric_review"
  | "metric_followups"
  | "office"
  | "signals"
  | "quality"
  | "roster"
  | "activity";

export const HOME_WIDGET_STORAGE_KEY = "agentdesk.dashboard.home.widgets.v1";

export const DEFAULT_HOME_WIDGET_ORDER: HomeWidgetId[] = [
  "metric_agents",
  "metric_dispatch",
  "metric_review",
  "metric_followups",
  "office",
  "signals",
  "quality",
  "roster",
  "activity",
];

export function normalizeHomeWidgetOrder(value: unknown): HomeWidgetId[] {
  if (!Array.isArray(value)) return DEFAULT_HOME_WIDGET_ORDER;
  const valid = new Set<HomeWidgetId>(DEFAULT_HOME_WIDGET_ORDER);
  const next: HomeWidgetId[] = [];
  for (const item of value) {
    if (typeof item !== "string" || !valid.has(item as HomeWidgetId) || next.includes(item as HomeWidgetId)) {
      continue;
    }
    next.push(item as HomeWidgetId);
  }
  for (const item of DEFAULT_HOME_WIDGET_ORDER) {
    if (!next.includes(item)) next.push(item);
  }
  return next;
}

export function readStoredHomeWidgetOrder(storage: Storage | null | undefined): HomeWidgetId[] {
  if (!storage) return DEFAULT_HOME_WIDGET_ORDER;
  try {
    const raw = storage.getItem(HOME_WIDGET_STORAGE_KEY);
    return raw ? normalizeHomeWidgetOrder(JSON.parse(raw)) : DEFAULT_HOME_WIDGET_ORDER;
  } catch {
    return DEFAULT_HOME_WIDGET_ORDER;
  }
}
