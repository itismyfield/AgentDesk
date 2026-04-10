import { Suspense, lazy, useEffect, useState, type CSSProperties, type ReactNode } from "react";
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
};

type ConfigEditValue = string | boolean;
type AuditNoteStatus = "read-only" | "managed-elsewhere" | "backend-contract";

interface AuditNote {
  id: string;
  titleKo: string;
  titleEn: string;
  descriptionKo: string;
  descriptionEn: string;
  keys: string[];
  status: AuditNoteStatus;
}

const CATEGORIES: Array<{
  titleKo: string;
  titleEn: string;
  descriptionKo: string;
  descriptionEn: string;
  fields: ConfigField[];
}> = [
  {
    titleKo: "폴링 & 타이머",
    titleEn: "Polling & Timers",
    descriptionKo: "백엔드 동기화와 배치 작업이 얼마나 자주 실행되는지 조정합니다.",
    descriptionEn: "Controls how often backend sync and batch jobs run.",
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
  "counter_model_review_enabled",
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
  server_port: {
    ko: "서버가 실제로 바인딩한 API 포트입니다. 부팅 시 설정 파일에서 다시 동기화되어 읽기 전용으로 취급해야 합니다.",
    en: "The actual API port bound by the server. It is synced from server config on boot and should be treated as read-only.",
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
  automation: {
    titleKo: "자동 머지",
    titleEn: "Merge Automation",
    descriptionKo: "자동 머지 허용 여부, 전략, 허용 작성자를 관리합니다.",
    descriptionEn: "Manages merge automation enablement, strategy, and allowed authors.",
  },
  timeout: {
    titleKo: "타임아웃",
    titleEn: "Timeouts",
    descriptionKo: "정체 감지와 자동 알림 시점을 조정합니다.",
    descriptionEn: "Tunes stale detection and automatic alert timing.",
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
    descriptionKo: "서버와 Discord 라우팅에 연결되는 핵심 시스템 값입니다.",
    descriptionEn: "Core system values tied to server and Discord routing.",
  },
} as const;

const AUDIT_NOTES: AuditNote[] = [
  {
    id: "settings-json-merge",
    titleKo: "회사 설정 JSON은 full replace 계약",
    titleEn: "Company settings JSON uses a full-replace contract",
    descriptionKo: "`/api/settings`는 body 전체를 저장하고, 정리 대상 legacy key는 서버에서 제거합니다. UI는 merged object를 보내고 저장 후 재조회해 서버 canonical state를 다시 맞춥니다.",
    descriptionEn: "`/api/settings` stores the full body and strips retired legacy keys on the server. The UI sends a merged object and re-fetches afterward to align with the canonical server state.",
    keys: ["settings"],
    status: "backend-contract",
  },
  {
    id: "server-port-readonly",
    titleKo: "`server_port`는 사실상 읽기 전용",
    titleEn: "`server_port` is effectively read-only",
    descriptionKo: "`src/server/mod.rs`에서 서버 부팅 시 `config.server.port` 값으로 매번 다시 기록합니다. UI에서 수정 가능한 설정처럼 보이면 오해를 만듭니다.",
    descriptionEn: "`src/server/mod.rs` rewrites it from `config.server.port` on every boot. Treating it as editable in the UI is misleading.",
    keys: ["server_port"],
    status: "read-only",
  },
  {
    id: "onboarding-secrets",
    titleKo: "온보딩 관련 설정은 별도 API/DB 전용",
    titleEn: "Onboarding settings are managed through a dedicated API/DB path",
    descriptionKo: "봇 토큰, guild/owner/provider, 보조 command token은 `/api/onboarding/*`와 개별 `kv_meta` 키로 관리됩니다. 일반 설정창에 text input으로 섞기보다 전용 온보딩 흐름으로 유지하는 편이 안전합니다.",
    descriptionEn: "Bot tokens, guild/owner/provider, and secondary command tokens are managed via `/api/onboarding/*` and dedicated `kv_meta` keys. They should stay behind onboarding-specific flows.",
    keys: [
      "onboarding_bot_token",
      "onboarding_guild_id",
      "onboarding_owner_id",
      "onboarding_announce_token",
      "onboarding_notify_token",
      "onboarding_command_token_2",
      "onboarding_provider",
      "onboarding_command_provider_2",
    ],
    status: "managed-elsewhere",
  },
  {
    id: "room-theme-multipath",
    titleKo: "`roomThemes`는 단일 정본이 아님",
    titleEn: "`roomThemes` is not a single-source setting",
    descriptionKo: "`dashboard/src/app/office-workflow-pack.ts`에서 preset room theme와 custom room theme를 합쳐 사용합니다. 단순 일반 설정 필드보다 office/visual 편집 흐름에서 관리하는 편이 맞습니다.",
    descriptionEn: "`dashboard/src/app/office-workflow-pack.ts` merges preset room themes with custom room themes. It fits office/visual editing better than a generic settings form.",
    keys: ["roomThemes"],
    status: "managed-elsewhere",
  },
  {
    id: "merge-automation-surface",
    titleKo: "merge automation은 개별 정책 키 surface",
    titleEn: "Merge automation now lives on the policy-key surface",
    descriptionKo: "`merge_automation_enabled`, `merge_strategy`, `merge_allowed_authors`는 `agentdesk.yaml`의 `automation:` baseline 위에 대시보드가 `kv_meta` runtime override를 덮는 구조입니다.",
    descriptionEn: "`merge_automation_enabled`, `merge_strategy`, and `merge_allowed_authors` now use `agentdesk.yaml` `automation:` as the startup baseline while the dashboard writes `kv_meta` runtime overrides on top.",
    keys: ["merge_automation_enabled", "merge_strategy", "merge_allowed_authors"],
    status: "backend-contract",
  },
];

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

function auditStatusLabel(status: AuditNoteStatus, isKo: boolean): string {
  if (isKo) {
    if (status === "read-only") return "읽기 전용";
    if (status === "managed-elsewhere") return "별도 관리";
    return "계약 명시";
  }
  if (status === "read-only") return "Read-only";
  if (status === "managed-elsewhere") return "Managed elsewhere";
  return "Contract clarified";
}

function auditStatusClass(status: AuditNoteStatus): string {
  if (status === "read-only") return "border-slate-400/30 bg-slate-400/10 text-slate-200";
  if (status === "managed-elsewhere") return "border-emerald-400/30 bg-emerald-400/10 text-emerald-200";
  return "border-sky-400/30 bg-sky-400/10 text-sky-100";
}

interface SectionHeadingProps {
  eyebrow: string;
  title: string;
  description: string;
  badge?: string;
}

function SectionHeading({ eyebrow, title, description, badge }: SectionHeadingProps) {
  return (
    <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
      <div className="min-w-0">
        <div className="text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
          {eyebrow}
        </div>
        <h3 className="mt-1 text-xl font-semibold tracking-tight" style={{ color: "var(--th-text)" }}>
          {title}
        </h3>
        <p className="mt-2 max-w-3xl text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
          {description}
        </p>
      </div>
      {badge && (
        <span
          className="inline-flex shrink-0 items-center rounded-full border px-3 py-1 text-[11px] font-medium"
          style={{ borderColor: "rgba(99,102,241,0.32)", background: "rgba(99,102,241,0.12)", color: "#c7d2fe" }}
        >
          {badge}
        </span>
      )}
    </div>
  );
}

interface SummaryCardProps {
  label: string;
  value: string;
  description: string;
  accent?: string;
}

function SummaryCard({ label, value, description, accent = "#6366f1" }: SummaryCardProps) {
  return (
    <div
      className="rounded-2xl border p-4"
      style={{
        borderColor: "rgba(148,163,184,0.16)",
        background: `linear-gradient(180deg, color-mix(in srgb, ${accent} 10%, rgba(15,23,42,0.96)) 0%, rgba(15,23,42,0.74) 100%)`,
      }}
    >
      <div className="text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
        {label}
      </div>
      <div className="mt-2 text-2xl font-semibold" style={{ color: "var(--th-text)" }}>
        {value}
      </div>
      <p className="mt-2 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
        {description}
      </p>
    </div>
  );
}

interface SurfaceCardProps {
  title: string;
  body: string;
  footer: string;
}

function SurfaceCard({ title, body, footer }: SurfaceCardProps) {
  return (
    <div
      className="rounded-2xl border p-4"
      style={{ borderColor: "rgba(148,163,184,0.14)", background: "rgba(15,23,42,0.34)" }}
    >
      <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
        {title}
      </div>
      <p className="mt-2 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
        {body}
      </p>
      <div className="mt-3 text-[11px] font-medium uppercase tracking-[0.16em]" style={{ color: "var(--th-text-secondary)" }}>
        {footer}
      </div>
    </div>
  );
}

interface InputCardProps {
  label: string;
  description: string;
  children: ReactNode;
}

function InputCard({ label, description, children }: InputCardProps) {
  return (
    <div
      className="rounded-2xl border p-4"
      style={{ borderColor: "rgba(148,163,184,0.18)", background: "rgba(15,23,42,0.28)" }}
    >
      <label className="block text-sm font-medium" style={{ color: "var(--th-text)" }}>
        {label}
      </label>
      <p className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
        {description}
      </p>
      <div className="mt-3">{children}</div>
    </div>
  );
}

interface AuditNoteCardProps {
  note: AuditNote;
  isKo: boolean;
}

function AuditNoteCard({ note, isKo }: AuditNoteCardProps) {
  return (
    <div
      className="rounded-2xl border p-4"
      style={{ borderColor: "rgba(148,163,184,0.16)", background: "rgba(15,23,42,0.28)" }}
    >
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
            {isKo ? note.titleKo : note.titleEn}
          </div>
          <p className="mt-2 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
            {isKo ? note.descriptionKo : note.descriptionEn}
          </p>
        </div>
        <span className={`inline-flex shrink-0 items-center rounded-full border px-2.5 py-1 text-[11px] font-medium ${auditStatusClass(note.status)}`}>
          {auditStatusLabel(note.status, isKo)}
        </span>
      </div>
      <div className="mt-3 flex flex-wrap gap-2">
        {note.keys.map((key) => (
          <span
            key={key}
            className="inline-flex items-center rounded-full border px-2.5 py-1 text-[11px]"
            style={{ borderColor: "rgba(148,163,184,0.22)", color: "var(--th-text-secondary)" }}
          >
            {key}
          </span>
        ))}
      </div>
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
  const tr = (ko: string, en: string) => (isKo ? ko : en);

  const [rcValues, setRcValues] = useState<Record<string, number>>({});
  const [rcDefaults, setRcDefaults] = useState<Record<string, number>>({});
  const [rcLoaded, setRcLoaded] = useState(false);
  const [rcSaving, setRcSaving] = useState(false);
  const [rcDirty, setRcDirty] = useState(false);
  const [escalationSettings, setEscalationSettings] = useState<api.EscalationSettings | null>(null);
  const [escalationDefaults, setEscalationDefaults] = useState<api.EscalationSettings | null>(null);
  const [escalationBaseline, setEscalationBaseline] = useState<api.EscalationSettings | null>(null);
  const [escalationSaving, setEscalationSaving] = useState(false);

  const [configEntries, setConfigEntries] = useState<ConfigEntry[]>([]);
  const [configEdits, setConfigEdits] = useState<Record<string, ConfigEditValue>>({});
  const [configSaving, setConfigSaving] = useState(false);
  const [showOnboarding, setShowOnboarding] = useState(false);

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
      .then((data: { entries: ConfigEntry[] }) => setConfigEntries(data.entries || []))
      .catch(() => {});

    void api.getEscalationSettings()
      .then((data) => {
        setEscalationSettings(data.current);
        setEscalationBaseline(data.current);
        setEscalationDefaults(data.defaults);
      })
      .catch(() => {});
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
  const runtimeFieldCount = CATEGORIES.reduce((sum, category) => sum + category.fields.length, 0);

  const inputStyle: CSSProperties = {
    background: "var(--th-bg-surface)",
    border: "1px solid var(--th-border)",
    color: "var(--th-text)",
  };
  const sectionStyle: CSSProperties = {
    border: "1px solid rgba(148,163,184,0.16)",
    background: "linear-gradient(180deg, rgba(15,23,42,0.72) 0%, rgba(15,23,42,0.44) 100%)",
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
      setEscalationDefaults(data.defaults);
    } finally {
      setEscalationSaving(false);
    }
  };

  return (
    <div
      className="mx-auto h-full max-w-5xl min-w-0 space-y-6 overflow-x-hidden overflow-y-auto px-4 py-5 pb-40 sm:px-6"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <section
        className="rounded-[28px] border p-5 sm:p-6"
        style={{
          borderColor: "rgba(99,102,241,0.22)",
          background: "radial-gradient(circle at top left, rgba(99,102,241,0.22), rgba(15,23,42,0.9) 48%, rgba(15,23,42,0.75) 100%)",
        }}
      >
        <div className="flex flex-col gap-4 xl:flex-row xl:items-start xl:justify-between">
          <div className="min-w-0">
            <div className="text-[11px] font-semibold uppercase tracking-[0.22em]" style={{ color: "#c7d2fe" }}>
              {tr("설정 제어실", "Settings Control Room")}
            </div>
            <h2 className="mt-2 text-2xl font-semibold tracking-tight sm:text-3xl" style={{ color: "var(--th-text)" }}>
              {tr("AgentDesk 설정창 재정렬", "Reframing AgentDesk settings")}
            </h2>
            <p className="mt-3 max-w-3xl text-sm leading-6" style={{ color: "rgba(226,232,240,0.82)" }}>
              {tr(
                "설정은 단순 입력 폼이 아니라 저장 위치와 적용 범위를 이해해야 안전하게 다룰 수 있습니다. 이 화면은 회사 설정, 즉시 반영 런타임 설정, 개별 정책 키, 별도 관리 surface를 분리해서 보여줍니다.",
                "Settings are safe only when their storage surface and effect scope are visible. This view separates company settings, live runtime tuning, individual policy keys, and surfaces managed elsewhere.",
              )}
            </p>
          </div>
          <div
            className="rounded-2xl border px-4 py-3 text-sm"
            style={{ borderColor: "rgba(148,163,184,0.2)", background: "rgba(15,23,42,0.38)", color: "var(--th-text-secondary)" }}
          >
            {tr("현재 일반 설정 저장은 merged object로 수행되어 hidden JSON key를 보존합니다.", "General settings now save as a merged object so hidden JSON keys stay intact.")}
          </div>
        </div>

        <div className="mt-6 grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
          <SummaryCard
            label={tr("회사 설정", "Company Settings")}
            value="4"
            description={tr("이 화면에서 직접 편집하는 브랜드/언어/테마 설정", "Brand, language, and theme controls edited directly here")}
          />
          <SummaryCard
            label={tr("런타임 튜닝", "Live Runtime")}
            value={String(runtimeFieldCount)}
            description={tr("재시작 없이 즉시 반영되는 운영 숫자 설정", "Operational tuning values that apply without restart")}
            accent="#22c55e"
          />
          <SummaryCard
            label={tr("정책 키", "Policy Keys")}
            value={String(configEntries.length)}
            description={tr("YAML baseline 위에 `kv_meta` override로 동작하는 파이프라인 정책", "Pipeline policy keys layered as YAML baselines with `kv_meta` overrides")}
            accent="#f59e0b"
          />
          <SummaryCard
            label={tr("정리 대상", "Audit Findings")}
            value={String(AUDIT_NOTES.length)}
            description={tr("별도 관리/읽기 전용/계약 명시로 남긴 설정 메모", "Settings notes kept as managed-elsewhere, read-only, or contract clarifications")}
            accent="#fb7185"
          />
        </div>
      </section>

      <section className="rounded-[28px] border p-5 sm:p-6" style={sectionStyle}>
        <SectionHeading
          eyebrow={tr("에스컬레이션", "Escalation")}
          title={tr("PM / owner 라우팅 전환", "Switch between PM and owner routing")}
          description={tr(
            "pending_decision 에스컬레이션을 PM 채널로 보낼지, owner 스레드로 보낼지, 시간대 기반으로 전환할지를 관리합니다.",
            "Controls whether pending-decision escalations go to the PM channel, an owner thread, or switch automatically by time window.",
          )}
          badge={tr("api/settings/escalation", "api/settings/escalation")}
        />

        {escalationSettings ? (
          <div className="mt-5 space-y-4">
            <div className="grid gap-4 lg:grid-cols-2">
              <InputCard
                label={tr("라우팅 모드", "Routing mode")}
                description={tr(
                  "PM 고정, owner 고정, 또는 scheduled 자동 전환 중 하나를 고릅니다.",
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
              </InputCard>

              <InputCard
                label={tr("fallback owner user ID", "Fallback owner user ID")}
                description={tr(
                  "live owner 추적이 비어 있을 때 owner 멘션에 사용할 Discord user ID입니다.",
                  "Discord user ID used for owner mentions when live owner tracking is unavailable.",
                )}
              >
                <input
                  type="text"
                  className="w-full rounded-2xl px-3 py-2.5 text-sm"
                  style={inputStyle}
                  value={escalationSettings.owner_user_id ?? ""}
                  onChange={(event) =>
                    handleEscalationChange((prev) => ({
                      ...prev,
                      owner_user_id: event.target.value.trim()
                        ? Number(event.target.value.trim())
                        : null,
                    }))
                  }
                />
              </InputCard>

              <InputCard
                label={tr("PM channel ID", "PM channel ID")}
                description={tr(
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
              </InputCard>

              <InputCard
                label={tr("PM hours", "PM hours")}
                description={tr(
                  "scheduled 모드에서 이 시간대에는 PM 라우팅으로 전환합니다. 형식: `HH:MM-HH:MM`.",
                  "When scheduled mode is active, this window routes to PM. Format: `HH:MM-HH:MM`.",
                )}
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
              </InputCard>

              <InputCard
                label={tr("Timezone", "Timezone")}
                description={tr(
                  "scheduled 모드 판단에 사용할 IANA timezone입니다. 예: `Asia/Seoul`.",
                  "IANA timezone used for scheduled-mode evaluation. Example: `Asia/Seoul`.",
                )}
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
              </InputCard>
            </div>

            <div className="flex flex-col gap-3 rounded-2xl border p-4 sm:flex-row sm:items-center sm:justify-between" style={{ borderColor: "rgba(148,163,184,0.16)", background: "rgba(15,23,42,0.28)" }}>
              <p className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
                {tr(
                  "Discord `!escalation` 명령과 같은 endpoint를 공유합니다. 기본값은 `agentdesk.yaml`, runtime override는 DB `kv_meta`에 저장됩니다.",
                  "Shares the same endpoint as the Discord `!escalation` command. Defaults come from `agentdesk.yaml`, while runtime overrides are stored in DB `kv_meta`.",
                )}
                {escalationDefaults && (
                  <>
                    {" "}
                    {tr(
                      `기본 schedule은 ${escalationDefaults.schedule.pm_hours} / ${escalationDefaults.schedule.timezone} 입니다.`,
                      `Default schedule is ${escalationDefaults.schedule.pm_hours} / ${escalationDefaults.schedule.timezone}.`,
                    )}
                  </>
                )}
              </p>
              <button
                onClick={handleEscalationSave}
                disabled={escalationSaving || !escalationDirty}
                className="inline-flex min-h-[44px] shrink-0 items-center justify-center rounded-2xl bg-amber-600 px-5 py-2.5 text-sm font-medium text-white transition-colors hover:bg-amber-500 disabled:opacity-50"
              >
                {escalationSaving ? tr("저장 중...", "Saving...") : tr("에스컬레이션 저장", "Save escalation")}
              </button>
            </div>
          </div>
        ) : (
          <div className="mt-5 rounded-2xl border p-4 text-sm" style={{ borderColor: "rgba(148,163,184,0.16)", background: "rgba(15,23,42,0.28)", color: "var(--th-text-muted)" }}>
            {tr("에스컬레이션 설정을 불러오는 중입니다.", "Loading escalation settings.")}
          </div>
        )}
      </section>

      <section className="rounded-[28px] border p-5 sm:p-6" style={sectionStyle}>
        <SectionHeading
          eyebrow={tr("저장 경로", "Storage Surfaces")}
          title={tr("어디에 저장되는지 먼저 보이게", "Make storage surfaces explicit")}
          description={tr(
            "설정이 한 곳에만 있지 않아서, 저장 경로를 이해하지 못하면 UI가 쉽게 거짓말을 하게 됩니다. 아래 카드는 현재 AgentDesk 설정의 실제 저장면을 요약합니다.",
            "Settings do not live in one place, so the UI becomes misleading unless the storage path is explicit. These cards summarize the current storage surfaces.",
          )}
        />

        <div className="mt-5 grid gap-3 lg:grid-cols-2 2xl:grid-cols-4">
          <SurfaceCard
            title={tr("회사 설정 JSON", "Company settings JSON")}
            body={tr(
              "`/api/settings`가 `kv_meta['settings']` 전체 JSON을 저장합니다. 부분 patch가 아니라 full replace라서, 저장할 때 기존 값을 합친 merged object가 필요하고 legacy key는 서버에서 제거됩니다.",
              "`/api/settings` stores the full `kv_meta['settings']` JSON. It is a full replace rather than a patch merge, so callers must send a merged object and the server strips retired legacy keys.",
            )}
            footer={tr("source: kv_meta['settings']", "source: kv_meta['settings']")}
          />
          <SurfaceCard
            title={tr("런타임 설정", "Runtime config")}
            body={tr(
              "`agentdesk.yaml`의 `runtime:` 섹션이 재시작 시 baseline이 되고, 대시보드 변경은 `kv_meta['runtime-config']` override로 즉시 반영됩니다.",
              "The `runtime:` section in `agentdesk.yaml` becomes the restart baseline, while dashboard edits apply immediately as `kv_meta['runtime-config']` overrides.",
            )}
            footer={tr("source: agentdesk.yaml runtime + kv_meta['runtime-config']", "source: agentdesk.yaml runtime + kv_meta['runtime-config']")}
          />
          <SurfaceCard
            title={tr("정책/파이프라인 키", "Policy and pipeline keys")}
            body={tr(
              "리뷰, 타임아웃, context compact, merge automation 값은 `agentdesk.yaml` baseline에서 시작하고, 운영 중 수정은 개별 `kv_meta` 키 override로 유지됩니다.",
              "Review, timeout, context-compaction, and merge automation values start from the `agentdesk.yaml` baseline and keep live edits as individual `kv_meta` overrides.",
            )}
            footer={tr("source: agentdesk.yaml + individual kv_meta overrides", "source: agentdesk.yaml + individual kv_meta overrides")}
          />
          <SurfaceCard
            title={tr("온보딩/시크릿", "Onboarding and secrets")}
            body={tr(
              "토큰과 온보딩 provider 설정은 일반 설정창이 아니라 전용 온보딩 API와 wizard에서 관리됩니다.",
              "Tokens and onboarding providers are managed through a dedicated onboarding API and wizard instead of the general settings form.",
            )}
            footer={tr("source: onboarding API + kv_meta", "source: onboarding API + kv_meta")}
          />
        </div>
      </section>

      <section className="rounded-[28px] border p-5 sm:p-6" style={sectionStyle}>
        <SectionHeading
          eyebrow={tr("회사 설정", "Workspace Identity")}
          title={tr("브랜드/언어/테마", "Brand, language, and theme")}
            description={tr(
              "이 섹션은 대시보드가 사람에게 어떻게 보일지 결정합니다. 저장 시 현재 `settings` JSON 전체와 합쳐 보내고, 서버 canonical state를 다시 불러와 숨겨진 키와 legacy key 정리를 동시에 맞춥니다.",
              "This section controls how the dashboard presents itself to people. Saves are merged with the current `settings` JSON and then re-fetched so hidden keys stay intact while retired keys are stripped server-side.",
            )}
            badge={tr("full replace API → merged save", "full replace API → merged save")}
        />

        <div className="mt-5 grid gap-3 lg:grid-cols-2">
          <InputCard
            label={tr("회사 이름", "Company name")}
            description={tr("대시보드 hero와 주요 타이틀에 노출됩니다.", "Shown in the dashboard hero and primary titles.")}
          >
            <input
              type="text"
              value={companyName}
              onChange={(event) => setCompanyName(event.target.value)}
              className="w-full rounded-2xl px-3 py-2.5 text-sm"
              style={inputStyle}
            />
          </InputCard>

          <InputCard
            label={tr("CEO 이름", "CEO name")}
            description={tr("오피스와 일부 운영 UI에서 대표 인물 이름으로 사용됩니다.", "Used as the representative persona name in office and ops surfaces.")}
          >
            <input
              type="text"
              value={ceoName}
              onChange={(event) => setCeoName(event.target.value)}
              className="w-full rounded-2xl px-3 py-2.5 text-sm"
              style={inputStyle}
            />
          </InputCard>

          <InputCard
            label={tr("언어", "Language")}
            description={tr("대시보드 전반의 로캘과 기본 텍스트 방향을 정합니다.", "Controls dashboard locale and default text language.")}
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
          </InputCard>

          <InputCard
            label={tr("테마", "Theme")}
            description={tr("대시보드 전체 테마와 오피스 화면의 기본 분위기를 정합니다.", "Sets the overall dashboard theme and base office mood.")}
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
          </InputCard>
        </div>

        <div className="mt-5 flex flex-col gap-3 rounded-2xl border p-4 sm:flex-row sm:items-center sm:justify-between" style={{ borderColor: "rgba(148,163,184,0.16)", background: "rgba(15,23,42,0.28)" }}>
          <p className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
            {tr(
              "이 저장 버튼은 현재 회사 설정 patch를 기존 `settings` JSON과 합쳐 보내고, 저장 후 서버 값을 다시 불러옵니다. `roomThemes` 같은 숨겨진 키는 보존하고 retired key는 서버 정본에 맞춰 제거합니다.",
              "This save action merges the edited patch with the existing `settings` JSON and re-fetches the server value afterward. Hidden keys such as `roomThemes` are preserved while retired keys are removed to match the server canonical state.",
            )}
          </p>
          <button
            onClick={handleSave}
            disabled={saving || !companyDirty}
            className="inline-flex min-h-[44px] shrink-0 items-center justify-center rounded-2xl bg-indigo-600 px-5 py-2.5 text-sm font-medium text-white transition-colors hover:bg-indigo-500 disabled:opacity-50"
          >
            {saving ? tr("저장 중...", "Saving...") : tr("회사 설정 저장", "Save company settings")}
          </button>
        </div>
      </section>

      <section className="rounded-[28px] border p-5 sm:p-6" style={sectionStyle}>
        <SectionHeading
          eyebrow={tr("즉시 반영", "Live Runtime")}
          title={tr("운영 리듬과 캐시 튜닝", "Tune runtime cadence and caching")}
          description={tr(
            "이 값들은 `agentdesk.yaml` `runtime:` baseline 위에 저장되는 live override입니다. 저장 즉시 반영되지만, 재시작하면 YAML baseline이 다시 기준이 됩니다.",
            "These values are live overrides layered on top of the `agentdesk.yaml` `runtime:` baseline. They apply immediately, but the YAML baseline becomes authoritative again on restart.",
          )}
          badge={tr("live override on YAML baseline", "live override on YAML baseline")}
        />

        {!rcLoaded ? (
          <div className="mt-5 rounded-2xl border p-4 text-sm" style={{ borderColor: "rgba(148,163,184,0.16)", color: "var(--th-text-muted)" }}>
            {tr("런타임 설정을 불러오는 중...", "Loading runtime config...")}
          </div>
        ) : (
          <div className="mt-5 space-y-5">
            {CATEGORIES.map((category) => (
              <div key={category.titleEn} className="rounded-3xl border p-4 sm:p-5" style={{ borderColor: "rgba(148,163,184,0.14)", background: "rgba(15,23,42,0.26)" }}>
                <div className="mb-4">
                  <h4 className="text-base font-medium" style={{ color: "var(--th-text)" }}>
                    {tr(category.titleKo, category.titleEn)}
                  </h4>
                  <p className="mt-1 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
                    {tr(category.descriptionKo, category.descriptionEn)}
                  </p>
                </div>
                <div className="grid gap-3 xl:grid-cols-2">
                  {category.fields.map((field) => {
                    const value = rcValues[field.key] ?? rcDefaults[field.key] ?? 0;
                    const defaultValue = rcDefaults[field.key] ?? 0;
                    const isDefault = value === defaultValue;

                    return (
                      <div
                        key={field.key}
                        className="rounded-2xl border p-4"
                        style={{ borderColor: "rgba(148,163,184,0.16)", background: "rgba(15,23,42,0.32)" }}
                      >
                        <div className="flex items-start justify-between gap-3">
                          <div className="min-w-0">
                            <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
                              {tr(field.labelKo, field.labelEn)}
                            </div>
                            <p className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                              {tr(field.descriptionKo, field.descriptionEn)}
                            </p>
                          </div>
                          <div className="shrink-0 text-right">
                            <div className="text-sm font-semibold" style={{ color: isDefault ? "var(--th-text)" : "#fbbf24" }}>
                              {formatUnit(value, field.unit)}
                            </div>
                            <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                              {tr("기본값", "Default")}: {formatUnit(defaultValue, field.unit)}
                            </div>
                          </div>
                        </div>

                        <div className="mt-4 flex items-center gap-3">
                          <input
                            type="range"
                            min={field.min}
                            max={field.max}
                            step={field.step}
                            value={value}
                            onChange={(event) => handleRcChange(field.key, Number(event.target.value))}
                            className="h-1.5 flex-1 cursor-pointer appearance-none rounded-full"
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
                            className="w-20 rounded-xl px-2.5 py-1.5 text-right text-xs font-mono"
                            style={inputStyle}
                          />
                        </div>

                        {!isDefault && (
                          <div className="mt-3 flex justify-end">
                            <button
                              onClick={() => handleRcReset(field.key)}
                              className="inline-flex items-center rounded-full border px-3 py-1 text-[11px] font-medium transition-colors hover:border-indigo-400/50 hover:text-indigo-200"
                              style={{ borderColor: "rgba(148,163,184,0.2)", color: "var(--th-text-muted)" }}
                            >
                              {tr("기본값으로 되돌리기", "Reset to default")}
                            </button>
                          </div>
                        )}
                      </div>
                    );
                  })}
                </div>
              </div>
            ))}

            <div className="flex flex-col gap-3 rounded-2xl border p-4 sm:flex-row sm:items-center sm:justify-between" style={{ borderColor: "rgba(148,163,184,0.16)", background: "rgba(15,23,42,0.28)" }}>
              <p className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
                {tr("런타임 설정은 저장 즉시 적용됩니다. 값 조정이 잦다면 먼저 작은 폭으로 바꾸고 Pulse/영수증/로그 반응을 확인하는 편이 안전합니다.", "Runtime config applies immediately. If you tune frequently, prefer small changes first and verify Pulse, receipts, and logs before making larger moves.")}
              </p>
              <button
                onClick={handleRcSave}
                disabled={rcSaving || !rcDirty}
                className="inline-flex min-h-[44px] shrink-0 items-center justify-center rounded-2xl bg-indigo-600 px-5 py-2.5 text-sm font-medium text-white transition-colors hover:bg-indigo-500 disabled:opacity-50"
              >
                {rcSaving ? tr("저장 중...", "Saving...") : tr("런타임 설정 저장", "Save runtime config")}
              </button>
            </div>
          </div>
        )}
      </section>

      <section className="rounded-[28px] border p-5 sm:p-6" style={sectionStyle}>
        <SectionHeading
          eyebrow={tr("파이프라인 정책", "Pipeline Policy")}
          title={tr("개별 `kv_meta` 키 관리", "Manage individual `kv_meta` keys")}
          description={tr(
            "리뷰, 자동 머지, 타임아웃, context compact, Discord 채널 연결 값은 `agentdesk.yaml` baseline과 개별 `kv_meta` override의 조합입니다. 여기서는 live override만 수정하고, restart baseline은 YAML이 담당합니다.",
            "Review, merge automation, timeout, context-compaction, and Discord channel IDs now combine an `agentdesk.yaml` baseline with individual `kv_meta` overrides. This section edits only the live override layer while YAML owns the restart baseline.",
          )}
          badge={tr("YAML baseline + live override", "YAML baseline + live override")}
        />

        {configEntries.length === 0 ? (
          <div className="mt-5 rounded-2xl border p-4 text-sm" style={{ borderColor: "rgba(148,163,184,0.16)", color: "var(--th-text-muted)" }}>
            {tr("시스템 설정을 불러오는 중...", "Loading system config...")}
          </div>
        ) : (
          <div className="mt-5 space-y-4">
            {(Object.keys(SYSTEM_CATEGORY_META) as Array<keyof typeof SYSTEM_CATEGORY_META>).map((categoryKey) => {
              const entries = configEntries.filter((entry) => entry.category === categoryKey);
              if (entries.length === 0) return null;
              const meta = SYSTEM_CATEGORY_META[categoryKey];

              return (
                <div key={categoryKey} className="rounded-3xl border p-4 sm:p-5" style={{ borderColor: "rgba(148,163,184,0.14)", background: "rgba(15,23,42,0.26)" }}>
                  <div className="mb-4">
                    <div className="text-base font-medium" style={{ color: "var(--th-text)" }}>
                      {tr(meta.titleKo, meta.titleEn)}
                    </div>
                    <p className="mt-1 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
                      {tr(meta.descriptionKo, meta.descriptionEn)}
                    </p>
                  </div>

                  <div className="grid gap-3 xl:grid-cols-2">
                    {entries.map((entry) => {
                      const description = SYSTEM_CONFIG_DESCRIPTIONS[entry.key];
                      const hasLocalEdit = Object.prototype.hasOwnProperty.call(configEdits, entry.key);
                      const currentValue = hasLocalEdit ? configEdits[entry.key] : (entry.value ?? entry.default ?? "");
                      const defaultLabel = entry.default ?? tr("없음", "None");
                      const readOnly = isReadOnlyConfigKey(entry.key);
                      const isEnabled = parseBooleanConfigValue(currentValue);

                      return (
                        <div
                          key={entry.key}
                          className="rounded-2xl border p-4"
                          style={{ borderColor: "rgba(148,163,184,0.16)", background: "rgba(15,23,42,0.32)" }}
                        >
                          <div className="flex flex-wrap items-start justify-between gap-3">
                            <div className="min-w-0">
                              <div className="flex flex-wrap items-center gap-2">
                                <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
                                  {isKo ? entry.label_ko : entry.label_en}
                                </div>
                                <span
                                  className="inline-flex items-center rounded-full border px-2 py-0.5 text-[10px] font-medium"
                                  style={{ borderColor: "rgba(148,163,184,0.2)", color: "var(--th-text-muted)" }}
                                >
                                  kv_meta
                                </span>
                                {readOnly && (
                                  <span className={`inline-flex items-center rounded-full border px-2 py-0.5 text-[10px] font-medium ${auditStatusClass("read-only")}`}>
                                    {auditStatusLabel("read-only", isKo)}
                                  </span>
                                )}
                              </div>
                              {description && (
                                <p className="mt-2 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                                  {isKo ? description.ko : description.en}
                                </p>
                              )}
                            </div>
                            <code className="max-w-full overflow-hidden text-ellipsis whitespace-nowrap text-[11px]" style={{ color: "var(--th-text-secondary)" }}>
                              {entry.key}
                            </code>
                          </div>

                          <div className="mt-4">
                            {isBooleanConfigKey(entry.key) ? (
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
                                <div className="pr-3">
                                  <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
                                    {isEnabled ? tr("활성화됨", "Enabled") : tr("비활성화됨", "Disabled")}
                                  </div>
                                  <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                                    {readOnly
                                      ? tr("서버 부팅 시 다시 동기화되는 항목입니다.", "This value is resynced on server boot.")
                                      : tr(`기본값: ${defaultLabel}`, `Default: ${defaultLabel}`)}
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
                            ) : entry.key === "merge_strategy" ? (
                              <>
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
                                <div className="mt-2 flex flex-wrap items-center gap-2 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                                  <span>{tr(`기본값: ${defaultLabel}`, `Default: ${defaultLabel}`)}</span>
                                  <span>{tr("GitHub auto-merge 전략과 1:1로 대응합니다.", "Maps directly to the GitHub auto-merge strategy.")}</span>
                                </div>
                              </>
                            ) : (
                              <>
                                <input
                                  type={isNumericConfigKey(entry.key) && !readOnly ? "number" : "text"}
                                  inputMode={isNumericConfigKey(entry.key) ? "numeric" : undefined}
                                  disabled={readOnly}
                                  className="w-full rounded-2xl px-3 py-2.5 text-sm disabled:cursor-not-allowed disabled:opacity-80"
                                  style={inputStyle}
                                  value={String(currentValue)}
                                  onChange={(event) => handleConfigEdit(entry.key, event.target.value)}
                                />
                                <div className="mt-2 flex flex-wrap items-center gap-2 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                                  <span>{tr(`기본값: ${defaultLabel}`, `Default: ${defaultLabel}`)}</span>
                                  {readOnly && (
                                    <span>{tr("이 값은 서버 설정에서 덮어써집니다.", "This value is overwritten from server config.")}</span>
                                  )}
                                  {entry.key.endsWith("_channel_id") && (
                                    <span>{tr("Discord ID는 정밀도 손실을 피하려고 문자열로 유지합니다.", "Discord IDs stay as strings to avoid precision loss.")}</span>
                                  )}
                                </div>
                              </>
                            )}
                          </div>
                        </div>
                      );
                    })}
                  </div>
                </div>
              );
            })}

            <div className="flex flex-col gap-3 rounded-2xl border p-4 sm:flex-row sm:items-center sm:justify-between" style={{ borderColor: "rgba(148,163,184,0.16)", background: "rgba(15,23,42,0.28)" }}>
              <p className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
                {tr(
                  "이 섹션은 whitelist된 개별 `kv_meta` override만 편집합니다. dead config였던 `max_chain_depth`와 `context_clear_*`는 surface에서 제거했고, 재시작 baseline은 `agentdesk.yaml`이 다시 적용됩니다.",
                  "This section edits only whitelisted `kv_meta` override keys. Retired keys such as `max_chain_depth` and `context_clear_*` have been removed from the surface, and the `agentdesk.yaml` baseline is re-applied on restart.",
                )}
              </p>
              <button
                onClick={handleConfigSave}
                disabled={configSaving || !configDirty}
                className="inline-flex min-h-[44px] shrink-0 items-center justify-center rounded-2xl bg-emerald-600 px-5 py-2.5 text-sm font-medium text-white transition-colors hover:bg-emerald-500 disabled:opacity-50"
              >
                {configSaving ? tr("저장 중...", "Saving...") : tr("정책 설정 저장", "Save policy settings")}
              </button>
            </div>
          </div>
        )}
      </section>

      <section className="rounded-[28px] border p-5 sm:p-6" style={sectionStyle}>
        <SectionHeading
          eyebrow={tr("감사 결과", "Audit Findings")}
          title={tr("별도 관리 / 정리 필요 항목", "Managed elsewhere / cleanup-needed items")}
          description={tr(
            "이 항목들은 일반 설정창에서 바로 편집하지 않는 편이 맞거나, read-only/계약 메모로 남겨두는 편이 더 정확한 surface입니다.",
            "These items are either better managed outside the general settings form or are more truthful when kept as read-only or contract notes.",
          )}
        />

        <div className="mt-5 grid gap-3 xl:grid-cols-2">
          {AUDIT_NOTES.map((note) => (
            <AuditNoteCard key={note.id} note={note} isKo={isKo} />
          ))}
        </div>
      </section>

      <section className="rounded-[28px] border p-5 sm:p-6" style={sectionStyle}>
        <SectionHeading
          eyebrow={tr("온보딩", "Onboarding")}
          title={tr("토큰과 첫 설정은 별도 흐름으로", "Keep secrets and first-run setup in a dedicated flow")}
          description={tr(
            "온보딩 관련 토큰/채널/provider 값은 일반 설정 필드보다 wizard가 더 안전하고 이해하기 쉽습니다. 그래서 여기서는 직접 text input으로 섞지 않고 전용 흐름으로 다시 진입하게 했습니다.",
            "Onboarding tokens, channel IDs, and provider values are safer and easier to understand inside a dedicated wizard than in the general settings form, so this screen links back to that flow instead of embedding raw text inputs.",
          )}
          badge={tr("dashboard > discord onboarding bridge", "dashboard > Discord onboarding bridge")}
        />

        <div className="mt-5 rounded-3xl border p-4 sm:p-5" style={{ borderColor: "rgba(148,163,184,0.16)", background: "rgba(15,23,42,0.28)" }}>
          <div className="grid gap-4 lg:grid-cols-[1fr_auto] lg:items-center">
            <div>
              <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
                {tr("온보딩 위저드 재실행", "Re-run onboarding wizard")}
              </div>
              <p className="mt-2 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
                {tr(
                  "봇 토큰, guild/owner, provider, announce/notify/command 토큰은 이 버튼으로 다시 설정합니다. 일반 설정창에서 직접 노출하지 않는 이유는 보안과 의미 설명을 같이 다루기 위해서입니다.",
                  "Use this button to reconfigure bot token, guild/owner, provider, and announce/notify/command tokens. They are kept out of the general settings form so security and meaning stay together.",
                )}
              </p>
            </div>
            <button
              onClick={() => setShowOnboarding(true)}
              className="inline-flex min-h-[44px] items-center justify-center rounded-2xl border px-5 py-2.5 text-sm font-medium transition-colors hover:border-indigo-400/50 hover:text-indigo-200"
              style={{ borderColor: "rgba(148,163,184,0.28)", color: "var(--th-text-secondary)" }}
            >
              {tr("온보딩 다시 열기", "Open onboarding again")}
            </button>
          </div>
        </div>
      </section>

      {showOnboarding && (
        <div className="fixed inset-0 z-50 overflow-y-auto bg-[#0a0e1a]" role="dialog" aria-modal="true" aria-label="Onboarding wizard">
          <div className="flex min-h-screen items-start justify-center pt-8 pb-16">
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
