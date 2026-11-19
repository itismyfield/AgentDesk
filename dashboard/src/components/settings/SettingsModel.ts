import type { Agent } from "../../types";
import type { GitHubRepoOption, RuntimeConfigMap, RuntimeConfigValue } from "../../api";
import {
  HIDDEN_SETTING_KEYS,
  SETTINGS_CATEGORIES,
  settingKeyMeta,
  settingKeyOrder,
  type RelatedSetting,
  type SettingsCategory,
} from "./SettingsCopy";
import { isDangerousConfigKey } from "./settingsDangerousConfig";
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

type Tr = (ko: string, en: string) => string;

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
/** A save held back until the operator confirms the dangerous keys it touches. */
export type PendingDangerousSave = {
  keys: string[];
  run: () => void;
};
export type SettingsNotificationType = "info" | "success" | "warning" | "error";
export type StatusTone = "ok" | "warn" | "unknown";

/** Read-only detail for a connection or status row: facts, server signals and setup steps. */
export interface StatusDetail {
  tone: StatusTone;
  /** Label, value and an optional note such as "진행 중 3 · 마무리 중 1". */
  facts: Array<[string, string, string?]>;
  notes?: Array<{ title: string; detail: string; tone: "warn" | "danger"; tooltip?: string; count?: number }>;
  steps?: { lines: string[]; translated: boolean };
  /** Last successful fetch; failed means the newest fetch failed and the facts are from then. */
  updatedAt?: number;
  failed?: boolean;
  refresh?: { run: () => void; busy: boolean };
}

/** One row of the settings catalog: /api/settings/config, /api/settings/runtime-config, /api/settings ("app") or a status source. */
export interface CatalogItem {
  key: string;
  source: "config" | "runtime" | "app" | "status";
  category: SettingsCategory;
  label: string;
  /** Empty until the key's behavior is confirmed in code (server meta: #8249). */
  description: string;
  input: "toggle" | "number" | "text" | "choice" | "readonly" | "status";
  options?: Array<{ value: string; label: string }>;
  /** Stored value as text; "" means none. */
  saved: string;
  /** Unsaved value held by the page, or null. */
  draft: string | null;
  baseline: string | null;
  baselineSource: string | null;
  /** Value "되돌리기" writes: the baseline, or "" for a key that then inherits another. */
  resetTo?: string | null;
  inherits?: boolean;
  overridden: boolean;
  restartNeeded: boolean;
  /** Server restart_behavior, or "config-file" for a config-file value that reloads live. */
  applies: string;
  min?: number;
  max?: number;
  related: RelatedSetting[];
  dangerous: boolean;
  status?: StatusDetail;
}

export type CategoryFilter = SettingsCategory | "all";

export interface CatalogFilter {
  category: CategoryFilter;
  query: string;
  differs: boolean;
  restart: boolean;
}

export const SETTINGS_CATEGORY_QUERY_KEY = "category";

// Tab ids from the previous settings page still arrive through bookmarks.
const LEGACY_PANEL_CATEGORY: Record<string, SettingsCategory> = {
  workflow: "workflow", pipeline: "workflow", runtime: "workflow", ai: "usage", providers: "usage",
  connections: "connections", connectors: "connections", onboarding: "connections", machine: "system",
  general: "system", voice: "system",
};

export function resolveCategory(params: URLSearchParams): CategoryFilter {
  const raw = params.get(SETTINGS_CATEGORY_QUERY_KEY);
  if (raw && SETTINGS_CATEGORIES.some((category) => category.id === raw)) return raw as SettingsCategory;
  return LEGACY_PANEL_CATEGORY[params.get("settingsPanel") ?? ""] ?? "all";
}

function describe(key: string, isKo: boolean): string {
  const meta = settingKeyMeta(key);
  return (isKo ? meta?.descKo : meta?.descEn) ?? "";
}

export function configItems(entries: ConfigEntry[], edits: Record<string, ConfigEditValue>, isKo: boolean): CatalogItem[] {
  return entries.filter((entry) => !HIDDEN_SETTING_KEYS.has(entry.key)).map((entry) => {
    const meta = settingKeyMeta(entry.key);
    const readOnly = entry.editable === false;
    const saved = entry.value ?? "";
    const baseline = entry.baseline ?? entry.default ?? null;
    // A stored "" on an inheriting key means "follow the shared value", which the server still flags as an override.
    const inherits = Boolean(meta?.inherits);
    return {
      key: entry.key,
      source: "config",
      category: meta?.category ?? "other",
      label: (isKo ? entry.label_ko : entry.label_en) || entry.key,
      description: describe(entry.key, isKo),
      input: readOnly ? "readonly" : meta?.input ?? "text",
      saved,
      draft: entry.key in edits ? String(edits[entry.key]) : null,
      baseline,
      baselineSource: entry.baseline_source ?? null,
      resetTo: baseline ?? (inherits && entry.override_active && saved !== "" ? "" : null),
      inherits,
      overridden: Boolean(entry.override_active) && !(inherits && saved === ""),
      restartNeeded: entry.restart_behavior === "config-only",
      applies: entry.restart_behavior ?? "persist-live-override",
      min: meta?.min,
      max: meta?.max,
      related: meta?.related ?? [],
      dangerous: !readOnly && isDangerousConfigKey(entry.key),
    };
  });
}

const asText = (value: RuntimeConfigValue | undefined) => (value === undefined ? "" : String(value));

export interface RuntimeSnapshot {
  saved: RuntimeConfigMap;
  draft: RuntimeConfigMap;
  defaults: RuntimeConfigMap;
  changedKeys: Set<string>;
}

/** Runtime keys carry no server label or restart note; a reset-on-restart server shows in its config entries. */
export function runtimeItems(runtime: RuntimeSnapshot, resetOnRestart: boolean, isKo: boolean): CatalogItem[] {
  const keys = new Set([...Object.keys(runtime.defaults), ...Object.keys(runtime.saved)]);
  return [...keys].filter((key) => !HIDDEN_SETTING_KEYS.has(key)).map((key) => {
    const meta = settingKeyMeta(key);
    const sample = runtime.saved[key] ?? runtime.defaults[key];
    const saved = asText(sample);
    const baseline = runtime.defaults[key] === undefined ? null : asText(runtime.defaults[key]);
    return {
      key,
      source: "runtime",
      category: meta?.category ?? "other",
      label: (isKo ? meta?.labelKo : meta?.labelEn) ?? key,
      description: describe(key, isKo),
      input: meta?.input ?? (typeof sample === "boolean" ? "toggle" : typeof sample === "number" ? "number" : "text"),
      saved,
      draft: runtime.changedKeys.has(key) ? asText(runtime.draft[key]) : null,
      baseline,
      // The API merges code and YAML defaults into one map, so the source is only "the server's default".
      baselineSource: "server-default",
      resetTo: baseline,
      overridden: baseline !== null && saved !== baseline,
      restartNeeded: false,
      applies: resetOnRestart ? "reset-to-baseline" : "persist-live-override",
      min: meta?.min,
      max: meta?.max,
      related: meta?.related ?? [],
      dangerous: false,
    };
  });
}

/** Language and default theme from /api/settings; DEFAULT_SETTINGS fills what the server has not stored. */
export function appItems(saved: { language: string; theme: string }, drafts: Record<string, string>, isKo: boolean): CatalogItem[] {
  const choices: Record<"language" | "theme", { baseline: string; options: Array<[string, string, string]> }> = {
    language: { baseline: "ko", options: [["ko", "한국어", "한국어"], ["en", "English", "English"], ["ja", "日本語", "日本語"], ["zh", "中文", "中文"]] },
    theme: { baseline: "dark", options: [["dark", "어둡게", "Dark"], ["light", "밝게", "Light"], ["auto", "시스템 설정 따르기", "Match system"]] },
  };
  return (["language", "theme"] as const).map((key) => {
    const meta = settingKeyMeta(key);
    const { baseline, options } = choices[key];
    return {
      key,
      source: "app",
      category: "system",
      label: (isKo ? meta?.labelKo : meta?.labelEn) ?? key,
      description: describe(key, isKo),
      input: "choice",
      options: options.map(([value, ko, en]) => ({ value, label: isKo ? ko : en })),
      saved: saved[key],
      draft: drafts[key] ?? null,
      baseline,
      baselineSource: "hardcoded",
      resetTo: baseline,
      overridden: saved[key] !== baseline,
      restartNeeded: false,
      applies: "persist-live-override",
      related: [],
      dangerous: false,
    };
  });
}

/** True when the server wipes live overrides on restart, read from any config entry's restart note. */
export function resetsOnRestart(entries: ConfigEntry[]): boolean {
  return entries.some((entry) => entry.restart_behavior === "reset-to-baseline" || entry.restart_behavior === "clear-on-restart");
}

const CATEGORY_ORDER = SETTINGS_CATEGORIES.map((category) => category.id as SettingsCategory);

export function sortCatalog(items: CatalogItem[]): CatalogItem[] {
  return [...items].sort((a, b) =>
    CATEGORY_ORDER.indexOf(a.category) - CATEGORY_ORDER.indexOf(b.category)
    || settingKeyOrder(a.key) - settingKeyOrder(b.key)
    || a.key.localeCompare(b.key));
}

export function matchesQuery(item: CatalogItem, query: string): boolean {
  const needle = query.trim().toLowerCase();
  return !needle || `${item.label} ${item.key} ${item.description}`.toLowerCase().includes(needle);
}

export function filterCatalog(items: CatalogItem[], filter: CatalogFilter): CatalogItem[] {
  return items.filter((item) =>
    (filter.category === "all" || item.category === filter.category)
    && (!filter.differs || item.overridden)
    && (!filter.restart || item.restartNeeded)
    && matchesQuery(item, filter.query));
}

export function catalogCounts(items: CatalogItem[]) {
  const byCategory: Partial<Record<SettingsCategory, number>> = {};
  for (const item of items) byCategory[item.category] = (byCategory[item.category] ?? 0) + 1;
  return {
    total: items.length,
    byCategory,
    differs: items.filter((item) => item.overridden).length,
    restart: items.filter((item) => item.restartNeeded).length,
  };
}

export function formatSettingValue(value: string, item: Pick<CatalogItem, "input" | "options" | "inherits">, tr: Tr): string {
  if (value === "") return item.inherits ? tr("공통 값 따름", "Follows the shared value") : tr("없음", "None");
  const option = item.options?.find((candidate) => candidate.value === value);
  if (option) return option.label;
  if (item.input === "toggle" && value === "true") return tr("켜짐", "On");
  if (item.input === "toggle" && value === "false") return tr("꺼짐", "Off");
  return value;
}

const SOURCE_LABELS: Record<string, [string, string]> = {
  hardcoded: ["코드", "code"],
  yaml: ["설정 파일", "config file"],
  config: ["설정 파일", "config file"],
  "server-default": ["서버 기본값", "server default"],
};

/** Inspector form: "3 · 코드", or "없음". */
export function defaultText(item: CatalogItem, tr: Tr): string {
  if (item.baseline === null) return tr("없음", "None");
  const value = formatSettingValue(item.baseline, item, tr);
  const source = item.baselineSource ? SOURCE_LABELS[item.baselineSource] : undefined;
  return source ? `${value} · ${tr(source[0], source[1])}` : value;
}

/** Row form: "기본 3 · 코드", "기본 없음", or "" for status rows. */
export function baselineText(item: CatalogItem, tr: Tr): string {
  if (item.source === "status") return "";
  return item.baseline === null ? tr("기본 없음", "No default") : tr(`기본 ${defaultText(item, tr)}`, `Default ${defaultText(item, tr)}`);
}

const APPLY_TEXT: Record<string, [string, string]> = {
  "persist-live-override": ["바로 · 재시작해도 유지", "Right away · kept after restart"],
  "reseed-from-yaml": ["바로 · 재시작하면 설정 파일 값으로", "Right away · back to the config file on restart"],
  "reset-to-baseline": ["바로 · 재시작하면 기본값으로", "Right away · back to the default on restart"],
  "clear-on-restart": ["바로 · 재시작하면 지워짐", "Right away · cleared on restart"],
  "config-only": ["설정 파일에서만 · 재시작해야 반영", "Config file only · after a restart"],
  "config-file": ["설정 파일에서만 · 바로 반영", "Config file only · reloads live"],
};

export function applyText(item: CatalogItem, tr: Tr): string {
  const text = APPLY_TEXT[item.applies];
  return item.source === "status" && !item.applies.startsWith("config") ? "" : text ? tr(text[0], text[1]) : "";
}

/** The row's apply chip; status rows show their state instead. */
export function applyChip(item: CatalogItem, tr: Tr): string | null {
  if (item.applies.startsWith("config")) return tr("설정 파일에서만", "Config file only");
  return item.source === "status" ? null : tr("바로 적용", "Applies now");
}

/** Why a draft cannot be saved, or null. Every number key is an integer on the server; config numbers may be empty. */
export function draftProblem(item: CatalogItem, text: string, tr: Tr): string | null {
  if (item.input !== "number") return null;
  if (text.trim() === "") return item.source === "runtime" ? tr("숫자를 넣어 주세요.", "Enter a number.") : null;
  const value = Number(text);
  if (!Number.isSafeInteger(value)) return tr("소수점 없는 정수를 넣어 주세요.", "Enter a whole number.");
  const low = item.min ?? -Infinity;
  const high = item.max ?? Infinity;
  if (value >= low && value <= high) return null;
  if (item.min !== undefined && item.max !== undefined) {
    return tr(`${item.min}~${item.max} 사이 숫자를 넣어 주세요.`, `Enter a number from ${item.min} to ${item.max}.`);
  }
  return item.min !== undefined
    ? tr(`${item.min} 이상인 숫자를 넣어 주세요.`, `Enter a number of at least ${item.min}.`)
    : tr("숫자를 넣어 주세요.", "Enter a number.");
}

/** Config wire value: numbers go out as plain decimal text ("1e2" → "100"), the way the server parses u64. */
export function configValue(item: CatalogItem, text: string): string {
  return item.input === "number" && text.trim() !== "" ? String(Number(text)) : text;
}

/** Typed runtime value; a half-typed number stays text so the field can be cleared (draftProblem blocks saving it). */
export function runtimeValue(item: CatalogItem, text: string): RuntimeConfigValue {
  if (item.input === "toggle") return text === "true";
  if (item.input === "number" && text.trim() !== "" && Number.isFinite(Number(text))) return Number(text);
  return text;
}

/** Appends 으로/로 the way the value reads aloud: "3으로", "10으로", "1로", "켜짐으로". */
export function withRo(word: string): string {
  const last = word.trim().slice(-1);
  if (/\d/.test(last)) return `${word}${"1245789".includes(last) ? "로" : "으로"}`;
  const code = last.charCodeAt(0) - 0xac00;
  if (code < 0 || code >= 11172) return `${word}(으)로`;
  const final = code % 28;
  return `${word}${final === 0 || final === 8 ? "로" : "으로"}`;
}

export function applyConfigEdits(
  entries: ConfigEntry[],
  edits: Record<string, ConfigEditValue>,
): ConfigEntry[] {
  if (Object.keys(edits).length === 0) return entries;
  return entries.map((entry) => {
    if (!(entry.key in edits)) return entry;
    return { ...entry, value: String(edits[entry.key]), override_active: true };
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
  return isKo ? agent.name_ko || agent.name : agent.name || agent.name_ko;
}
