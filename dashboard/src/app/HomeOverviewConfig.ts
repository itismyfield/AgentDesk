export const HOME_DEFAULT_WIDGETS = [
  "working",
  "attention",
  "m_tokens",
  "m_cost",
  "m_rate_limit",
  "routines",
] as const;

export function areStringArraysEqual(left: readonly string[], right: readonly string[]) {
  if (left.length !== right.length) return false;
  return left.every((value, index) => value === right[index]);
}

export function normalizeHomeWidgetOrder(
  value: unknown,
  defaults: readonly string[] = HOME_DEFAULT_WIDGETS,
) {
  const allowed = new Set(defaults);
  const normalized: string[] = [];
  if (Array.isArray(value)) {
    value.forEach((entry) => {
      if (typeof entry !== "string" || !allowed.has(entry) || normalized.includes(entry)) {
        return;
      }
      normalized.push(entry);
    });
  }
  defaults.forEach((widgetId) => {
    if (!normalized.includes(widgetId)) {
      normalized.push(widgetId);
    }
  });
  return normalized;
}
