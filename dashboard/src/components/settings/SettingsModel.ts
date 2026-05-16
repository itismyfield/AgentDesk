import type { Agent } from "../../types";
import type { GitHubRepoOption } from "../../api";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import { readLocalStorageValue } from "../../lib/useLocalStorage";
import { isDangerousConfigKey } from "./settingsDangerousConfig";
import { CATEGORIES } from "./SettingsRuntimeCategories";
export { CATEGORIES, type ConfigField } from "./SettingsRuntimeCategories";
export {
  PIPELINE_SELECTOR_CACHE_MAX_AGE_MS,
  PIPELINE_SELECTOR_REFRESH_TIMEOUT_MS,
  getCachedPipelineAgentEntry,
  getCachedPipelineRepoEntry,
  isCacheFresh,
  isPipelineAgentCacheEntry,
  isPipelineRepoCacheEntry,
  pickMostRecentCache,
  readStoredPipelineAgentCache,
  readStoredPipelineRepoCache,
  writeStoredPipelineAgentCache,
  writeStoredPipelineRepoCache,
  type PipelineAgentCacheEntry,
  type PipelineRepoCacheEntry,
} from "./SettingsPipelineCache";
export {
  VOICE_SENSITIVITY_OPTIONS,
  cloneVoiceConfig,
  findVoiceAliasConflict,
  normalizeVoiceAliasKey,
  splitVoiceAliases,
  voiceAgentBuiltInAliases,
  voiceAgentKeys,
  voiceConfigComparable,
  voiceSaveBody,
  type VoiceAliasConflict,
} from "./SettingsVoiceModel";

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

