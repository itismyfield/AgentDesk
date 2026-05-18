import type { CSSProperties, ReactNode } from "react";
import { AlertTriangle, Loader2, Inbox } from "lucide-react";
import { getSystemHealthTone, type SystemHealthTone } from "../../theme/statusTokens";

export type WidgetStateKind = "loading" | "empty" | "error" | "stale";

interface WidgetStateProps {
  kind: WidgetStateKind;
  /** Headline message. */
  title: string;
  /** Optional one-line elaboration. */
  description?: string;
  /** Optional action (typically a retry button). */
  action?: ReactNode;
  /** Optional override of the kind→tone mapping. */
  tone?: SystemHealthTone;
  /** Optional override of the leading icon. */
  icon?: ReactNode;
  /** Force compact layout (smaller padding/font) for inline use. */
  compact?: boolean;
  className?: string;
  style?: CSSProperties;
}

function defaultToneFor(kind: WidgetStateKind): SystemHealthTone {
  switch (kind) {
    case "loading":
      return "info";
    case "stale":
      return "warning";
    case "error":
      return "critical";
    case "empty":
    default:
      return "idle";
  }
}

function defaultIconFor(kind: WidgetStateKind) {
  switch (kind) {
    case "loading":
      // motion-safe: only animate when prefers-reduced-motion: no-preference.
      return <Loader2 size={18} className="motion-safe:animate-spin" aria-hidden />;
    case "error":
      return <AlertTriangle size={18} aria-hidden />;
    case "stale":
      return <AlertTriangle size={18} aria-hidden />;
    case "empty":
    default:
      return <Inbox size={18} aria-hidden />;
  }
}

/**
 * Consistent loading / empty / error / stale surface for any widget.
 *
 * Why this exists: bespoke "syncing…" / "no data" / colored error block
 * spellings across the dashboard were drifting in size, tone, and message
 * style. Centralizing makes silent failures (blank cards) much less likely
 * and gives every widget the same retry/refresh affordance shape.
 */
export function WidgetState({
  kind,
  title,
  description,
  action,
  tone,
  icon,
  compact = false,
  className,
  style,
}: WidgetStateProps) {
  const resolved = getSystemHealthTone(tone ?? defaultToneFor(kind));
  const padding = compact ? "12px 14px" : "20px 18px";
  const titleSize = compact ? 12 : 13;
  const descriptionSize = compact ? 11 : 12;
  return (
    <div
      role={kind === "error" ? "alert" : "status"}
      aria-live={kind === "error" ? "assertive" : "polite"}
      className={className}
      data-widget-state={kind}
      style={{
        display: "flex",
        flexDirection: "row",
        alignItems: "flex-start",
        gap: 12,
        padding,
        borderRadius: 14,
        border: `1px solid ${resolved.accent}33`,
        background: resolved.bg,
        color: resolved.text,
        ...style,
      }}
    >
      <span
        aria-hidden
        style={{
          display: "inline-flex",
          alignItems: "center",
          justifyContent: "center",
          flexShrink: 0,
          marginTop: 1,
          color: resolved.accent,
        }}
      >
        {icon ?? defaultIconFor(kind)}
      </span>
      <div style={{ display: "flex", flexDirection: "column", gap: 4, minWidth: 0, flex: 1 }}>
        <div style={{ fontSize: titleSize, fontWeight: 600, lineHeight: 1.4 }}>{title}</div>
        {description ? (
          <div style={{ fontSize: descriptionSize, lineHeight: 1.5, opacity: 0.85 }}>
            {description}
          </div>
        ) : null}
        {action ? <div style={{ marginTop: 6 }}>{action}</div> : null}
      </div>
    </div>
  );
}
