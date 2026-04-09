import { useI18n } from "../i18n";

export const PROVIDER_META: Record<string, { label: string; bg: string; color: string; border: string }> = {
  claude: {
    label: "Claude",
    bg: "rgba(245,158,11,0.12)",
    color: "#fbbf24",
    border: "rgba(245,158,11,0.25)",
  },
  codex: {
    label: "Codex",
    bg: "rgba(56,189,248,0.12)",
    color: "#7dd3fc",
    border: "rgba(56,189,248,0.24)",
  },
  gemini: {
    label: "Gemini",
    bg: "rgba(59,130,246,0.12)",
    color: "#60a5fa",
    border: "rgba(59,130,246,0.25)",
  },
  qwen: {
    label: "Qwen",
    bg: "rgba(34,197,94,0.12)",
    color: "#86efac",
    border: "rgba(34,197,94,0.25)",
  },
  opencode: {
    label: "OpenCode",
    bg: "rgba(168,85,247,0.12)",
    color: "#c084fc",
    border: "rgba(168,85,247,0.25)",
  },
  copilot: {
    label: "Copilot",
    bg: "rgba(16,185,129,0.12)",
    color: "#6ee7b7",
    border: "rgba(16,185,129,0.25)",
  },
  antigravity: {
    label: "Antigravity",
    bg: "rgba(244,114,182,0.12)",
    color: "#f9a8d4",
    border: "rgba(244,114,182,0.25)",
  },
  api: {
    label: "API",
    bg: "rgba(148,163,184,0.12)",
    color: "#cbd5e1",
    border: "rgba(148,163,184,0.25)",
  },
};

export function getProviderMeta(provider: string | null) {
  if (!provider) {
    return {
      label: "Unknown",
      bg: "rgba(148,163,184,0.1)",
      color: "#cbd5e1",
      border: "rgba(148,163,184,0.18)",
    };
  }
  return PROVIDER_META[provider.toLowerCase()] ?? {
    label: provider.toUpperCase(),
    bg: "rgba(148,163,184,0.1)",
    color: "#cbd5e1",
    border: "rgba(148,163,184,0.18)",
  };
}

export function formatProviderFlow(
  primaryProvider: string | null,
  reviewerProvider: string | null,
): string {
  const primary = getProviderMeta(primaryProvider).label;
  const reviewer = getProviderMeta(reviewerProvider).label;
  return `${primary} -> ${reviewer}`;
}

export function providerFlowCaption(
  primaryProvider: string | null,
  reviewerProvider: string | null,
  t?: (text: { ko: string; en: string }) => string,
): string {
  const primary = getProviderMeta(primaryProvider).label;
  const reviewer = getProviderMeta(reviewerProvider).label;
  if (t) {
    return t({
      ko: `초안/최종: ${primary} · 비판 검토: ${reviewer}`,
      en: `Draft/Final: ${primary} · Critique: ${reviewer}`,
    });
  }
  return `Draft/Final: ${primary} · Critique: ${reviewer}`;
}

export default function MeetingProviderFlow({
  primaryProvider,
  reviewerProvider,
  compact = false,
}: {
  primaryProvider: string | null;
  reviewerProvider: string | null;
  compact?: boolean;
}) {
  const { t } = useI18n();
  if (!primaryProvider && !reviewerProvider) return null;

  const primary = getProviderMeta(primaryProvider);
  const reviewer = getProviderMeta(reviewerProvider);

  return (
    <div className={`flex min-w-0 max-w-full flex-wrap items-center gap-1.5 ${compact ? "" : "rounded-xl px-3 py-2"}`} style={compact ? undefined : {
      background: "rgba(148,163,184,0.08)",
      border: "1px solid rgba(148,163,184,0.14)",
    }}>
      {!compact && (
        <span className="min-w-0 text-xs font-semibold uppercase tracking-widest" style={{ color: "var(--th-text-muted)", overflowWrap: "anywhere" }}>
          Provider Flow
        </span>
      )}
      <ProviderChip label={primary.label} bg={primary.bg} color={primary.color} border={primary.border} />
      <span className="min-w-0 text-xs font-semibold" style={{ color: "var(--th-text-muted)", overflowWrap: "anywhere" }}>
        {t({ ko: "초안/최종", en: "draft/final" })}
      </span>
      <span className="text-xs font-semibold px-1" style={{ color: "var(--th-text-muted)" }}>
        →
      </span>
      <ProviderChip label={reviewer.label} bg={reviewer.bg} color={reviewer.color} border={reviewer.border} />
      <span className="min-w-0 text-xs font-semibold" style={{ color: "var(--th-text-muted)", overflowWrap: "anywhere" }}>
        {t({ ko: "비판 검토", en: "critique" })}
      </span>
    </div>
  );
}

function ProviderChip({
  label,
  bg,
  color,
  border,
}: {
  label: string;
  bg: string;
  color: string;
  border: string;
}) {
  return (
    <span
      className="max-w-full rounded-full px-2 py-0.5 text-xs font-semibold"
      style={{ background: bg, color, border: `1px solid ${border}`, overflowWrap: "anywhere" }}
    >
      {label}
    </span>
  );
}
