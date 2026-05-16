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
    <article className="card min-h-[128px]">
      <div className="card-body metric min-w-0">
        <div className="flex items-start justify-between gap-3">
          <div
            className="card-title text-[10.5px] font-semibold uppercase tracking-[0.18em]"
            style={{ color: "var(--th-text-muted)", cursor: "help" }}
            data-tip={tip}
            title={tip}
            aria-label={`${title}: ${tip}`}
          >
            <span>{title}</span>
            <Info
              size={11}
              style={{ color: "var(--th-text-muted)", flexShrink: 0 }}
            />
          </div>
          {delta ? (
            <span className={cx("delta", deltaTone ?? "flat")}>{delta}</span>
          ) : null}
        </div>
        <div
          className="metric-value mt-3"
          style={{ ...NUMERIC_STYLE, color: "var(--th-text-heading)" }}
        >
          {value}
        </div>
        <div
          className="metric-sub mt-2 text-xs leading-5"
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
    <div className="card-head">
      <div className="min-w-0">
        <div className="card-title">{title}</div>
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
  color: "var(--ok)",
  borderColor:
    "color-mix(in oklch, var(--ok) 20%, var(--th-border-subtle) 80%)",
};
