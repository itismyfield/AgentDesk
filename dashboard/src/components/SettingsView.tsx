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
  interface ConfigEntry { key: string; value: string | null; category: string; label_ko: string; label_en: string; }
  const [configEntries, setConfigEntries] = useState<ConfigEntry[]>([]);
  const [configEdits, setConfigEdits] = useState<Record<string, string>>({});
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
      className="p-6 max-w-2xl mx-auto space-y-6 overflow-auto h-full pb-40"
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
              const catLabel: Record<string, string> = {
                pipeline: tr("파이프라인", "Pipeline"),
                review: tr("리뷰", "Review"),
                timeout: tr("칸반 타임아웃", "Kanban Timeouts"),
                dispatch: tr("디스패치", "Dispatch"),
                context: tr("컨텍스트 관리", "Context Management"),
                system: tr("시스템", "System"),
              };
              return (
                <div key={cat} className="mb-4">
                  <h4 className="text-sm font-medium mb-2" style={{ color: "var(--th-text-secondary)" }}>
                    {catLabel[cat] || cat}
                  </h4>
                  <div className="space-y-2">
                    {items.map((entry) => (
                      <div key={entry.key} className="rounded-xl border px-4 py-3 space-y-1.5" style={{ borderColor: "rgba(148,163,184,0.2)" }}>
                        <div className="flex items-center justify-between gap-2">
                          <span className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
                            {isKo ? entry.label_ko : entry.label_en}
                          </span>
                          <span className="text-xs shrink-0" style={{ color: "var(--th-text-muted)" }}>{entry.key}</span>
                        </div>
                        <input
                          type="text"
                          className="w-full rounded-lg px-3 py-2 text-sm bg-surface-subtle border"
                          style={{ borderColor: "rgba(148,163,184,0.24)", color: "var(--th-text-primary)" }}
                          defaultValue={entry.value ?? ""}
                          onChange={(e) => setConfigEdits((prev) => ({ ...prev, [entry.key]: e.target.value }))}
                        />
                      </div>
                    ))}
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
