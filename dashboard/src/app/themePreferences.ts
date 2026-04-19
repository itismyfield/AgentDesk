export type ThemePreference = "auto" | "dark" | "light";
export type AccentPreset =
  | "indigo"
  | "violet"
  | "amber"
  | "rose"
  | "cyan"
  | "lime";

export const THEME_STORAGE_KEY = "agentdesk.theme";
export const ACCENT_STORAGE_KEY = "agentdesk.accent";
export const DEFAULT_ACCENT_PRESET: AccentPreset = "indigo";

const THEME_PREFERENCES = new Set<ThemePreference>(["auto", "dark", "light"]);
const ACCENT_PRESETS = new Set<AccentPreset>([
  "indigo",
  "violet",
  "amber",
  "rose",
  "cyan",
  "lime",
]);

export function isThemePreference(value: string | null): value is ThemePreference {
  return value !== null && THEME_PREFERENCES.has(value as ThemePreference);
}

export function isAccentPreset(value: string | null): value is AccentPreset {
  return value !== null && ACCENT_PRESETS.has(value as AccentPreset);
}

export function readStoredThemePreference(
  storage: Storage | null | undefined,
  fallback: ThemePreference,
): ThemePreference {
  const stored = storage?.getItem(THEME_STORAGE_KEY) ?? null;
  return isThemePreference(stored) ? stored : fallback;
}

export function readStoredAccentPreset(
  storage: Storage | null | undefined,
  fallback: AccentPreset = DEFAULT_ACCENT_PRESET,
): AccentPreset {
  const stored = storage?.getItem(ACCENT_STORAGE_KEY) ?? null;
  return isAccentPreset(stored) ? stored : fallback;
}

export function resolveThemePreference(
  preference: ThemePreference,
  prefersDarkScheme: boolean,
): "dark" | "light" {
  if (preference === "auto") {
    return prefersDarkScheme ? "dark" : "light";
  }
  return preference;
}

export function persistThemePreference(
  storage: Storage | null | undefined,
  preference: ThemePreference,
): void {
  storage?.setItem(THEME_STORAGE_KEY, preference);
}

export function persistAccentPreset(
  storage: Storage | null | undefined,
  accent: AccentPreset,
): void {
  storage?.setItem(ACCENT_STORAGE_KEY, accent);
}

export function readThemePreferenceFromPatch(
  patch: Record<string, unknown>,
): ThemePreference | null {
  const value = patch.theme;
  return typeof value === "string" && isThemePreference(value) ? value : null;
}

export function applyThemeAccentDataset(
  element: HTMLElement,
  theme: "dark" | "light",
  accent: AccentPreset,
): void {
  element.dataset.theme = theme;
  element.dataset.accent = accent;
}
