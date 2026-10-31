import type { Agent } from "../../types";
import type { GitHubRepoOption } from "../../api";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import { readLocalStorageValue } from "../../lib/useLocalStorage";
import { isDangerousConfigKey } from "./settingsDangerousConfig";
import { configKeyCopy, restartBehaviorNote } from "./SettingsCopy";
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

export const SETTINGS_PANELS = ["general", "workflow", "ai", "connections", "machine"] as const;
export type SettingsPanel = (typeof SETTINGS_PANELS)[number];
export type SettingsNotificationType = "info" | "success" | "warning" | "error";

// Panel ids from the previous 8-tab layout still arrive through bookmarks, in-app links and localStorage.
const LEGACY_PANEL_ALIASES: Record<string, SettingsPanel> = {
  pipeline: "workflow",
  runtime: "workflow",
  providers: "ai",
  connectors: "connections",
  onboarding: "connections",
  voice: "general",
};

export type SettingFlag = "alert" | "read_only" | "restart_required";

export type ValidationState =
  | { ok: true }
  | { ok: false; messageKo: string; messageEn: string };

/** One editable or read-only row on the settings page. */
export interface SettingRowMeta {
  key: string;
  panel: SettingsPanel;
  section: string;
  editable: boolean;
  restartRequired: boolean;
  defaultValue?: unknown;
  effectiveValue: unknown;
  changed?: boolean;
  resettable?: boolean;
  validation?: ValidationState;
  flags: SettingFlag[];
  labelKo: string;
  labelEn: string;
  hintKo?: string;
  hintEn?: string;
  inputKind: "text" | "number" | "range" | "toggle" | "select" | "readonly";
  selectOptions?: Array<{ value: string; labelKo: string; labelEn: string }>;
  valueUnit?: string;
  numericRange?: { min: number; max: number; step: number };
  applyNoteKo?: string;
  applyNoteEn?: string;
}

export const SETTINGS_PANEL_QUERY_KEY = "settingsPanel";
export const DEFAULT_SETTINGS_PANEL: SettingsPanel = "general";
export const BOOLEAN_CONFIG_KEYS = new Set(["review_enabled"]);

export const NUMERIC_CONFIG_KEYS = new Set([
  "max_review_rounds",
  "requested_timeout_min",
  "in_progress_stale_min",
  "max_chain_depth",
  "context_compact_percent",
  "context_compact_percent_codex",
  "context_compact_percent_claude",
  "context_compact_lower_bound_tokens",
  "server_port",
]);

export const READ_ONLY_CONFIG_KEYS = new Set(["server_port"]);
export const GENERAL_FIELD_LIMITS = {
  companyName: 80,
  ceoName: 60,
} as const;

export function isSettingsPanel(value: string | null): value is SettingsPanel {
  return value !== null && (SETTINGS_PANELS as readonly string[]).includes(value);
}

export function resolveSettingsPanel(value: string | null | undefined): SettingsPanel | null {
  if (!value) return null;
  if (isSettingsPanel(value)) return value;
  return LEGACY_PANEL_ALIASES[value] ?? null;
}

export function readSettingsPanelFromUrl(): SettingsPanel | null {
  if (typeof window === "undefined") return null;
  return resolveSettingsPanel(new URLSearchParams(window.location.search).get(SETTINGS_PANEL_QUERY_KEY));
}

export function readStoredSettingsPanel(): SettingsPanel {
  const panelFromUrl = readSettingsPanelFromUrl();
  if (panelFromUrl) return panelFromUrl;
  return readLocalStorageValue<SettingsPanel>(STORAGE_KEYS.settingsPanel, DEFAULT_SETTINGS_PANEL, {
    validate: (value): value is SettingsPanel => typeof value === "string" && isSettingsPanel(value),
    legacy: (raw) => resolveSettingsPanel(raw),
  });
}

export function isReadOnlyConfigKey(key: string): boolean {
  return READ_ONLY_CONFIG_KEYS.has(key);
}

/** Where a /api/settings/config entry lives in the 5-tab layout; agent-quality keys stay hidden. */
export function configEntryPlacement(entry: ConfigEntry): { panel: SettingsPanel; section: string } | null {
  if (entry.category === "quality") return null;
  if (entry.key === "server_port") return { panel: "machine", section: "server" };
  if (entry.key.startsWith("context_")) return { panel: "ai", section: "context" };
  if (entry.key.endsWith("_channel_id")) return { panel: "connections", section: "channels" };
  if (entry.category === "timeout") return { panel: "workflow", section: "timeouts" };
  if (entry.category === "review") return { panel: "workflow", section: "review" };
  return { panel: "workflow", section: "other" };
}

export function metaFromConfigEntry(
  entry: ConfigEntry,
  edits: Record<string, ConfigEditValue>,
  placement: { panel: SettingsPanel; section: string },
): SettingRowMeta {
  const hasEdit = Object.prototype.hasOwnProperty.call(edits, entry.key);
  const effective = hasEdit ? edits[entry.key] : (entry.value ?? entry.default ?? "");
  const readOnly = READ_ONLY_CONFIG_KEYS.has(entry.key) || entry.editable === false;
  const restartRequired =
    entry.restart_behavior === "reseed-from-yaml" ||
    entry.restart_behavior === "reset-to-baseline" ||
    entry.restart_behavior === "config-only";

  const flags: SettingFlag[] = [];
  if (!readOnly && isDangerousConfigKey(entry.key)) flags.push("alert");
  if (readOnly) flags.push("read_only");
  if (restartRequired && !readOnly) flags.push("restart_required");

  const copy = configKeyCopy(entry.key);
  let inputKind: SettingRowMeta["inputKind"] = "text";
  if (readOnly) inputKind = "readonly";
  else if (BOOLEAN_CONFIG_KEYS.has(entry.key)) inputKind = "toggle";
  else if (NUMERIC_CONFIG_KEYS.has(entry.key)) inputKind = "number";

  return {
    key: entry.key,
    panel: placement.panel,
    section: placement.section,
    editable: !readOnly,
    restartRequired,
    defaultValue: entry.default ?? null,
    effectiveValue: effective,
    changed: hasEdit,
    flags,
    labelKo: copy?.labelKo ?? entry.label_ko ?? entry.key,
    labelEn: copy?.labelEn ?? entry.label_en ?? entry.key,
    hintKo: copy?.hintKo,
    hintEn: copy?.hintEn,
    inputKind,
    applyNoteKo: restartBehaviorNote(entry.restart_behavior, true) ?? undefined,
    applyNoteEn: restartBehaviorNote(entry.restart_behavior, false) ?? undefined,
  };
}

export function applyConfigEdits(
  entries: ConfigEntry[],
  edits: Record<string, ConfigEditValue>,
): ConfigEntry[] {
  if (Object.keys(edits).length === 0) return entries;
  return entries.map((entry) => {
    if (!Object.prototype.hasOwnProperty.call(edits, entry.key)) return entry;
    const value = edits[entry.key];
    return { ...entry, value: typeof value === "boolean" ? String(value) : value, override_active: true };
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
