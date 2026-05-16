import { Check, ChevronDown, Eye, Info, Search, X } from "lucide-react";
import { Suspense, lazy, useCallback, useEffect, useMemo, useRef, useState, type CSSProperties, type FormEvent, type ReactNode } from "react";
import type {
  Agent,
  CompanySettings,
  VoiceAgentConfig,
  VoiceConfigPutBody,
  VoiceConfigResponse,
  VoiceGlobalConfig,
  VoiceSensitivityMode,
} from "../types";
import * as api from "../api";
import type { GitHubRepoOption } from "../api";
import { STORAGE_KEYS } from "../lib/storageKeys";
import {
  readLocalStorageValue,
  writeLocalStorageValue,
} from "../lib/useLocalStorage";
import {
  SurfaceCallout as SettingsCallout,
  SurfaceCard as SettingsCard,
  SurfaceEmptyState as SettingsEmptyState,
  SurfaceSection as SettingsSection,
  SurfaceSubsection as SettingsSubsection,
} from "./common/SurfacePrimitives";

const OnboardingWizard = lazy(() => import("./OnboardingWizard"));
const FsmEditor = lazy(() => import("./agent-manager/FsmEditor"));
const PipelineVisualEditor = lazy(() => import("./agent-manager/PipelineVisualEditor"));

interface SettingsViewProps {
  settings: CompanySettings;
  onSave: (patch: Record<string, unknown>) => Promise<void>;
  isKo: boolean;
  onNotify?: (message: string, type?: SettingsNotificationType) => string | void;
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
type SettingsPanel = "general" | "runtime" | "pipeline" | "onboarding" | "voice";
type AuditNoteStatus = "read-only" | "managed-elsewhere" | "backend-contract" | "typed-only" | "backend-followup";
type SettingsNotificationType = "info" | "success" | "warning" | "error";

interface AuditNote {
  id: string;
  titleKo: string;
  titleEn: string;
  descriptionKo: string;
  descriptionEn: string;
  keys: string[];
  status: AuditNoteStatus;
}

/**
 * Source of a setting (where the value lives + governance).
 * - repo_canonical:  canonical config (e.g. agentdesk.yaml / repo defaults)
 * - runtime_config:  kv_meta['runtime-config'] live values
 * - kv_meta:         individual whitelisted kv_meta keys
 * - live_override:   kv_meta override active over baseline
 * - legacy_readonly: alias / non-canonical surface kept visible only
 * - computed:        derived value (no direct edit path)
 */
export type SettingSource =
  | "repo_canonical"
  | "runtime_config"
  | "kv_meta"
  | "live_override"
  | "legacy_readonly"
  | "computed";

export type SettingFlag =
  | "kv_meta"
  | "live_override"
  | "alert"
  | "read_only"
  | "restart_required";

export type ValidationState =
  | { ok: true }
  | { ok: false; messageKo: string; messageEn: string };

export type SettingGroupId = "pipeline" | "runtime" | "onboarding" | "general" | "voice";

/**
 * Canonical metadata that drives every SettingRow rendered in the settings page.
 * All settings — whether kv_meta, runtime config, or general identity — funnel
 * through this type so the UI can expose source / editable / restartRequired
 * uniformly.
 */
export interface SettingRowMeta {
  key: string;
  group: SettingGroupId | string;
  source: SettingSource;
  editable: boolean;
  restartRequired: boolean;
  defaultValue?: unknown;
  effectiveValue: unknown;
  validation?: ValidationState;
  flags: SettingFlag[];
  // Presentation extras (not part of the issue's required type but used by
  // the UI; declared as optional so the public type signature still matches).
  labelKo?: string;
  labelEn?: string;
  hintKo?: string;
  hintEn?: string;
  inputKind?: "text" | "number" | "toggle" | "select" | "readonly";
  selectOptions?: Array<{ value: string; labelKo: string; labelEn: string }>;
  valueUnit?: string;
  numericRange?: { min: number; max: number; step: number };
  storageLayerKo?: string;
  storageLayerEn?: string;
  restartNoteKo?: string;
  restartNoteEn?: string;
}

const SETTINGS_PANEL_QUERY_KEY = "settingsPanel";
const GENERAL_FIELD_KEYS = ["companyName", "ceoName", "language", "theme"] as const;
const PIPELINE_SELECTOR_REFRESH_TIMEOUT_MS = 5_000;
const PIPELINE_SELECTOR_CACHE_MAX_AGE_MS = 60_000;

const SETTINGS_GLOSSARY = [
  {
    termKo: "Dispatch",
    termEn: "Dispatch",
    definitionKo: "에이전트에게 작업을 전달하고 상태를 추적하는 실행 단위입니다.",
    definitionEn: "A unit of work handed to an agent and tracked through execution.",
  },
  {
    termKo: "Triage",
    termEn: "Triage",
    definitionKo: "새 이슈나 요청을 분류해 backlog, queue, review 중 어디로 보낼지 정하는 단계입니다.",
    definitionEn: "Classifies new issues or requests into backlog, queue, review, or another path.",
  },
  {
    termKo: "TTL",
    termEn: "TTL",
    definitionKo: "값이나 캐시가 신뢰 가능한 것으로 간주되는 유효 시간입니다.",
    definitionEn: "The time window where a value or cache entry is considered fresh.",
  },
  {
    termKo: "Context compact",
    termEn: "Context compact",
    definitionKo: "긴 대화나 로그를 요약해 다음 실행에 넘기는 컨텍스트 절약 단계입니다.",
    definitionEn: "Compacts long conversations or logs before handing them to the next run.",
  },
  {
    termKo: "Barge-in",
    termEn: "Barge-in",
    definitionKo: "음성 응답 중 사용자가 끼어들어 새 명령을 말하는 동작입니다.",
    definitionEn: "A user interruption while a voice response is still being spoken.",
  },
  {
    termKo: "NFC normalization",
    termEn: "NFC normalization",
    definitionKo: "한글과 유니코드 문자를 비교 가능한 조합형으로 정리하는 처리입니다.",
    definitionEn: "Normalizes Unicode text so Korean and other composed characters compare reliably.",
  },
] as const;

interface PipelineRepoCacheEntry {
  viewerLogin: string;
  repos: GitHubRepoOption[];
  fetchedAt: number;
}

interface PipelineAgentCacheEntry {
  agents: Agent[];
  fetchedAt: number;
}

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
    id: "autoQueue",
    titleKo: "자동 큐",
    titleEn: "Auto Queue",
    descriptionKo: "auto-queue entry 실패 재시도 상한과 복구 동작을 조절합니다.",
    descriptionEn: "Controls retry ceilings and recovery behavior for auto-queue entries.",
    fields: [
      {
        key: "maxEntryRetries",
        labelKo: "Entry 최대 재시도 횟수",
        labelEn: "Entry max retries",
        descriptionKo: "dispatch 생성 실패가 이 횟수에 도달하면 entry를 failed로 전환합니다.",
        descriptionEn: "Turns an entry into failed after this many dispatch creation failures.",
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
const GENERAL_FIELD_LIMITS = {
  companyName: 80,
  ceoName: 60,
} as const;

const SYSTEM_CONFIG_DESCRIPTIONS: Record<string, { ko: string; en: string }> = {
  kanban_manager_channel_id: {
    ko: "칸반 상태 변경과 자동화 명령을 수신하는 Discord 채널입니다.",
    en: "Discord channel used for kanban state changes and automation commands.",
  },
  deadlock_manager_channel_id: {
    ko: "교착 상태나 멈춤 감지를 보고하는 Discord 채널입니다.",
    en: "Discord channel that receives deadlock and stalled-work alerts.",
  },
  kanban_human_alert_channel_id: {
    ko: "에이전트 fallback이나 수동 개입이 사람에게 라우팅될 Discord 채널입니다.",
    en: "Discord channel used when alerts must be routed to a human instead of an agent.",
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
const AUDIT_NOTES: AuditNote[] = [
  {
    id: "settings-json-merge",
    titleKo: "회사 설정 JSON은 전체 덮어쓰기 모델",
    titleEn: "Company settings JSON uses full replacement",
    descriptionKo: "`/api/settings`는 patch merge가 아니라 body 전체를 저장합니다. 현재 UI는 기존 `settings` JSON과 병합해 hidden key 손실을 막아야 합니다.",
    descriptionEn: "`/api/settings` stores the full body instead of merging patches. The UI must merge with the existing `settings` JSON to avoid losing hidden keys.",
    keys: ["settings"],
    status: "backend-followup",
  },
  {
    id: "server-port-readonly",
    titleKo: "`server_port`는 사실상 읽기 전용",
    titleEn: "`server_port` is effectively read-only",
    descriptionKo: "`src/server/mod.rs`에서 서버 부팅 시 `config.server.port` 값으로 다시 기록합니다. 편집 가능한 값처럼 보이면 운영 오해를 만듭니다.",
    descriptionEn: "`src/server/mod.rs` rewrites it from `config.server.port` on boot. Presenting it as editable is misleading.",
    keys: ["server_port"],
    status: "read-only",
  },
  {
    id: "context-clear-gap",
    titleKo: "`context_clear_*`는 설명은 있지만 settings API에 없음",
    titleEn: "`context_clear_*` is described but not exposed by settings API",
    descriptionKo: "UI 설명에는 등장하지만 `/api/settings/config` whitelist에는 없습니다. dead config인지 빠진 API 항목인지 본체 정리가 필요합니다.",
    descriptionEn: "The UI descriptions mention it, but `/api/settings/config` does not expose it. ADK core should decide whether it is dead config or a missing API field.",
    keys: ["context_clear_percent", "context_clear_idle_minutes"],
    status: "backend-followup",
  },
  {
    id: "onboarding-secrets",
    titleKo: "온보딩 관련 설정은 별도 API/DB 전용",
    titleEn: "Onboarding settings are managed through a dedicated API/DB path",
    descriptionKo: "봇 토큰, guild/owner/provider, 보조 command token은 `/api/onboarding/*`와 개별 `kv_meta` 키로 관리됩니다. 일반 설정창보다 위저드가 안전합니다.",
    descriptionEn: "Bot tokens, guild/owner/provider, and secondary command tokens are managed via `/api/onboarding/*` and dedicated `kv_meta` keys. A wizard is safer than the general settings form.",
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
    descriptionKo: "`dashboard/src/app/office-workflow-pack.ts`에서 preset room theme와 custom room theme를 합쳐 사용합니다. 일반 설정 필드보다 office/visual 편집 흐름에서 관리하는 편이 맞습니다.",
    descriptionEn: "`dashboard/src/app/office-workflow-pack.ts` merges preset room themes with custom room themes. It fits office/visual editing better than a generic settings form.",
    keys: ["roomThemes"],
    status: "managed-elsewhere",
  },
  {
    id: "typed-only-company-settings",
    titleKo: "타입에는 있지만 현재 소비/편집 경로가 확인되지 않은 회사 설정",
    titleEn: "Company settings with no confirmed editor or runtime consumer",
    descriptionKo: "현재 audit 기준으로 일부 `CompanySettings` 필드는 타입에는 있지만 실제 편집 화면이나 소비처가 확인되지 않았습니다. 제거/활성화/문서화 중 하나가 필요합니다.",
    descriptionEn: "In the current audit, some `CompanySettings` fields exist in types but have no confirmed editor or runtime consumer. They should be removed, activated, or documented.",
    keys: [
      "autoUpdateEnabled",
      "autoUpdateNoticePending",
      "oauthAutoSwap",
      "officeWorkflowPack",
      "providerModelConfig",
      "messengerChannels",
      "officePackProfiles",
      "officePackHydratedPacks",
    ],
    status: "typed-only",
  },
  {
    id: "merge-automation-gap",
    titleKo: "merge automation 설정은 policy에서 읽지만 UI/API에는 없음",
    titleEn: "Merge automation settings are consumed by policy but absent from UI/API",
    descriptionKo: "`merge_automation_enabled`, `merge_strategy`, `merge_allowed_authors`는 policy에서 실제 사용되지만 현재 settings API whitelist와 UI에는 없습니다.",
    descriptionEn: "`merge_automation_enabled`, `merge_strategy`, and `merge_allowed_authors` are consumed by policy, but they are absent from the current settings API whitelist and UI.",
    keys: ["merge_automation_enabled", "merge_strategy", "merge_allowed_authors"],
    status: "backend-followup",
  },
  {
    id: "workspace-fallback-gap",
    titleKo: "`workspace`는 policy fallback에서 읽지만 정본이 아님",
    titleEn: "`workspace` is read as a policy fallback but is not canonical",
    descriptionKo: "`agentdesk.config.get('workspace')`는 `kv_meta` fallback일 뿐이고 실제 정본은 agent/session/runtime에 퍼져 있습니다. 일반 설정값처럼 설명하면 오해가 생깁니다.",
    descriptionEn: "`agentdesk.config.get('workspace')` is only a `kv_meta` fallback. The real source of truth is spread across agent, session, and runtime surfaces.",
    keys: ["workspace"],
    status: "backend-followup",
  },
  {
    id: "max-chain-depth-consumer-gap",
    titleKo: "`max_chain_depth`는 노출되지만 실제 소비처가 확인되지 않음",
    titleEn: "`max_chain_depth` is exposed but has no confirmed runtime consumer",
    descriptionKo: "`/api/settings/config` whitelist에는 있지만 현재 코드 검색 기준으로 확실한 런타임 소비처가 보이지 않습니다. dead config인지 누락 연결인지 본체 정리가 필요합니다.",
    descriptionEn: "It is in the `/api/settings/config` whitelist, but the current code audit did not find a confirmed runtime consumer. ADK core should decide whether it is dead config or a missing integration.",
    keys: ["max_chain_depth"],
    status: "backend-followup",
  },
];

function isSettingsPanel(value: string | null): value is SettingsPanel {
  return value === "general" || value === "runtime" || value === "pipeline" || value === "onboarding" || value === "voice";
}

function isRuntimeCategoryId(value: string | null): value is string {
  return CATEGORIES.some((category) => category.id === value);
}

function readSettingsPanelFromUrl(): SettingsPanel | null {
  if (typeof window === "undefined") return null;
  const value = new URLSearchParams(window.location.search).get(SETTINGS_PANEL_QUERY_KEY);
  return isSettingsPanel(value) ? value : null;
}

function readStoredSettingsPanel(): SettingsPanel {
  const panelFromUrl = readSettingsPanelFromUrl();
  if (panelFromUrl) {
    return panelFromUrl;
  }
  return readLocalStorageValue<SettingsPanel>(STORAGE_KEYS.settingsPanel, "pipeline", {
    validate: (value): value is SettingsPanel => typeof value === "string" && isSettingsPanel(value),
    legacy: (raw) => (isSettingsPanel(raw) ? raw : null),
  });
}

function readStoredRuntimeCategory(): string {
  return readLocalStorageValue<string>(STORAGE_KEYS.settingsRuntimeCategory, CATEGORIES[0]?.id ?? "polling", {
    validate: (value): value is string => typeof value === "string" && isRuntimeCategoryId(value),
    legacy: (raw) => (isRuntimeCategoryId(raw) ? raw : null),
  });
}

const VOICE_SENSITIVITY_OPTIONS: Array<{
  value: VoiceSensitivityMode;
  labelKo: string;
  labelEn: string;
}> = [
  { value: "normal", labelKo: "보통", labelEn: "Normal" },
  { value: "conservative", labelKo: "보수적", labelEn: "Conservative" },
];

interface VoiceAliasConflict {
  normalized: string;
  firstAgent: VoiceAgentConfig;
  firstAlias: string;
  secondAgent: VoiceAgentConfig;
  secondAlias: string;
}

function cloneVoiceConfig(config: VoiceConfigResponse): VoiceConfigResponse {
  return {
    ...config,
    global: { ...config.global },
    agents: config.agents.map((agent) => ({
      ...agent,
      aliases: [...agent.aliases],
    })),
  };
}

function normalizeVoiceAliasKey(value: string): string {
  return Array.from(value.normalize("NFC").toLocaleLowerCase())
    .filter((ch) => /[\p{Letter}\p{Number}]/u.test(ch))
    .join("")
    .normalize("NFC");
}

function splitVoiceAliases(value: string): string[] {
  return value
    .split(/[,\n]/)
    .map((alias) => alias.trim())
    .filter((alias, index, aliases) => alias.length > 0 && aliases.indexOf(alias) === index);
}

function voiceAgentBuiltInAliases(agent: VoiceAgentConfig): string[] {
  return [agent.id, agent.name, agent.name_ko ?? ""].filter((value) => value.trim().length > 0);
}

function findVoiceAliasConflict(config: VoiceConfigResponse | null): VoiceAliasConflict | null {
  if (!config) return null;
  const seen = new Map<string, { agent: VoiceAgentConfig; alias: string }>();
  for (const agent of config.agents) {
    for (const alias of [...voiceAgentBuiltInAliases(agent), ...agent.aliases]) {
      const normalized = normalizeVoiceAliasKey(alias);
      if (!normalized) continue;
      const existing = seen.get(normalized);
      if (existing && existing.agent.id !== agent.id) {
        return {
          normalized,
          firstAgent: existing.agent,
          firstAlias: existing.alias,
          secondAgent: agent,
          secondAlias: alias,
        };
      }
      if (!existing) {
        seen.set(normalized, { agent, alias });
      }
    }
  }
  return null;
}

function voiceAgentKeys(agentId: string): string[] {
  return [
    `voice.agent.${agentId}.enabled`,
    `voice.agent.${agentId}.wake_word`,
    `voice.agent.${agentId}.aliases`,
    `voice.agent.${agentId}.sensitivity`,
  ];
}

function voiceConfigComparable(config: VoiceConfigResponse | null): unknown {
  if (!config) return null;
  return {
    global: config.global,
    agents: config.agents.map((agent) => ({
      id: agent.id,
      voice_enabled: agent.voice_enabled,
      wake_word: agent.wake_word,
      aliases: agent.aliases,
      sensitivity_mode: agent.sensitivity_mode,
    })),
  };
}

function voiceSaveBody(config: VoiceConfigResponse): VoiceConfigPutBody {
  return {
    version: config.version,
    actor: "dashboard",
    global: {
      lobby_channel_id: config.global.lobby_channel_id?.trim() || null,
      active_agent_ttl_seconds: Math.max(1, Math.round(config.global.active_agent_ttl_seconds || 180)),
      default_sensitivity_mode: config.global.default_sensitivity_mode,
    },
    agents: config.agents.map((agent) => ({
      ...agent,
      wake_word: agent.wake_word.trim(),
      aliases: splitVoiceAliases(agent.aliases.join("\n")),
    })),
  };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function isPipelineRepoCacheEntry(value: unknown): value is PipelineRepoCacheEntry {
  return isRecord(value)
    && typeof value.viewerLogin === "string"
    && typeof value.fetchedAt === "number"
    && Array.isArray(value.repos);
}

function isPipelineAgentCacheEntry(value: unknown): value is PipelineAgentCacheEntry {
  return isRecord(value)
    && typeof value.fetchedAt === "number"
    && Array.isArray(value.agents);
}

function readStoredPipelineRepoCache(): PipelineRepoCacheEntry | null {
  return readLocalStorageValue<PipelineRepoCacheEntry | null>(
    STORAGE_KEYS.settingsPipelineRepoCache,
    null,
    {
      validate: (value): value is PipelineRepoCacheEntry | null =>
        value === null || isPipelineRepoCacheEntry(value),
    },
  );
}

function writeStoredPipelineRepoCache(cache: PipelineRepoCacheEntry): void {
  writeLocalStorageValue(STORAGE_KEYS.settingsPipelineRepoCache, cache);
}

function readStoredPipelineAgentCache(): PipelineAgentCacheEntry | null {
  return readLocalStorageValue<PipelineAgentCacheEntry | null>(
    STORAGE_KEYS.settingsPipelineAgentCache,
    null,
    {
      validate: (value): value is PipelineAgentCacheEntry | null =>
        value === null || isPipelineAgentCacheEntry(value),
    },
  );
}

function writeStoredPipelineAgentCache(cache: PipelineAgentCacheEntry): void {
  writeLocalStorageValue(STORAGE_KEYS.settingsPipelineAgentCache, cache);
}

function pickMostRecentCache<T extends { fetchedAt: number }>(...entries: Array<T | null>): T | null {
  return entries.reduce<T | null>((latest, entry) => {
    if (!entry) return latest;
    if (!latest || entry.fetchedAt > latest.fetchedAt) {
      return entry;
    }
    return latest;
  }, null);
}

function isCacheFresh(cache: { fetchedAt: number } | null): boolean {
  if (!cache) return false;
  return Date.now() - cache.fetchedAt < PIPELINE_SELECTOR_CACHE_MAX_AGE_MS;
}

function getCachedPipelineRepoEntry(): PipelineRepoCacheEntry | null {
  const memoryCache = api.getCachedGitHubRepos();
  return pickMostRecentCache(
    memoryCache
      ? {
          viewerLogin: memoryCache.data.viewer_login,
          repos: memoryCache.data.repos,
          fetchedAt: memoryCache.fetchedAt,
        }
      : null,
    readStoredPipelineRepoCache(),
  );
}

function getCachedPipelineAgentEntry(): PipelineAgentCacheEntry | null {
  const memoryCache = api.getCachedAgents();
  return pickMostRecentCache(
    memoryCache
      ? {
          agents: memoryCache.data,
          fetchedAt: memoryCache.fetchedAt,
        }
      : null,
    readStoredPipelineAgentCache(),
  );
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

function auditStatusLabel(status: AuditNoteStatus, isKo: boolean): string {
  if (isKo) {
    if (status === "read-only") return "읽기 전용";
    if (status === "managed-elsewhere") return "별도 관리";
    if (status === "typed-only") return "타입 전용 후보";
    return "본체 정리 필요";
  }
  if (status === "read-only") return "Read-only";
  if (status === "managed-elsewhere") return "Managed elsewhere";
  if (status === "typed-only") return "Typed-only candidate";
  return "Core cleanup needed";
}

function auditStatusClass(status: AuditNoteStatus): string {
  if (status === "read-only") return "border-slate-400/30 bg-slate-400/10 text-slate-200";
  if (status === "managed-elsewhere") return "border-emerald-400/30 bg-emerald-400/10 text-emerald-200";
  return "border-sky-400/30 bg-sky-400/10 text-sky-100";
}

function configLayerLabel(overrideActive: boolean, isKo: boolean): string {
  return overrideActive ? (isKo ? "실시간 override" : "Live override") : (isKo ? "기준값" : "Baseline");
}

function configLayerClass(overrideActive: boolean): string {
  return overrideActive ? "border-amber-400/30 bg-amber-400/10 text-amber-100" : "border-emerald-400/30 bg-emerald-400/10 text-emerald-100";
}

function configSourceLabel(entry: ConfigEntry, isKo: boolean): string {
  if (entry.override_active) return "kv_meta";
  if (entry.baseline_source === "config") {
    return isKo ? "env/config" : "env/config";
  }
  return isKo ? "default" : "default";
}

function configSourceClass(entry: ConfigEntry): string {
  if (entry.override_active) {
    return "border-sky-400/30 bg-sky-400/10 text-sky-100";
  }
  if (entry.baseline_source === "config") {
    return "border-violet-400/30 bg-violet-400/10 text-violet-100";
  }
  return "border-emerald-400/30 bg-emerald-400/10 text-emerald-100";
}

function formatConfigValue(value: ConfigEditValue): string {
  return typeof value === "boolean" ? String(value) : value;
}

/**
 * Build a SettingRowMeta from a /api/settings/config kv_meta entry.
 * Drives the pipeline + runtime panel rows.
 */
function metaFromConfigEntry(
  entry: ConfigEntry,
  edits: Record<string, ConfigEditValue>,
): SettingRowMeta {
  const hasEdit = Object.prototype.hasOwnProperty.call(edits, entry.key);
  const effective = hasEdit ? edits[entry.key] : (entry.value ?? entry.default ?? "");
  const readOnly =
    READ_ONLY_CONFIG_KEYS.has(entry.key) || entry.editable === false;
  const overrideActive = Boolean(entry.override_active);
  const restartRequired =
    entry.restart_behavior === "reseed-from-yaml" ||
    entry.restart_behavior === "reset-to-baseline" ||
    entry.restart_behavior === "config-only";

  let source: SettingSource;
  if (readOnly) {
    source = entry.baseline_source === "config" ? "repo_canonical" : "legacy_readonly";
  } else if (overrideActive) {
    source = "live_override";
  } else if (entry.baseline_source === "yaml" || entry.baseline_source === "hardcoded") {
    source = "repo_canonical";
  } else if (entry.baseline_source === "config") {
    source = "repo_canonical";
  } else {
    source = "kv_meta";
  }

  const flags: SettingFlag[] = [];
  if (!readOnly) flags.push("kv_meta");
  if (overrideActive) flags.push("live_override");
  if (readOnly) flags.push("read_only");
  if (restartRequired) flags.push("restart_required");

  const description = SYSTEM_CONFIG_DESCRIPTIONS[entry.key];

  let inputKind: SettingRowMeta["inputKind"] = "text";
  if (readOnly) inputKind = "readonly";
  else if (BOOLEAN_CONFIG_KEYS.has(entry.key)) inputKind = "toggle";
  else if (NUMERIC_CONFIG_KEYS.has(entry.key)) inputKind = "number";

  return {
    key: entry.key,
    group: configCategoryToGroup(entry.category, entry.key),
    source,
    editable: !readOnly,
    restartRequired,
    defaultValue: entry.default ?? null,
    effectiveValue: effective,
    flags,
    labelKo: entry.label_ko ?? entry.key,
    labelEn: entry.label_en ?? entry.key,
    hintKo: description?.ko,
    hintEn: description?.en,
    inputKind,
    storageLayerKo:
      source === "live_override"
        ? "kv_meta override"
        : source === "kv_meta"
          ? "kv_meta"
          : source === "repo_canonical"
            ? entry.baseline_source === "yaml"
              ? "agentdesk.yaml"
              : entry.baseline_source === "config"
                ? "server config"
                : "default"
            : undefined,
    storageLayerEn:
      source === "live_override"
        ? "kv_meta override"
        : source === "kv_meta"
          ? "kv_meta"
          : source === "repo_canonical"
            ? entry.baseline_source === "yaml"
              ? "agentdesk.yaml"
              : entry.baseline_source === "config"
                ? "server config"
                : "default"
            : undefined,
    restartNoteKo: restartBehaviorNote(entry.restart_behavior, true) ?? undefined,
    restartNoteEn: restartBehaviorNote(entry.restart_behavior, false) ?? undefined,
  };
}

function applyConfigEdits(
  entries: ConfigEntry[],
  edits: Record<string, ConfigEditValue>,
): ConfigEntry[] {
  if (Object.keys(edits).length === 0) return entries;
  return entries.map((entry) => {
    if (!Object.prototype.hasOwnProperty.call(edits, entry.key)) {
      return entry;
    }
    return {
      ...entry,
      value: formatConfigValue(edits[entry.key]),
      override_active: true,
    };
  });
}

function selectDefaultPipelineRepo(
  repos: GitHubRepoOption[],
  viewerLogin: string,
): string {
  return (
    repos.find((repo) => repo.nameWithOwner === "itismyfield/AgentDesk")
      ?.nameWithOwner
    || repos.find((repo) => repo.nameWithOwner.endsWith("/AgentDesk"))
      ?.nameWithOwner
    || repos.find(
      (repo) => viewerLogin && repo.nameWithOwner.startsWith(`${viewerLogin}/`),
    )?.nameWithOwner
    || repos[0]?.nameWithOwner
    || ""
  );
}

function formatPipelineAgentLabel(agent: Agent, isKo: boolean): string {
  // Native <option> cannot render React components, so we omit the emoji
  // fallback (sprite rendering happens in non-<option> avatar UIs).
  // Keeps the sprite-first policy from #1251 (emoji fallback禁止).
  const name = isKo ? agent.name_ko || agent.name : agent.name || agent.name_ko;
  return name;
}

function baselineSourceNote(source: string | null | undefined, isKo: boolean): string | null {
  if (source === "yaml") return isKo ? "기준값 출처: agentdesk.yaml" : "Baseline source: agentdesk.yaml";
  if (source === "hardcoded") return isKo ? "기준값 출처: 하드코딩 기본값" : "Baseline source: hardcoded default";
  if (source === "config") return isKo ? "기준값 출처: 서버 설정" : "Baseline source: server config";
  return null;
}

function restartBehaviorNote(behavior: string | null | undefined, isKo: boolean): string | null {
  if (behavior === "reseed-from-yaml") {
    return isKo ? "재시작 시 YAML baseline이 다시 적용됩니다." : "Restart re-applies the YAML baseline.";
  }
  if (behavior === "persist-live-override") {
    return isKo ? "재시작 후에도 현재 live override가 유지됩니다." : "The live override persists across restart.";
  }
  if (behavior === "reset-to-baseline") {
    return isKo ? "재시작 시 baseline으로 초기화됩니다." : "Restart resets this back to baseline.";
  }
  if (behavior === "clear-on-restart") {
    return isKo ? "재시작 시 override가 제거됩니다." : "Restart clears this override.";
  }
  if (behavior === "config-only") {
    return isKo ? "서버 설정에서 직접 읽는 값이라 여기서는 읽기 전용입니다." : "This value comes directly from server config and is read-only here.";
  }
  return null;
}

interface SettingGroupMeta {
  id: SettingGroupId;
  nameKo: string;
  nameEn: string;
  descKo: string;
  descEn: string;
}

const SETTING_GROUPS: SettingGroupMeta[] = [
  {
    id: "pipeline",
    nameKo: "파이프라인",
    nameEn: "Pipeline",
    descKo: "칸반 흐름과 상태 전환에 직접 영향을 주는 값입니다.",
    descEn: "Values that directly affect kanban flow and state transitions.",
  },
  {
    id: "runtime",
    nameKo: "런타임",
    nameEn: "Runtime",
    descKo: "실행 환경과 리소스 제어, 컨텍스트 정책을 다룹니다.",
    descEn: "Execution environment, resource controls, and context policy.",
  },
  {
    id: "voice",
    nameKo: "음성",
    nameEn: "Voice",
    descKo: "voice-lobby와 에이전트별 wake word, alias, 민감도를 관리합니다.",
    descEn: "Voice-lobby plus per-agent wake words, aliases, and sensitivity.",
  },
  {
    id: "onboarding",
    nameKo: "온보딩",
    nameEn: "Onboarding",
    descKo: "신규 워크스페이스가 처음 겪는 경로와 위저드 전용 키입니다.",
    descEn: "First-run path and wizard-managed keys for new workspaces.",
  },
  {
    id: "general",
    nameKo: "일반",
    nameEn: "General",
    descKo: "회사 정보, 표시 환경, 메타 설정.",
    descEn: "Company identity, display environment, and meta settings.",
  },
];

/**
 * Maps a kv_meta whitelist category onto the four spec groups.
 * Kept as a function so individual keys can override the default.
 */
function configCategoryToGroup(category: string, key: string): SettingGroupId {
  if (key === "server_port") return "general";
  if (key.startsWith("context_compact_")) return "runtime";
  if (key.startsWith("context_clear_")) return "runtime";
  if (category === "pipeline") return "pipeline";
  if (category === "review") return "pipeline";
  if (category === "timeout") return "pipeline";
  if (category === "dispatch") return "pipeline";
  if (category === "context") return "runtime";
  if (category === "system") return "pipeline";
  return "pipeline";
}

function flagTone(flag: SettingFlag): { bg: string; fg: string; border: string } {
  switch (flag) {
    case "kv_meta":
      return {
        bg: "color-mix(in srgb, var(--th-overlay-medium) 92%, transparent)",
        fg: "var(--th-text-secondary)",
        border: "color-mix(in srgb, var(--th-border) 70%, transparent)",
      };
    case "live_override":
      return {
        bg: "rgba(56, 189, 248, 0.16)",
        fg: "rgba(186, 230, 253, 0.92)",
        border: "rgba(56, 189, 248, 0.42)",
      };
    case "alert":
      return {
        bg: "rgba(251, 191, 36, 0.16)",
        fg: "rgba(253, 230, 138, 0.92)",
        border: "rgba(251, 191, 36, 0.42)",
      };
    case "read_only":
      return {
        bg: "rgba(148, 163, 184, 0.18)",
        fg: "rgba(226, 232, 240, 0.85)",
        border: "rgba(148, 163, 184, 0.40)",
      };
    case "restart_required":
      return {
        bg: "rgba(244, 114, 182, 0.16)",
        fg: "rgba(251, 207, 232, 0.92)",
        border: "rgba(244, 114, 182, 0.42)",
      };
  }
}

function flagLabel(flag: SettingFlag, isKo: boolean): string {
  if (flag === "kv_meta") return "kv_meta";
  if (flag === "live_override") return isKo ? "live override" : "live override";
  if (flag === "alert") return isKo ? "alert" : "alert";
  if (flag === "read_only") return isKo ? "read-only" : "read-only";
  if (flag === "restart_required") return isKo ? "restart" : "restart";
  return flag;
}

function settingSourceLabel(source: SettingSource, isKo: boolean): string {
  if (source === "repo_canonical") return isKo ? "repo 정본" : "repo canonical";
  if (source === "runtime_config") return isKo ? "런타임 설정" : "runtime config";
  if (source === "kv_meta") return "kv_meta";
  if (source === "live_override") return isKo ? "실시간 override" : "live override";
  if (source === "legacy_readonly") return isKo ? "legacy alias" : "legacy alias";
  if (source === "computed") return isKo ? "유도값" : "computed";
  return source;
}

interface SettingRowProps {
  meta: SettingRowMeta;
  isKo: boolean;
  onChange?: (key: string, value: string | boolean | number) => void;
  // Allow callers to render bespoke control surfaces (e.g. range slider) when
  // the default control is not enough. The default control is rendered if not
  // provided.
  renderControl?: (meta: SettingRowMeta) => ReactNode;
  controlOverlay?: ReactNode;
  trailingMeta?: ReactNode;
}

function SettingRow({
  meta,
  isKo,
  onChange,
  renderControl,
  controlOverlay,
  trailingMeta,
}: SettingRowProps) {
  const [open, setOpen] = useState(false);
  const tr = (ko: string, en: string) => (isKo ? ko : en);

  const labelText = isKo ? meta.labelKo ?? meta.key : meta.labelEn ?? meta.key;
  const hintText = isKo ? meta.hintKo : meta.hintEn;
  const readOnly = !meta.editable;

  const renderDefaultControl = () => {
    if (renderControl) return renderControl(meta);
    if (readOnly) {
      return (
        <div
          className="w-full truncate rounded-xl px-3 py-2 text-xs"
          style={{
            background: "color-mix(in srgb, var(--th-bg-surface) 60%, transparent)",
            border: "1px dashed color-mix(in srgb, var(--th-border) 70%, transparent)",
            color: "var(--th-text-muted)",
            fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
          }}
        >
          {String(meta.effectiveValue ?? "")}
        </div>
      );
    }
    if (meta.inputKind === "toggle") {
      const enabled = Boolean(
        meta.effectiveValue === true ||
          meta.effectiveValue === "true" ||
          meta.effectiveValue === 1 ||
          meta.effectiveValue === "1",
      );
      return (
        <button
          type="button"
          role="switch"
          aria-checked={enabled}
          onClick={() => onChange?.(meta.key, !enabled)}
          className="relative inline-flex h-6 w-11 items-center rounded-full transition-colors"
          style={{
            background: enabled ? "var(--th-accent-primary)" : "color-mix(in srgb, var(--th-border) 80%, transparent)",
          }}
        >
          <span
            className="inline-block h-5 w-5 rounded-full bg-white shadow transition-transform"
            style={{ transform: enabled ? "translateX(1.4rem)" : "translateX(0.15rem)" }}
          />
        </button>
      );
    }
    if (meta.inputKind === "select" && meta.selectOptions) {
      return (
        <select
          value={String(meta.effectiveValue ?? "")}
          onChange={(event) => onChange?.(meta.key, event.target.value)}
          className="w-full rounded-xl px-3 py-2 text-sm"
          style={{
            background: "var(--th-bg-surface)",
            border: "1px solid color-mix(in srgb, var(--th-border) 70%, transparent)",
            color: "var(--th-text)",
          }}
        >
          {meta.selectOptions.map((opt) => (
            <option key={opt.value} value={opt.value}>
              {isKo ? opt.labelKo : opt.labelEn}
            </option>
          ))}
        </select>
      );
    }
    return (
      <input
        type={meta.inputKind === "number" ? "number" : "text"}
        inputMode={meta.inputKind === "number" ? "numeric" : undefined}
        min={meta.numericRange?.min}
        max={meta.numericRange?.max}
        step={meta.numericRange?.step}
        value={String(meta.effectiveValue ?? "")}
        onChange={(event) =>
          onChange?.(
            meta.key,
            meta.inputKind === "number" ? Number(event.target.value) : event.target.value,
          )
        }
        className="w-full rounded-xl px-3 py-2 text-xs"
        style={{
          background: "var(--th-bg-surface)",
          border: "1px solid color-mix(in srgb, var(--th-border) 70%, transparent)",
          color: "var(--th-text)",
          fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
        }}
      />
    );
  };

  return (
    <div
      className="setting-row border-b last:border-b-0"
      style={{ borderColor: "color-mix(in srgb, var(--th-border) 60%, transparent)" }}
      data-testid={`setting-row-${meta.key}`}
    >
      <div className="setting-row-grid items-center gap-3 px-2 py-3 sm:gap-4 sm:px-3 sm:py-4">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-1.5">
            <span className="setting-row-label text-sm font-medium" style={{ color: "var(--th-text)" }}>
              {labelText}
            </span>
            {meta.flags.map((flag) => {
              const tone = flagTone(flag);
              return (
                <span
                  key={flag}
                  className="inline-flex items-center rounded-full border px-1.5 py-px text-[10px] font-medium uppercase tracking-wide"
                  style={{ background: tone.bg, color: tone.fg, borderColor: tone.border }}
                >
                  {flagLabel(flag, isKo)}
                </span>
              );
            })}
          </div>
          {hintText ? (
            <div className="setting-row-hint mt-1 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
              {hintText}
            </div>
          ) : null}
          <code
            className="setting-key-token mt-1 inline-block rounded px-1 py-px text-[10px]"
            title={meta.key}
            style={{
              background: "color-mix(in srgb, var(--th-overlay-medium) 80%, transparent)",
              color: "var(--th-text-muted)",
              fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
            }}
          >
            {meta.key}
          </code>
        </div>
        <div className="min-w-0">
          {controlOverlay ?? renderDefaultControl()}
        </div>
        <button
          type="button"
          aria-expanded={open}
          aria-label={tr("자세히 보기", "Show details")}
          onClick={() => setOpen((current) => !current)}
          className="grid h-8 w-8 place-items-center rounded-full border"
          style={{
            borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
            color: "var(--th-text-muted)",
            background: "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)",
          }}
        >
          <ChevronDown
            size={14}
            style={{
              transform: open ? "rotate(180deg)" : "none",
              transition: "transform 0.2s",
            }}
          />
        </button>
      </div>
      {open ? (
        <div
          className="mx-2 mb-3 grid gap-2 rounded-2xl p-3 text-[11px] sm:mx-3 sm:grid-cols-2 sm:gap-3 sm:p-4"
          style={{
            background: "color-mix(in srgb, var(--th-overlay-medium) 70%, transparent)",
            border: "1px solid color-mix(in srgb, var(--th-border) 60%, transparent)",
            color: "var(--th-text-muted)",
          }}
        >
          <div>
            <span style={{ color: "var(--th-text-muted)" }}>{tr("기본값:", "Default:")} </span>
            <code
              style={{
                fontFamily:
                  "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
                color: "var(--th-text)",
              }}
            >
              {meta.defaultValue === undefined || meta.defaultValue === null
                ? tr("없음", "—")
                : String(meta.defaultValue)}
            </code>
          </div>
          <div>
            <span style={{ color: "var(--th-text-muted)" }}>{tr("저장 레이어:", "Storage:")} </span>
            <code
              style={{
                fontFamily:
                  "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
                color: "var(--th-text)",
              }}
            >
              {settingSourceLabel(meta.source, isKo)}
            </code>
            {meta.storageLayerKo || meta.storageLayerEn ? (
              <span className="ml-1" style={{ color: "var(--th-text-muted)" }}>
                · {tr(meta.storageLayerKo ?? "", meta.storageLayerEn ?? "")}
              </span>
            ) : null}
          </div>
          <div>
            <span style={{ color: "var(--th-text-muted)" }}>{tr("편집 가능:", "Editable:")} </span>
            <span style={{ color: "var(--th-text)" }}>
              {meta.editable ? tr("예", "yes") : tr("아니오 (읽기 전용)", "no (read-only)")}
            </span>
          </div>
          <div>
            <span style={{ color: "var(--th-text-muted)" }}>{tr("재시작 필요:", "Restart required:")} </span>
            <span style={{ color: "var(--th-text)" }}>
              {meta.restartRequired ? tr("예", "yes") : tr("아니오", "no")}
            </span>
          </div>
          {meta.restartNoteKo || meta.restartNoteEn ? (
            <div className="sm:col-span-2" style={{ color: "var(--th-text-muted)" }}>
              {tr(meta.restartNoteKo ?? "", meta.restartNoteEn ?? "")}
            </div>
          ) : null}
          {meta.validation && meta.validation.ok === false ? (
            <div className="sm:col-span-2" style={{ color: "rgba(252,165,165,0.95)" }}>
              {tr(meta.validation.messageKo, meta.validation.messageEn)}
            </div>
          ) : null}
          {trailingMeta ? <div className="sm:col-span-2">{trailingMeta}</div> : null}
        </div>
      ) : null}
    </div>
  );
}

function PanelNavButton({
  id,
  active,
  title,
  detail,
  count,
  ariaControls,
  onClick,
}: {
  id: string;
  active: boolean;
  title: string;
  detail: string;
  count?: string;
  ariaControls?: string;
  onClick: () => void;
}) {
  return (
    <button
      id={id}
      type="button"
      onClick={onClick}
      aria-current={active ? "page" : undefined}
      aria-controls={ariaControls}
      className="w-full rounded-xl px-2.5 py-2.5 text-left transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[color:var(--th-accent-primary)] focus-visible:ring-offset-2 focus-visible:ring-offset-[color:var(--th-card-bg)]"
      style={{
        borderColor: "transparent",
        background: active
          ? "color-mix(in srgb, var(--th-overlay-medium) 92%, transparent)"
          : "transparent",
      }}
    >
      <div className="flex items-start gap-3">
        <span
          className="mt-1 h-2 w-2 shrink-0 rounded-full"
          style={{
            background: active ? "var(--th-accent-primary)" : "color-mix(in srgb, var(--th-text-muted) 50%, transparent)",
          }}
        />
        <div className="min-w-0 flex-1">
          <div className="flex items-start justify-between gap-3">
            <div
              className="text-sm font-semibold"
              style={{ color: active ? "var(--th-accent-primary)" : "var(--th-text-heading)" }}
            >
              {title}
            </div>
            {count && (
              <span
                className="shrink-0 rounded-full border px-2 py-0.5 text-[10px] font-medium"
                style={{
                  borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
                  background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
                  color: active ? "var(--th-text)" : "var(--th-text-muted)",
                }}
              >
                {count}
              </span>
            )}
          </div>
          <div className="mt-1 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
            {detail}
          </div>
        </div>
      </div>
    </button>
  );
}

function CompactFieldCard({
  label,
  description,
  children,
  footer,
}: {
  label: string;
  description: string;
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
      <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
        {label}
      </div>
      <p className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
        {description}
      </p>
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

function joinDescribedBy(...ids: Array<string | null | undefined | false>): string | undefined {
  const value = ids.filter(Boolean).join(" ");
  return value.length > 0 ? value : undefined;
}

function GeneralSettingsField({
  id,
  label,
  description,
  error,
  footer,
  children,
}: {
  id: string;
  label: string;
  description: string;
  error?: string | null;
  footer?: string;
  children: ReactNode;
}) {
  const descriptionId = `${id}-description`;
  const errorId = `${id}-error`;

  return (
    <SettingsCard
      className="rounded-2xl p-4"
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
      }}
    >
      <label htmlFor={id} className="block text-sm font-medium" style={{ color: "var(--th-text)" }}>
        {label}
      </label>
      <p
        id={descriptionId}
        className="mt-1 text-xs leading-5"
        style={{ color: "var(--th-text-muted)" }}
      >
        {description}
      </p>
      <div className="mt-3">{children}</div>
      {footer ? (
        <div className="mt-3 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
          {footer}
        </div>
      ) : null}
      {error ? (
        <p id={errorId} className="mt-3 text-xs" style={{ color: "#fca5a5" }}>
          {error}
        </p>
      ) : null}
    </SettingsCard>
  );
}

function StorageSurfaceCard({
  title,
  body,
  footer,
}: {
  title: string;
  body: string;
  footer: string;
}) {
  return (
    <SettingsCard
      className="rounded-2xl p-4"
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
      }}
    >
      <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
        {title}
      </div>
      <p className="mt-2 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
        {body}
      </p>
      <div className="mt-3 text-[11px] font-medium uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
        {footer}
      </div>
    </SettingsCard>
  );
}

function SettingsGlossary({ isKo }: { isKo: boolean }) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  return (
    <SettingsCard
      className="settings-glossary rounded-[18px] border px-4 py-4 sm:px-5"
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="min-w-0">
          <div className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {tr("용어 빠른 정의", "Quick term definitions")}
          </div>
          <p className="settings-copy mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
            {tr(
              "설정값을 바꾸기 전에 화면에 자주 나오는 운영 용어를 먼저 맞춥니다.",
              "Align on common operations terms before changing values.",
            )}
          </p>
        </div>
        <span
          className="settings-count-chip inline-flex shrink-0 items-center rounded-full border px-2.5 py-1 text-[10px] font-medium"
          style={{
            borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
            background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
            color: "var(--th-text-muted)",
          }}
        >
          {tr("핵심 6개", "6 core terms")}
        </span>
      </div>
      <dl className="mt-4 grid gap-3 md:grid-cols-2 xl:grid-cols-3">
        {SETTINGS_GLOSSARY.map((item) => (
          <div
            key={item.termEn}
            className="rounded-2xl border px-3 py-3"
            style={{
              borderColor: "color-mix(in srgb, var(--th-border) 64%, transparent)",
              background: "color-mix(in srgb, var(--th-bg-surface) 88%, transparent)",
            }}
          >
            <dt className="settings-term text-xs font-semibold" style={{ color: "var(--th-text)" }}>
              {tr(item.termKo, item.termEn)}
            </dt>
            <dd className="settings-copy mt-1 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
              {tr(item.definitionKo, item.definitionEn)}
            </dd>
          </div>
        ))}
      </dl>
    </SettingsCard>
  );
}

function AuditNoteCard({ note, isKo }: { note: AuditNote; isKo: boolean }) {
  return (
    <SettingsCard
      className="rounded-2xl p-4"
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
      }}
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
            style={{
              borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
              background: "color-mix(in srgb, var(--th-overlay-medium) 84%, transparent)",
              color: "var(--th-text-muted)",
            }}
          >
            {key}
          </span>
        ))}
      </div>
    </SettingsCard>
  );
}

export default function SettingsView({
  settings,
  onSave,
  isKo,
  onNotify,
}: SettingsViewProps) {
  const tr = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);

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
  const [voiceConfig, setVoiceConfig] = useState<VoiceConfigResponse | null>(null);
  const [voiceDraft, setVoiceDraft] = useState<VoiceConfigResponse | null>(null);
  const [voiceLoaded, setVoiceLoaded] = useState(false);
  const [voiceSaving, setVoiceSaving] = useState(false);
  const [voiceError, setVoiceError] = useState<string | null>(null);
  const [pipelineRepos, setPipelineRepos] = useState<GitHubRepoOption[]>([]);
  const [pipelineAgents, setPipelineAgents] = useState<Agent[]>([]);
  const [selectedPipelineRepo, setSelectedPipelineRepo] = useState("");
  const [selectedPipelineAgentId, setSelectedPipelineAgentId] = useState<string | null>(null);
  const [pipelineSelectorLoading, setPipelineSelectorLoading] = useState(false);
  const [pipelineSelectorError, setPipelineSelectorError] = useState<string | null>(null);

  const [activePanel, setActivePanel] = useState<SettingsPanel>(() => readStoredSettingsPanel());
  const [activeRuntimeCategoryId, setActiveRuntimeCategoryId] = useState<string>(() => readStoredRuntimeCategory());
  const [panelQuery, setPanelQuery] = useState("");
  const [showOnboarding, setShowOnboarding] = useState(false);
  const onboardingDialogRef = useRef<HTMLDivElement | null>(null);
  const onboardingCloseButtonRef = useRef<HTMLButtonElement | null>(null);
  const notify = useCallback(
    (ko: string, en: string, type: SettingsNotificationType = "info") => {
      onNotify?.(tr(ko, en), type);
    },
    [onNotify, tr],
  );
  const applyPipelineRepoCache = useCallback((cache: PipelineRepoCacheEntry) => {
    setPipelineRepos(cache.repos);
    setSelectedPipelineRepo((current) => {
      if (current && cache.repos.some((repo) => repo.nameWithOwner === current)) {
        return current;
      }
      return selectDefaultPipelineRepo(cache.repos, cache.viewerLogin);
    });
  }, []);
  const applyPipelineAgentCache = useCallback((cache: PipelineAgentCacheEntry) => {
    setPipelineAgents(cache.agents);
    setSelectedPipelineAgentId((current) => (
      current && cache.agents.some((agent) => agent.id === current) ? current : null
    ));
  }, []);
  const loadConfigEntries = useCallback(async () => {
    const response = await fetch("/api/settings/config", { credentials: "include" });
    if (!response.ok) {
      throw new Error("config-load-failed");
    }
    const data = await response.json() as { entries?: ConfigEntry[] };
    const entries = Array.isArray(data.entries) ? data.entries : [];
    setConfigEntries(entries);
    return entries;
  }, []);
  const loadVoiceConfig = useCallback(async () => {
    setVoiceError(null);
    try {
      const data = await api.getVoiceConfig();
      setVoiceConfig(data);
      setVoiceDraft(cloneVoiceConfig(data));
      setVoiceLoaded(true);
      return data;
    } catch {
      setVoiceLoaded(true);
      setVoiceError(tr("음성 설정을 불러오지 못했습니다.", "Failed to load voice settings."));
      return null;
    }
  }, [tr]);

  useEffect(() => {
    setCompanyName(settings.companyName);
    setCeoName(settings.ceoName);
    setLanguage(settings.language);
    setTheme(settings.theme);
  }, [settings.companyName, settings.ceoName, settings.language, settings.theme]);

  useEffect(() => {
    writeLocalStorageValue(STORAGE_KEYS.settingsPanel, activePanel);
  }, [activePanel]);

  useEffect(() => {
    writeLocalStorageValue(STORAGE_KEYS.settingsRuntimeCategory, activeRuntimeCategoryId);
  }, [activeRuntimeCategoryId]);

  useEffect(() => {
    if (typeof window === "undefined") return;
    if (readSettingsPanelFromUrl() !== activePanel) {
      const url = new URL(window.location.href);
      url.searchParams.set(SETTINGS_PANEL_QUERY_KEY, activePanel);
      window.history.replaceState(window.history.state, "", url);
    }
  }, [activePanel]);

  useEffect(() => {
    if (typeof window === "undefined") return;
    const handlePopState = () => {
      const panelFromUrl = readSettingsPanelFromUrl();
      if (panelFromUrl) setActivePanel(panelFromUrl);
    };
    window.addEventListener("popstate", handlePopState);
    return () => window.removeEventListener("popstate", handlePopState);
  }, []);

  useEffect(() => {
    if (!showOnboarding || typeof window === "undefined") return;
    const previousActiveElement =
      document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const focusCloseButton = window.setTimeout(() => {
      onboardingCloseButtonRef.current?.focus();
    }, 0);
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        setShowOnboarding(false);
        return;
      }
      if (event.key !== "Tab") return;
      const dialog = onboardingDialogRef.current;
      if (!dialog) return;
      const focusable = Array.from(
        dialog.querySelectorAll<HTMLElement>(
          'a[href], button:not([disabled]), textarea:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])',
        ),
      );
      if (focusable.length === 0) {
        event.preventDefault();
        return;
      }
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => {
      window.clearTimeout(focusCloseButton);
      window.removeEventListener("keydown", handleKeyDown);
      previousActiveElement?.focus();
    };
  }, [showOnboarding]);

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

    void loadConfigEntries()
      .catch(() => {});
  }, [loadConfigEntries]);

  useEffect(() => {
    if (activePanel !== "voice" || voiceLoaded) {
      return;
    }
    void loadVoiceConfig();
  }, [activePanel, loadVoiceConfig, voiceLoaded]);

  useEffect(() => {
    if (activePanel !== "pipeline") {
      return;
    }
    let stale = false;
    const cachedRepoEntry = getCachedPipelineRepoEntry();
    const cachedAgentEntry = getCachedPipelineAgentEntry();
    const hasCachedRepos = (cachedRepoEntry?.repos.length ?? 0) > 0;
    const shouldRefreshRepos = !isCacheFresh(cachedRepoEntry);
    const shouldRefreshAgents = !isCacheFresh(cachedAgentEntry);

    if (cachedRepoEntry) {
      applyPipelineRepoCache(cachedRepoEntry);
      setPipelineSelectorError(null);
    }
    if (cachedAgentEntry) {
      applyPipelineAgentCache(cachedAgentEntry);
    }

    if (!shouldRefreshRepos && !shouldRefreshAgents) {
      return;
    }

    setPipelineSelectorLoading(true);
    if (!hasCachedRepos) {
      setPipelineSelectorError(null);
    }

    const repoPromise = shouldRefreshRepos
      ? api.getGitHubRepos({
          timeoutMs: PIPELINE_SELECTOR_REFRESH_TIMEOUT_MS,
          maxRetries: 0,
        })
      : Promise.resolve(null);
    const agentPromise = shouldRefreshAgents
      ? api.getAgents(undefined, {
          timeoutMs: PIPELINE_SELECTOR_REFRESH_TIMEOUT_MS,
          maxRetries: 0,
        })
      : Promise.resolve(null);

    void Promise.allSettled([repoPromise, agentPromise])
      .then(([repoResult, agentResult]) => {
        if (stale) return;

        if (repoResult.status === "fulfilled" && repoResult.value) {
          const nextRepoCache: PipelineRepoCacheEntry = {
            viewerLogin: repoResult.value.viewer_login,
            repos: repoResult.value.repos,
            fetchedAt: Date.now(),
          };
          applyPipelineRepoCache(nextRepoCache);
          writeStoredPipelineRepoCache(nextRepoCache);
          setPipelineSelectorError(null);
        } else if (!hasCachedRepos) {
          setPipelineSelectorError(
            tr(
              "파이프라인 에디터용 repo 목록을 불러오지 못했습니다. 마지막 성공값이 없어 에디터를 열 수 없습니다.",
              "Failed to load repository options for the pipeline editor, and no cached data is available yet.",
            ),
          );
          notify(
            "파이프라인 에디터용 repo 목록을 불러오지 못했습니다.",
            "Failed to load repository options for the pipeline editor.",
            "error",
          );
        }

        if (agentResult.status === "fulfilled" && agentResult.value) {
          const nextAgentCache: PipelineAgentCacheEntry = {
            agents: agentResult.value,
            fetchedAt: Date.now(),
          };
          applyPipelineAgentCache(nextAgentCache);
          writeStoredPipelineAgentCache(nextAgentCache);
        }
      })
      .finally(() => {
        if (!stale) {
          setPipelineSelectorLoading(false);
        }
      });
    return () => {
      stale = true;
      setPipelineSelectorLoading(false);
    };
  }, [
    activePanel,
    applyPipelineAgentCache,
    applyPipelineRepoCache,
    notify,
    tr,
  ]);

  const normalizedCompanyName = companyName.trim();
  const normalizedCeoName = ceoName.trim();
  const companyNameError =
    normalizedCompanyName.length === 0
      ? tr("회사 이름은 비워둘 수 없습니다.", "Company name is required.")
      : normalizedCompanyName.length > GENERAL_FIELD_LIMITS.companyName
        ? tr(
            `회사 이름은 ${GENERAL_FIELD_LIMITS.companyName}자 이하여야 합니다.`,
            `Company name must be ${GENERAL_FIELD_LIMITS.companyName} characters or fewer.`,
          )
        : null;
  const ceoNameError =
    normalizedCeoName.length > GENERAL_FIELD_LIMITS.ceoName
      ? tr(
          `CEO 이름은 ${GENERAL_FIELD_LIMITS.ceoName}자 이하여야 합니다.`,
          `CEO name must be ${GENERAL_FIELD_LIMITS.ceoName} characters or fewer.`,
        )
      : null;
  const generalFormInvalid = Boolean(companyNameError || ceoNameError);
  const generalFieldCount = GENERAL_FIELD_KEYS.length;

  const companyDirty =
    normalizedCompanyName !== settings.companyName.trim() ||
    normalizedCeoName !== settings.ceoName.trim() ||
    language !== settings.language ||
    theme !== settings.theme;
  const configDirty = Object.keys(configEdits).length > 0;
  const runtimeFieldCount = CATEGORIES.reduce((sum, category) => sum + category.fields.length, 0);
  const voiceAliasConflict = useMemo(() => findVoiceAliasConflict(voiceDraft), [voiceDraft]);
  const voiceDirty = useMemo(
    () => JSON.stringify(voiceConfigComparable(voiceConfig)) !== JSON.stringify(voiceConfigComparable(voiceDraft)),
    [voiceConfig, voiceDraft],
  );

  const visibleConfigEntries = useMemo(() => configEntries, [configEntries]);

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

  const handlePanelChange = useCallback((panel: SettingsPanel, mode: "push" | "replace" = "push") => {
    setActivePanel((current) => {
      if (typeof window !== "undefined" && !(current === panel && mode === "push")) {
        const url = new URL(window.location.href);
        url.searchParams.set(SETTINGS_PANEL_QUERY_KEY, panel);
        if (mode === "replace") {
          window.history.replaceState(window.history.state, "", url);
        } else {
          window.history.pushState(window.history.state, "", url);
        }
      }
      return panel;
    });
  }, []);

  const openOnboarding = useCallback(() => {
    handlePanelChange("onboarding");
    setShowOnboarding(true);
  }, [handlePanelChange]);

  // ----- SettingRowMeta roster -----
  // Every visible setting funnels through this catalog so the spec invariant
  // "all SettingRow rendered through SettingRowMeta" holds and search/badges
  // share the same data source.
  const generalMetas = useMemo<SettingRowMeta[]>(
    () => [
      {
        key: "companyName",
        group: "general",
        source: "kv_meta",
        editable: true,
        restartRequired: false,
        defaultValue: settings.companyName,
        effectiveValue: companyName,
        validation: companyNameError
          ? { ok: false, messageKo: companyNameError, messageEn: companyNameError }
          : { ok: true },
        flags: ["kv_meta"],
        labelKo: "회사 이름",
        labelEn: "Company name",
        hintKo: "대시보드와 주요 헤더에 표시되는 이름입니다.",
        hintEn: "Shown in the dashboard and primary headers.",
        inputKind: "text",
        storageLayerKo: "kv_meta['settings']",
        storageLayerEn: "kv_meta['settings']",
      },
      {
        key: "ceoName",
        group: "general",
        source: "kv_meta",
        editable: true,
        restartRequired: false,
        defaultValue: settings.ceoName,
        effectiveValue: ceoName,
        validation: ceoNameError
          ? { ok: false, messageKo: ceoNameError, messageEn: ceoNameError }
          : { ok: true },
        flags: ["kv_meta"],
        labelKo: "CEO 이름",
        labelEn: "CEO name",
        hintKo: "오피스와 일부 운영 UI에서 대표 인물 이름으로 사용됩니다.",
        hintEn: "Used as the representative persona name in office and ops surfaces.",
        inputKind: "text",
        storageLayerKo: "kv_meta['settings']",
        storageLayerEn: "kv_meta['settings']",
      },
      {
        key: "language",
        group: "general",
        source: "kv_meta",
        editable: true,
        restartRequired: false,
        defaultValue: settings.language,
        effectiveValue: language,
        flags: ["kv_meta"],
        labelKo: "언어",
        labelEn: "Language",
        hintKo: "대시보드 전반의 기본 언어와 로캘을 정합니다.",
        hintEn: "Sets the default language and locale across the dashboard.",
        inputKind: "select",
        selectOptions: [
          { value: "ko", labelKo: "한국어", labelEn: "Korean" },
          { value: "en", labelKo: "영어", labelEn: "English" },
          { value: "ja", labelKo: "일본어", labelEn: "Japanese" },
          { value: "zh", labelKo: "중국어", labelEn: "Chinese" },
        ],
        storageLayerKo: "kv_meta['settings']",
        storageLayerEn: "kv_meta['settings']",
      },
      {
        key: "theme",
        group: "general",
        source: "kv_meta",
        editable: true,
        restartRequired: false,
        defaultValue: settings.theme,
        effectiveValue: theme,
        flags: ["kv_meta"],
        labelKo: "테마",
        labelEn: "Theme",
        hintKo: "대시보드와 오피스 화면의 기본 분위기를 정합니다.",
        hintEn: "Sets the base look and feel for dashboard and office views.",
        inputKind: "select",
        selectOptions: [
          { value: "dark", labelKo: "다크", labelEn: "Dark" },
          { value: "light", labelKo: "라이트", labelEn: "Light" },
          { value: "auto", labelKo: "자동 (시스템)", labelEn: "Auto (System)" },
        ],
        storageLayerKo: "kv_meta['settings']",
        storageLayerEn: "kv_meta['settings']",
      },
    ],
    [
      ceoName,
      ceoNameError,
      companyName,
      companyNameError,
      language,
      settings.ceoName,
      settings.companyName,
      settings.language,
      settings.theme,
      theme,
    ],
  );

  const runtimeMetas = useMemo<SettingRowMeta[]>(
    () =>
      CATEGORIES.flatMap((category) =>
        category.fields.map<SettingRowMeta>((field) => {
          const current = rcValues[field.key] ?? rcDefaults[field.key] ?? 0;
          const def = rcDefaults[field.key] ?? 0;
          const overrideActive = current !== def;
          return {
            key: field.key,
            group: "runtime",
            source: overrideActive ? "live_override" : "runtime_config",
            editable: true,
            restartRequired: false,
            defaultValue: def,
            effectiveValue: current,
            flags: overrideActive ? ["kv_meta", "live_override"] : ["kv_meta"],
            labelKo: field.labelKo,
            labelEn: field.labelEn,
            hintKo: `${field.descriptionKo} · ${field.min}–${field.max}${field.unit}`,
            hintEn: `${field.descriptionEn} · ${field.min}–${field.max}${field.unit}`,
            inputKind: "number",
            valueUnit: field.unit,
            numericRange: { min: field.min, max: field.max, step: field.step },
            storageLayerKo: "kv_meta['runtime-config']",
            storageLayerEn: "kv_meta['runtime-config']",
            restartNoteKo: "저장 즉시 반영, 재시작 없이 다음 폴링 주기에 적용됩니다.",
            restartNoteEn: "Applies on the next poll without restart.",
          };
        }),
      ),
    [rcValues, rcDefaults],
  );

  const pipelineMetas = useMemo<SettingRowMeta[]>(
    () => configEntries.map((entry) => metaFromConfigEntry(entry, configEdits)),
    [configEntries, configEdits],
  );

  // Onboarding-managed kv_meta keys are exposed read-only here. They are
  // edited through the onboarding wizard / dedicated API, but the spec
  // requires them to be visible (with legacy_readonly chip + editable=false).
  const onboardingMetas = useMemo<SettingRowMeta[]>(
    () => [
      {
        key: "greeting_template",
        group: "onboarding",
        source: "kv_meta",
        editable: false,
        restartRequired: false,
        defaultValue: "welcome to AgentDesk",
        effectiveValue: "(managed by wizard)",
        flags: ["kv_meta", "read_only"],
        labelKo: "인사 템플릿",
        labelEn: "Greeting template",
        hintKo: "신규 에이전트 첫 메시지. 위저드에서 관리합니다.",
        hintEn: "First message for new agents. Managed by the wizard.",
        inputKind: "readonly",
        storageLayerKo: "kv_meta (wizard)",
        storageLayerEn: "kv_meta (wizard)",
      },
      {
        key: "trial_card_count",
        group: "onboarding",
        source: "kv_meta",
        editable: false,
        restartRequired: false,
        defaultValue: 2,
        effectiveValue: "(managed by wizard)",
        flags: ["kv_meta", "read_only"],
        labelKo: "트라이얼 카드 수",
        labelEn: "Trial card count",
        hintKo: "연습용으로 할당하는 카드 수입니다.",
        hintEn: "Practice cards allocated to a new workspace.",
        inputKind: "readonly",
        storageLayerKo: "kv_meta (wizard)",
        storageLayerEn: "kv_meta (wizard)",
      },
      {
        key: "onboarding_bot_token",
        group: "onboarding",
        source: "legacy_readonly",
        editable: false,
        restartRequired: false,
        effectiveValue: "(stored via onboarding API)",
        flags: ["read_only"],
        labelKo: "Discord 봇 토큰",
        labelEn: "Discord bot token",
        hintKo: "/api/onboarding/* 가 관리합니다. 위저드를 사용하세요.",
        hintEn: "Managed by /api/onboarding/*. Use the wizard.",
        inputKind: "readonly",
        storageLayerKo: "/api/onboarding + kv_meta",
        storageLayerEn: "/api/onboarding + kv_meta",
      },
      {
        key: "onboarding_guild_id",
        group: "onboarding",
        source: "legacy_readonly",
        editable: false,
        restartRequired: false,
        effectiveValue: "(stored via onboarding API)",
        flags: ["read_only"],
        labelKo: "Guild ID",
        labelEn: "Guild ID",
        hintKo: "/api/onboarding/* 가 관리합니다.",
        hintEn: "Managed by /api/onboarding/*.",
        inputKind: "readonly",
        storageLayerKo: "/api/onboarding + kv_meta",
        storageLayerEn: "/api/onboarding + kv_meta",
      },
      {
        key: "onboarding_owner_id",
        group: "onboarding",
        source: "legacy_readonly",
        editable: false,
        restartRequired: false,
        effectiveValue: "(stored via onboarding API)",
        flags: ["read_only"],
        labelKo: "Owner ID",
        labelEn: "Owner ID",
        hintKo: "/api/onboarding/* 가 관리합니다.",
        hintEn: "Managed by /api/onboarding/*.",
        inputKind: "readonly",
        storageLayerKo: "/api/onboarding + kv_meta",
        storageLayerEn: "/api/onboarding + kv_meta",
      },
      {
        key: "onboarding_provider",
        group: "onboarding",
        source: "legacy_readonly",
        editable: false,
        restartRequired: false,
        effectiveValue: "(stored via onboarding API)",
        flags: ["read_only"],
        labelKo: "Provider 연결",
        labelEn: "Provider wiring",
        hintKo: "/api/onboarding/* 가 관리합니다.",
        hintEn: "Managed by /api/onboarding/*.",
        inputKind: "readonly",
        storageLayerKo: "/api/onboarding + kv_meta",
        storageLayerEn: "/api/onboarding + kv_meta",
      },
    ],
    [],
  );

  const voiceMetas = useMemo<SettingRowMeta[]>(() => {
    const global = voiceDraft?.global;
    const metas: SettingRowMeta[] = [
      {
        key: "voice.global.lobby_channel_id",
        group: "voice",
        source: "repo_canonical",
        editable: true,
        restartRequired: false,
        effectiveValue: global?.lobby_channel_id ?? "",
        flags: [],
        labelKo: "Lobby 채널 ID",
        labelEn: "Lobby channel ID",
        hintKo: "단일 voice-lobby로 들어오는 음성을 agent alias 라우팅에 사용합니다.",
        hintEn: "Single voice-lobby channel used for agent alias routing.",
        inputKind: "text",
        storageLayerKo: "agentdesk.yaml voice.lobby_channel_id",
        storageLayerEn: "agentdesk.yaml voice.lobby_channel_id",
      },
      {
        key: "voice.global.active_agent_ttl_seconds",
        group: "voice",
        source: "repo_canonical",
        editable: true,
        restartRequired: false,
        defaultValue: 180,
        effectiveValue: global?.active_agent_ttl_seconds ?? 180,
        flags: [],
        labelKo: "Active agent TTL",
        labelEn: "Active agent TTL",
        hintKo: "alias 없이 이어 말할 수 있는 최근 agent 유지 시간입니다.",
        hintEn: "How long follow-up speech can continue without repeating an alias.",
        inputKind: "number",
        valueUnit: "s",
        numericRange: { min: 30, max: 1800, step: 30 },
        storageLayerKo: "agentdesk.yaml voice.active_agent_ttl_seconds",
        storageLayerEn: "agentdesk.yaml voice.active_agent_ttl_seconds",
      },
      {
        key: "voice.global.default_sensitivity_mode",
        group: "voice",
        source: "repo_canonical",
        editable: true,
        restartRequired: false,
        defaultValue: "normal",
        effectiveValue: global?.default_sensitivity_mode ?? "normal",
        flags: [],
        labelKo: "기본 민감도",
        labelEn: "Default sensitivity",
        hintKo: "agent별 override가 없을 때 적용할 barge-in 민감도입니다.",
        hintEn: "Barge-in sensitivity used when an agent has no override.",
        inputKind: "select",
        selectOptions: VOICE_SENSITIVITY_OPTIONS,
        storageLayerKo: "agentdesk.yaml voice.default_sensitivity_mode",
        storageLayerEn: "agentdesk.yaml voice.default_sensitivity_mode",
      },
      {
        key: "voice.global.version",
        group: "voice",
        source: "repo_canonical",
        editable: false,
        restartRequired: false,
        effectiveValue: voiceDraft?.version ?? "",
        flags: ["read_only"],
        labelKo: "설정 버전",
        labelEn: "Config version",
        hintKo: "저장 시 optimistic locking에 사용하는 버전 해시입니다.",
        hintEn: "Version hash used for optimistic locking on save.",
        inputKind: "readonly",
        storageLayerKo: "server-computed",
        storageLayerEn: "server-computed",
      },
    ];
    for (const agent of voiceDraft?.agents ?? []) {
      metas.push(
        {
          key: `voice.agent.${agent.id}.enabled`,
          group: "voice",
          source: "repo_canonical",
          editable: true,
          restartRequired: false,
          effectiveValue: agent.voice_enabled,
          flags: [],
          labelKo: `${agent.name_ko ?? agent.name} 음성 활성화`,
          labelEn: `${agent.name} voice enabled`,
          hintKo: "voice-lobby 라우팅 대상에 포함할지 결정합니다.",
          hintEn: "Controls whether this agent participates in voice-lobby routing.",
          inputKind: "toggle",
          storageLayerKo: `agentdesk.yaml agents.${agent.id}.voice_enabled`,
          storageLayerEn: `agentdesk.yaml agents.${agent.id}.voice_enabled`,
        },
        {
          key: `voice.agent.${agent.id}.wake_word`,
          group: "voice",
          source: "repo_canonical",
          editable: true,
          restartRequired: false,
          effectiveValue: agent.wake_word,
          flags: [],
          labelKo: `${agent.name_ko ?? agent.name} wake word`,
          labelEn: `${agent.name} wake word`,
          hintKo: "비어 있으면 agent alias만으로 라우팅합니다.",
          hintEn: "When empty, the agent routes by alias only.",
          inputKind: "text",
          storageLayerKo: `agentdesk.yaml agents.${agent.id}.wake_word`,
          storageLayerEn: `agentdesk.yaml agents.${agent.id}.wake_word`,
        },
        {
          key: `voice.agent.${agent.id}.aliases`,
          group: "voice",
          source: "repo_canonical",
          editable: true,
          restartRequired: false,
          effectiveValue: agent.aliases.join(", "),
          validation: voiceAliasConflict &&
            (voiceAliasConflict.firstAgent.id === agent.id || voiceAliasConflict.secondAgent.id === agent.id)
            ? {
                ok: false,
                messageKo: `alias 충돌: ${voiceAliasConflict.normalized}`,
                messageEn: `alias collision: ${voiceAliasConflict.normalized}`,
              }
            : { ok: true },
          flags: voiceAliasConflict &&
            (voiceAliasConflict.firstAgent.id === agent.id || voiceAliasConflict.secondAgent.id === agent.id)
            ? ["alert"]
            : [],
          labelKo: `${agent.name_ko ?? agent.name} aliases`,
          labelEn: `${agent.name} aliases`,
          hintKo: "쉼표 또는 줄바꿈으로 여러 호출명을 입력합니다.",
          hintEn: "Enter multiple spoken aliases separated by commas or new lines.",
          inputKind: "text",
          storageLayerKo: `agentdesk.yaml agents.${agent.id}.aliases`,
          storageLayerEn: `agentdesk.yaml agents.${agent.id}.aliases`,
        },
        {
          key: `voice.agent.${agent.id}.sensitivity`,
          group: "voice",
          source: "repo_canonical",
          editable: true,
          restartRequired: false,
          effectiveValue: agent.sensitivity_mode,
          flags: [],
          labelKo: `${agent.name_ko ?? agent.name} 민감도`,
          labelEn: `${agent.name} sensitivity`,
          hintKo: "agent별 barge-in 감지 민감도입니다.",
          hintEn: "Per-agent barge-in detection sensitivity.",
          inputKind: "select",
          selectOptions: VOICE_SENSITIVITY_OPTIONS,
          storageLayerKo: `agentdesk.yaml agents.${agent.id}.sensitivity_mode`,
          storageLayerEn: `agentdesk.yaml agents.${agent.id}.sensitivity_mode`,
        },
      );
    }
    return metas;
  }, [voiceAliasConflict, voiceDraft]);

  const allMetas = useMemo<SettingRowMeta[]>(
    () => [...pipelineMetas, ...runtimeMetas, ...voiceMetas, ...onboardingMetas, ...generalMetas],
    [pipelineMetas, runtimeMetas, voiceMetas, onboardingMetas, generalMetas],
  );

  const groupCounts = useMemo(() => {
    const counts: Record<string, number> = {
      pipeline: 0,
      runtime: 0,
      voice: 0,
      onboarding: 0,
      general: 0,
    };
    for (const m of allMetas) {
      const g = String(m.group);
      counts[g] = (counts[g] ?? 0) + 1;
    }
    return counts;
  }, [allMetas]);

  const navItems = useMemo(
    () =>
      SETTING_GROUPS.map((group) => ({
        id: group.id,
        title: tr(group.nameKo, group.nameEn),
        detail: tr(group.descKo, group.descEn),
        count: String(groupCounts[group.id] ?? 0),
      })),
    [groupCounts, tr],
  );
  const panelQueryNormalized = panelQuery.trim().toLowerCase();
  const filteredNavItems = useMemo(
    () =>
      navItems.filter((item) => {
        if (!panelQueryNormalized) return true;
        if (`${item.title} ${item.detail}`.toLowerCase().includes(panelQueryNormalized)) {
          return true;
        }
        // also match if any item inside the group matches the search
        return allMetas.some((meta) => {
          if (meta.group !== item.id) return false;
          const haystack =
            `${meta.key} ${meta.labelKo ?? ""} ${meta.labelEn ?? ""} ${meta.hintKo ?? ""} ${meta.hintEn ?? ""}`.toLowerCase();
          return haystack.includes(panelQueryNormalized);
        });
      }),
    [allMetas, navItems, panelQueryNormalized],
  );
  // Track which row keys match the current search inside the active panel.
  const matchingKeysInActivePanel = useMemo<Set<string>>(() => {
    const set = new Set<string>();
    if (!panelQueryNormalized) return set;
    for (const meta of allMetas) {
      if (meta.group !== activePanel) continue;
      const haystack =
        `${meta.key} ${meta.labelKo ?? ""} ${meta.labelEn ?? ""} ${meta.hintKo ?? ""} ${meta.hintEn ?? ""}`.toLowerCase();
      if (haystack.includes(panelQueryNormalized)) {
        set.add(meta.key);
      }
    }
    return set;
  }, [activePanel, allMetas, panelQueryNormalized]);
  const isRowVisible = useCallback(
    (key: string) => {
      if (!panelQueryNormalized) return true;
      return matchingKeysInActivePanel.has(key);
    },
    [matchingKeysInActivePanel, panelQueryNormalized],
  );
  const activeNavItem = navItems.find((item) => item.id === activePanel) ?? navItems[0];
  const pipelineLiveOverrideCount = useMemo(
    () => configEntries.filter((entry) => entry.override_active).length,
    [configEntries],
  );
  const pipelineReadOnlyCount = useMemo(
    () =>
      configEntries.filter(
        (entry) => isReadOnlyConfigKey(entry.key) || entry.editable === false,
      ).length,
    [configEntries],
  );

  const inputStyle: CSSProperties = {
    background: "var(--th-bg-surface)",
    border: "1px solid var(--th-border)",
    color: "var(--th-text)",
  };
  const primaryActionClass = "inline-flex min-h-[44px] shrink-0 items-center justify-center whitespace-nowrap rounded-2xl px-5 py-2.5 text-sm font-medium text-white transition-colors disabled:opacity-50";
  const primaryActionStyle: CSSProperties = { background: "var(--th-accent-primary)" };
  const secondaryActionClass = "inline-flex min-h-[44px] items-center justify-center whitespace-nowrap rounded-2xl border px-5 py-2.5 text-sm font-medium transition-[opacity,color,border-color] hover:opacity-100";
  const secondaryActionStyle: CSSProperties = {
    borderColor: "rgba(148,163,184,0.28)",
    color: "var(--th-text-secondary)",
    background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)",
  };
  const subtleButtonClass = "inline-flex items-center justify-center whitespace-nowrap rounded-full border px-3 py-1.5 text-[11px] font-medium transition-colors";
  const subtleButtonStyle: CSSProperties = {
    borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
    color: "var(--th-text-muted)",
    background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)",
  };

  const handleSave = async (event?: FormEvent<HTMLFormElement>) => {
    event?.preventDefault();
    if (generalFormInvalid) return;
    setSaving(true);
    try {
      await onSave({
        companyName: normalizedCompanyName,
        ceoName: normalizedCeoName,
        language,
        theme,
      });
      notify("일반 설정을 저장했습니다.", "Saved general settings.", "success");
    } catch {
      notify("일반 설정 저장에 실패했습니다.", "Failed to save general settings.", "error");
    } finally {
      setSaving(false);
    }
  };

  const handleRcSave = async () => {
    setRcSaving(true);
    try {
      await api.saveRuntimeConfig(rcValues);
      setRcDirty(false);
      notify("런타임 설정을 저장했습니다.", "Saved runtime settings.", "success");
    } catch {
      notify("런타임 설정 저장에 실패했습니다.", "Failed to save runtime settings.", "error");
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
    const pendingEdits = { ...configEdits };
    const previousEntries = configEntries;
    setConfigSaving(true);
    setConfigEntries((current) => applyConfigEdits(current, pendingEdits));
    setConfigEdits({});
    try {
      const response = await fetch("/api/settings/config", {
        method: "PATCH",
        credentials: "include",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(pendingEdits),
      });
      if (!response.ok) {
        throw new Error("config-save-failed");
      }
      await loadConfigEntries();
      notify(
        "파이프라인 설정을 저장했습니다.",
        "Saved pipeline settings.",
        "success",
      );
    } catch {
      setConfigEntries(previousEntries);
      setConfigEdits(pendingEdits);
      notify(
        "파이프라인 설정 저장에 실패해 이전 값으로 복원했습니다.",
        "Failed to save pipeline settings and restored the previous values.",
        "error",
      );
    } finally {
      setConfigSaving(false);
    }
  };

  const updateVoiceGlobal = useCallback(
    <K extends keyof VoiceGlobalConfig>(key: K, value: VoiceGlobalConfig[K]) => {
      setVoiceDraft((current) =>
        current
          ? {
              ...current,
              global: {
                ...current.global,
                [key]: value,
              },
            }
          : current,
      );
    },
    [],
  );

  const updateVoiceAgent = useCallback(
    (agentId: string, patch: Partial<VoiceAgentConfig>) => {
      setVoiceDraft((current) =>
        current
          ? {
              ...current,
              agents: current.agents.map((agent) =>
                agent.id === agentId ? { ...agent, ...patch } : agent,
              ),
            }
          : current,
      );
    },
    [],
  );

  const handleVoiceSave = async () => {
    if (!voiceDraft || !voiceDirty || voiceAliasConflict) return;
    setVoiceSaving(true);
    setVoiceError(null);
    try {
      const saved = await api.saveVoiceConfig(voiceSaveBody(voiceDraft));
      setVoiceConfig(saved);
      setVoiceDraft(cloneVoiceConfig(saved));
      notify("음성 설정을 저장했습니다.", "Saved voice settings.", "success");
    } catch (error) {
      const message =
        error instanceof api.VoiceConfigApiError
          ? error.message
          : tr("음성 설정 저장에 실패했습니다.", "Failed to save voice settings.");
      setVoiceError(message);
      notify("음성 설정 저장에 실패했습니다.", "Failed to save voice settings.", "error");
      if (error instanceof api.VoiceConfigApiError && error.status === 409) {
        void loadVoiceConfig();
      }
    } finally {
      setVoiceSaving(false);
    }
  };

  // Dispatcher for SettingRow value changes — routes to the correct setter
  // based on the meta.group + key.
  const handleSettingRowChange = useCallback(
    (key: string, value: string | boolean | number) => {
      // general
      if (key === "companyName" && typeof value === "string") {
        setCompanyName(value);
        return;
      }
      if (key === "ceoName" && typeof value === "string") {
        setCeoName(value);
        return;
      }
      if (key === "language" && typeof value === "string") {
        setLanguage(value as typeof language);
        return;
      }
      if (key === "theme" && typeof value === "string") {
        setTheme(value as typeof theme);
        return;
      }
      // runtime
      if (rcDefaults[key] !== undefined && typeof value === "number") {
        handleRcChange(key, value);
        return;
      }
      // pipeline kv_meta — value can be string or boolean
      if (typeof value === "boolean") {
        handleConfigEdit(key, value);
        return;
      }
      handleConfigEdit(key, String(value));
    },
    [handleRcChange, rcDefaults],
  );

  // Render a SettingRow for a meta with optional control overlay (e.g. range).
  const renderSettingRow = useCallback(
    (meta: SettingRowMeta, options?: { controlOverlay?: ReactNode; trailingMeta?: ReactNode }) => {
      if (!isRowVisible(meta.key)) return null;
      return (
        <SettingRow
          key={meta.key}
          meta={meta}
          isKo={isKo}
          onChange={handleSettingRowChange}
          controlOverlay={options?.controlOverlay}
          trailingMeta={options?.trailingMeta}
        />
      );
    },
    [handleSettingRowChange, isKo, isRowVisible],
  );

  // Render a card-shaped group of SettingRow entries (header + count chip + rows).
  const renderSettingGroupCard = useCallback(
    (
      args: {
        titleKo: string;
        titleEn: string;
        descriptionKo: string;
        descriptionEn: string;
        rows: ReactNode[];
        totalCount: number;
      },
    ) => {
      const filteredRows = args.rows.filter(Boolean);
      const countLabel = panelQueryNormalized
        ? `${filteredRows.length}/${args.totalCount}`
        : tr(`${args.totalCount}개`, `${args.totalCount} items`);
      return (
        <div
          className="setting-group-card overflow-hidden rounded-[20px] border"
          style={{
            borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
            background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
          }}
        >
          <div
            className="flex flex-wrap items-start justify-between gap-3 border-b px-4 py-4 sm:px-5"
            style={{ borderColor: "color-mix(in srgb, var(--th-border) 60%, transparent)" }}
          >
            <div className="min-w-0">
              <div className="settings-section-title text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                {tr(args.titleKo, args.titleEn)}
              </div>
              <div className="settings-copy mt-1 text-[12px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                {tr(args.descriptionKo, args.descriptionEn)}
              </div>
            </div>
            <span
              className="settings-count-chip inline-flex shrink-0 items-center rounded-full border px-2.5 py-1 text-[10px] font-medium"
              style={{
                borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
                background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
                color: "var(--th-text-muted)",
              }}
            >
              {countLabel}
            </span>
          </div>
          <div className="px-2 pb-1 pt-1 sm:px-3">
            {filteredRows.length > 0 ? (
              filteredRows
            ) : (
              <SettingsEmptyState className="text-sm">
                {tr("검색 결과가 없습니다.", "No matching settings.")}
              </SettingsEmptyState>
            )}
          </div>
        </div>
      );
    },
    [panelQueryNormalized, tr],
  );

  const renderGeneralPanel = () => (
    <form className="space-y-5" onSubmit={handleSave} noValidate>
      {renderSettingGroupCard({
        titleKo: "일반",
        titleEn: "General",
        descriptionKo: "회사 정보와 표시 환경, 메타 설정.",
        descriptionEn: "Company identity, display environment, and meta settings.",
        totalCount: generalMetas.length,
        rows: generalMetas.map((meta) => renderSettingRow(meta)),
      })}

      <SettingsCallout
        action={(
          <button
            type="submit"
            disabled={saving || !companyDirty || generalFormInvalid}
            className={primaryActionClass}
            style={primaryActionStyle}
          >
            {saving ? tr("저장 중...", "Saving...") : tr("일반 설정 저장", "Save general settings")}
          </button>
        )}
      >
        <p className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
          {tr(
            "일반 설정은 한 번에 저장되며 기존 `settings` JSON과 병합해 hidden key 손실을 막습니다. 회사 이름은 필수이고 텍스트 입력은 저장 시 trim 처리됩니다.",
            "General settings save together and merge into the existing `settings` JSON so hidden keys are preserved. Company name is required, and text inputs are trimmed on save.",
          )}
        </p>
      </SettingsCallout>

      <SettingsSubsection
        title={tr("저장 경로", "Storage surfaces")}
        description={tr(
          "이 화면의 값이 어디에 저장되는지 먼저 보여줍니다. 저장면을 숨기면 운영자가 설정의 실제 영향 범위를 오해하게 됩니다.",
          "Show where each setting is persisted. Hiding storage surfaces makes the UI misleading for operators.",
        )}
      >
        <div className="grid gap-3 md:grid-cols-2 2xl:grid-cols-4">
          <StorageSurfaceCard
            title={tr("회사 설정 JSON", "Company settings JSON")}
            body={tr(
              "`/api/settings`가 `kv_meta['settings']` 전체 JSON을 저장합니다. 부분 patch가 아니라 full replace라서 merged save가 필요합니다.",
              "`/api/settings` stores the full `kv_meta['settings']` JSON. It is a full replace API, so the UI must send a merged save.",
            )}
            footer={tr("source: kv_meta['settings']", "source: kv_meta['settings']")}
          />
          <StorageSurfaceCard
            title={tr("런타임 설정", "Runtime config")}
            body={tr(
              "폴링 주기와 cache TTL 같은 값은 `kv_meta['runtime-config']`에 저장되고 재시작 없이 반영됩니다.",
              "Polling intervals and cache TTL values live in `kv_meta['runtime-config']` and apply without restart.",
            )}
            footer={tr("source: kv_meta['runtime-config']", "source: kv_meta['runtime-config']")}
          />
          <StorageSurfaceCard
            title={tr("정책/파이프라인 키", "Policy and pipeline keys")}
            body={tr(
              "리뷰, 타임아웃, context compact 같은 값은 개별 `kv_meta` 키로 저장되고 `/api/settings/config` whitelist를 통해 노출됩니다.",
              "Review, timeout, and context-compaction values are stored as individual `kv_meta` keys and exposed through `/api/settings/config`.",
            )}
            footer={tr("source: individual kv_meta keys", "source: individual kv_meta keys")}
          />
          <StorageSurfaceCard
            title={tr("온보딩/시크릿", "Onboarding and secrets")}
            body={tr(
              "봇 토큰과 guild/owner/provider 설정은 일반 폼이 아니라 전용 온보딩 API와 위저드가 관리합니다.",
              "Bot tokens and guild/owner/provider wiring are managed by the dedicated onboarding API and wizard rather than the general form.",
            )}
            footer={tr("source: onboarding API + kv_meta", "source: onboarding API + kv_meta")}
          />
        </div>
      </SettingsSubsection>
    </form>
  );

  const renderRuntimePanel = () => (
    <div className="space-y-4">
      {!rcLoaded ? (
        <SettingsEmptyState className="text-sm">
          {tr("런타임 설정을 불러오는 중...", "Loading runtime config...")}
        </SettingsEmptyState>
      ) : (
        <div className="space-y-4">
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
                {tr(category.titleKo, category.titleEn)}{" "}
                <span className="ml-1 opacity-60">{category.fields.length}</span>
              </button>
            ))}
          </div>

          {activeRuntimeCategory &&
            (() => {
              const categoryMetas = runtimeMetas.filter((meta) =>
                activeRuntimeCategory.fields.some((f) => f.key === meta.key),
              );
              return renderSettingGroupCard({
                titleKo: activeRuntimeCategory.titleKo,
                titleEn: activeRuntimeCategory.titleEn,
                descriptionKo: activeRuntimeCategory.descriptionKo,
                descriptionEn: activeRuntimeCategory.descriptionEn,
                totalCount: categoryMetas.length,
                rows: categoryMetas.map((meta) => {
                  const field = activeRuntimeCategory.fields.find((f) => f.key === meta.key);
                  if (!field) return renderSettingRow(meta);
                  const value = Number(meta.effectiveValue) || 0;
                  const defaultValue = Number(meta.defaultValue) || 0;
                  const isDefault = value === defaultValue;
                  // Use a custom control: range slider + numeric input + reset.
                  const controlOverlay = (
                    <div className="flex items-center gap-2">
                      <input
                        type="range"
                        min={field.min}
                        max={field.max}
                        step={field.step}
                        value={value}
                        onChange={(event) =>
                          handleRcChange(field.key, Number(event.target.value))
                        }
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
                        className="w-20 rounded-xl px-2 py-1.5 text-right text-xs"
                        style={{
                          ...inputStyle,
                          fontFamily:
                            "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
                        }}
                      />
                    </div>
                  );
                  const trailingMeta = !isDefault ? (
                    <button
                      type="button"
                      onClick={() => handleRcReset(field.key)}
                      className={subtleButtonClass}
                      style={subtleButtonStyle}
                    >
                      {tr("기본값 복원", "Reset to default")}
                    </button>
                  ) : null;
                  return renderSettingRow(meta, { controlOverlay, trailingMeta });
                }),
              });
            })()}

          <SettingsCallout
            className="mt-0"
            action={(
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
            <p className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "런타임 설정은 저장 즉시 반영됩니다. 현재 선택한 하위 카테고리는 브라우저에 기억해 두었다가 다음 방문 때 다시 엽니다.",
                "Runtime settings apply immediately on save. The selected subcategory is remembered in the browser and restored on the next visit.",
              )}
            </p>
          </SettingsCallout>
        </div>
      )}
    </div>
  );

  const renderPipelineCategory = (categoryKey: keyof typeof SYSTEM_CATEGORY_META) => {
    // Pipeline group rows are built directly from configEntries via pipelineMetas.
    // We still group them by category for visual hierarchy inside the pipeline panel.
    const entries = groupedConfigEntries[categoryKey] ?? [];
    if (entries.length === 0) return null;
    const meta = SYSTEM_CATEGORY_META[categoryKey];
    const metasInCategory = entries
      .map((entry) => pipelineMetas.find((m) => m.key === entry.key))
      .filter((m): m is SettingRowMeta => Boolean(m));
    return renderSettingGroupCard({
      titleKo: meta.titleKo,
      titleEn: meta.titleEn,
      descriptionKo: meta.descriptionKo,
      descriptionEn: meta.descriptionEn,
      totalCount: metasInCategory.length,
      rows: metasInCategory.map((m) => {
        const trailingMeta = m.key.endsWith("_channel_id") ? (
          <span style={{ color: "var(--th-text-muted)" }}>
            {tr(
              "Discord channel ID는 정밀도 손실을 피하려고 문자열로 유지합니다.",
              "Discord channel IDs stay as strings to avoid precision loss.",
            )}
          </span>
        ) : null;
        return renderSettingRow(m, { trailingMeta });
      }),
    });
  };

  const renderPipelinePanel = () => (
    <div className="space-y-5">
      {configEntries.length === 0 ? (
        <SettingsEmptyState className="text-sm">
          {tr("파이프라인 설정을 불러오는 중...", "Loading pipeline config...")}
        </SettingsEmptyState>
      ) : (
        <div className="space-y-5">
          <SettingsCallout
            action={(
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
            <p className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "이 섹션은 whitelist된 개별 `kv_meta` 키만 편집합니다. read-only 항목도 숨기지 않고 현재 상태를 드러내며, `context_clear_*` 같은 API 바깥 항목은 아래 audit 노트에서 별도로 정리합니다.",
                "This section edits only whitelisted individual `kv_meta` keys. Read-only items remain visible as status, and API-outside items such as `context_clear_*` are tracked in the audit notes below.",
              )}
            </p>
          </SettingsCallout>

          <SettingsSubsection
            title={tr("FSM 비주얼 에디터", "FSM visual editor")}
            description={tr(
              "repo/agent 범위를 먼저 고른 뒤, 상태 전환 event·hook·policy를 전용 FSM 캔버스에서 조정합니다.",
              "Pick the repo or agent scope first, then tune transition events, hooks, and policies on the dedicated FSM canvas.",
            )}
          >
            {pipelineSelectorLoading && pipelineRepos.length === 0 ? (
              <SettingsEmptyState className="text-sm">
                {tr("파이프라인 에디터 대상을 불러오는 중...", "Loading pipeline editor targets...")}
              </SettingsEmptyState>
            ) : pipelineSelectorError ? (
              <SettingsEmptyState className="text-sm">
                {pipelineSelectorError}
              </SettingsEmptyState>
            ) : pipelineRepos.length === 0 ? (
              <SettingsEmptyState className="text-sm">
                {tr("편집 가능한 repo가 없습니다.", "No editable repositories are available.")}
              </SettingsEmptyState>
            ) : (
              <div className="space-y-4">
                <div className="grid gap-3 md:grid-cols-[minmax(0,1fr)_220px]">
                  <CompactFieldCard
                    label={tr("대상 repo", "Target repo")}
                    description={tr(
                      "기본 FSM은 repo 레벨에서 편집하고, 필요할 때만 agent override로 내려갑니다.",
                      "Start at the repo-level FSM and only drop to an agent override when needed.",
                    )}
                  >
                    <select
                      value={selectedPipelineRepo}
                      onChange={(event) => setSelectedPipelineRepo(event.target.value)}
                      className="w-full rounded-2xl px-3 py-2.5 text-sm"
                      style={inputStyle}
                    >
                      {pipelineRepos.map((repo) => (
                        <option key={repo.nameWithOwner} value={repo.nameWithOwner}>
                          {repo.nameWithOwner}
                        </option>
                      ))}
                    </select>
                  </CompactFieldCard>
                  <CompactFieldCard
                    label={tr("에이전트 override", "Agent override")}
                    description={tr(
                      "선택하면 editor 안에서 agent 레벨 전환을 활성화합니다.",
                      "Selecting an agent enables the agent-level path inside the editor.",
                    )}
                  >
                    <select
                      value={selectedPipelineAgentId ?? ""}
                      onChange={(event) => setSelectedPipelineAgentId(event.target.value || null)}
                      className="w-full rounded-2xl px-3 py-2.5 text-sm"
                      style={inputStyle}
                    >
                      <option value="">{tr("없음", "None")}</option>
                      {pipelineAgents.map((agent) => (
                        <option key={agent.id} value={agent.id}>
                          {formatPipelineAgentLabel(agent, isKo)}
                        </option>
                      ))}
                    </select>
                  </CompactFieldCard>
                </div>

                {selectedPipelineRepo ? (
                  <div className="space-y-4">
                    <Suspense
                      fallback={(
                        <SettingsEmptyState className="text-sm">
                          {tr("FSM 에디터를 준비하는 중...", "Preparing FSM editor...")}
                        </SettingsEmptyState>
                      )}
                    >
                      <FsmEditor
                        tr={tr}
                        locale={isKo ? "ko" : "en"}
                        repo={selectedPipelineRepo}
                        agents={pipelineAgents}
                        selectedAgentId={selectedPipelineAgentId}
                      />
                    </Suspense>

                    <SettingsSubsection
                      title={tr("고급 / Agent별 파이프라인 편집기", "Advanced / agent-specific pipeline editor")}
                      description={tr(
                        "FSM 바깥의 state hook, timeout, phase gate, stage 실행 순서는 아래 고급 편집기에서 따로 다룹니다.",
                        "State hooks, timeouts, phase gates, and stage execution stay in the advanced editor below.",
                      )}
                    >
                      <Suspense
                        fallback={(
                          <SettingsEmptyState className="text-sm">
                            {tr("고급 파이프라인 편집기를 준비하는 중...", "Preparing advanced pipeline editor...")}
                          </SettingsEmptyState>
                        )}
                      >
                        <PipelineVisualEditor
                          tr={tr}
                          locale={isKo ? "ko" : "en"}
                          repo={selectedPipelineRepo}
                          agents={pipelineAgents}
                          selectedAgentId={selectedPipelineAgentId}
                          variant="advanced"
                        />
                      </Suspense>
                    </SettingsSubsection>
                  </div>
                ) : (
                  <SettingsEmptyState className="text-sm">
                    {tr("repo를 선택하면 FSM 에디터가 열립니다.", "Select a repo to open the FSM editor.")}
                  </SettingsEmptyState>
                )}
              </div>
            )}
          </SettingsSubsection>

          <div className="space-y-3">
            <GroupLabel title={tr("자주 쓰는 설정", "Frequent settings")} />
            {PRIMARY_PIPELINE_CATEGORIES.map(renderPipelineCategory)}
          </div>
          <div className="space-y-3">
            <GroupLabel title={tr("고급 설정", "Advanced settings")} />
            {ADVANCED_PIPELINE_CATEGORIES.map(renderPipelineCategory)}
          </div>

          <details
            id="settings-audit-notes"
            data-testid="settings-audit-notes"
            className="settings-audit-disclosure rounded-[20px] border"
            style={{
              borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
              background: "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
            }}
          >
            <summary
              className="flex cursor-pointer list-none items-start justify-between gap-3 px-4 py-4 sm:px-5"
              data-testid="settings-audit-summary"
            >
              <span className="min-w-0">
                <span className="settings-section-title block text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                  {tr("운영자 감사 노트", "Operator audit notes")}
                </span>
                <span className="settings-copy mt-1 block text-[12px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {tr(
                    "일반 폼에 넣으면 오해를 만드는 backend-followup과 read-only 항목만 접어 둡니다.",
                    "Backend-followup and read-only items stay folded away from the regular form.",
                  )}
                </span>
              </span>
              <span
                className="settings-count-chip inline-flex shrink-0 items-center rounded-full border px-2.5 py-1 text-[10px] font-medium"
                style={{
                  borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
                  background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
                  color: "var(--th-text-muted)",
                }}
              >
                {tr(`${AUDIT_NOTES.length}개`, `${AUDIT_NOTES.length} notes`)}
              </span>
            </summary>
            <div className="border-t px-4 py-4 sm:px-5" style={{ borderColor: "color-mix(in srgb, var(--th-border) 62%, transparent)" }}>
              <div className="grid gap-3 md:grid-cols-2">
                {AUDIT_NOTES.map((note) => (
                  <AuditNoteCard key={note.id} note={note} isKo={isKo} />
                ))}
              </div>
            </div>
          </details>
        </div>
      )}
    </div>
  );

  const renderVoicePanel = () => {
    if (!voiceLoaded) {
      return (
        <SettingsEmptyState className="text-sm">
          {tr("음성 설정을 불러오는 중...", "Loading voice config...")}
        </SettingsEmptyState>
      );
    }
    if (!voiceDraft) {
      return (
        <div className="space-y-4">
          <SettingsEmptyState className="text-sm">
            {voiceError ?? tr("음성 설정을 불러오지 못했습니다.", "Failed to load voice settings.")}
          </SettingsEmptyState>
          <button
            type="button"
            onClick={() => void loadVoiceConfig()}
            className={secondaryActionClass}
            style={secondaryActionStyle}
          >
            {tr("다시 불러오기", "Retry")}
          </button>
        </div>
      );
    }

    const visibleGlobalCards = [
      isRowVisible("voice.global.lobby_channel_id") ? (
        <CompactFieldCard
          key="lobby"
          label={tr("Lobby 채널 ID", "Lobby channel ID")}
          description={tr(
            "단일 voice-lobby로 들어오는 음성을 agent alias 라우팅에 사용합니다.",
            "Single voice-lobby channel used for agent alias routing.",
          )}
        >
          <input
            value={voiceDraft.global.lobby_channel_id ?? ""}
            onChange={(event) => updateVoiceGlobal("lobby_channel_id", event.target.value)}
            className="w-full rounded-2xl px-3 py-2.5 text-sm"
            style={inputStyle}
            placeholder={tr("예: 1503294653313712169", "e.g. 1503294653313712169")}
          />
        </CompactFieldCard>
      ) : null,
      isRowVisible("voice.global.active_agent_ttl_seconds") ? (
        <CompactFieldCard
          key="ttl"
          label={tr("Active agent TTL", "Active agent TTL")}
          description={tr(
            "alias 없이 이어 말할 수 있는 최근 agent 유지 시간입니다.",
            "How long follow-up speech can continue without repeating an alias.",
          )}
        >
          <div className="flex items-center gap-3">
            <input
              type="range"
              min={30}
              max={1800}
              step={30}
              value={voiceDraft.global.active_agent_ttl_seconds}
              onChange={(event) =>
                updateVoiceGlobal("active_agent_ttl_seconds", Number(event.target.value))
              }
              className="h-1.5 flex-1 cursor-pointer appearance-none rounded-full"
              style={{ accentColor: "var(--th-accent-primary)" }}
            />
            <input
              type="number"
              min={1}
              step={30}
              value={voiceDraft.global.active_agent_ttl_seconds}
              onChange={(event) =>
                updateVoiceGlobal("active_agent_ttl_seconds", Number(event.target.value) || 180)
              }
              className="w-24 rounded-xl px-2 py-1.5 text-right text-xs"
              style={{
                ...inputStyle,
                fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
              }}
            />
          </div>
        </CompactFieldCard>
      ) : null,
      isRowVisible("voice.global.default_sensitivity_mode") ? (
        <CompactFieldCard
          key="sensitivity"
          label={tr("기본 민감도", "Default sensitivity")}
          description={tr(
            "agent별 override가 없을 때 적용할 barge-in 민감도입니다.",
            "Barge-in sensitivity used when an agent has no override.",
          )}
        >
          <select
            value={voiceDraft.global.default_sensitivity_mode}
            onChange={(event) =>
              updateVoiceGlobal("default_sensitivity_mode", event.target.value as VoiceSensitivityMode)
            }
            className="w-full rounded-2xl px-3 py-2.5 text-sm"
            style={inputStyle}
          >
            {VOICE_SENSITIVITY_OPTIONS.map((option) => (
              <option key={option.value} value={option.value}>
                {tr(option.labelKo, option.labelEn)}
              </option>
            ))}
          </select>
        </CompactFieldCard>
      ) : null,
      isRowVisible("voice.global.version") ? (
        <CompactFieldCard
          key="version"
          label={tr("설정 버전", "Config version")}
          description={tr(
            "저장 시 optimistic locking에 사용하는 버전 해시입니다.",
            "Version hash used for optimistic locking on save.",
          )}
        >
          <code
            className="block truncate rounded-xl px-3 py-2 text-xs"
            style={{
              background: "color-mix(in srgb, var(--th-overlay-medium) 80%, transparent)",
              color: "var(--th-text-muted)",
              fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
            }}
          >
            {voiceDraft.version}
          </code>
        </CompactFieldCard>
      ) : null,
    ].filter(Boolean);

    const agentCards = voiceDraft.agents
      .filter((agent) =>
        voiceAgentKeys(agent.id).some((key) => isRowVisible(key)),
      )
      .map((agent) => {
        const displayName = isKo && agent.name_ko ? agent.name_ko : agent.name;
        const conflictInAgent =
          voiceAliasConflict &&
          (voiceAliasConflict.firstAgent.id === agent.id || voiceAliasConflict.secondAgent.id === agent.id);
        return (
          <SettingsCard
            key={agent.id}
            className="rounded-2xl p-4"
            style={{
              borderColor: conflictInAgent
                ? "rgba(248,113,113,0.45)"
                : "color-mix(in srgb, var(--th-border) 72%, transparent)",
              background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
            }}
          >
            <div className="flex flex-wrap items-start justify-between gap-3">
              <div className="min-w-0">
                <div className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                  {displayName}
                </div>
                <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                  {agent.id} · {agent.name}
                </div>
              </div>
              <button
                type="button"
                role="switch"
                aria-checked={agent.voice_enabled}
                onClick={() => updateVoiceAgent(agent.id, { voice_enabled: !agent.voice_enabled })}
                className="relative inline-flex h-7 w-12 items-center rounded-full transition-colors"
                style={{
                  background: agent.voice_enabled
                    ? "var(--th-accent-primary)"
                    : "color-mix(in srgb, var(--th-border) 80%, transparent)",
                }}
              >
                <span
                  className="inline-block h-6 w-6 rounded-full bg-white shadow transition-transform"
                  style={{ transform: agent.voice_enabled ? "translateX(1.45rem)" : "translateX(0.13rem)" }}
                />
              </button>
            </div>

            <div className="mt-4 grid gap-3 md:grid-cols-2">
              {isRowVisible(`voice.agent.${agent.id}.wake_word`) ? (
                <CompactFieldCard
                  label={tr("Wake word", "Wake word")}
                  description={tr("비어 있으면 agent alias만으로 라우팅합니다.", "When empty, the agent routes by alias only.")}
                >
                  <input
                    value={agent.wake_word}
                    onChange={(event) => updateVoiceAgent(agent.id, { wake_word: event.target.value })}
                    className="w-full rounded-2xl px-3 py-2.5 text-sm"
                    style={inputStyle}
                    placeholder={tr("예: 에이전트", "e.g. agent")}
                  />
                </CompactFieldCard>
              ) : null}
              {isRowVisible(`voice.agent.${agent.id}.sensitivity`) ? (
                <CompactFieldCard
                  label={tr("민감도", "Sensitivity")}
                  description={tr("agent별 barge-in 감지 민감도입니다.", "Per-agent barge-in detection sensitivity.")}
                >
                  <select
                    value={agent.sensitivity_mode}
                    onChange={(event) =>
                      updateVoiceAgent(agent.id, { sensitivity_mode: event.target.value as VoiceSensitivityMode })
                    }
                    className="w-full rounded-2xl px-3 py-2.5 text-sm"
                    style={inputStyle}
                  >
                    {VOICE_SENSITIVITY_OPTIONS.map((option) => (
                      <option key={option.value} value={option.value}>
                        {tr(option.labelKo, option.labelEn)}
                      </option>
                    ))}
                  </select>
                </CompactFieldCard>
              ) : null}
              {isRowVisible(`voice.agent.${agent.id}.aliases`) ? (
                <CompactFieldCard
                  label={tr("Aliases", "Aliases")}
                  description={tr("쉼표 또는 줄바꿈으로 여러 호출명을 입력합니다.", "Enter multiple spoken aliases separated by commas or new lines.")}
                  footer={tr(
                    `기본 alias: ${voiceAgentBuiltInAliases(agent).join(", ")}`,
                    `Built-in aliases: ${voiceAgentBuiltInAliases(agent).join(", ")}`,
                  )}
                >
                  <textarea
                    value={agent.aliases.join("\n")}
                    onChange={(event) => updateVoiceAgent(agent.id, { aliases: splitVoiceAliases(event.target.value) })}
                    className="min-h-[92px] w-full resize-y rounded-2xl px-3 py-2.5 text-sm"
                    style={inputStyle}
                  />
                </CompactFieldCard>
              ) : null}
            </div>
          </SettingsCard>
        );
      });

    return (
      <div className="space-y-5">
        <SettingsCallout
          action={(
            <button
              type="button"
              onClick={() => void handleVoiceSave()}
              disabled={voiceSaving || !voiceDirty || Boolean(voiceAliasConflict)}
              className={primaryActionClass}
              style={primaryActionStyle}
            >
              {voiceSaving ? tr("저장 중...", "Saving...") : tr("음성 설정 저장", "Save voice")}
            </button>
          )}
        >
          <p className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
            {tr(
              "음성 설정은 agentdesk.yaml에 저장되며 runtime voice routing이 다음 발화부터 다시 읽습니다. alias는 NFC/lowercase/공백·특수문자 제거 기준으로 충돌을 막습니다.",
              "Voice settings are stored in agentdesk.yaml and runtime voice routing reloads them on the next utterance. Aliases reject collisions after NFC/lowercase and removing spaces/special characters.",
            )}
          </p>
        </SettingsCallout>

        {voiceError ? (
          <SettingsEmptyState className="text-sm">{voiceError}</SettingsEmptyState>
        ) : null}

        {voiceAliasConflict ? (
          <SettingsCallout>
            <p className="text-sm leading-6" style={{ color: "rgba(252,165,165,0.95)" }}>
              {tr(
                `alias 충돌: ${voiceAliasConflict.firstAgent.name} "${voiceAliasConflict.firstAlias}" ↔ ${voiceAliasConflict.secondAgent.name} "${voiceAliasConflict.secondAlias}" (${voiceAliasConflict.normalized})`,
                `Alias collision: ${voiceAliasConflict.firstAgent.name} "${voiceAliasConflict.firstAlias}" ↔ ${voiceAliasConflict.secondAgent.name} "${voiceAliasConflict.secondAlias}" (${voiceAliasConflict.normalized})`,
              )}
            </p>
          </SettingsCallout>
        ) : null}

        {renderSettingGroupCard({
          titleKo: "Voice lobby",
          titleEn: "Voice lobby",
          descriptionKo: "lobby 채널, active-agent TTL, 기본 민감도와 버전입니다.",
          descriptionEn: "Lobby channel, active-agent TTL, default sensitivity, and version.",
          totalCount: 4,
          rows: visibleGlobalCards,
        })}

        <SettingsSubsection
          title={tr("에이전트 음성 라우팅", "Agent voice routing")}
          description={tr(
            "각 agent의 음성 활성화, wake word, 호출 alias, 민감도 override를 편집합니다.",
            "Edit each agent's voice enablement, wake word, spoken aliases, and sensitivity override.",
          )}
        >
          <div className="grid gap-3">
            {agentCards.length > 0 ? (
              agentCards
            ) : (
              <SettingsEmptyState className="text-sm">
                {tr("검색 결과가 없습니다.", "No matching agents.")}
              </SettingsEmptyState>
            )}
          </div>
        </SettingsSubsection>
      </div>
    );
  };

  const renderOnboardingPanel = () => (
    <div className="space-y-5">
      {renderSettingGroupCard({
        titleKo: "온보딩",
        titleEn: "Onboarding",
        descriptionKo: "위저드 / /api/onboarding/* 가 관리하는 키. 일반 폼이 아니라 위저드를 사용하세요.",
        descriptionEn: "Wizard- and /api/onboarding/*-managed keys. Use the wizard instead of editing here.",
        totalCount: onboardingMetas.length,
        rows: onboardingMetas.map((meta) => renderSettingRow(meta)),
      })}

      <div className="grid gap-3 md:grid-cols-[minmax(0,1.15fr)_minmax(16rem,0.85fr)]">
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
    </div>
  );

  const renderActivePanel = () => {
    switch (activePanel) {
      case "runtime":
        return renderRuntimePanel();
      case "pipeline":
        return renderPipelinePanel();
      case "voice":
        return renderVoicePanel();
      case "onboarding":
        return renderOnboardingPanel();
      case "general":
      default:
        return renderGeneralPanel();
    }
  };
  const renderHeaderActions = () => {
    if (activePanel === "onboarding") {
      return (
        <button
          onClick={openOnboarding}
          className={secondaryActionClass}
          style={secondaryActionStyle}
        >
          {tr("온보딩 다시 실행", "Re-run onboarding")}
        </button>
      );
    }

    if (activePanel === "pipeline") {
      return (
        <>
          <button
            type="button"
            onClick={() =>
              document
                .getElementById("settings-audit-notes")
                ?.scrollIntoView({ behavior: "smooth", block: "start" })
            }
            className={secondaryActionClass}
            style={secondaryActionStyle}
          >
            <Eye size={12} />
            {tr("audit 노트", "Audit notes")}
          </button>
          <button
            onClick={handleConfigSave}
            disabled={configSaving || !configDirty}
            className={primaryActionClass}
            style={primaryActionStyle}
          >
            <Check size={12} />
            {configSaving ? tr("저장 중...", "Saving...") : tr("저장", "Save")}
          </button>
        </>
      );
    }

    if (activePanel === "runtime") {
      return (
        <button
          onClick={handleRcSave}
          disabled={rcSaving || !rcDirty}
          className={primaryActionClass}
          style={primaryActionStyle}
        >
          <Check size={12} />
          {rcSaving ? tr("저장 중...", "Saving...") : tr("저장", "Save")}
        </button>
      );
    }

    if (activePanel === "voice") {
      return (
        <>
          <button
            type="button"
            onClick={() => void loadVoiceConfig()}
            className={secondaryActionClass}
            style={secondaryActionStyle}
          >
            {tr("다시 불러오기", "Reload")}
          </button>
          <button
            type="button"
            onClick={() => void handleVoiceSave()}
            disabled={voiceSaving || !voiceDirty || Boolean(voiceAliasConflict)}
            className={primaryActionClass}
            style={primaryActionStyle}
          >
            <Check size={12} />
            {voiceSaving ? tr("저장 중...", "Saving...") : tr("저장", "Save")}
          </button>
        </>
      );
    }

    return (
      <button
        onClick={() => void handleSave()}
        disabled={saving || generalFormInvalid || !companyDirty}
        className={primaryActionClass}
        style={primaryActionStyle}
      >
        <Check size={12} />
        {saving ? tr("저장 중...", "Saving...") : tr("저장", "Save")}
      </button>
    );
  };
  const settingsInfoNotice = (
    <div
      className="flex items-start gap-3 rounded-[18px] border px-4 py-4 sm:px-5"
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
      }}
    >
      <div
        className="grid h-7 w-7 shrink-0 place-items-center rounded-[10px]"
        style={{
          background: "var(--th-accent-primary-soft)",
          color: "var(--th-accent-primary)",
        }}
      >
        <Info size={14} />
      </div>
      <div className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
        {tr("whitelist된 ", "Only whitelisted ")}
        <code
          className="rounded px-1.5 py-0.5 text-[12px]"
          style={{
            fontFamily: "var(--font-mono)",
            color: "var(--th-text)",
            background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
          }}
        >
          kv_meta
        </code>{" "}
        {tr(
          "키와 agentdesk.yaml 음성 설정만 편집합니다. read-only 항목도 숨기지 않고 현재 상태를 그대로 보여줍니다.",
          "keys and agentdesk.yaml voice settings are editable. Read-only items stay visible so the current state remains explicit.",
        )}
      </div>
    </div>
  );

  return (
    <div
      data-testid="settings-page"
      className="page fade-in mx-auto h-full w-full max-w-[1600px] min-w-0 overflow-x-hidden overflow-y-auto px-4 py-4 pb-40 sm:px-6"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <div className="page-header">
        <div className="min-w-0">
          <div className="page-title">{tr("설정", "Settings")}</div>
          <div className="page-sub">
            {tr(
              "카탈로그에서 꺼내 쓰는 kv_meta 설정",
              "Catalog-driven kv_meta configuration",
            )}
          </div>
        </div>
        <div className="flex flex-wrap gap-2">{renderHeaderActions()}</div>
      </div>

      <div className="settings-grid mt-4 grid gap-4 md:grid-cols-[220px_minmax(0,1fr)]">
        <aside className="settings-nav min-w-0 md:sticky md:top-4 md:self-start">
          <div className="relative mb-3">
            <Search
              size={13}
              className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2"
              style={{ color: "var(--th-text-muted)" }}
            />
            <input
              type="search"
              value={panelQuery}
              onChange={(event) => setPanelQuery(event.target.value)}
              placeholder={tr("설정 검색", "Search settings")}
              aria-label={tr("설정 검색", "Search settings")}
              className="w-full rounded-xl py-2.5 pl-9 pr-3 text-sm"
              style={inputStyle}
              data-testid="settings-search-input"
            />
            {panelQuery ? (
              <button
                type="button"
                onClick={() => setPanelQuery("")}
                className="absolute right-2 top-1/2 grid h-7 w-7 -translate-y-1/2 place-items-center rounded-lg"
                style={{ color: "var(--th-text-muted)" }}
                aria-label={tr("검색 지우기", "Clear search")}
              >
                <X size={13} />
              </button>
            ) : null}
          </div>

          {panelQueryNormalized ? (
            <div
              className="settings-search-summary mb-3 rounded-xl border px-3 py-2 text-[11px] leading-5"
              style={{
                borderColor: "color-mix(in srgb, var(--th-border) 64%, transparent)",
                background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
                color: "var(--th-text-muted)",
              }}
              data-testid="settings-search-summary"
            >
              {tr(
                `현재 패널 ${matchingKeysInActivePanel.size}개 항목 일치`,
                `${matchingKeysInActivePanel.size} matches in this panel`,
              )}
            </div>
          ) : null}

          <div
            role="tablist"
            aria-label={tr("설정 패널", "Settings panels")}
            className="settings-nav-items space-y-1"
          >
            {filteredNavItems.length > 0 ? (
              filteredNavItems.map((item) => (
                <PanelNavButton
                  key={item.id}
                  id={`settings-tab-${item.id}`}
                  active={activePanel === item.id}
                  title={item.title}
                  detail={item.detail}
                  count={item.count}
                  ariaControls="settings-panel-content"
                  onClick={() => handlePanelChange(item.id)}
                />
              ))
            ) : (
              <SettingsEmptyState className="text-sm">
                {tr("검색 결과가 없습니다.", "No groups match the search.")}
              </SettingsEmptyState>
            )}
          </div>
        </aside>

        <div className="min-w-0 space-y-4">
          {settingsInfoNotice}
          <SettingsGlossary isKo={isKo} />

          <SettingsCard
            id="settings-panel-content"
            role="tabpanel"
            aria-labelledby={`settings-tab-${activePanel}`}
            tabIndex={-1}
            className="min-w-0 rounded-[28px] border px-4 py-4 outline-none sm:px-5 sm:py-5"
            style={{
              borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
              background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
            }}
          >
            <div className="flex flex-wrap items-start justify-between gap-3 border-b pb-4" style={{ borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)" }}>
              <div className="min-w-0">
                <div className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                  {activeNavItem.title}
                </div>
                <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {activeNavItem.detail}
                </div>
              </div>
              {activeNavItem.count ? (
                <span
                  className="inline-flex items-center rounded-full border px-2.5 py-1 text-[10px] font-medium"
                  style={{
                    borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
                    background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
                    color: "var(--th-text-muted)",
                  }}
                >
                  {activeNavItem.count}
                </span>
              ) : null}
            </div>
            <div className="mt-5 min-w-0">
              {renderActivePanel()}
            </div>
          </SettingsCard>
        </div>
      </div>

      {showOnboarding && (
        <div className="fixed inset-0 z-50 overflow-y-auto bg-[#0a0e1a]" role="dialog" aria-modal="true" aria-label="Onboarding wizard">
          <div className="flex min-h-screen items-start justify-center pb-16 pt-8">
            <div ref={onboardingDialogRef} className="w-full max-w-2xl">
              <div className="mb-2 flex justify-end px-4">
                <button
                  ref={onboardingCloseButtonRef}
                  onClick={() => setShowOnboarding(false)}
                  className="min-h-[44px] rounded-lg border px-4 py-2.5 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[color:var(--th-accent-primary)] focus-visible:ring-offset-2 focus-visible:ring-offset-[#0a0e1a]"
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
