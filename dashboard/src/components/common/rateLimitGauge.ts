import type { CSSProperties } from "react";

const MIN_VISIBLE_FILL_PCT = 6;

export const RATE_LIMIT_GAUGE_TRACK_STYLE: CSSProperties = {
  background: "color-mix(in oklch, var(--fg-faint) 18%, var(--th-bg-surface) 82%)",
  border: "1px solid color-mix(in oklch, var(--fg-faint) 30%, var(--line) 70%)",
  boxShadow: "inset 0 1px 2px color-mix(in oklch, var(--fg) 12%, transparent)",
};

export function rateLimitFillWidth(utilization: number | null): string {
  if (utilization === null || utilization <= 0) return "0%";
  return `${Math.min(Math.max(utilization, MIN_VISIBLE_FILL_PCT), 100)}%`;
}

export function rateLimitProjectionWidth(
  projectedUtilization: number | null,
  currentUtilization: number | null,
): string {
  if (projectedUtilization === null || projectedUtilization <= 0) return "0%";
  return rateLimitFillWidth(Math.max(projectedUtilization, currentUtilization ?? 0));
}

export function rateLimitFillStyle(
  barColor: string,
  glowColor: string,
  glowPx: number,
): CSSProperties {
  return {
    background: `linear-gradient(90deg, ${barColor}, color-mix(in oklch, ${barColor} 76%, var(--fg) 24%))`,
    boxShadow: `0 0 0 1px color-mix(in oklch, ${barColor} 62%, var(--fg) 38%), 0 0 ${glowPx}px ${glowColor}`,
  };
}

export function rateLimitProjectionStyle(
  barColor: string,
  stripePx: number,
  gapPx: number,
): CSSProperties {
  return {
    opacity: 0.46,
    backgroundColor: `color-mix(in oklch, ${barColor} 12%, transparent)`,
    backgroundImage: `repeating-linear-gradient(90deg, ${barColor} 0 ${stripePx}px, transparent ${stripePx}px ${gapPx}px)`,
  };
}
