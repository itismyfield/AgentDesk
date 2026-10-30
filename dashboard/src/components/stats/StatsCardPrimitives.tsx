import type { CSSProperties, ReactNode } from "react";
import { Info } from "lucide-react";
import { cx } from "../dashboard/ui";
import type { MetricDeltaTone } from "./statsModel";

export const NUMERIC_STYLE: CSSProperties = {
  fontFamily: "var(--font-mono)",
  fontVariantNumeric: "tabular-nums",
  fontFeatureSettings: '"tnum" 1',
};

export function HeadlineMetricCard({
  title,
  value,
  sub,
  tip,
  delta,
  deltaTone,
}: {
  title: string;
  value: string;
  sub: string;
  tip: string;
  delta?: string;
  deltaTone?: MetricDeltaTone;
}) {
  return (
    <article className="min-w-0 overflow-hidden rounded-lg border border-th-border-subtle bg-th-bg-surface min-h-[128px]">
      <div className="px-4 py-4 sm:px-5 flex min-w-0 flex-col">
        <div className="flex items-start justify-between gap-3">
          <details className="min-w-0 flex-1">
            <summary className="flex min-h-11 min-w-11 cursor-pointer list-none items-center gap-2 text-xs font-medium text-th-text-muted focus-visible:outline-2 focus-visible:outline-[var(--th-accent-primary)]">
              <span>{title}</span><Info size={13} className="shrink-0" aria-hidden="true" />
            </summary>
            <p className="mt-1 text-xs leading-5 text-th-text-secondary">{tip}</p>
          </details>
          {delta ? (
            <span className={cx("inline-flex rounded px-1.5 py-1 font-mono text-[11px]", deltaTone === "up" ? "text-[var(--th-badge-emerald-text)]" : deltaTone === "down" ? "text-[var(--th-accent-danger)]" : "text-th-text-muted")}>{delta}</span>
          ) : null}
        </div>
        <div
          className="min-w-0 text-[28px] font-semibold leading-9 tracking-tight tabular-nums [overflow-wrap:anywhere] mt-3"
          style={{ ...NUMERIC_STYLE, color: "var(--th-text-heading)" }}
        >
          {value}
        </div>
        <div
          className="mt-2 text-xs leading-5 text-th-text-muted"
          style={{ color: "var(--th-text-muted)" }}
        >
          {sub}
        </div>
      </div>
    </article>
  );
}

export function CardHead({
  title,
  subtitle,
  actions,
}: {
  title: string;
  subtitle: string;
  actions?: ReactNode;
}) {
  return (
    <div className="flex flex-wrap items-start justify-between gap-3 border-b border-th-border-subtle px-4 py-4 sm:px-5">
      <div className="min-w-0">
        <h2 className="text-base font-semibold text-th-text-heading">{title}</h2>
        <div
          className="mt-1 text-[11px] leading-5"
          style={{ color: "var(--th-text-muted)" }}
        >
          {subtitle}
        </div>
      </div>
      {actions ? (
        <div className="flex shrink-0 flex-wrap gap-2">{actions}</div>
      ) : null}
    </div>
  );
}

export function LegendDot({ color, label }: { color: string; label: string }) {
  return (
    <span className="inline-flex items-center gap-1.5">
      <span className="h-2 w-2 rounded-[2px]" style={{ background: color }} />
      <span>{label}</span>
    </span>
  );
}

export const numericBadgeStyle: CSSProperties = {
  ...NUMERIC_STYLE,
  background: "var(--th-overlay-light)",
  color: "var(--th-text-secondary)",
  borderColor: "var(--th-border-subtle)",
};

export const positiveChipStyle: CSSProperties = {
  ...NUMERIC_STYLE,
  background: "color-mix(in oklch, var(--ok) 10%, transparent)",
  color: "var(--th-badge-emerald-text)",
  borderColor:
    "color-mix(in oklch, var(--ok) 20%, var(--th-border-subtle) 80%)",
};
