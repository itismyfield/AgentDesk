import { Suspense, lazy, useEffect, useState } from "react";
import type { CompanySettings } from "../types";
import * as api from "../api";

const OnboardingWizard = lazy(() => import("./OnboardingWizard"));

interface SettingsViewProps {
  settings: CompanySettings;
  onSave: (patch: Record<string, unknown>) => Promise<void>;
  isKo: boolean;
}

// ── Runtime Config field definitions ──

interface ConfigField {
  key: string;
  labelKo: string;
  labelEn: string;
  unit: string;
  min: number;
  max: number;
  step: number;
}

const CATEGORIES: Array<{
  titleKo: string;
  titleEn: string;
  fields: ConfigField[];
}> = [
  {
    titleKo: "폴링 & 타이머",
    titleEn: "Polling & Timers",
    fields: [
      { key: "dispatchPollSec", labelKo: "디스패치 폴링 주기", labelEn: "Dispatch poll interval", unit: "s", min: 5, max: 300, step: 5 },
      { key: "agentSyncSec", labelKo: "에이전트 상태 동기화 주기", labelEn: "Agent status sync interval", unit: "s", min: 30, max: 1800, step: 30 },
      { key: "githubIssueSyncSec", labelKo: "GitHub 이슈 동기화 주기", labelEn: "GitHub issue sync interval", unit: "s", min: 300, max: 7200, step: 60 },
      { key: "claudeRateLimitPollSec", labelKo: "Claude Rate Limit 폴링", labelEn: "Claude rate limit poll", unit: "s", min: 30, max: 1800, step: 30 },
      { key: "codexRateLimitPollSec", labelKo: "Codex Rate Limit 폴링", labelEn: "Codex rate limit poll", unit: "s", min: 30, max: 1800, step: 30 },
      { key: "issueTriagePollSec", labelKo: "이슈 트리아지 주기", labelEn: "Issue triage interval", unit: "s", min: 60, max: 3600, step: 60 },
    ],
  },
  {
    titleKo: "디스패치 제한",
    titleEn: "Dispatch Limits",
    fields: [
      { key: "ceoWarnDepth", labelKo: "CEO 경고 깊이", labelEn: "CEO warning depth", unit: "", min: 1, max: 10, step: 1 },
      { key: "maxRetries", labelKo: "최대 재시도 횟수", labelEn: "Max retries", unit: "", min: 1, max: 10, step: 1 },
    ],
  },
  {
    titleKo: "리뷰",
    titleEn: "Review",
    fields: [
      { key: "reviewReminderMin", labelKo: "리뷰 리마인드 간격", labelEn: "Review reminder interval", unit: "min", min: 5, max: 120, step: 5 },
    ],
  },
  {
    titleKo: "알림 임계값",
    titleEn: "Alert Thresholds",
    fields: [
      { key: "rateLimitWarningPct", labelKo: "Rate Limit 경고 수준", labelEn: "Rate limit warning level", unit: "%", min: 50, max: 99, step: 1 },
      { key: "rateLimitDangerPct", labelKo: "Rate Limit 위험 수준", labelEn: "Rate limit danger level", unit: "%", min: 60, max: 100, step: 1 },
    ],
  },
  {
    titleKo: "캐시 TTL",
    titleEn: "Cache TTL",
    fields: [
      { key: "githubRepoCacheSec", labelKo: "GitHub 레포 캐시", labelEn: "GitHub repo cache", unit: "s", min: 30, max: 1800, step: 30 },
      { key: "rateLimitStaleSec", labelKo: "Rate Limit 캐시 stale 판정", labelEn: "Rate limit cache stale", unit: "s", min: 30, max: 1800, step: 30 },
    ],
  },
];

interface ConfigEntry {
  key: string;
  value: string | null;
  category: string;
  label_ko: string;
  label_en: string;
  default?: string | null;
}

type ConfigEditValue = string | boolean;

const BOOLEAN_CONFIG_KEYS = new Set([
  "review_enabled",
  "counter_model_review_enabled",
  "pm_decision_gate_enabled",
]);

const SYSTEM_CONFIG_DESCRIPTIONS: Record<string, { ko: string; en: string }> = {
  kanban_manager_channel_id: {
    ko: "칸반 상태 변경과 자동화 명령을 수신하는 Discord 채널입니다.",
    en: "Discord channel used for kanban state changes and automation commands.",
  },
  deadlock_manager_channel_id: {
    ko: "교착 상태나 멈춤 감지를 보고하는 Discord 채널입니다.",
    en: "Discord channel that receives deadlock and stalled-work alerts.",
  },
  review_enabled: {
    ko: "리뷰 단계를 전체 파이프라인에 적용할지 결정합니다.",
    en: "Controls whether the review step is enforced across the pipeline.",
  },
  counter_model_review_enabled: {
    ko: "다른 모델을 이용한 교차 리뷰를 자동으로 붙입니다.",
    en: "Automatically adds cross-review using a different model provider.",
  },
  max_review_rounds: {
    ko: "한 작업이 반복 리뷰를 수행할 수 있는 최대 횟수입니다.",
    en: "Maximum number of repeated review rounds allowed for one task.",
  },
  pm_decision_gate_enabled: {
    ko: "PM 판단 게이트를 거쳐야 다음 단계로 전환됩니다.",
    en: "Requires PM decision gate approval before the next transition.",
  },
  server_port: {
    ko: "릴리즈 서버가 바인딩되는 API 포트입니다.",
    en: "API port used by the release server.",
  },
  requested_timeout_min: {
    ko: "requested 상태에서 오래 머무는 카드를 경고하는 기준입니다.",
    en: "Timeout threshold for cards stuck in requested state.",
  },
  in_progress_stale_min: {
    ko: "in_progress 상태가 정체로 간주되는 기준 시간입니다.",
    en: "Threshold for considering in-progress work stale.",
  },
  max_chain_depth: {
    ko: "디스패치가 재귀적으로 이어질 수 있는 최대 깊이입니다.",
    en: "Maximum recursive depth allowed for chained dispatches.",
  },
  context_compact_percent: {
    ko: "컨텍스트를 compact 대상으로 보는 사용률 기준입니다.",
    en: "Usage threshold that triggers context compaction.",
  },
  context_clear_percent: {
    ko: "컨텍스트를 clear 대상으로 보는 잔여율 기준입니다.",
    en: "Remaining-capacity threshold that triggers context clearing.",
  },
  context_clear_idle_minutes: {
    ko: "유휴 상태일 때 컨텍스트를 비우기 전까지 기다리는 시간입니다.",
    en: "Idle duration before clearing context state.",
  },
};

const SYSTEM_CATEGORY_LABELS = {
  pipeline: { ko: "파이프라인", en: "Pipeline" },
  review: { ko: "리뷰", en: "Review" },
  timeout: { ko: "칸반 타임아웃", en: "Kanban Timeouts" },
  dispatch: { ko: "디스패치", en: "Dispatch" },
  context: { ko: "컨텍스트 관리", en: "Context Management" },
  system: { ko: "시스템", en: "System" },
} as const;

const SYSTEM_CATEGORY_DESCRIPTIONS = {
  pipeline: {
    ko: "칸반 진행과 의사결정 흐름에 직접 영향을 주는 값입니다.",
    en: "Values that directly affect kanban flow and decision gates.",
  },
  review: {
    ko: "리뷰 단계의 사용 여부와 반복 횟수를 정의합니다.",
    en: "Defines review enablement and review repetition limits.",
  },
  timeout: {
    ko: "정체 상태 감지와 자동 알림 타이밍을 조정합니다.",
    en: "Tunes stale detection and automatic alert timing.",
  },
  dispatch: {
    ko: "작업 디스패치가 얼마나 깊게 확장될지 제한합니다.",
    en: "Limits how far task dispatching can fan out.",
  },
  context: {
    ko: "컨텍스트 압축과 정리 임계값을 관리합니다.",
    en: "Manages thresholds for context compaction and clearing.",
  },
  system: {
    ko: "서버 자체 동작에 필요한 핵심 시스템 값입니다.",
    en: "Core system values required for server behavior.",
  },
} as const;

function isBooleanConfigKey(key: string): boolean {
  return BOOLEAN_CONFIG_KEYS.has(key);
}

function parseBooleanConfigValue(value: string | boolean | null | undefined): boolean {
  if (typeof value === "boolean") return value;
  const normalized = String(value ?? "").trim().toLowerCase();
  return normalized === "true" || normalized === "1" || normalized === "yes" || normalized === "on";
}

function formatUnit(value: number, unit: string): string {
  if (unit === "s" && value >= 60) {
    const m = Math.floor(value / 60);
    const s = value % 60;
    return s > 0 ? `${m}m${s}s` : `${m}m`;
  }
  if (unit === "min" && value >= 60) {
    const h = Math.floor(value / 60);
    const m = value % 60;
    return m > 0 ? `${h}h${m}m` : `${h}h`;
  }
  return unit ? `${value}${unit}` : `${value}`;
}

export default function SettingsView({
  settings,
  onSave,
  isKo,
}: SettingsViewProps) {
  const [companyName, setCompanyName] = useState(settings.companyName);
  const [ceoName, setCeoName] = useState(settings.ceoName);
  const [language, setLanguage] = useState(settings.language);
  const [theme, setTheme] = useState(settings.theme);
  const [saving, setSaving] = useState(false);
  const tr = (ko: string, en: string) => (isKo ? ko : en);

  // ── Runtime Config state ──
  const [rcValues, setRcValues] = useState<Record<string, number>>({});
  const [rcDefaults, setRcDefaults] = useState<Record<string, number>>({});
  const [rcLoaded, setRcLoaded] = useState(false);
  const [rcSaving, setRcSaving] = useState(false);
  const [rcDirty, setRcDirty] = useState(false);

  // ── kv_meta Config state ──
  const [configEntries, setConfigEntries] = useState<ConfigEntry[]>([]);
  const [configEdits, setConfigEdits] = useState<Record<string, ConfigEditValue>>({});
  const [configSaving, setConfigSaving] = useState(false);
  const [showOnboarding, setShowOnboarding] = useState(false);

  useEffect(() => {
    void api.getRuntimeConfig().then((data) => {
      setRcValues(data?.current ?? {});
      setRcDefaults(data?.defaults ?? {});
      setRcLoaded(true);
    }).catch(() => { setRcLoaded(true); });
    // Load kv_meta config
    void fetch("/api/settings/config", { credentials: "include" })
      .then((r) => r.json())
      .then((d: { entries: ConfigEntry[] }) => setConfigEntries(d.entries || []))
      .catch(() => {});
  }, []);

  const handleSave = async () => {
    setSaving(true);
    try {
      await onSave({ companyName, ceoName, language, theme });
    } finally {
      setSaving(false);
    }
  };

  const handleRcSave = async () => {
    setRcSaving(true);
    try {
      // Only send changed values
      const patch: Record<string, number> = {};
      for (const [key, val] of Object.entries(rcValues)) {
        if (val !== rcDefaults[key]) {
          patch[key] = val;
        }
      }
      // If all values match defaults, send the full object to save explicitly
      const result = await api.saveRuntimeConfig(
        Object.keys(patch).length > 0 ? rcValues : rcValues,
      );
      setRcValues(result?.config ?? rcValues);
      setRcDirty(false);
    } finally {
      setRcSaving(false);
    }
  };

  const handleRcChange = (key: string, value: number) => {
    setRcValues((prev) => ({ ...prev, [key]: value }));
    setRcDirty(true);
  };

  const handleRcReset = (key: string) => {
    if (rcDefaults[key] !== undefined) {
      setRcValues((prev) => ({ ...prev, [key]: rcDefaults[key] }));
      setRcDirty(true);
    }
  };

  const inputStyle = { background: "var(--th-bg-surface)", border: "1px solid var(--th-border)", color: "var(--th-text)" };
  const cardStyle = { background: "var(--th-surface)", border: "1px solid var(--th-border)" };

  return (
    <div
      className="mx-auto h-full max-w-2xl min-w-0 space-y-6 overflow-x-hidden overflow-y-auto p-6 pb-40"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <h2 className="text-xl font-bold" style={{ color: "var(--th-text)" }}>
        {tr("설정", "Settings")}
      </h2>

      <div>
        <h3 className="text-xs font-semibold uppercase mb-2" style={{ color: "var(--th-text-muted)" }}>
          {tr("일반", "General")}
        </h3>
        <div className="space-y-3">
          <div className="rounded-xl p-4" style={cardStyle}>
            <label className="block text-xs font-medium mb-1" style={{ color: "var(--th-text-muted)" }}>
              {tr("회사 이름", "Company Name")}
            </label>
            <input
              type="text"
              value={companyName}
              onChange={(e) => setCompanyName(e.target.value)}
              className="w-full px-3 py-2 rounded-lg text-sm"
              style={inputStyle}
            />
          </div>

          <div className="rounded-xl p-4" style={cardStyle}>
            <label className="block text-xs font-medium mb-1" style={{ color: "var(--th-text-muted)" }}>
              {tr("CEO 이름", "CEO Name")}
            </label>
            <input
              type="text"
              value={ceoName}
              onChange={(e) => setCeoName(e.target.value)}
              className="w-full px-3 py-2 rounded-lg text-sm"
              style={inputStyle}
            />
          </div>

          <div className="grid grid-cols-2 gap-3">
            <div className="rounded-xl p-4" style={cardStyle}>
              <label className="block text-xs font-medium mb-1" style={{ color: "var(--th-text-muted)" }}>
                {tr("언어", "Language")}
              </label>
              <select
                value={language}
                onChange={(e) => setLanguage(e.target.value as typeof language)}
                className="w-full px-3 py-2 rounded-lg text-sm"
                style={inputStyle}
              >
                <option value="ko">한국어</option>
                <option value="en">English</option>
                <option value="ja">日本語</option>
                <option value="zh">中文</option>
              </select>
            </div>

            <div className="rounded-xl p-4" style={cardStyle}>
              <label className="block text-xs font-medium mb-1" style={{ color: "var(--th-text-muted)" }}>
                {tr("테마", "Theme")}
              </label>
              <select
                value={theme}
                onChange={(e) => setTheme(e.target.value as typeof theme)}
                className="w-full px-3 py-2 rounded-lg text-sm"
                style={inputStyle}
              >
                <option value="dark">{tr("다크", "Dark")}</option>
                <option value="light">{tr("라이트", "Light")}</option>
                <option value="auto">{tr("자동 (시스템)", "Auto (System)")}</option>
              </select>
            </div>
          </div>
        </div>
      </div>

      <button
        onClick={handleSave}
        disabled={saving}
        className="px-6 py-2.5 rounded-xl text-sm font-medium bg-indigo-600 text-white hover:bg-indigo-500 disabled:opacity-50 transition-colors"
      >
        {saving ? tr("저장 중...", "Saving...") : tr("저장", "Save")}
      </button>

      {/* ── Runtime Config ── */}
      {rcLoaded && (
        <>
          <div className="border-t pt-6" style={{ borderColor: "var(--th-border)" }}>
            <h2 className="text-xl font-bold mb-1" style={{ color: "var(--th-text)" }}>
              {tr("런타임 설정", "Runtime Config")}
            </h2>
            <p className="text-xs mb-4" style={{ color: "var(--th-text-muted)" }}>
              {tr("변경 즉시 반영 (재시작 불필요)", "Changes apply immediately (no restart needed)")}
            </p>
          </div>

          {CATEGORIES.map((cat) => (
            <div key={cat.titleEn}>
              <h3 className="text-xs font-semibold uppercase mb-2" style={{ color: "var(--th-text-muted)" }}>
                {tr(cat.titleKo, cat.titleEn)}
              </h3>
              <div className="space-y-2">
                {cat.fields.map((f) => {
                  const val = rcValues[f.key] ?? rcDefaults[f.key] ?? 0;
                  const def = rcDefaults[f.key] ?? 0;
                  const isDefault = val === def;

                  return (
                    <div key={f.key} className="rounded-xl p-3" style={cardStyle}>
                      <div className="flex items-center justify-between mb-1">
                        <label className="text-xs font-medium" style={{ color: "var(--th-text-muted)" }}>
                          {tr(f.labelKo, f.labelEn)}
                        </label>
                        <div className="flex items-center gap-2">
                          <span className="text-xs font-mono" style={{ color: isDefault ? "var(--th-text-muted)" : "#fbbf24" }}>
                            {formatUnit(val, f.unit)}
                          </span>
                          {!isDefault && (
                            <button
                              onClick={() => handleRcReset(f.key)}
                              className="text-xs px-1.5 py-0.5 rounded"
                              style={{ color: "var(--th-text-muted)", background: "var(--th-bg-surface)" }}
                              title={`${tr("기본값", "Default")}: ${formatUnit(def, f.unit)}`}
                            >
                              {tr("초기화", "Reset")}
                            </button>
                          )}
                        </div>
                      </div>
                      <div className="flex items-center gap-2">
                        <input
                          type="range"
                          min={f.min}
                          max={f.max}
                          step={f.step}
                          value={val}
                          onChange={(e) => handleRcChange(f.key, Number(e.target.value))}
                          className="flex-1 h-1.5 rounded-full appearance-none cursor-pointer"
                          style={{ accentColor: "#6366f1" }}
                        />
                        <input
                          type="number"
                          min={f.min}
                          max={f.max}
                          step={f.step}
                          value={val}
                          onChange={(e) => {
                            const n = Number(e.target.value);
                            if (Number.isFinite(n) && n >= f.min && n <= f.max) {
                              handleRcChange(f.key, n);
                            }
                          }}
                          className="w-16 px-2 py-1 rounded text-xs text-right font-mono"
                          style={inputStyle}
                        />
                      </div>
                      {!isDefault && (
                        <div className="text-xs mt-0.5" style={{ color: "var(--th-text-muted)" }}>
                          {tr("기본값", "Default")}: {formatUnit(def, f.unit)}
                        </div>
                      )}
                    </div>
                  );
                })}
              </div>
            </div>
          ))}

          <button
            onClick={handleRcSave}
            disabled={rcSaving || !rcDirty}
            className="px-6 py-2.5 rounded-xl text-sm font-medium bg-indigo-600 text-white hover:bg-indigo-500 disabled:opacity-50 transition-colors"
          >
            {rcSaving ? tr("저장 중...", "Saving...") : tr("런타임 설정 저장", "Save Runtime Config")}
          </button>
        </>
      )}

      {/* ── kv_meta Config Section ── */}
      <div className="mt-8">
        <h3 className="text-lg font-semibold mb-4" style={{ color: "var(--th-text-heading)" }}>
          {tr("시스템 설정", "System Config")}
        </h3>
        {configEntries.length === 0 ? (
          <p className="text-sm" style={{ color: "var(--th-text-muted)" }}>{tr("설정 로딩 중...", "Loading config...")}</p>
        ) : (
          <>
            {["pipeline", "review", "timeout", "dispatch", "context", "system"].map((cat) => {
              const items = configEntries.filter((e) => e.category === cat);
              if (items.length === 0) return null;
              const catLabel = SYSTEM_CATEGORY_LABELS[cat as keyof typeof SYSTEM_CATEGORY_LABELS];
              const catDescription = SYSTEM_CATEGORY_DESCRIPTIONS[cat as keyof typeof SYSTEM_CATEGORY_DESCRIPTIONS];
              return (
                <div key={cat} className="mb-4 rounded-2xl border p-4" style={{ borderColor: "rgba(148,163,184,0.16)", background: "rgba(15,23,42,0.12)" }}>
                  <h4 className="text-sm font-medium mb-1" style={{ color: "var(--th-text-secondary)" }}>
                    {catLabel ? tr(catLabel.ko, catLabel.en) : cat}
                  </h4>
                  {catDescription && (
                    <p className="mb-3 text-xs" style={{ color: "var(--th-text-muted)" }}>
                      {tr(catDescription.ko, catDescription.en)}
                    </p>
                  )}
                  <div className="space-y-2">
                    {items.map((entry) => {
                      const description = SYSTEM_CONFIG_DESCRIPTIONS[entry.key];
                      const hasLocalEdit = Object.prototype.hasOwnProperty.call(configEdits, entry.key);
                      const currentValue = hasLocalEdit ? configEdits[entry.key] : (entry.value ?? entry.default ?? "");
                      const defaultLabel = entry.default ?? tr("없음", "None");
                      return (
                        <div key={entry.key} className="rounded-xl border px-4 py-3 space-y-2" style={{ borderColor: "rgba(148,163,184,0.2)", background: "var(--th-surface)" }}>
                          <div className="flex items-start justify-between gap-3">
                            <div className="min-w-0">
                              <div className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
                                {isKo ? entry.label_ko : entry.label_en}
                              </div>
                              {description && (
                                <div className="mt-1 text-xs leading-relaxed" style={{ color: "var(--th-text-muted)" }}>
                                  {isKo ? description.ko : description.en}
                                </div>
                              )}
                            </div>
                            <span className="shrink-0 text-[10px]" style={{ color: "var(--th-text-muted)" }}>{entry.key}</span>
                          </div>

                          {isBooleanConfigKey(entry.key) ? (
                            <button
                              type="button"
                              role="switch"
                              aria-checked={parseBooleanConfigValue(currentValue)}
                              onClick={() => setConfigEdits((prev) => ({ ...prev, [entry.key]: !parseBooleanConfigValue(currentValue) }))}
                              className="flex w-full items-center justify-between rounded-xl border px-3 py-3 text-left transition-colors"
                              style={{
                                borderColor: parseBooleanConfigValue(currentValue) ? "rgba(52,211,153,0.35)" : "rgba(148,163,184,0.24)",
                                background: parseBooleanConfigValue(currentValue) ? "rgba(16,185,129,0.12)" : "rgba(15,23,42,0.2)",
                              }}
                            >
                              <div>
                                <div className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
                                  {parseBooleanConfigValue(currentValue) ? tr("활성화됨", "Enabled") : tr("비활성화됨", "Disabled")}
                                </div>
                                <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                                  {tr(`기본값: ${defaultLabel}`, `Default: ${defaultLabel}`)}
                                </div>
                              </div>
                              <span
                                className="relative inline-flex h-7 w-12 shrink-0 items-center rounded-full transition-colors"
                                style={{ background: parseBooleanConfigValue(currentValue) ? "#10b981" : "rgba(148,163,184,0.32)" }}
                              >
                                <span
                                  className="absolute h-5 w-5 rounded-full bg-white transition-transform"
                                  style={{ transform: parseBooleanConfigValue(currentValue) ? "translateX(1.55rem)" : "translateX(0.3rem)" }}
                                />
                              </span>
                            </button>
                          ) : (
                            <label className="block">
                              <input
                                type="text"
                                className="w-full rounded-lg px-3 py-2 text-sm bg-white/5 border"
                                style={{ borderColor: "rgba(148,163,184,0.24)", color: "var(--th-text-primary)" }}
                                value={String(currentValue)}
                                onChange={(e) => setConfigEdits((prev) => ({ ...prev, [entry.key]: e.target.value }))}
                              />
                              <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                                {tr(`기본값: ${defaultLabel}`, `Default: ${defaultLabel}`)}
                              </div>
                            </label>
                          )}
                        </div>
                      );
                    })}
                  </div>
                </div>
              );
            })}
            <button
              onClick={async () => {
                if (Object.keys(configEdits).length === 0) return;
                setConfigSaving(true);
                try {
                  await fetch("/api/settings/config", {
                    method: "PATCH",
                    credentials: "include",
                    headers: { "Content-Type": "application/json" },
                    body: JSON.stringify(configEdits),
                  });
                  setConfigEdits({});
                  // Reload
                  const r = await fetch("/api/settings/config", { credentials: "include" });
                  const d = await r.json();
                  setConfigEntries(d.entries || []);
                } finally {
                  setConfigSaving(false);
                }
              }}
              disabled={configSaving || Object.keys(configEdits).length === 0}
              className="px-6 py-2.5 rounded-xl text-sm font-medium bg-emerald-600 text-white hover:bg-emerald-500 disabled:opacity-50 transition-colors"
            >
              {configSaving ? tr("저장 중...", "Saving...") : tr("시스템 설정 저장", "Save System Config")}
            </button>
          </>
        )}
      </div>

      {/* Onboarding re-run */}
      <div className="mt-8 pt-6 border-t" style={{ borderColor: "rgba(148,163,184,0.15)" }}>
        <button
          onClick={() => setShowOnboarding(true)}
          className="px-6 py-2.5 rounded-xl text-sm font-medium border hover:bg-surface-subtle transition-colors"
          style={{ borderColor: "rgba(148,163,184,0.3)", color: "var(--th-text-secondary)" }}
        >
          {tr("온보딩 재수행", "Re-run Onboarding")}
        </button>
        <p className="mt-2 text-xs" style={{ color: "var(--th-text-muted)" }}>
          {tr("봇 토큰, 채널, 에이전트 구성을 다시 설정합니다.", "Reconfigure bot token, channels, and agents.")}
        </p>
      </div>

      {showOnboarding && (
        <div className="fixed inset-0 z-50 bg-[#0a0e1a] overflow-y-auto" role="dialog" aria-modal="true" aria-label="Onboarding wizard">
          <div className="min-h-screen flex items-start justify-center pt-8 pb-16">
            <div className="w-full max-w-2xl">
              <div className="flex justify-end px-4 mb-2">
                <button
                  onClick={() => setShowOnboarding(false)}
                  className="text-sm px-4 py-2.5 rounded-lg border min-h-[44px]"
                  style={{ borderColor: "rgba(148,163,184,0.3)", color: "var(--th-text-muted)" }}
                >
                  ✕ {tr("닫기", "Close")}
                </button>
              </div>
              <Suspense fallback={<div className="text-center py-8" style={{ color: "var(--th-text-muted)" }}>Loading...</div>}>
                <OnboardingWizard isKo={isKo} onComplete={() => { setShowOnboarding(false); window.location.reload(); }} />
              </Suspense>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
