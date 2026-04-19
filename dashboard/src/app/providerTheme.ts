type ProviderLevel = "normal" | "warning" | "danger";

const PROVIDER_LABELS: Record<string, string> = {
  claude: "Claude",
  codex: "Codex",
  gemini: "Gemini",
  qwen: "Qwen",
  opencode: "OpenCode",
  copilot: "Copilot",
  antigravity: "Antigravity",
  api: "API",
};

const PROVIDER_ACCENTS: Record<string, string> = {
  claude: "var(--claude)",
  codex: "var(--codex)",
  gemini: "var(--gemini)",
  qwen: "var(--qwen)",
  opencode: "var(--opencode)",
  copilot: "var(--copilot)",
  antigravity: "var(--antigravity)",
  api: "var(--api)",
};

const PROVIDER_SOFTS: Record<string, string> = {
  claude: "var(--claude-soft)",
  codex: "var(--codex-soft)",
  gemini: "var(--gemini-soft)",
  qwen: "var(--qwen-soft)",
  opencode: "var(--opencode-soft)",
  copilot: "var(--copilot-soft)",
  antigravity: "var(--antigravity-soft)",
  api: "var(--api-soft)",
};

export interface ProviderMeta {
  id: string | null;
  label: string;
  bg: string;
  color: string;
  border: string;
}

function normalizeProviderId(provider: string | null | undefined): string | null {
  if (!provider) return null;
  const normalized = provider.trim().toLowerCase();
  return normalized || null;
}

export function getProviderLabel(provider: string | null | undefined): string {
  const normalized = normalizeProviderId(provider);
  if (!normalized) return "Unknown";
  return PROVIDER_LABELS[normalized] ?? provider!.toUpperCase();
}

export function getProviderAccent(provider: string | null | undefined): string {
  const normalized = normalizeProviderId(provider);
  if (!normalized) return "var(--fg-faint)";
  return PROVIDER_ACCENTS[normalized] ?? "var(--fg-faint)";
}

export function getProviderSoft(provider: string | null | undefined): string {
  const normalized = normalizeProviderId(provider);
  if (!normalized) {
    return "color-mix(in oklch, var(--fg-faint) 18%, var(--bg-2) 82%)";
  }
  return PROVIDER_SOFTS[normalized] ?? "color-mix(in oklch, var(--fg-faint) 18%, var(--bg-2) 82%)";
}

export function getProviderBorder(provider: string | null | undefined): string {
  return `color-mix(in oklch, ${getProviderAccent(provider)} 34%, var(--line) 66%)`;
}

export function getProviderMeta(provider: string | null | undefined): ProviderMeta {
  return {
    id: normalizeProviderId(provider),
    label: getProviderLabel(provider),
    bg: getProviderSoft(provider),
    color: getProviderAccent(provider),
    border: getProviderBorder(provider),
  };
}

export function getProviderSeries(provider: string | null | undefined): string[] {
  const accent = getProviderAccent(provider);
  return [
    accent,
    `color-mix(in oklch, ${accent} 82%, var(--fg) 18%)`,
    `color-mix(in oklch, ${accent} 70%, var(--bg-3) 30%)`,
    `color-mix(in oklch, ${accent} 58%, var(--accent) 42%)`,
  ];
}

export function getProviderLevelColors(
  provider: string | null | undefined,
  level: ProviderLevel,
): { bar: string; text: string; glow: string } {
  const bar =
    level === "danger"
      ? "var(--err)"
      : level === "warning"
        ? "var(--warn)"
        : getProviderAccent(provider);

  return {
    bar,
    text: `color-mix(in oklch, ${bar} 78%, var(--fg) 22%)`,
    glow: `color-mix(in oklch, ${bar} 36%, transparent)`,
  };
}
