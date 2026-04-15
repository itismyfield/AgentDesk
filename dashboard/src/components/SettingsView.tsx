import { Suspense, lazy, useEffect, useMemo, useState, type CSSProperties, type ReactNode } from "react";
import type { CompanySettings } from "../types";
import * as api from "../api";
import TooltipLabel from "./common/TooltipLabel";
import {
  SettingsCard,
  SettingsEmptyState,
  SettingsSection,
  SettingsSubsection,
} from "./common/SettingsPrimitives";

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
type SettingsPanel = "general" | "runtime" | "pipeline" | "onboarding";

const SETTINGS_PANEL_STORAGE_KEY = "agentdesk.settings.active-panel";

const CATEGORIES: Array<{
  id: string;
  titleKo: string;
  titleEn: string;
  descriptionKo: string;
  descriptionEn: string;
  fields: ConfigField[];
}> = [
  {
    id: "polling",
    titleKo: "폴링 & 타이머",
    titleEn: "Polling & Timers",
    descriptionKo: "백엔드 동기화와 배치 작업의 리듬을 조절합니다.",
    descriptionEn: "Controls the cadence of backend sync and batch work.",
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
    id: "dispatch",
    titleKo: "디스패치 제한",
    titleEn: "Dispatch Limits",
    descriptionKo: "경고 임계값과 자동 재시도 횟수 같은 운영 제한을 조정합니다.",
    descriptionEn: "Adjusts operational limits such as warnings and retries.",
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
    id: "review",
    titleKo: "리뷰",
    titleEn: "Review",
    descriptionKo: "리뷰 리마인드와 운영 리듬을 다듬습니다.",
    descriptionEn: "Tunes review reminder cadence.",
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
    id: "alerts",
    titleKo: "알림 임계값",
    titleEn: "Alert Thresholds",
    descriptionKo: "사용량 경고를 얼마나 이르게 띄울지 조절합니다.",
    descriptionEn: "Controls how early usage warnings appear.",
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
    id: "cache",
    titleKo: "캐시 TTL",
    titleEn: "Cache TTL",
    descriptionKo: "외부 데이터와 사용량 정보를 얼마나 오래 캐시할지 정합니다.",
    descriptionEn: "Controls how long external data and usage stay cached.",
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
  "narrate_progress",
]);

const NUMERIC_CONFIG_KEYS = new Set([
  "max_review_rounds",
  "requested_timeout_min",
  "in_progress_stale_min",
  "max_chain_depth",
  "context_compact_percent",
  "context_compact_percent_codex",
  "context_compact_percent_claude",
  "server_port",
]);

const READ_ONLY_CONFIG_KEYS = new Set(["server_port"]);

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
  merge_strategy_mode: {
    ko: "터미널 카드에서 direct merge를 먼저 시도할지, 항상 PR을 만들지 결정합니다.",
    en: "Chooses whether terminal cards try direct merge first or always open a PR.",
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

const SYSTEM_CATEGORY_META = {
  pipeline: {
    titleKo: "파이프라인",
    titleEn: "Pipeline",
    descriptionKo: "칸반 흐름과 상태 전환에 직접 영향을 주는 값입니다.",
    descriptionEn: "Values that directly affect kanban flow and transitions.",
  },
  review: {
    titleKo: "리뷰",
    titleEn: "Review",
    descriptionKo: "리뷰 단계 활성화와 반복 횟수를 정의합니다.",
    descriptionEn: "Defines review enablement and repetition limits.",
  },
  timeout: {
    titleKo: "타임아웃",
    titleEn: "Timeouts",
    descriptionKo: "정체 감지와 자동 알림 시점을 조정합니다.",
    descriptionEn: "Tunes stale detection and automatic alert timing.",
  },
  dispatch: {
    titleKo: "디스패치",
    titleEn: "Dispatch",
    descriptionKo: "작업 fan-out과 체인 깊이 한계를 관리합니다.",
    descriptionEn: "Controls task fan-out and chain-depth limits.",
  },
  context: {
    titleKo: "컨텍스트",
    titleEn: "Context",
    descriptionKo: "세션 compact 임계값처럼 모델별 컨텍스트 정책을 관리합니다.",
    descriptionEn: "Manages model-specific context policies such as compaction thresholds.",
  },
  system: {
    titleKo: "시스템",
    titleEn: "System",
    descriptionKo: "Discord 라우팅처럼 운영 연결에 필요한 핵심 값입니다.",
    descriptionEn: "Core values required for operational routing such as Discord wiring.",
  },
} as const;

const PRIMARY_PIPELINE_CATEGORIES: Array<keyof typeof SYSTEM_CATEGORY_META> = ["pipeline", "review", "timeout", "dispatch"];
const ADVANCED_PIPELINE_CATEGORIES: Array<keyof typeof SYSTEM_CATEGORY_META> = ["context", "system"];

function isSettingsPanel(value: string | null): value is SettingsPanel {
  return value === "general" || value === "runtime" || value === "pipeline" || value === "onboarding";
}

function readStoredSettingsPanel(): SettingsPanel {
  if (typeof window === "undefined") return "general";
  const value = window.localStorage.getItem(SETTINGS_PANEL_STORAGE_KEY);
  return isSettingsPanel(value) ? value : "general";
}

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

function PanelNavButton({
  active,
  title,
  detail,
  count,
  onClick,
}: {
  active: boolean;
  title: string;
  detail: string;
  count?: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="w-full rounded-2xl border px-4 py-3 text-left transition-colors"
      style={{
        borderColor: active
          ? "color-mix(in srgb, var(--th-accent-primary) 30%, var(--th-border) 70%)"
          : "color-mix(in srgb, var(--th-border) 72%, transparent)",
        background: active
          ? "color-mix(in srgb, var(--th-accent-primary-soft) 68%, transparent)"
          : "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
      }}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {title}
          </div>
          <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
            {detail}
          </div>
        </div>
        {count && (
          <span
            className="shrink-0 rounded-full border px-2 py-0.5 text-[10px] font-medium"
            style={{
              borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
              color: active ? "var(--th-text)" : "var(--th-text-muted)",
            }}
          >
            {count}
          </span>
        )}
      </div>
    </button>
  );
}

function CompactFieldCard({
  label,
  tooltip,
  children,
  footer,
}: {
  label: string;
  tooltip: string;
  children: ReactNode;
  footer?: ReactNode;
}) {
  return (
    <SettingsCard
      className="rounded-2xl p-4"
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
      }}
    >
      <TooltipLabel text={label} tooltip={tooltip} className="max-w-full text-sm font-medium" />
      <div className="mt-3">{children}</div>
      {footer && (
        <div className="mt-3 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
          {footer}
        </div>
      )}
    </SettingsCard>
  );
}

function GroupLabel({ title }: { title: string }) {
  return (
    <div
      className="text-[11px] font-semibold uppercase tracking-[0.18em]"
      style={{ color: "var(--th-text-muted)" }}
    >
      {title}
    </div>
  );
}

export default function SettingsView({
  settings,
  onSave,
  isKo,
}: SettingsViewProps) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);

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
  const [configEdits, setConfigEdits] = useState<Record<string, ConfigEditValue>>({});
  const [configSaving, setConfigSaving] = useState(false);

  const [activePanel, setActivePanel] = useState<SettingsPanel>(() => readStoredSettingsPanel());
  const [activeRuntimeCategoryId, setActiveRuntimeCategoryId] = useState<string>(CATEGORIES[0]?.id ?? "polling");
  const [showOnboarding, setShowOnboarding] = useState(false);

  useEffect(() => {
    setCompanyName(settings.companyName);
    setCeoName(settings.ceoName);
    setLanguage(settings.language);
    setTheme(settings.theme);
  }, [settings.companyName, settings.ceoName, settings.language, settings.theme]);

  useEffect(() => {
    if (typeof window === "undefined") return;
    window.localStorage.setItem(SETTINGS_PANEL_STORAGE_KEY, activePanel);
  }, [activePanel]);

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
      .then((data: { entries: ConfigEntry[] }) => setConfigEntries(data.entries || []))
      .catch(() => {});
  }, []);

  const companyDirty =
    companyName !== settings.companyName ||
    ceoName !== settings.ceoName ||
    language !== settings.language ||
    theme !== settings.theme;
  const configDirty = Object.keys(configEdits).length > 0;
  const runtimeFieldCount = CATEGORIES.reduce((sum, category) => sum + category.fields.length, 0);

  const visibleConfigEntries = useMemo(
    () => configEntries.filter((entry) => !isReadOnlyConfigKey(entry.key) && entry.editable !== false),
    [configEntries],
  );

  const groupedConfigEntries = useMemo(
    () =>
      (Object.keys(SYSTEM_CATEGORY_META) as Array<keyof typeof SYSTEM_CATEGORY_META>).reduce<Record<string, ConfigEntry[]>>(
        (acc, categoryKey) => {
          acc[categoryKey] = visibleConfigEntries.filter((entry) => entry.category === categoryKey);
          return acc;
        },
        {},
      ),
    [visibleConfigEntries],
  );

  const activeRuntimeCategory = CATEGORIES.find((category) => category.id === activeRuntimeCategoryId) ?? CATEGORIES[0];

  const navItems = useMemo(
    () => [
      {
        id: "general" as const,
        title: tr("일반", "General"),
        detail: tr("회사명, CEO, 언어, 테마", "Company name, CEO, language, theme"),
        count: "4",
      },
      {
        id: "runtime" as const,
        title: tr("런타임", "Runtime"),
        detail: tr("폴링, 캐시, 경고 임계값", "Polling, cache, alert thresholds"),
        count: String(runtimeFieldCount),
      },
      {
        id: "pipeline" as const,
        title: tr("파이프라인", "Pipeline"),
        detail: tr("리뷰, 타임아웃, 상태 전환 정책", "Review, timeout, transition policy"),
        count: String(visibleConfigEntries.length),
      },
      {
        id: "onboarding" as const,
        title: tr("온보딩", "Onboarding"),
        detail: tr("Discord 연결과 초기 세팅 재실행", "Re-run Discord wiring and first-run setup"),
      },
    ],
    [runtimeFieldCount, tr, visibleConfigEntries.length],
  );

  const inputStyle: CSSProperties = {
    background: "var(--th-bg-surface)",
    border: "1px solid var(--th-border)",
    color: "var(--th-text)",
  };
  const primaryActionClass = "inline-flex min-h-[44px] shrink-0 items-center justify-center rounded-2xl px-5 py-2.5 text-sm font-medium text-white transition-colors disabled:opacity-50";
  const primaryActionStyle: CSSProperties = { background: "var(--th-accent-primary)" };
  const secondaryActionClass = "inline-flex min-h-[44px] items-center justify-center rounded-2xl border px-5 py-2.5 text-sm font-medium transition-[opacity,color,border-color] hover:opacity-100";
  const secondaryActionStyle: CSSProperties = {
    borderColor: "rgba(148,163,184,0.28)",
    color: "var(--th-text-secondary)",
    background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)",
  };
  const subtleButtonClass = "inline-flex items-center justify-center rounded-full border px-3 py-1.5 text-[11px] font-medium transition-colors";
  const subtleButtonStyle: CSSProperties = {
    borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
    color: "var(--th-text-muted)",
    background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)",
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

  const renderGeneralPanel = () => (
    <SettingsSection
      eyebrow={tr("일반", "General")}
      title={tr("일반 설정 카탈로그", "General settings catalog")}
      description={tr(
        "브랜드 정보는 먼저, 표시 환경은 별도 그룹으로 나눠 빠르게 훑을 수 있게 구성했습니다.",
        "Brand basics come first, while display preferences stay in a separate group for faster scanning.",
      )}
      actions={(
        <button
          onClick={handleSave}
          disabled={saving || !companyDirty}
          className={primaryActionClass}
          style={primaryActionStyle}
        >
          {saving ? tr("저장 중...", "Saving...") : tr("일반 설정 저장", "Save general settings")}
        </button>
      )}
    >
      <div className="mt-5 space-y-5">
        <SettingsSubsection
          title={tr("자주 쓰는 설정", "Frequent settings")}
          description={tr(
            "오피스와 대시보드에서 가장 먼저 보이는 이름을 바로 바꿀 수 있습니다.",
            "Adjust the names that appear first across office and dashboard surfaces.",
          )}
        >
          <div className="grid gap-3 xl:grid-cols-2">
            <CompactFieldCard
              label={tr("회사 이름", "Company name")}
              tooltip={tr("대시보드와 주요 헤더에 표시되는 이름입니다.", "Shown in the dashboard and primary headers.")}
            >
              <input
                type="text"
                value={companyName}
                onChange={(event) => setCompanyName(event.target.value)}
                className="w-full rounded-2xl px-3 py-2.5 text-sm"
                style={inputStyle}
              />
            </CompactFieldCard>

            <CompactFieldCard
              label={tr("CEO 이름", "CEO name")}
              tooltip={tr("오피스와 일부 운영 UI에서 대표 인물 이름으로 사용됩니다.", "Used as the representative persona name in office and ops surfaces.")}
            >
              <input
                type="text"
                value={ceoName}
                onChange={(event) => setCeoName(event.target.value)}
                className="w-full rounded-2xl px-3 py-2.5 text-sm"
                style={inputStyle}
              />
            </CompactFieldCard>
          </div>
        </SettingsSubsection>

        <SettingsSubsection
          title={tr("표시 환경", "Display preferences")}
          description={tr(
            "언어와 테마처럼 덜 자주 바꾸는 표시 옵션은 별도 그룹으로 분리했습니다.",
            "Less frequently changed presentation options such as language and theme stay in their own group.",
          )}
        >
          <div className="grid gap-3 xl:grid-cols-2">
            <CompactFieldCard
              label={tr("언어", "Language")}
              tooltip={tr("대시보드 전반의 기본 언어와 로캘을 정합니다.", "Sets the default language and locale across the dashboard.")}
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
            </CompactFieldCard>

            <CompactFieldCard
              label={tr("테마", "Theme")}
              tooltip={tr("대시보드와 오피스 화면의 기본 분위기를 정합니다.", "Sets the base look and feel for dashboard and office views.")}
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
            </CompactFieldCard>
          </div>
        </SettingsSubsection>
      </div>
    </SettingsSection>
  );

  const renderRuntimePanel = () => (
    <SettingsSection
      eyebrow={tr("런타임", "Runtime")}
      title={tr("운영 리듬과 캐시", "Cadence and cache")}
      description={tr(
        "재시작 없이 바로 반영되는 값만 모았습니다.",
        "Only the values that apply without restart are shown here.",
      )}
      actions={(
        <button
          onClick={handleRcSave}
          disabled={rcSaving || !rcDirty}
          className={primaryActionClass}
          style={primaryActionStyle}
        >
          {rcSaving ? tr("저장 중...", "Saving...") : tr("런타임 저장", "Save runtime")}
        </button>
      )}
    >
      {!rcLoaded ? (
        <SettingsEmptyState className="mt-5 text-sm">
          {tr("런타임 설정을 불러오는 중...", "Loading runtime config...")}
        </SettingsEmptyState>
      ) : (
        <div className="mt-5 space-y-4">
          <div className="flex flex-wrap gap-2">
            {CATEGORIES.map((category) => (
              <button
                key={category.id}
                type="button"
                onClick={() => setActiveRuntimeCategoryId(category.id)}
                className={subtleButtonClass}
                style={{
                  ...subtleButtonStyle,
                  borderColor: activeRuntimeCategoryId === category.id
                    ? "color-mix(in srgb, var(--th-accent-primary) 30%, var(--th-border) 70%)"
                    : subtleButtonStyle.borderColor,
                  color: activeRuntimeCategoryId === category.id ? "var(--th-text)" : subtleButtonStyle.color,
                  background: activeRuntimeCategoryId === category.id
                    ? "color-mix(in srgb, var(--th-accent-primary-soft) 68%, transparent)"
                    : subtleButtonStyle.background,
                }}
              >
                {tr(category.titleKo, category.titleEn)}
              </button>
            ))}
          </div>

          {activeRuntimeCategory && (
            <SettingsSubsection
              title={tr(activeRuntimeCategory.titleKo, activeRuntimeCategory.titleEn)}
              description={tr(activeRuntimeCategory.descriptionKo, activeRuntimeCategory.descriptionEn)}
            >
              <div className="grid gap-3 xl:grid-cols-2">
                {activeRuntimeCategory.fields.map((field) => {
                  const value = rcValues[field.key] ?? rcDefaults[field.key] ?? 0;
                  const defaultValue = rcDefaults[field.key] ?? 0;
                  const isDefault = value === defaultValue;

                  return (
                    <CompactFieldCard
                      key={field.key}
                      label={tr(field.labelKo, field.labelEn)}
                      tooltip={tr(field.descriptionKo, field.descriptionEn)}
                      footer={tr(
                        `현재 ${formatUnit(value, field.unit)} · 기본값 ${formatUnit(defaultValue, field.unit)}`,
                        `Current ${formatUnit(value, field.unit)} · Default ${formatUnit(defaultValue, field.unit)}`,
                      )}
                    >
                      <div className="flex items-center gap-3">
                        <input
                          type="range"
                          min={field.min}
                          max={field.max}
                          step={field.step}
                          value={value}
                          onChange={(event) => handleRcChange(field.key, Number(event.target.value))}
                          className="h-1.5 flex-1 cursor-pointer appearance-none rounded-full"
                          style={{ accentColor: "var(--th-accent-primary)" }}
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
                          className="w-24 rounded-xl px-2.5 py-1.5 text-right text-xs font-mono"
                          style={inputStyle}
                        />
                      </div>
                      {!isDefault && (
                        <div className="mt-3 flex justify-end">
                          <button
                            type="button"
                            onClick={() => handleRcReset(field.key)}
                            className={subtleButtonClass}
                            style={subtleButtonStyle}
                          >
                            {tr("기본값 복원", "Reset")}
                          </button>
                        </div>
                      )}
                    </CompactFieldCard>
                  );
                })}
              </div>
            </SettingsSubsection>
          )}
        </div>
      )}
    </SettingsSection>
  );

  const renderPipelineCategory = (categoryKey: keyof typeof SYSTEM_CATEGORY_META) => {
    const entries = groupedConfigEntries[categoryKey] ?? [];
    if (entries.length === 0) return null;
    const meta = SYSTEM_CATEGORY_META[categoryKey];

    return (
      <SettingsSubsection
        key={categoryKey}
        title={tr(meta.titleKo, meta.titleEn)}
        description={tr(meta.descriptionKo, meta.descriptionEn)}
      >
        <div className="grid gap-3 xl:grid-cols-2">
          {entries.map((entry) => {
            const description = SYSTEM_CONFIG_DESCRIPTIONS[entry.key];
            const hasLocalEdit = Object.prototype.hasOwnProperty.call(configEdits, entry.key);
            const currentValue = hasLocalEdit ? configEdits[entry.key] : (entry.value ?? entry.default ?? "");
            const defaultLabel = entry.default ?? tr("없음", "None");
            const isEnabled = parseBooleanConfigValue(currentValue);

            if (isBooleanConfigKey(entry.key)) {
              return (
                <CompactFieldCard
                  key={entry.key}
                  label={isKo ? entry.label_ko : entry.label_en}
                  tooltip={isKo ? description?.ko ?? entry.key : description?.en ?? entry.key}
                  footer={tr(`기본값 ${defaultLabel}`, `Default ${defaultLabel}`)}
                >
                  <button
                    type="button"
                    role="switch"
                    aria-checked={isEnabled}
                    onClick={() => handleConfigEdit(entry.key, !isEnabled)}
                    className="flex min-h-[52px] w-full items-center justify-between rounded-2xl border px-3 py-3 text-left transition-colors"
                    style={{
                      borderColor: isEnabled ? "rgba(16,185,129,0.35)" : "rgba(148,163,184,0.24)",
                      background: isEnabled ? "rgba(16,185,129,0.12)" : "rgba(15,23,42,0.2)",
                    }}
                  >
                    <div className="pr-3">
                      <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
                        {isEnabled ? tr("활성화", "Enabled") : tr("비활성", "Disabled")}
                      </div>
                    </div>
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
                </CompactFieldCard>
              );
            }

            return (
              <CompactFieldCard
                key={entry.key}
                label={isKo ? entry.label_ko : entry.label_en}
                tooltip={isKo ? description?.ko ?? entry.key : description?.en ?? entry.key}
                footer={tr(`기본값 ${defaultLabel}`, `Default ${defaultLabel}`)}
              >
                <input
                  type={isNumericConfigKey(entry.key) ? "number" : "text"}
                  inputMode={isNumericConfigKey(entry.key) ? "numeric" : undefined}
                  className="w-full rounded-2xl px-3 py-2.5 text-sm"
                  style={inputStyle}
                  value={String(currentValue)}
                  onChange={(event) => handleConfigEdit(entry.key, event.target.value)}
                />
              </CompactFieldCard>
            );
          })}
        </div>
      </SettingsSubsection>
    );
  };

  const renderPipelinePanel = () => (
    <SettingsSection
      eyebrow={tr("파이프라인", "Pipeline")}
      title={tr("리뷰와 상태 전환 정책", "Review and transition policy")}
      description={tr(
        "운영 흐름에 직접 영향을 주는 항목만 남기고, 저장 위치 같은 내부 메모는 숨겼습니다.",
        "Only the controls that affect the live workflow remain visible; storage implementation notes are hidden.",
      )}
      actions={(
        <button
          onClick={handleConfigSave}
          disabled={configSaving || !configDirty}
          className={primaryActionClass}
          style={primaryActionStyle}
        >
          {configSaving ? tr("저장 중...", "Saving...") : tr("파이프라인 저장", "Save pipeline")}
        </button>
      )}
    >
      {configEntries.length === 0 ? (
        <SettingsEmptyState className="mt-5 text-sm">
          {tr("파이프라인 설정을 불러오는 중...", "Loading pipeline config...")}
        </SettingsEmptyState>
      ) : (
        <div className="mt-5 space-y-5">
          <div className="space-y-3">
            <GroupLabel title={tr("자주 쓰는 설정", "Frequent settings")} />
            {PRIMARY_PIPELINE_CATEGORIES.map(renderPipelineCategory)}
          </div>
          <div className="space-y-3">
            <GroupLabel title={tr("고급 설정", "Advanced settings")} />
            {ADVANCED_PIPELINE_CATEGORIES.map(renderPipelineCategory)}
          </div>
        </div>
      )}
    </SettingsSection>
  );

  const renderOnboardingPanel = () => (
    <SettingsSection
      eyebrow={tr("온보딩", "Onboarding")}
      title={tr("초기 연결과 기본 세팅", "Initial wiring and defaults")}
      description={tr(
        "Discord 연결, owner/provider, 기본 파이프라인 같은 첫 세팅은 전용 위저드에서 다시 수행합니다.",
        "Re-run Discord wiring, owner/provider setup, and first-run defaults from the dedicated wizard.",
      )}
      actions={(
        <button
          onClick={() => setShowOnboarding(true)}
          className={secondaryActionClass}
          style={secondaryActionStyle}
        >
          {tr("온보딩 다시 실행", "Re-run onboarding")}
        </button>
      )}
    >
      <div className="mt-5 grid gap-3 xl:grid-cols-[minmax(0,1.15fr)_minmax(16rem,0.85fr)]">
        <SettingsCard
          className="rounded-3xl p-5"
          style={{
            borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
            background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
          }}
        >
          <div className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {tr("위저드가 처리하는 범위", "What the wizard covers")}
          </div>
          <div className="mt-4 space-y-3 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
            <div>{tr("Discord 봇 토큰, guild/owner, provider 연결", "Discord bot token, guild/owner, and provider wiring")}</div>
            <div>{tr("기본 채널/카테고리와 role map 구성", "Default channels/categories and role-map setup")}</div>
            <div>{tr("기본 운영 파이프라인과 초기 설정 재생성", "Default operating pipeline and initial config regeneration")}</div>
          </div>
        </SettingsCard>

        <SettingsCard
          className="rounded-3xl p-5"
          style={{
            borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
            background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
          }}
        >
          <div className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {tr("권장 시점", "When to run it")}
          </div>
          <div className="mt-4 space-y-3 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
            <div>{tr("새 워크스페이스를 처음 붙일 때", "When wiring a new workspace for the first time")}</div>
            <div>{tr("봇 토큰이나 owner/provider를 바꿨을 때", "When bot tokens or owner/provider settings changed")}</div>
            <div>{tr("기본 채널/정책을 다시 생성해야 할 때", "When default channels or policies need to be recreated")}</div>
          </div>
        </SettingsCard>
      </div>
    </SettingsSection>
  );

  const renderActivePanel = () => {
    switch (activePanel) {
      case "runtime":
        return renderRuntimePanel();
      case "pipeline":
        return renderPipelinePanel();
      case "onboarding":
        return renderOnboardingPanel();
      case "general":
      default:
        return renderGeneralPanel();
    }
  };

  return (
    <div
      className="mx-auto h-full w-full max-w-6xl min-w-0 overflow-x-hidden px-4 py-4 pb-40 sm:px-6"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <div className="flex h-full min-h-0 flex-col gap-4 lg:flex-row">
        <aside
          className="hidden lg:flex lg:w-56 lg:shrink-0 lg:flex-col lg:gap-2 lg:rounded-[28px] lg:border lg:p-3"
          style={{
            borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
            background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
          }}
        >
          <div className="rounded-2xl px-2 py-3">
            <div className="text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
              {tr("설정", "Settings")}
            </div>
            <div className="mt-2 text-lg font-semibold tracking-tight" style={{ color: "var(--th-text)" }}>
              {tr("운영 설정 카탈로그", "Operations settings catalog")}
            </div>
            <div className="mt-2 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
              {tr("큰 화면에서는 왼쪽에서 섹션을 고정하고, 오른쪽 내용만 스크롤합니다.", "Keep the section picker fixed on large screens and scroll only the content pane.")}
            </div>
          </div>

          {navItems.map((item) => (
            <PanelNavButton
              key={item.id}
              active={activePanel === item.id}
              title={item.title}
              detail={item.detail}
              count={item.count}
              onClick={() => setActivePanel(item.id)}
            />
          ))}

          <div className="mt-auto pt-2">
            <button
              onClick={() => {
                setActivePanel("onboarding");
                setShowOnboarding(true);
              }}
              className={secondaryActionClass}
              style={{ ...secondaryActionStyle, width: "100%" }}
            >
              {tr("온보딩 다시 실행", "Re-run onboarding")}
            </button>
          </div>
        </aside>

        <div className="min-w-0 flex-1 lg:min-h-0">
          <div className="flex flex-col gap-4 lg:h-full">
            <div className="flex items-start justify-between gap-3 lg:hidden">
              <div className="min-w-0">
                <div className="text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                  {tr("설정", "Settings")}
                </div>
                <div className="mt-1 text-xl font-semibold tracking-tight" style={{ color: "var(--th-text)" }}>
                  {tr("운영 설정", "Operations settings")}
                </div>
              </div>
              <button
                onClick={() => {
                  setActivePanel("onboarding");
                  setShowOnboarding(true);
                }}
                className={secondaryActionClass}
                style={secondaryActionStyle}
              >
                {tr("온보딩 다시 실행", "Re-run onboarding")}
              </button>
            </div>

            <div className="flex gap-2 overflow-x-auto pb-1 lg:hidden">
              {navItems.map((item) => (
                <button
                  key={item.id}
                  type="button"
                  onClick={() => setActivePanel(item.id)}
                  className="shrink-0 rounded-full border px-3 py-2 text-xs font-medium transition-colors"
                  style={{
                    borderColor: activePanel === item.id
                      ? "color-mix(in srgb, var(--th-accent-primary) 30%, var(--th-border) 70%)"
                      : "color-mix(in srgb, var(--th-border) 72%, transparent)",
                    background: activePanel === item.id
                      ? "color-mix(in srgb, var(--th-accent-primary-soft) 68%, transparent)"
                      : "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
                    color: activePanel === item.id ? "var(--th-text)" : "var(--th-text-muted)",
                  }}
                >
                  {item.title}
                </button>
              ))}
            </div>

            <div className="min-w-0 lg:min-h-0 lg:flex-1 lg:overflow-y-auto lg:pr-1">
              {renderActivePanel()}
            </div>
          </div>
        </div>
      </div>

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
