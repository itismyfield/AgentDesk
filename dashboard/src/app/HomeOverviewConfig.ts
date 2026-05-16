export const HOME_PRIMARY_WIDGET_IDS = [
  "m_tokens",
  "m_cost",
  "m_progress",
  "m_rate_limit",
  "kanban",
] as const;
export const HOME_SUPPORT_WIDGET_IDS = ["quality", "missions"] as const;
export const HOME_DEFAULT_WIDGETS = [
  ...HOME_PRIMARY_WIDGET_IDS,
  ...HOME_SUPPORT_WIDGET_IDS,
];
export const HOME_PRIMARY_WIDGET_SET = new Set<string>(HOME_PRIMARY_WIDGET_IDS);
export const HOME_SUPPORT_WIDGET_SET = new Set<string>(HOME_SUPPORT_WIDGET_IDS);

const OPERATOR_LEVEL_TITLES_KO = [
  "신입",
  "수습",
  "사원",
  "주임",
  "대리",
  "과장",
  "차장",
  "부장",
  "이사",
  "사장",
];
const OPERATOR_LEVEL_TITLES_EN = [
  "Newbie",
  "Trainee",
  "Staff",
  "Associate",
  "Sr. Associate",
  "Manager",
  "Asst. Dir.",
  "Director",
  "VP",
  "President",
];

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

export function getOperatorLevelTitle(level: number, isKo: boolean) {
  const titles = isKo ? OPERATOR_LEVEL_TITLES_KO : OPERATOR_LEVEL_TITLES_EN;
  const index = Math.max(0, Math.min(level - 1, titles.length - 1));
  return titles[index];
}
