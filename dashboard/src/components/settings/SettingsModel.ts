import type {
  Agent,
  VoiceAgentConfig,
  VoiceConfigPutBody,
  VoiceConfigResponse,
  VoiceSensitivityMode,
} from "../../types";
import * as api from "../../api";
import type { GitHubRepoOption } from "../../api";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import {
  readLocalStorageValue,
  writeLocalStorageValue,
} from "../../lib/useLocalStorage";
import { isDangerousConfigKey } from "./settingsDangerousConfig";

export interface ConfigField {
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

export type ConfigEntry = {
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

export type ConfigEditValue = string | boolean;
export type PendingDangerousConfigSave = {
  edits: Record<string, ConfigEditValue>;
  keys: string[];
};
export type SettingsPanel = "general" | "runtime" | "pipeline" | "onboarding" | "voice";
export type SettingsNotificationType = "info" | "success" | "warning" | "error";

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

export const SETTINGS_PANEL_QUERY_KEY = "settingsPanel";
export const GENERAL_FIELD_KEYS = ["companyName", "ceoName", "language", "theme"] as const;
export const PIPELINE_SELECTOR_REFRESH_TIMEOUT_MS = 5_000;
export const PIPELINE_SELECTOR_CACHE_MAX_AGE_MS = 60_000;

export interface PipelineRepoCacheEntry {
  viewerLogin: string;
  repos: GitHubRepoOption[];
  fetchedAt: number;
}

export interface PipelineAgentCacheEntry {
  agents: Agent[];
  fetchedAt: number;
}

export const CATEGORIES: Array<{
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

export const BOOLEAN_CONFIG_KEYS = new Set([
  "review_enabled",
  "pm_decision_gate_enabled",
  "merge_automation_enabled",
]);

export const NUMERIC_CONFIG_KEYS = new Set([
  "max_review_rounds",
  "requested_timeout_min",
  "in_progress_stale_min",
  "max_chain_depth",
  "context_compact_percent",
  "context_compact_percent_codex",
  "context_compact_percent_claude",
  "server_port",
]);

export const READ_ONLY_CONFIG_KEYS = new Set(["server_port"]);
export const GENERAL_FIELD_LIMITS = {
  companyName: 80,
  ceoName: 60,
} as const;

export const SYSTEM_CONFIG_DESCRIPTIONS: Record<string, { ko: string; en: string }> = {
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

export const SYSTEM_CATEGORY_META = {
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

export const PRIMARY_PIPELINE_CATEGORIES: Array<keyof typeof SYSTEM_CATEGORY_META> = ["pipeline", "review", "timeout", "dispatch"];
export const ADVANCED_PIPELINE_CATEGORIES: Array<keyof typeof SYSTEM_CATEGORY_META> = ["context", "system"];

export function isSettingsPanel(value: string | null): value is SettingsPanel {
  return value === "general" || value === "runtime" || value === "pipeline" || value === "onboarding" || value === "voice";
}

export function isRuntimeCategoryId(value: string | null): value is string {
  return CATEGORIES.some((category) => category.id === value);
}

export function readSettingsPanelFromUrl(): SettingsPanel | null {
  if (typeof window === "undefined") return null;
  const value = new URLSearchParams(window.location.search).get(SETTINGS_PANEL_QUERY_KEY);
  return isSettingsPanel(value) ? value : null;
}

export function readStoredSettingsPanel(): SettingsPanel {
  const panelFromUrl = readSettingsPanelFromUrl();
  if (panelFromUrl) {
    return panelFromUrl;
  }
  return readLocalStorageValue<SettingsPanel>(STORAGE_KEYS.settingsPanel, "pipeline", {
    validate: (value): value is SettingsPanel => typeof value === "string" && isSettingsPanel(value),
    legacy: (raw) => (isSettingsPanel(raw) ? raw : null),
  });
}

export function readStoredRuntimeCategory(): string {
  return readLocalStorageValue<string>(STORAGE_KEYS.settingsRuntimeCategory, CATEGORIES[0]?.id ?? "polling", {
    validate: (value): value is string => typeof value === "string" && isRuntimeCategoryId(value),
    legacy: (raw) => (isRuntimeCategoryId(raw) ? raw : null),
  });
}

export const VOICE_SENSITIVITY_OPTIONS: Array<{
  value: VoiceSensitivityMode;
  labelKo: string;
  labelEn: string;
}> = [
  { value: "normal", labelKo: "보통", labelEn: "Normal" },
  { value: "conservative", labelKo: "보수적", labelEn: "Conservative" },
];

export interface VoiceAliasConflict {
  normalized: string;
  firstAgent: VoiceAgentConfig;
  firstAlias: string;
  secondAgent: VoiceAgentConfig;
  secondAlias: string;
}

export function cloneVoiceConfig(config: VoiceConfigResponse): VoiceConfigResponse {
  return {
    ...config,
    global: { ...config.global },
    agents: config.agents.map((agent) => ({
      ...agent,
      aliases: [...agent.aliases],
    })),
  };
}

export function normalizeVoiceAliasKey(value: string): string {
  return Array.from(value.normalize("NFC").toLocaleLowerCase())
    .filter((ch) => /[\p{Letter}\p{Number}]/u.test(ch))
    .join("")
    .normalize("NFC");
}

export function splitVoiceAliases(value: string): string[] {
  return value
    .split(/[,\n]/)
    .map((alias) => alias.trim())
    .filter((alias, index, aliases) => alias.length > 0 && aliases.indexOf(alias) === index);
}

export function voiceAgentBuiltInAliases(agent: VoiceAgentConfig): string[] {
  return [agent.id, agent.name, agent.name_ko ?? ""].filter((value) => value.trim().length > 0);
}

export function findVoiceAliasConflict(config: VoiceConfigResponse | null): VoiceAliasConflict | null {
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

export function voiceAgentKeys(agentId: string): string[] {
  return [
    `voice.agent.${agentId}.enabled`,
    `voice.agent.${agentId}.wake_word`,
    `voice.agent.${agentId}.aliases`,
    `voice.agent.${agentId}.sensitivity`,
  ];
}

export function voiceConfigComparable(config: VoiceConfigResponse | null): unknown {
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

export function voiceSaveBody(config: VoiceConfigResponse): VoiceConfigPutBody {
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

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

export function isPipelineRepoCacheEntry(value: unknown): value is PipelineRepoCacheEntry {
  return isRecord(value)
    && typeof value.viewerLogin === "string"
    && typeof value.fetchedAt === "number"
    && Array.isArray(value.repos);
}

export function isPipelineAgentCacheEntry(value: unknown): value is PipelineAgentCacheEntry {
  return isRecord(value)
    && typeof value.fetchedAt === "number"
    && Array.isArray(value.agents);
}

export function readStoredPipelineRepoCache(): PipelineRepoCacheEntry | null {
  return readLocalStorageValue<PipelineRepoCacheEntry | null>(
    STORAGE_KEYS.settingsPipelineRepoCache,
    null,
    {
      validate: (value): value is PipelineRepoCacheEntry | null =>
        value === null || isPipelineRepoCacheEntry(value),
    },
  );
}

export function writeStoredPipelineRepoCache(cache: PipelineRepoCacheEntry): void {
  writeLocalStorageValue(STORAGE_KEYS.settingsPipelineRepoCache, cache);
}

export function readStoredPipelineAgentCache(): PipelineAgentCacheEntry | null {
  return readLocalStorageValue<PipelineAgentCacheEntry | null>(
    STORAGE_KEYS.settingsPipelineAgentCache,
    null,
    {
      validate: (value): value is PipelineAgentCacheEntry | null =>
        value === null || isPipelineAgentCacheEntry(value),
    },
  );
}

export function writeStoredPipelineAgentCache(cache: PipelineAgentCacheEntry): void {
  writeLocalStorageValue(STORAGE_KEYS.settingsPipelineAgentCache, cache);
}

export function pickMostRecentCache<T extends { fetchedAt: number }>(...entries: Array<T | null>): T | null {
  return entries.reduce<T | null>((latest, entry) => {
    if (!entry) return latest;
    if (!latest || entry.fetchedAt > latest.fetchedAt) {
      return entry;
    }
    return latest;
  }, null);
}

export function isCacheFresh(cache: { fetchedAt: number } | null): boolean {
  if (!cache) return false;
  return Date.now() - cache.fetchedAt < PIPELINE_SELECTOR_CACHE_MAX_AGE_MS;
}

export function getCachedPipelineRepoEntry(): PipelineRepoCacheEntry | null {
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

export function getCachedPipelineAgentEntry(): PipelineAgentCacheEntry | null {
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

export function isBooleanConfigKey(key: string): boolean {
  return BOOLEAN_CONFIG_KEYS.has(key);
}

export function isNumericConfigKey(key: string): boolean {
  return NUMERIC_CONFIG_KEYS.has(key);
}

export function isReadOnlyConfigKey(key: string): boolean {
  return READ_ONLY_CONFIG_KEYS.has(key);
}

export function parseBooleanConfigValue(value: string | boolean | null | undefined): boolean {
  if (typeof value === "boolean") return value;
  const normalized = String(value ?? "").trim().toLowerCase();
  return normalized === "true" || normalized === "1" || normalized === "yes" || normalized === "on";
}

export function formatUnit(value: number, unit: string): string {
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

export function configLayerLabel(overrideActive: boolean, isKo: boolean): string {
  return overrideActive ? (isKo ? "실시간 override" : "Live override") : (isKo ? "기준값" : "Baseline");
}

export function configLayerClass(overrideActive: boolean): string {
  return overrideActive ? "border-amber-400/30 bg-amber-400/10 text-amber-100" : "border-emerald-400/30 bg-emerald-400/10 text-emerald-100";
}

export function configSourceLabel(entry: ConfigEntry, isKo: boolean): string {
  if (entry.override_active) return "kv_meta";
  if (entry.baseline_source === "config") {
    return isKo ? "env/config" : "env/config";
  }
  return isKo ? "default" : "default";
}

export function configSourceClass(entry: ConfigEntry): string {
  if (entry.override_active) {
    return "border-sky-400/30 bg-sky-400/10 text-sky-100";
  }
  if (entry.baseline_source === "config") {
    return "border-violet-400/30 bg-violet-400/10 text-violet-100";
  }
  return "border-emerald-400/30 bg-emerald-400/10 text-emerald-100";
}

export function formatConfigValue(value: ConfigEditValue): string {
  return typeof value === "boolean" ? String(value) : value;
}

/**
 * Build a SettingRowMeta from a /api/settings/config kv_meta entry.
 * Drives the pipeline + runtime panel rows.
 */
export function metaFromConfigEntry(
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
  if (!readOnly && isDangerousConfigKey(entry.key)) flags.push("alert");
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

export function applyConfigEdits(
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

export function selectDefaultPipelineRepo(
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

export function formatPipelineAgentLabel(agent: Agent, isKo: boolean): string {
  // Native <option> cannot render React components, so we omit the emoji
  // fallback (sprite rendering happens in non-<option> avatar UIs).
  // Keeps the sprite-first policy from #1251 (emoji fallback禁止).
  const name = isKo ? agent.name_ko || agent.name : agent.name || agent.name_ko;
  return name;
}

export function baselineSourceNote(source: string | null | undefined, isKo: boolean): string | null {
  if (source === "yaml") return isKo ? "기준값 출처: agentdesk.yaml" : "Baseline source: agentdesk.yaml";
  if (source === "hardcoded") return isKo ? "기준값 출처: 하드코딩 기본값" : "Baseline source: hardcoded default";
  if (source === "config") return isKo ? "기준값 출처: 서버 설정" : "Baseline source: server config";
  return null;
}

export function restartBehaviorNote(behavior: string | null | undefined, isKo: boolean): string | null {
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

export interface SettingGroupMeta {
  id: SettingGroupId;
  nameKo: string;
  nameEn: string;
  descKo: string;
  descEn: string;
}

export const SETTING_GROUPS: SettingGroupMeta[] = [
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
export function configCategoryToGroup(category: string, key: string): SettingGroupId {
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

