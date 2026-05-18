import type { CSSProperties, ReactNode } from "react";
import {
  SYSTEM_HEALTH_TONES,
  type StatusTone,
  type SystemHealthTone,
  getSystemHealthTone,
} from "../../theme/statusTokens";

export type BadgeSize = "xs" | "sm" | "md";

interface StatusBadgeProps {
  /** Either a named system-health tone or a custom StatusTone object. */
  tone: SystemHealthTone | StatusTone;
  /** Optional small leading icon (lucide icon, dot, emoji, etc.). */
  icon?: ReactNode;
  /** Visual size. Defaults to "sm". */
  size?: BadgeSize;
  /** Pulse animation for "live/active" semantics. */
  pulse?: boolean;
  /** Title attribute for tooltip-on-hover (a11y). */
  title?: string;
  className?: string;
  style?: CSSProperties;
  children: ReactNode;
}

const SIZE_TOKENS: Record<BadgeSize, { padding: string; font: string; gap: string; radius: string }> = {
  xs: { padding: "1px 6px", font: "10px", gap: "4px", radius: "999px" },
  sm: { padding: "2px 8px", font: "11px", gap: "5px", radius: "999px" },
  md: { padding: "4px 10px", font: "12px", gap: "6px", radius: "999px" },
};

function resolveTone(tone: SystemHealthTone | StatusTone): StatusTone {
  if (typeof tone === "string") return getSystemHealthTone(tone);
  return tone;
}

/**
 * Generic, themable status badge.
 *
 * Use this whenever you need a pill that communicates state. Prefer the named
 * system-health tones ("healthy" | "warning" | "critical" | "idle" | "info" |
 * "unknown") so the dashboard reads as one visual language. Pass a custom
 * StatusTone object only when wrapping a domain-specific palette
 * (e.g. KANBAN_STATUS_TONES, QUEUE_ENTRY_STATUS_TONES).
 */
export function StatusBadge({
  tone,
  icon,
  size = "sm",
  pulse = false,
  title,
  className,
  style,
  children,
}: StatusBadgeProps) {
  const t = resolveTone(tone);
  const s = SIZE_TOKENS[size];
  return (
    <span
      role="status"
      title={title}
      className={className}
      data-pulse={pulse || undefined}
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: s.gap,
        padding: s.padding,
        fontSize: s.font,
        fontWeight: 600,
        lineHeight: 1.2,
        borderRadius: s.radius,
        background: t.bg,
        color: t.text,
        border: `1px solid ${t.accent}33`,
        whiteSpace: "nowrap",
        ...style,
      }}
    >
      {icon ? (
        <span aria-hidden style={{ display: "inline-flex", alignItems: "center" }}>
          {icon}
        </span>
      ) : null}
      <span>{children}</span>
      {pulse ? (
        <span
          aria-hidden
          style={{
            width: 6,
            height: 6,
            borderRadius: "50%",
            background: t.accent,
            boxShadow: `0 0 0 0 ${t.accent}66`,
            animation: "adkStatusPulse 1.6s ease-out infinite",
          }}
        />
      ) : null}
    </span>
  );
}

/** Single source of truth so consumers can reuse the same tone in non-Badge UI. */
export const STATUS_BADGE_TONES = SYSTEM_HEALTH_TONES;
