export const DANGEROUS_CONFIG_KEY_DETAILS = {
  review_enabled: {
    ko: "리뷰 게이트",
    en: "Review gate",
  },
  pm_decision_gate_enabled: {
    ko: "PM 판단 게이트",
    en: "PM decision gate",
  },
  context_clear_percent: {
    ko: "컨텍스트 초기화 기준",
    en: "Context clear threshold",
  },
  context_clear_idle_minutes: {
    ko: "컨텍스트 초기화 대기 시간",
    en: "Context clear idle window",
  },
} as const;

export type DangerousConfigKey = keyof typeof DANGEROUS_CONFIG_KEY_DETAILS;

export function isDangerousConfigKey(key: string): key is DangerousConfigKey {
  return Object.prototype.hasOwnProperty.call(DANGEROUS_CONFIG_KEY_DETAILS, key);
}

export function getDangerousConfigKeys(edits: Record<string, unknown>): DangerousConfigKey[] {
  return Object.keys(edits).filter(isDangerousConfigKey);
}

export function getDangerousConfigLabel(key: string, isKo: boolean): string {
  if (!isDangerousConfigKey(key)) return key;
  const detail = DANGEROUS_CONFIG_KEY_DETAILS[key];
  return isKo ? detail.ko : detail.en;
}
