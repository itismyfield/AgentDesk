import { useI18n } from "../i18n";
import { getProviderMeta } from "../app/providerTheme";

export { getProviderMeta };

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
    <div className={`flex min-w-0 max-w-full items-center gap-1.5 overflow-hidden flex-wrap ${compact ? "" : "rounded-xl px-3 py-2"}`} style={compact ? undefined : {
      background: "rgba(148,163,184,0.08)",
      border: "1px solid rgba(148,163,184,0.14)",
    }}>
      {!compact && (
        <span className="text-xs font-semibold uppercase tracking-widest" style={{ color: "var(--th-text-muted)" }}>
          Provider Flow
        </span>
      )}
      <ProviderChip label={primary.label} bg={primary.bg} color={primary.color} border={primary.border} />
      <span className="min-w-0 text-xs font-semibold" style={{ color: "var(--th-text-muted)" }}>
        {t({ ko: "초안/최종", en: "draft/final" })}
      </span>
      <span className="text-xs font-semibold px-1" style={{ color: "var(--th-text-muted)" }}>
        →
      </span>
      <ProviderChip label={reviewer.label} bg={reviewer.bg} color={reviewer.color} border={reviewer.border} />
      <span className="min-w-0 text-xs font-semibold" style={{ color: "var(--th-text-muted)" }}>
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
      className="max-w-full truncate rounded-full px-2 py-0.5 text-xs font-semibold"
      style={{ background: bg, color, border: `1px solid ${border}` }}
    >
      {label}
    </span>
  );
}
