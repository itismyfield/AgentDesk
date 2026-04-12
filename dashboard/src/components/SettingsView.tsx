import { Suspense, lazy, useEffect, useState, type CSSProperties, type ReactNode } from "react";
import { Info, Rocket, SlidersHorizontal, Sparkles } from "lucide-react";
import type { CompanySettings } from "../types";
import * as api from "../api";

const OnboardingWizard = lazy(() => import("./OnboardingWizard"));

interface SettingsViewProps {
  settings: CompanySettings;
  onSave: (patch: Record<string, unknown>) => Promise<void>;
  isKo: boolean;
}

interface ConfigField {
  key: string;
  labelKo: string;
  labelEn: string;
  descriptionKo: string;
  descriptionEn: string;
  unit: string;
  min: number;
  max: number;
  step: number;
}

type ConfigEntry = {
  key: string;
  value: string | null;
  category: string;
  label_ko: string;
  label_en: string;
  default?: string | null;
  baseline?: string | null;
  baseline_source?: string | null;
  override_active?: boolean;
  editable?: boolean;
  restart_behavior?: string | null;
};

type ConfigEditValue = string | boolean;

const CATEGORIES: Array<{
  titleKo: string;
  titleEn: string;
  fields: ConfigField[];
}> = [
  {
    titleKo: "폴링 & 타이머",
    titleEn: "Polling & Timers",
    fields: [
      {
        key: "dispatchPollSec",
        labelKo: "디스패치 폴링 주기",
        labelEn: "Dispatch poll interval",
        descriptionKo: "새 디스패치를 읽어오는 간격입니다.",
        descriptionEn: "How often new dispatches are polled.",
        unit: "s",
        min: 5,
        max: 300,
        step: 5,
      },
      {
        key: "agentSyncSec",
        labelKo: "에이전트 상태 동기화 주기",
        labelEn: "Agent status sync interval",
        descriptionKo: "에이전트 상태를 다시 수집하는 간격입니다.",
        descriptionEn: "How often agent status is refreshed.",
        unit: "s",
        min: 30,
        max: 1800,
        step: 30,
      },
      {
        key: "githubIssueSyncSec",
        labelKo: "GitHub 이슈 동기화 주기",
        labelEn: "GitHub issue sync interval",
        descriptionKo: "GitHub 이슈 데이터를 다시 가져오는 간격입니다.",
        descriptionEn: "How often GitHub issue data is refreshed.",
        unit: "s",
        min: 300,
        max: 7200,
        step: 60,
      },
      {
        key: "claudeRateLimitPollSec",
        labelKo: "Claude Rate Limit 폴링",
        labelEn: "Claude rate limit poll",
        descriptionKo: "Claude 사용량/제한 정보를 다시 확인하는 간격입니다.",
        descriptionEn: "Polling interval for Claude rate-limit usage.",
        unit: "s",
        min: 30,
        max: 1800,
        step: 30,
      },
      {
        key: "codexRateLimitPollSec",
        labelKo: "Codex Rate Limit 폴링",
        labelEn: "Codex rate limit poll",
        descriptionKo: "Codex 사용량/제한 정보를 다시 확인하는 간격입니다.",
        descriptionEn: "Polling interval for Codex rate-limit usage.",
        unit: "s",
        min: 30,
        max: 1800,
        step: 30,
      },
      {
        key: "issueTriagePollSec",
        labelKo: "이슈 트리아지 주기",
        labelEn: "Issue triage interval",
        descriptionKo: "신규 이슈 triage 자동화를 다시 실행하는 간격입니다.",
        descriptionEn: "How often issue triage automation runs.",
        unit: "s",
        min: 60,
        max: 3600,
        step: 60,
      },
    ],
  },
  {
    titleKo: "디스패치 제한",
    titleEn: "Dispatch Limits",
    fields: [
      {
        key: "ceoWarnDepth",
        labelKo: "CEO 경고 깊이",
        labelEn: "CEO warning depth",
        descriptionKo: "체인이 이 깊이를 넘으면 경고를 강화합니다.",
        descriptionEn: "Escalates warnings after this chain depth.",
        unit: "",
        min: 1,
        max: 10,
        step: 1,
      },
      {
        key: "maxRetries",
        labelKo: "최대 재시도 횟수",
        labelEn: "Max retries",
        descriptionKo: "자동 재시도가 허용되는 최대 횟수입니다.",
        descriptionEn: "Maximum number of automatic retries allowed.",
        unit: "",
        min: 1,
        max: 10,
        step: 1,
      },
    ],
  },
  {
    titleKo: "리뷰",
    titleEn: "Review",
    fields: [
      {
        key: "reviewReminderMin",
        labelKo: "리뷰 리마인드 간격",
        labelEn: "Review reminder interval",
        descriptionKo: "리뷰 대기 작업에 다시 알림을 보내는 간격입니다.",
        descriptionEn: "Reminder interval for work waiting in review.",
        unit: "min",
        min: 5,
        max: 120,
        step: 5,
      },
    ],
  },
  {
    titleKo: "알림 임계값",
    titleEn: "Alert Thresholds",
    fields: [
      {
        key: "rateLimitWarningPct",
        labelKo: "Rate Limit 경고 수준",
        labelEn: "Rate limit warning level",
        descriptionKo: "이 비율 이상 사용 시 경고 상태로 표시합니다.",
        descriptionEn: "Shows warning state above this usage percentage.",
        unit: "%",
        min: 50,
        max: 99,
        step: 1,
      },
      {
        key: "rateLimitDangerPct",
        labelKo: "Rate Limit 위험 수준",
        labelEn: "Rate limit danger level",
        descriptionKo: "이 비율 이상 사용 시 위험 상태로 표시합니다.",
        descriptionEn: "Shows danger state above this usage percentage.",
        unit: "%",
        min: 60,
        max: 100,
        step: 1,
      },
    ],
  },
  {
    titleKo: "캐시 TTL",
    titleEn: "Cache TTL",
    fields: [
      {
        key: "githubRepoCacheSec",
        labelKo: "GitHub 레포 캐시",
        labelEn: "GitHub repo cache",
        descriptionKo: "GitHub 레포 메타데이터를 캐시하는 시간입니다.",
        descriptionEn: "Cache TTL for GitHub repository metadata.",
        unit: "s",
        min: 30,
        max: 1800,
        step: 30,
      },
      {
        key: "rateLimitStaleSec",
        labelKo: "Rate Limit stale 판정",
        labelEn: "Rate limit stale threshold",
        descriptionKo: "이 시간 이후 사용량 데이터를 오래된 것으로 봅니다.",
        descriptionEn: "Marks usage data stale after this duration.",
        unit: "s",
        min: 30,
        max: 1800,
        step: 30,
      },
    ],
  },
];

const BOOLEAN_CONFIG_KEYS = new Set([
  "review_enabled",
  "pm_decision_gate_enabled",
  "merge_automation_enabled",
  "narrate_progress",
]);

const NUMERIC_CONFIG_KEYS = new Set([
  "max_review_rounds",
  "requested_timeout_min",
  "in_progress_stale_min",
  "context_compact_percent",
  "context_compact_percent_codex",
  "context_compact_percent_claude",
  "server_port",
]);

const READ_ONLY_CONFIG_KEYS = new Set(["server_port"]);

const SYSTEM_CONFIG_DESCRIPTIONS: Record<string, { ko: string; en: string }> = {
  review_enabled: {
    ko: "리뷰 단계를 전체 파이프라인에 적용할지 결정합니다.",
    en: "Controls whether the review step is enforced across the pipeline.",
  },
  max_review_rounds: {
    ko: "한 작업이 반복 리뷰를 수행할 수 있는 최대 횟수입니다.",
    en: "Maximum number of repeated review rounds allowed for one task.",
  },
  pm_decision_gate_enabled: {
    ko: "PM 판단 게이트를 거쳐야 다음 단계로 전환됩니다.",
    en: "Requires PM decision gate approval before the next transition.",
  },
  merge_automation_enabled: {
    ko: "허용된 작성자의 PR을 조건 충족 시 자동 머지합니다.",
    en: "Automatically merges eligible PRs from allowed authors when checks pass.",
  },
  merge_strategy: {
    ko: "자동 머지 시 사용할 GitHub 머지 전략입니다.",
    en: "GitHub merge strategy used by merge automation.",
  },
  merge_allowed_authors: {
    ko: "자동 머지를 허용할 작성자 목록입니다. 쉼표로 구분합니다.",
    en: "Comma-separated list of authors allowed for automated merge.",
  },
  requested_timeout_min: {
    ko: "requested 상태에서 오래 머무는 카드를 경고하는 기준입니다.",
    en: "Timeout threshold for cards stuck in requested state.",
  },
  in_progress_stale_min: {
    ko: "in_progress 상태가 정체로 간주되는 기준 시간입니다.",
    en: "Threshold for considering in-progress work stale.",
  },
  context_compact_percent: {
    ko: "공통 컨텍스트 compact 기준입니다.",
    en: "Global threshold for context compaction.",
  },
  context_compact_percent_codex: {
    ko: "Codex 전용 컨텍스트 compact 기준입니다.",
    en: "Provider-specific context compaction threshold for Codex.",
  },
  context_compact_percent_claude: {
    ko: "Claude 전용 컨텍스트 compact 기준입니다.",
    en: "Provider-specific context compaction threshold for Claude.",
  },
  narrate_progress: {
    ko: "Discord 응답에서 중간 진행 설명을 기본적으로 포함할지 결정합니다.",
    en: "Controls whether Discord replies include progress narration by default.",
  },
};

const QUICK_RUNTIME_KEYS = [
  "dispatchPollSec",
  "agentSyncSec",
  "issueTriagePollSec",
  "reviewReminderMin",
  "rateLimitWarningPct",
  "rateLimitDangerPct",
];

const ADVANCED_RUNTIME_KEYS = [
  "githubIssueSyncSec",
  "claudeRateLimitPollSec",
  "codexRateLimitPollSec",
  "ceoWarnDepth",
  "maxRetries",
  "githubRepoCacheSec",
  "rateLimitStaleSec",
];

const QUICK_POLICY_KEYS = [
  "review_enabled",
  "pm_decision_gate_enabled",
  "merge_automation_enabled",
  "narrate_progress",
];

const ADVANCED_POLICY_KEYS = [
  "merge_strategy",
  "merge_allowed_authors",
  "max_review_rounds",
  "requested_timeout_min",
  "in_progress_stale_min",
  "context_compact_percent",
  "context_compact_percent_codex",
  "context_compact_percent_claude",
];

const VISIBLE_POLICY_KEYS = new Set([...QUICK_POLICY_KEYS, ...ADVANCED_POLICY_KEYS]);

const RUNTIME_FIELD_INDEX = new Map<string, ConfigField>(
  CATEGORIES.flatMap((category) => category.fields.map((field) => [field.key, field] as const)),
);

function isBooleanConfigKey(key: string): boolean {
  return BOOLEAN_CONFIG_KEYS.has(key);
}

function isNumericConfigKey(key: string): boolean {
  return NUMERIC_CONFIG_KEYS.has(key);
}

function isReadOnlyConfigKey(key: string): boolean {
  return READ_ONLY_CONFIG_KEYS.has(key);
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

function getRuntimeField(key: string): ConfigField | undefined {
  return RUNTIME_FIELD_INDEX.get(key);
}

function buttonToneClass(tone: "indigo" | "emerald" | "amber"): string {
  if (tone === "emerald") return "bg-emerald-600 hover:bg-emerald-500";
  if (tone === "amber") return "bg-amber-600 hover:bg-amber-500";
  return "bg-indigo-600 hover:bg-indigo-500";
}

interface InfoTipProps {
  tooltip: string;
}

function InfoTip({ tooltip }: InfoTipProps) {
  const [open, setOpen] = useState(false);

  useEffect(() => {
    if (!open) return;
    const timeout = window.setTimeout(() => setOpen(false), 2200);
    return () => window.clearTimeout(timeout);
  }, [open]);

  return (
    <span className="relative inline-flex shrink-0">
      <button
        type="button"
        className="inline-flex h-5 w-5 items-center justify-center rounded-full border transition-colors"
        style={{
          borderColor: "rgba(148,163,184,0.22)",
          color: "var(--th-text-muted)",
          background: "rgba(15,23,42,0.44)",
        }}
        title={tooltip}
        aria-label={tooltip}
        onMouseEnter={() => setOpen(true)}
        onMouseLeave={() => setOpen(false)}
        onFocus={() => setOpen(true)}
        onBlur={() => setOpen(false)}
        onTouchStart={(event) => {
          event.stopPropagation();
          setOpen((value) => !value);
        }}
        onClick={(event) => event.stopPropagation()}
      >
        <Info size={12} />
      </button>
      {open && (
        <span
          className="absolute right-0 top-[calc(100%+0.5rem)] z-30 w-56 rounded-2xl border px-3 py-2 text-[11px] leading-5"
          style={{
            borderColor: "rgba(148,163,184,0.22)",
            background: "rgba(15,23,42,0.98)",
            color: "var(--th-text)",
            boxShadow: "0 18px 40px rgba(0,0,0,0.32)",
          }}
        >
          {tooltip}
        </span>
      )}
    </span>
  );
}

interface ChipProps {
  children: ReactNode;
  tone?: "default" | "accent";
}

function Chip({ children, tone = "default" }: ChipProps) {
  return (
    <span
      className="inline-flex items-center rounded-full border px-2.5 py-1 text-[11px] font-medium"
      style={
        tone === "accent"
          ? {
              borderColor: "rgba(99,102,241,0.32)",
              background: "rgba(99,102,241,0.12)",
              color: "#c7d2fe",
            }
          : {
              borderColor: "rgba(148,163,184,0.18)",
              background: "rgba(15,23,42,0.34)",
              color: "var(--th-text-secondary)",
            }
      }
    >
      {children}
    </span>
  );
}

interface SectionHeaderProps {
  eyebrow: string;
  title: string;
  tooltip?: string;
  badge?: string;
  icon?: ReactNode;
  action?: ReactNode;
}

function SectionHeader({ eyebrow, title, tooltip, badge, icon, action }: SectionHeaderProps) {
  return (
    <div className="flex flex-col gap-4 lg:flex-row lg:items-start lg:justify-between">
      <div className="min-w-0">
        <div className="text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
          {eyebrow}
        </div>
        <div className="mt-2 flex flex-wrap items-center gap-2">
          {icon && (
            <span
              className="inline-flex h-9 w-9 items-center justify-center rounded-2xl border"
              style={{ borderColor: "rgba(99,102,241,0.22)", background: "rgba(99,102,241,0.12)", color: "#c7d2fe" }}
            >
              {icon}
            </span>
          )}
          <h2 className="text-xl font-semibold tracking-tight sm:text-2xl" style={{ color: "var(--th-text)" }}>
            {title}
          </h2>
          {tooltip && <InfoTip tooltip={tooltip} />}
          {badge && <Chip tone="accent">{badge}</Chip>}
        </div>
      </div>
      {action}
    </div>
  );
}

interface CatalogCardProps {
  title: string;
  tooltip?: string;
  accent?: string;
  children: ReactNode;
}

function CatalogCard({ title, tooltip, accent = "#6366f1", children }: CatalogCardProps) {
  return (
    <div
      className="rounded-[26px] border p-4 sm:p-5"
      style={{
        borderColor: `color-mix(in srgb, ${accent} 22%, rgba(148,163,184,0.12))`,
        background: `linear-gradient(180deg, color-mix(in srgb, ${accent} 10%, rgba(15,23,42,0.96)) 0%, rgba(15,23,42,0.34) 100%)`,
      }}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="text-base font-semibold" style={{ color: "var(--th-text)" }}>
          {title}
        </div>
        {tooltip && <InfoTip tooltip={tooltip} />}
      </div>
      <div className="mt-4 space-y-3">{children}</div>
    </div>
  );
}

interface FieldShellProps {
  label: string;
  tooltip: string;
  trailing?: ReactNode;
  footer?: ReactNode;
  children: ReactNode;
}

function FieldShell({ label, tooltip, trailing, footer, children }: FieldShellProps) {
  return (
    <div
      className="rounded-2xl border px-4 py-3"
      style={{ borderColor: "rgba(148,163,184,0.16)", background: "rgba(15,23,42,0.28)" }}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0 flex items-center gap-2">
          <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
            {label}
          </div>
          <InfoTip tooltip={tooltip} />
        </div>
        {trailing}
      </div>
      <div className="mt-3">{children}</div>
      {footer && <div className="mt-3 flex flex-wrap items-center gap-2">{footer}</div>}
    </div>
  );
}

interface SaveFooterProps {
  dirty: boolean;
  saving: boolean;
  onSave: () => void | Promise<void>;
  idleLabel: string;
  savingLabel: string;
  dirtyLabel: string;
  cleanLabel: string;
  tone: "indigo" | "emerald" | "amber";
}

function SaveFooter({
  dirty,
  saving,
  onSave,
  idleLabel,
  savingLabel,
  dirtyLabel,
  cleanLabel,
  tone,
}: SaveFooterProps) {
  return (
    <div
      className="mt-4 flex flex-col gap-3 border-t pt-4 sm:flex-row sm:items-center sm:justify-between"
      style={{ borderColor: "rgba(148,163,184,0.14)" }}
    >
      <Chip tone={dirty ? "accent" : "default"}>{dirty ? dirtyLabel : cleanLabel}</Chip>
      <button
        onClick={() => void onSave()}
        disabled={saving || !dirty}
        className={`inline-flex min-h-[44px] items-center justify-center rounded-2xl px-5 py-2.5 text-sm font-medium text-white transition-colors disabled:opacity-50 ${buttonToneClass(tone)}`}
      >
        {saving ? savingLabel : idleLabel}
      </button>
    </div>
  );
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

  const [rcValues, setRcValues] = useState<Record<string, number>>({});
  const [rcDefaults, setRcDefaults] = useState<Record<string, number>>({});
  const [rcLoaded, setRcLoaded] = useState(false);
  const [rcSaving, setRcSaving] = useState(false);
  const [rcDirty, setRcDirty] = useState(false);

  const [configEntries, setConfigEntries] = useState<ConfigEntry[]>([]);
  const [configLoaded, setConfigLoaded] = useState(false);
  const [configEdits, setConfigEdits] = useState<Record<string, ConfigEditValue>>({});
  const [configSaving, setConfigSaving] = useState(false);

  const [escalationSettings, setEscalationSettings] = useState<api.EscalationSettings | null>(null);
  const [escalationBaseline, setEscalationBaseline] = useState<api.EscalationSettings | null>(null);
  const [escalationLoaded, setEscalationLoaded] = useState(false);
  const [escalationSaving, setEscalationSaving] = useState(false);

  const [showOnboarding, setShowOnboarding] = useState(false);

  const tr = (ko: string, en: string) => (isKo ? ko : en);

  useEffect(() => {
    setCompanyName(settings.companyName);
    setCeoName(settings.ceoName);
    setLanguage(settings.language);
    setTheme(settings.theme);
  }, [settings.companyName, settings.ceoName, settings.language, settings.theme]);

  useEffect(() => {
    void api.getRuntimeConfig()
      .then((data) => {
        setRcValues(data?.current ?? {});
        setRcDefaults(data?.defaults ?? {});
        setRcLoaded(true);
      })
      .catch(() => {
        setRcLoaded(true);
      });

    void fetch("/api/settings/config", { credentials: "include" })
      .then((response) => response.json())
      .then((data: { entries: ConfigEntry[] }) => {
        setConfigEntries(data.entries || []);
        setConfigLoaded(true);
      })
      .catch(() => {
        setConfigLoaded(true);
      });

    void api.getEscalationSettings()
      .then((data) => {
        setEscalationSettings(data.current);
        setEscalationBaseline(data.current);
        setEscalationLoaded(true);
      })
      .catch(() => {
        setEscalationLoaded(true);
      });
  }, []);

  const companyDirty =
    companyName !== settings.companyName ||
    ceoName !== settings.ceoName ||
    language !== settings.language ||
    theme !== settings.theme;
  const configDirty = Object.keys(configEdits).length > 0;
  const escalationDirty =
    escalationSettings !== null &&
    escalationBaseline !== null &&
    JSON.stringify(escalationSettings) !== JSON.stringify(escalationBaseline);

  const quickRuntimeFields = QUICK_RUNTIME_KEYS
    .map((key) => getRuntimeField(key))
    .filter((field): field is ConfigField => Boolean(field));
  const advancedRuntimeFields = ADVANCED_RUNTIME_KEYS
    .map((key) => getRuntimeField(key))
    .filter((field): field is ConfigField => Boolean(field));

  const quickPolicyEntries = QUICK_POLICY_KEYS
    .map((key) => configEntries.find((entry) => entry.key === key && VISIBLE_POLICY_KEYS.has(entry.key)))
    .filter((entry): entry is ConfigEntry => Boolean(entry));
  const advancedPolicyEntries = ADVANCED_POLICY_KEYS
    .map((key) => configEntries.find((entry) => entry.key === key && VISIBLE_POLICY_KEYS.has(entry.key)))
    .filter((entry): entry is ConfigEntry => Boolean(entry));

  const inputStyle: CSSProperties = {
    background: "var(--th-bg-surface)",
    border: "1px solid var(--th-border)",
    color: "var(--th-text)",
  };

  const sectionStyle: CSSProperties = {
    border: "1px solid rgba(148,163,184,0.16)",
    background: "linear-gradient(180deg, rgba(15,23,42,0.74) 0%, rgba(15,23,42,0.44) 100%)",
  };

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
      await api.saveRuntimeConfig(rcValues);
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

  const handleConfigEdit = (key: string, value: ConfigEditValue) => {
    if (isReadOnlyConfigKey(key)) return;
    setConfigEdits((prev) => ({ ...prev, [key]: value }));
  };

  const handleConfigSave = async () => {
    if (!configDirty) return;
    setConfigSaving(true);
    try {
      await fetch("/api/settings/config", {
        method: "PATCH",
        credentials: "include",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(configEdits),
      });
      setConfigEdits({});
      const response = await fetch("/api/settings/config", { credentials: "include" });
      const data = await response.json();
      setConfigEntries(data.entries || []);
    } finally {
      setConfigSaving(false);
    }
  };

  const handleEscalationChange = (
    patch:
      | Partial<api.EscalationSettings>
      | ((prev: api.EscalationSettings) => api.EscalationSettings),
  ) => {
    setEscalationSettings((prev) => {
      if (!prev) return prev;
      return typeof patch === "function" ? patch(prev) : { ...prev, ...patch };
    });
  };

  const handleEscalationSave = async () => {
    if (!escalationSettings) return;
    setEscalationSaving(true);
    try {
      const data = await api.saveEscalationSettings(escalationSettings);
      setEscalationSettings(data.current);
      setEscalationBaseline(data.current);
    } finally {
      setEscalationSaving(false);
    }
  };

  const renderRuntimeField = (field: ConfigField) => {
    const value = rcValues[field.key] ?? rcDefaults[field.key] ?? 0;
    const defaultValue = rcDefaults[field.key] ?? 0;
    const isDefault = value === defaultValue;

    return (
      <FieldShell
        key={field.key}
        label={tr(field.labelKo, field.labelEn)}
        tooltip={tr(field.descriptionKo, field.descriptionEn)}
        trailing={
          <div className="text-right">
            <div className="text-sm font-semibold" style={{ color: isDefault ? "var(--th-text)" : "#fbbf24" }}>
              {formatUnit(value, field.unit)}
            </div>
          </div>
        }
        footer={
          <>
            <Chip>{tr("기본값", "Default")}: {formatUnit(defaultValue, field.unit)}</Chip>
            {!isDefault && (
              <button
                onClick={() => handleRcReset(field.key)}
                className="inline-flex items-center rounded-full border px-3 py-1 text-[11px] font-medium transition-colors hover:border-indigo-400/50 hover:text-indigo-200"
                style={{ borderColor: "rgba(148,163,184,0.2)", color: "var(--th-text-muted)" }}
              >
                {tr("기본값으로 되돌리기", "Reset")}
              </button>
            )}
          </>
        }
      >
        <div className="grid gap-3 sm:grid-cols-[1fr_auto] sm:items-center">
          <input
            type="range"
            min={field.min}
            max={field.max}
            step={field.step}
            value={value}
            onChange={(event) => handleRcChange(field.key, Number(event.target.value))}
            className="h-1.5 cursor-pointer appearance-none rounded-full"
            style={{ accentColor: "#6366f1" }}
          />
          <input
            type="number"
            min={field.min}
            max={field.max}
            step={field.step}
            value={value}
            onChange={(event) => {
              const next = Number(event.target.value);
              if (Number.isFinite(next) && next >= field.min && next <= field.max) {
                handleRcChange(field.key, next);
              }
            }}
            className="w-full rounded-xl px-3 py-2 text-right text-sm font-mono sm:w-24"
            style={inputStyle}
          />
        </div>
      </FieldShell>
    );
  };

  const renderConfigEntry = (entry: ConfigEntry) => {
    const description = SYSTEM_CONFIG_DESCRIPTIONS[entry.key];
    const hasLocalEdit = Object.prototype.hasOwnProperty.call(configEdits, entry.key);
    const currentValue = hasLocalEdit ? configEdits[entry.key] : (entry.value ?? entry.default ?? "");
    const baselineValue = entry.baseline ?? entry.default ?? null;
    const defaultLabel = baselineValue ?? tr("없음", "None");
    const readOnly = entry.editable === false || isReadOnlyConfigKey(entry.key);
    const pendingSave = hasLocalEdit;
    const customized = baselineValue !== null
      ? String(currentValue) !== baselineValue
      : String(currentValue ?? "").trim().length > 0;
    const footer = (
      <>
        <Chip>{tr("기본값", "Default")}: {defaultLabel}</Chip>
        {(pendingSave || customized) && (
          <Chip tone="accent">
            {pendingSave ? tr("저장 대기", "Pending save") : tr("사용자 지정", "Customized")}
          </Chip>
        )}
      </>
    );
    const tooltip = description ? tr(description.ko, description.en) : (isKo ? entry.label_ko : entry.label_en);

    if (isBooleanConfigKey(entry.key)) {
      const isEnabled = parseBooleanConfigValue(currentValue);

      return (
        <FieldShell
          key={entry.key}
          label={isKo ? entry.label_ko : entry.label_en}
          tooltip={tooltip}
          trailing={<Chip tone={isEnabled ? "accent" : "default"}>{isEnabled ? tr("켜짐", "On") : tr("꺼짐", "Off")}</Chip>}
          footer={footer}
        >
          <button
            type="button"
            role="switch"
            aria-checked={isEnabled}
            disabled={readOnly}
            onClick={() => handleConfigEdit(entry.key, !isEnabled)}
            className="flex min-h-[52px] w-full items-center justify-between rounded-2xl border px-3 py-3 text-left transition-colors disabled:cursor-not-allowed disabled:opacity-70"
            style={{
              borderColor: isEnabled ? "rgba(16,185,129,0.35)" : "rgba(148,163,184,0.24)",
              background: isEnabled ? "rgba(16,185,129,0.12)" : "rgba(15,23,42,0.2)",
            }}
          >
            <span className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
              {isEnabled ? tr("활성화", "Enabled") : tr("비활성화", "Disabled")}
            </span>
            <span
              className="relative inline-flex h-7 w-12 shrink-0 items-center rounded-full transition-colors"
              style={{ background: isEnabled ? "#10b981" : "rgba(148,163,184,0.32)" }}
            >
              <span
                className="absolute h-5 w-5 rounded-full bg-white transition-transform"
                style={{ transform: isEnabled ? "translateX(1.55rem)" : "translateX(0.3rem)" }}
              />
            </span>
          </button>
        </FieldShell>
      );
    }

    if (entry.key === "merge_strategy") {
      return (
        <FieldShell
          key={entry.key}
          label={isKo ? entry.label_ko : entry.label_en}
          tooltip={tooltip}
          footer={footer}
        >
          <select
            disabled={readOnly}
            className="w-full rounded-2xl px-3 py-2.5 text-sm disabled:cursor-not-allowed disabled:opacity-80"
            style={inputStyle}
            value={String(currentValue || "squash")}
            onChange={(event) => handleConfigEdit(entry.key, event.target.value)}
          >
            <option value="squash">squash</option>
            <option value="merge">merge</option>
            <option value="rebase">rebase</option>
          </select>
        </FieldShell>
      );
    }

    return (
      <FieldShell
        key={entry.key}
        label={isKo ? entry.label_ko : entry.label_en}
        tooltip={tooltip}
        footer={footer}
      >
        <input
          type={isNumericConfigKey(entry.key) && !readOnly ? "number" : "text"}
          inputMode={isNumericConfigKey(entry.key) ? "numeric" : undefined}
          disabled={readOnly}
          className="w-full rounded-2xl px-3 py-2.5 text-sm disabled:cursor-not-allowed disabled:opacity-80"
          style={inputStyle}
          value={String(currentValue)}
          onChange={(event) => handleConfigEdit(entry.key, event.target.value)}
        />
      </FieldShell>
    );
  };

  return (
    <div
      className="mx-auto h-full max-w-6xl min-w-0 space-y-6 overflow-x-hidden overflow-y-auto px-4 py-5 pb-40 sm:px-6"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <section
        className="rounded-[30px] border p-5 sm:p-6"
        style={{
          borderColor: "rgba(99,102,241,0.22)",
          background: "radial-gradient(circle at top left, rgba(99,102,241,0.24), rgba(15,23,42,0.92) 48%, rgba(15,23,42,0.74) 100%)",
        }}
      >
        <div className="flex flex-col gap-5 xl:flex-row xl:items-start xl:justify-between">
          <div className="min-w-0">
            <div className="text-[11px] font-semibold uppercase tracking-[0.22em]" style={{ color: "#c7d2fe" }}>
              {tr("설정", "Settings")}
            </div>
            <div className="mt-2 flex flex-wrap items-center gap-2">
              <h1 className="text-2xl font-semibold tracking-tight sm:text-3xl" style={{ color: "var(--th-text)" }}>
                {tr("일반 설정 카탈로그", "General settings catalog")}
              </h1>
              <InfoTip
                tooltip={tr(
                  "자주 바꾸는 값은 먼저, 덜 만지는 값은 아래 고급 섹션으로 분리했습니다. 설명은 각 항목의 info 아이콘에서 확인할 수 있습니다.",
                  "Frequently changed controls are grouped first, while lower-frequency tuning lives in the advanced section. Each explanation is available from the info icon on the field.",
                )}
              />
            </div>
            <div className="mt-4 flex flex-wrap gap-2">
              <Chip tone="accent">{tr(`빠른 설정 ${4 + quickPolicyEntries.length + quickRuntimeFields.length}개`, `${4 + quickPolicyEntries.length + quickRuntimeFields.length} quick controls`)}</Chip>
              <Chip>{tr(`고급 조정 ${advancedPolicyEntries.length + advancedRuntimeFields.length + 2}개`, `${advancedPolicyEntries.length + advancedRuntimeFields.length + 2} advanced controls`)}</Chip>
              <Chip>{tr("모바일 대응", "Mobile-ready")}</Chip>
            </div>
          </div>

          <div
            className="w-full max-w-md rounded-[26px] border p-4 sm:p-5"
            style={{ borderColor: "rgba(244,114,182,0.24)", background: "rgba(15,23,42,0.44)" }}
          >
            <div className="flex items-start justify-between gap-3">
              <div className="min-w-0">
                <div className="flex items-center gap-2">
                  <span
                    className="inline-flex h-9 w-9 items-center justify-center rounded-2xl border"
                    style={{ borderColor: "rgba(244,114,182,0.24)", background: "rgba(244,114,182,0.12)", color: "#f9a8d4" }}
                  >
                    <Rocket size={16} />
                  </span>
                  <div className="text-base font-semibold" style={{ color: "var(--th-text)" }}>
                    {tr("온보딩 재수행", "Re-run onboarding")}
                  </div>
                  <InfoTip
                    tooltip={tr(
                      "봇 토큰, Discord 연결, provider 같은 초기 세팅은 일반 폼 대신 전용 온보딩 흐름에서 다시 설정합니다.",
                      "Initial setup such as bot tokens, Discord bindings, and provider selection is managed through the dedicated onboarding flow instead of the general form.",
                    )}
                  />
                </div>
                <div className="mt-3 flex flex-wrap gap-2">
                  <Chip>Discord</Chip>
                  <Chip>Provider</Chip>
                  <Chip>{tr("토큰", "Tokens")}</Chip>
                </div>
              </div>
            </div>
            <button
              onClick={() => setShowOnboarding(true)}
              className="mt-4 inline-flex min-h-[44px] w-full items-center justify-center rounded-2xl bg-pink-600 px-5 py-2.5 text-sm font-medium text-white transition-colors hover:bg-pink-500"
            >
              {tr("온보딩 다시 열기", "Open onboarding again")}
            </button>
          </div>
        </div>
      </section>

      <section className="rounded-[30px] border p-5 sm:p-6" style={sectionStyle}>
        <SectionHeader
          eyebrow={tr("빠른 설정", "Quick")}
          title={tr("자주 바꾸는 설정", "Settings you change often")}
          tooltip={tr(
            "브랜드, 언어, 기본 라우팅, 자주 쓰는 운영 토글과 즉시 반영되는 리듬 조정을 먼저 모았습니다.",
            "Branding, language, primary routing, frequent workflow toggles, and the runtime controls you are most likely to adjust live here.",
          )}
          badge={tr("상단 고정", "Top")}
          icon={<Sparkles size={16} />}
        />

        <div className="mt-5 grid gap-4 xl:grid-cols-2">
          <CatalogCard
            title={tr("브랜드 & 언어", "Brand & language")}
            tooltip={tr(
              "대시보드에서 가장 눈에 띄는 기본 표기와 시각 모드를 바꾸는 영역입니다.",
              "This card controls the most visible dashboard identity and visual mode settings.",
            )}
            accent="#6366f1"
          >
            <div className="grid gap-3 sm:grid-cols-2">
              <FieldShell
                label={tr("회사 이름", "Company name")}
                tooltip={tr("대시보드 주요 타이틀에 노출됩니다.", "Shown in the main dashboard titles.")}
              >
                <input
                  type="text"
                  value={companyName}
                  onChange={(event) => setCompanyName(event.target.value)}
                  className="w-full rounded-2xl px-3 py-2.5 text-sm"
                  style={inputStyle}
                />
              </FieldShell>

              <FieldShell
                label={tr("CEO 이름", "CEO name")}
                tooltip={tr("오피스와 운영 UI에서 대표 인물 이름으로 사용됩니다.", "Used as the representative persona name across office and ops surfaces.")}
              >
                <input
                  type="text"
                  value={ceoName}
                  onChange={(event) => setCeoName(event.target.value)}
                  className="w-full rounded-2xl px-3 py-2.5 text-sm"
                  style={inputStyle}
                />
              </FieldShell>

              <FieldShell
                label={tr("언어", "Language")}
                tooltip={tr("대시보드 기본 언어를 정합니다.", "Sets the dashboard's primary UI language.")}
              >
                <select
                  value={language}
                  onChange={(event) => setLanguage(event.target.value as typeof language)}
                  className="w-full rounded-2xl px-3 py-2.5 text-sm"
                  style={inputStyle}
                >
                  <option value="ko">한국어</option>
                  <option value="en">English</option>
                  <option value="ja">日本語</option>
                  <option value="zh">中文</option>
                </select>
              </FieldShell>

              <FieldShell
                label={tr("테마", "Theme")}
                tooltip={tr("대시보드 전체 색 모드를 정합니다.", "Sets the overall dashboard color mode.")}
              >
                <select
                  value={theme}
                  onChange={(event) => setTheme(event.target.value as typeof theme)}
                  className="w-full rounded-2xl px-3 py-2.5 text-sm"
                  style={inputStyle}
                >
                  <option value="dark">{tr("다크", "Dark")}</option>
                  <option value="light">{tr("라이트", "Light")}</option>
                  <option value="auto">{tr("자동 (시스템)", "Auto (System)")}</option>
                </select>
              </FieldShell>
            </div>

            <SaveFooter
              dirty={companyDirty}
              saving={saving}
              onSave={handleSave}
              idleLabel={tr("일반 설정 저장", "Save general settings")}
              savingLabel={tr("저장 중...", "Saving...")}
              dirtyLabel={tr("변경 사항 있음", "Unsaved changes")}
              cleanLabel={tr("저장됨", "Saved")}
              tone="indigo"
            />
          </CatalogCard>

          <CatalogCard
            title={tr("에스컬레이션 모드", "Escalation mode")}
            tooltip={tr(
              "pending_decision 에스컬레이션을 PM 쪽으로 보낼지 owner 쪽으로 보낼지 결정하는 기본 라우팅입니다.",
              "This card controls the primary routing for pending-decision escalations between PM and owner flows.",
            )}
            accent="#f59e0b"
          >
            {!escalationLoaded ? (
              <div className="rounded-2xl border px-4 py-5 text-sm" style={{ borderColor: "rgba(148,163,184,0.16)", color: "var(--th-text-muted)" }}>
                {tr("에스컬레이션 설정을 불러오는 중...", "Loading escalation settings...")}
              </div>
            ) : escalationSettings ? (
              <>
                <FieldShell
                  label={tr("라우팅 모드", "Routing mode")}
                  tooltip={tr(
                    "PM 고정, owner 고정, 또는 scheduled 자동 전환 중 하나를 선택합니다.",
                    "Choose fixed PM, fixed owner, or scheduled automatic switching.",
                  )}
                >
                  <select
                    className="w-full rounded-2xl px-3 py-2.5 text-sm"
                    style={inputStyle}
                    value={escalationSettings.mode}
                    onChange={(event) =>
                      handleEscalationChange({ mode: event.target.value as api.EscalationMode })
                    }
                  >
                    <option value="pm">{tr("PM 모드", "PM mode")}</option>
                    <option value="user">{tr("Owner 모드", "Owner mode")}</option>
                    <option value="scheduled">{tr("시간대 기반", "Scheduled")}</option>
                  </select>
                </FieldShell>

                {escalationSettings.mode === "scheduled" && (
                  <div className="grid gap-3 sm:grid-cols-2">
                    <FieldShell
                      label={tr("PM 시간대", "PM hours")}
                      tooltip={tr("scheduled 모드에서 PM 라우팅으로 전환할 시간 구간입니다. 형식: HH:MM-HH:MM.", "Time window used for PM routing in scheduled mode. Format: HH:MM-HH:MM.")}
                    >
                      <input
                        type="text"
                        className="w-full rounded-2xl px-3 py-2.5 text-sm"
                        style={inputStyle}
                        value={escalationSettings.schedule.pm_hours}
                        onChange={(event) =>
                          handleEscalationChange((prev) => ({
                            ...prev,
                            schedule: {
                              ...prev.schedule,
                              pm_hours: event.target.value,
                            },
                          }))
                        }
                      />
                    </FieldShell>

                    <FieldShell
                      label={tr("Timezone", "Timezone")}
                      tooltip={tr("scheduled 모드 판단에 사용할 IANA timezone입니다. 예: Asia/Seoul.", "IANA timezone used when evaluating scheduled mode. Example: Asia/Seoul.")}
                    >
                      <input
                        type="text"
                        className="w-full rounded-2xl px-3 py-2.5 text-sm"
                        style={inputStyle}
                        value={escalationSettings.schedule.timezone}
                        onChange={(event) =>
                          handleEscalationChange((prev) => ({
                            ...prev,
                            schedule: {
                              ...prev.schedule,
                              timezone: event.target.value,
                            },
                          }))
                        }
                      />
                    </FieldShell>
                  </div>
                )}

                <SaveFooter
                  dirty={escalationDirty}
                  saving={escalationSaving}
                  onSave={handleEscalationSave}
                  idleLabel={tr("라우팅 저장", "Save routing")}
                  savingLabel={tr("저장 중...", "Saving...")}
                  dirtyLabel={tr("변경 사항 있음", "Unsaved changes")}
                  cleanLabel={tr("저장됨", "Saved")}
                  tone="amber"
                />
              </>
            ) : (
              <div className="rounded-2xl border px-4 py-5 text-sm" style={{ borderColor: "rgba(148,163,184,0.16)", color: "var(--th-text-muted)" }}>
                {tr("에스컬레이션 설정을 불러오지 못했습니다.", "Failed to load escalation settings.")}
              </div>
            )}
          </CatalogCard>

          <CatalogCard
            title={tr("운영 토글", "Workflow toggles")}
            tooltip={tr(
              "리뷰, 자동 머지, PM 판단 게이트, Discord 진행 설명처럼 자주 켜고 끄는 운영 스위치입니다.",
              "These are the workflow switches you are most likely to turn on and off, such as review, merge automation, PM gating, and Discord progress narration.",
            )}
            accent="#10b981"
          >
            {!configLoaded ? (
              <div className="rounded-2xl border px-4 py-5 text-sm" style={{ borderColor: "rgba(148,163,184,0.16)", color: "var(--th-text-muted)" }}>
                {tr("정책 설정을 불러오는 중...", "Loading policy settings...")}
              </div>
            ) : (
              <>
                {quickPolicyEntries.map((entry) => renderConfigEntry(entry))}
                {quickPolicyEntries.length === 0 && (
                  <div className="rounded-2xl border px-4 py-5 text-sm" style={{ borderColor: "rgba(148,163,184,0.16)", color: "var(--th-text-muted)" }}>
                    {tr("표시할 운영 토글이 없습니다.", "No workflow toggles are available.")}
                  </div>
                )}
                <SaveFooter
                  dirty={configDirty}
                  saving={configSaving}
                  onSave={handleConfigSave}
                  idleLabel={tr("토글 저장", "Save toggles")}
                  savingLabel={tr("저장 중...", "Saving...")}
                  dirtyLabel={tr("변경 사항 있음", "Unsaved changes")}
                  cleanLabel={tr("저장됨", "Saved")}
                  tone="emerald"
                />
              </>
            )}
          </CatalogCard>

          <CatalogCard
            title={tr("운영 리듬", "Runtime favorites")}
            tooltip={tr(
              "즉시 반영되는 주기와 임계값 중 자주 건드리는 항목만 먼저 모았습니다.",
              "This card surfaces the runtime intervals and thresholds that are commonly adjusted live.",
            )}
            accent="#22c55e"
          >
            {!rcLoaded ? (
              <div className="rounded-2xl border px-4 py-5 text-sm" style={{ borderColor: "rgba(148,163,184,0.16)", color: "var(--th-text-muted)" }}>
                {tr("런타임 설정을 불러오는 중...", "Loading runtime settings...")}
              </div>
            ) : (
              <>
                {quickRuntimeFields.map((field) => renderRuntimeField(field))}
                <SaveFooter
                  dirty={rcDirty}
                  saving={rcSaving}
                  onSave={handleRcSave}
                  idleLabel={tr("리듬 저장", "Save runtime")}
                  savingLabel={tr("저장 중...", "Saving...")}
                  dirtyLabel={tr("변경 사항 있음", "Unsaved changes")}
                  cleanLabel={tr("저장됨", "Saved")}
                  tone="indigo"
                />
              </>
            )}
          </CatalogCard>
        </div>
      </section>

      <section className="rounded-[30px] border p-5 sm:p-6" style={sectionStyle}>
        <SectionHeader
          eyebrow={tr("고급 설정", "Advanced")}
          title={tr("세부 조정", "Detailed tuning")}
          tooltip={tr(
            "평소에는 자주 건드리지 않지만 운영 정책과 세밀한 튜닝에 필요한 항목을 분리했습니다.",
            "These controls are separated for lower-frequency tuning and deeper operational policy adjustments.",
          )}
          badge={tr("고급", "Advanced")}
          icon={<SlidersHorizontal size={16} />}
        />

        <div className="mt-5 grid gap-4 xl:grid-cols-[1.05fr_1fr]">
          <CatalogCard
            title={tr("고급 라우팅", "Advanced routing")}
            tooltip={tr(
              "owner fallback ID와 PM 채널 식별자처럼 일반 사용자보다 운영 담당자가 주로 만지는 라우팅 세부값입니다.",
              "Routing details such as the owner fallback ID and PM channel target are primarily used by operators rather than everyday users.",
            )}
            accent="#f97316"
          >
            {!escalationLoaded ? (
              <div className="rounded-2xl border px-4 py-5 text-sm" style={{ borderColor: "rgba(148,163,184,0.16)", color: "var(--th-text-muted)" }}>
                {tr("에스컬레이션 설정을 불러오는 중...", "Loading escalation settings...")}
              </div>
            ) : escalationSettings ? (
              <>
                <FieldShell
                  label={tr("fallback owner user ID", "Fallback owner user ID")}
                  tooltip={tr(
                    "live owner 추적이 비어 있을 때 owner 멘션에 사용할 Discord user ID입니다.",
                    "Discord user ID used for owner mentions when live owner tracking is unavailable.",
                  )}
                >
                  <input
                    type="text"
                    className="w-full rounded-2xl px-3 py-2.5 text-sm"
                    style={inputStyle}
                    value={escalationSettings.owner_user_id ?? ""}
                    onChange={(event) => {
                      const trimmed = event.target.value.trim();
                      const parsed = Number(trimmed);
                      handleEscalationChange({
                        owner_user_id: trimmed && Number.isFinite(parsed) ? parsed : null,
                      });
                    }}
                  />
                </FieldShell>

                <FieldShell
                  label={tr("PM channel ID", "PM channel ID")}
                  tooltip={tr(
                    "PM fallback 및 PM mode에서 사용할 Discord channel ID 또는 alias입니다.",
                    "Discord channel ID or alias used for PM fallback and PM mode.",
                  )}
                >
                  <input
                    type="text"
                    className="w-full rounded-2xl px-3 py-2.5 text-sm"
                    style={inputStyle}
                    value={escalationSettings.pm_channel_id ?? ""}
                    onChange={(event) =>
                      handleEscalationChange({
                        pm_channel_id: event.target.value.trim() || null,
                      })
                    }
                  />
                </FieldShell>

                <SaveFooter
                  dirty={escalationDirty}
                  saving={escalationSaving}
                  onSave={handleEscalationSave}
                  idleLabel={tr("고급 라우팅 저장", "Save advanced routing")}
                  savingLabel={tr("저장 중...", "Saving...")}
                  dirtyLabel={tr("변경 사항 있음", "Unsaved changes")}
                  cleanLabel={tr("저장됨", "Saved")}
                  tone="amber"
                />
              </>
            ) : (
              <div className="rounded-2xl border px-4 py-5 text-sm" style={{ borderColor: "rgba(148,163,184,0.16)", color: "var(--th-text-muted)" }}>
                {tr("에스컬레이션 설정을 불러오지 못했습니다.", "Failed to load escalation settings.")}
              </div>
            )}
          </CatalogCard>

          <div className="grid gap-4">
            <CatalogCard
              title={tr("고급 런타임", "Advanced runtime")}
              tooltip={tr(
                "자주 바꾸는 값 아래에 두는 세밀한 주기와 캐시 관련 조정입니다.",
                "These are lower-frequency runtime and cache tuning controls kept below the main favorites.",
              )}
              accent="#14b8a6"
            >
              {!rcLoaded ? (
                <div className="rounded-2xl border px-4 py-5 text-sm" style={{ borderColor: "rgba(148,163,184,0.16)", color: "var(--th-text-muted)" }}>
                  {tr("런타임 설정을 불러오는 중...", "Loading runtime settings...")}
                </div>
              ) : (
                <>
                  {advancedRuntimeFields.map((field) => renderRuntimeField(field))}
                  <SaveFooter
                    dirty={rcDirty}
                    saving={rcSaving}
                    onSave={handleRcSave}
                    idleLabel={tr("고급 런타임 저장", "Save advanced runtime")}
                    savingLabel={tr("저장 중...", "Saving...")}
                    dirtyLabel={tr("변경 사항 있음", "Unsaved changes")}
                    cleanLabel={tr("저장됨", "Saved")}
                    tone="indigo"
                  />
                </>
              )}
            </CatalogCard>

            <CatalogCard
              title={tr("정책 세부 설정", "Policy details")}
              tooltip={tr(
                "자동 머지 전략, 타임아웃, 컨텍스트 compact 같은 운영 정책 세부값을 모았습니다.",
                "This card groups the detailed operational policy controls such as merge strategy, timeouts, and context compaction.",
              )}
              accent="#8b5cf6"
            >
              {!configLoaded ? (
                <div className="rounded-2xl border px-4 py-5 text-sm" style={{ borderColor: "rgba(148,163,184,0.16)", color: "var(--th-text-muted)" }}>
                  {tr("정책 설정을 불러오는 중...", "Loading policy settings...")}
                </div>
              ) : (
                <>
                  {advancedPolicyEntries.map((entry) => renderConfigEntry(entry))}
                  {advancedPolicyEntries.length === 0 && (
                    <div className="rounded-2xl border px-4 py-5 text-sm" style={{ borderColor: "rgba(148,163,184,0.16)", color: "var(--th-text-muted)" }}>
                      {tr("표시할 고급 정책 설정이 없습니다.", "No advanced policy settings are available.")}
                    </div>
                  )}
                  <SaveFooter
                    dirty={configDirty}
                    saving={configSaving}
                    onSave={handleConfigSave}
                    idleLabel={tr("정책 세부 저장", "Save policy details")}
                    savingLabel={tr("저장 중...", "Saving...")}
                    dirtyLabel={tr("변경 사항 있음", "Unsaved changes")}
                    cleanLabel={tr("저장됨", "Saved")}
                    tone="emerald"
                  />
                </>
              )}
            </CatalogCard>
          </div>
        </div>
      </section>

      {showOnboarding && (
        <div className="fixed inset-0 z-50 overflow-y-auto bg-[#0a0e1a]" role="dialog" aria-modal="true" aria-label="Onboarding wizard">
          <div className="flex min-h-screen items-start justify-center pb-16 pt-8">
            <div className="w-full max-w-2xl">
              <div className="mb-2 flex justify-end px-4">
                <button
                  onClick={() => setShowOnboarding(false)}
                  className="min-h-[44px] rounded-lg border px-4 py-2.5 text-sm"
                  style={{ borderColor: "rgba(148,163,184,0.3)", color: "var(--th-text-muted)" }}
                >
                  ✕ {tr("닫기", "Close")}
                </button>
              </div>
              <Suspense fallback={<div className="py-8 text-center" style={{ color: "var(--th-text-muted)" }}>Loading...</div>}>
                <OnboardingWizard
                  isKo={isKo}
                  onComplete={() => {
                    setShowOnboarding(false);
                    window.location.reload();
                  }}
                />
              </Suspense>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
