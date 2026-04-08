import type { ButtonHTMLAttributes, CSSProperties, HTMLAttributes, ReactNode } from "react";

function joinClasses(...classes: Array<string | undefined | false>): string {
  return classes.filter(Boolean).join(" ");
}

export type SurfaceTone =
  | "neutral"
  | "accent"
  | "info"
  | "success"
  | "warn"
  | "danger";

function getToneChrome(tone: SurfaceTone): {
  borderColor: string;
  background: string;
  accent: string;
} {
  switch (tone) {
    case "accent":
      return {
        borderColor: "color-mix(in srgb, var(--th-accent-primary) 28%, var(--th-border) 72%)",
        background: "color-mix(in srgb, var(--th-accent-primary-soft) 68%, var(--th-card-bg) 32%)",
        accent: "var(--th-accent-primary)",
      };
    case "info":
      return {
        borderColor: "color-mix(in srgb, var(--th-accent-info) 30%, var(--th-border) 70%)",
        background: "color-mix(in srgb, var(--th-badge-sky-bg) 82%, var(--th-card-bg) 18%)",
        accent: "var(--th-accent-info)",
      };
    case "success":
      return {
        borderColor: "color-mix(in srgb, var(--th-accent-primary) 28%, var(--th-border) 72%)",
        background: "color-mix(in srgb, var(--th-badge-emerald-bg) 82%, var(--th-card-bg) 18%)",
        accent: "var(--th-accent-primary)",
      };
    case "warn":
      return {
        borderColor: "color-mix(in srgb, var(--th-accent-warn) 32%, var(--th-border) 68%)",
        background: "color-mix(in srgb, var(--th-badge-amber-bg) 84%, var(--th-card-bg) 16%)",
        accent: "var(--th-accent-warn)",
      };
    case "danger":
      return {
        borderColor: "color-mix(in srgb, var(--th-accent-danger) 32%, var(--th-border) 68%)",
        background: "color-mix(in srgb, rgba(255, 107, 107, 0.18) 84%, var(--th-card-bg) 16%)",
        accent: "var(--th-accent-danger)",
      };
    case "neutral":
    default:
      return {
        borderColor: "color-mix(in srgb, var(--th-border) 76%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
        accent: "var(--th-text-muted)",
      };
  }
}

const defaultSectionStyle: CSSProperties = {
  border: "1px solid color-mix(in srgb, var(--th-border) 72%, transparent)",
  background:
    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
};

const defaultCardStyle: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
  background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
};

interface SurfaceSectionProps {
  eyebrow?: string;
  title: string;
  description?: string;
  badge?: string;
  actions?: ReactNode;
  children?: ReactNode;
  className?: string;
  style?: CSSProperties;
}

export function SurfaceSection({
  eyebrow,
  title,
  description,
  badge,
  actions,
  children,
  className,
  style,
}: SurfaceSectionProps) {
  return (
    <section
      className={joinClasses("rounded-[28px] border p-5 sm:p-6", className)}
      style={{ ...defaultSectionStyle, ...style }}
    >
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div className="min-w-0">
          {eyebrow && (
            <div
              className="text-[11px] font-semibold uppercase tracking-[0.18em]"
              style={{ color: "var(--th-text-muted)" }}
            >
              {eyebrow}
            </div>
          )}
          <h3 className={joinClasses(eyebrow ? "mt-1" : undefined, "text-xl font-semibold tracking-tight")} style={{ color: "var(--th-text)" }}>
            {title}
          </h3>
          {description && (
            <p className="mt-2 max-w-3xl text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
              {description}
            </p>
          )}
        </div>
        {(badge || actions) && (
          <div className="flex shrink-0 flex-wrap items-center gap-2">
            {badge && (
              <span
                className="inline-flex items-center rounded-full border px-3 py-1 text-[11px] font-medium"
                style={{
                  borderColor: "color-mix(in srgb, var(--th-accent-primary) 30%, var(--th-border) 70%)",
                  background: "var(--th-accent-primary-soft)",
                  color: "var(--th-text-primary)",
                }}
              >
                {badge}
              </span>
            )}
            {actions}
          </div>
        )}
      </div>
      {children}
    </section>
  );
}

interface SurfaceCardProps extends HTMLAttributes<HTMLDivElement> {
  children: ReactNode;
  className?: string;
  style?: CSSProperties;
}

export function SurfaceCard({ children, className, style, ...rest }: SurfaceCardProps) {
  return (
    <div
      {...rest}
      className={joinClasses("rounded-2xl border p-4", className)}
      style={{ ...defaultCardStyle, ...style }}
    >
      {children}
    </div>
  );
}

interface SurfaceSubsectionProps {
  title: string;
  description?: string;
  actions?: ReactNode;
  children: ReactNode;
  className?: string;
  style?: CSSProperties;
}

export function SurfaceSubsection({
  title,
  description,
  actions,
  children,
  className,
  style,
}: SurfaceSubsectionProps) {
  return (
    <SurfaceCard
      className={joinClasses("min-w-0 w-full rounded-3xl p-4 sm:p-5", className)}
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 62%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
        ...style,
      }}
    >
      <div className="mb-4 flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0 flex-1">
          <h4 className="text-base font-medium" style={{ color: "var(--th-text)" }}>
            {title}
          </h4>
          {description && (
            <p className="mt-1 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
              {description}
            </p>
          )}
        </div>
        {actions && <div className="flex shrink-0 flex-wrap items-center gap-2">{actions}</div>}
      </div>
      {children}
    </SurfaceCard>
  );
}

interface SurfaceFieldCardProps {
  label: string;
  description: string;
  children: ReactNode;
  className?: string;
  style?: CSSProperties;
}

export function SurfaceFieldCard({
  label,
  description,
  children,
  className,
  style,
}: SurfaceFieldCardProps) {
  return (
    <SurfaceCard
      className={className}
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
        ...style,
      }}
    >
      <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
        {label}
      </div>
      <p className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
        {description}
      </p>
      <div className="mt-3">{children}</div>
    </SurfaceCard>
  );
}

interface SurfaceCalloutProps {
  children: ReactNode;
  action?: ReactNode;
  className?: string;
  style?: CSSProperties;
}

export function SurfaceCallout({ children, action, className, style }: SurfaceCalloutProps) {
  return (
    <SurfaceCard
      className={joinClasses("flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between", className)}
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
        ...style,
      }}
    >
      <div className="min-w-0">{children}</div>
      {action}
    </SurfaceCard>
  );
}

interface SurfaceEmptyStateProps {
  children: ReactNode;
  className?: string;
  style?: CSSProperties;
}

export function SurfaceEmptyState({ children, className, style }: SurfaceEmptyStateProps) {
  return (
    <SurfaceCard
      className={className}
      style={{
        borderStyle: "dashed",
        borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
        color: "var(--th-text-muted)",
        ...style,
      }}
    >
      {children}
    </SurfaceCard>
  );
}

interface SurfaceNoticeProps {
  children: ReactNode;
  tone?: SurfaceTone;
  action?: ReactNode;
  className?: string;
  style?: CSSProperties;
  leading?: ReactNode;
  compact?: boolean;
}

export function SurfaceNotice({
  children,
  tone = "neutral",
  action,
  className,
  style,
  leading,
  compact = false,
}: SurfaceNoticeProps) {
  const chrome = getToneChrome(tone);

  return (
    <div
      className={joinClasses(
        "flex items-start gap-3 rounded-2xl border",
        compact ? "px-3 py-2 text-xs" : "px-3 py-3 text-sm",
        className,
      )}
      style={{
        borderColor: chrome.borderColor,
        background: chrome.background,
        color: "var(--th-text)",
        ...style,
      }}
    >
      {leading ?? (
        <span
          className={joinClasses("shrink-0 rounded-full", compact ? "mt-1 h-2 w-2" : "mt-1 h-2.5 w-2.5")}
          style={{ background: chrome.accent }}
        />
      )}
      <div className="min-w-0 flex-1">{children}</div>
      {action}
    </div>
  );
}

interface SurfaceMetricPillProps {
  label: string;
  value: ReactNode;
  tone?: SurfaceTone;
  className?: string;
  style?: CSSProperties;
}

export function SurfaceMetricPill({
  label,
  value,
  tone = "neutral",
  className,
  style,
}: SurfaceMetricPillProps) {
  const chrome = getToneChrome(tone);

  return (
    <div
      className={joinClasses("inline-flex min-w-[148px] flex-col rounded-2xl border px-3 py-2", className)}
      style={{
        borderColor: chrome.borderColor,
        background: chrome.background,
        ...style,
      }}
    >
      <span className="text-[10px] font-semibold uppercase tracking-[0.14em]" style={{ color: chrome.accent }}>
        {label}
      </span>
      <span className="mt-1 text-xs font-medium leading-relaxed" style={{ color: "var(--th-text)" }}>
        {value}
      </span>
    </div>
  );
}

interface SurfaceTabCardProps {
  title: string;
  description: string;
  count?: ReactNode;
  active?: boolean;
  tone?: SurfaceTone;
  onClick?: () => void;
  className?: string;
  style?: CSSProperties;
}

export function SurfaceTabCard({
  title,
  description,
  count,
  active = false,
  tone = "accent",
  onClick,
  className,
  style,
}: SurfaceTabCardProps) {
  const chrome = getToneChrome(tone);

  return (
    <button
      type="button"
      onClick={onClick}
      className={joinClasses(
        "min-w-[180px] rounded-2xl border px-4 py-3 text-left transition-colors",
        className,
      )}
      style={{
        borderColor: active ? chrome.borderColor : defaultCardStyle.borderColor,
        background: active ? chrome.background : defaultCardStyle.background,
        ...style,
      }}
    >
      <div className="flex items-center justify-between gap-2">
        <div
          className="text-sm font-semibold"
          style={{ color: active ? chrome.accent : "var(--th-text-heading)" }}
        >
          {title}
        </div>
        {count !== undefined && (
          <span
            className="rounded-full px-2 py-0.5 text-[10px]"
            style={{
              background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
              color: "var(--th-text-muted)",
            }}
          >
            {count}
          </span>
        )}
      </div>
      <div className="mt-1 text-xs leading-relaxed" style={{ color: "var(--th-text-muted)" }}>
        {description}
      </div>
    </button>
  );
}

interface SurfaceSegmentButtonProps {
  children: ReactNode;
  active?: boolean;
  tone?: SurfaceTone;
  onClick?: () => void;
  className?: string;
  style?: CSSProperties;
}

export function SurfaceSegmentButton({
  children,
  active = false,
  tone = "accent",
  onClick,
  className,
  style,
}: SurfaceSegmentButtonProps) {
  const chrome = getToneChrome(tone);

  return (
    <button
      type="button"
      onClick={onClick}
      className={joinClasses("rounded-full px-3 py-1.5 text-xs font-medium transition-colors", className)}
      style={{
        border: `1px solid ${active ? chrome.borderColor : "color-mix(in srgb, var(--th-border) 72%, transparent)"}`,
        background: active
          ? chrome.background
          : "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)",
        color: active ? chrome.accent : "var(--th-text-muted)",
        ...style,
      }}
    >
      {children}
    </button>
  );
}

interface SurfaceActionButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  children: ReactNode;
  tone?: SurfaceTone;
  compact?: boolean;
  type?: "button" | "submit" | "reset";
  disabled?: boolean;
  onClick?: () => void;
  className?: string;
  style?: CSSProperties;
}

export function SurfaceActionButton({
  children,
  tone = "accent",
  compact = false,
  type = "button",
  disabled = false,
  onClick,
  className,
  style,
  ...rest
}: SurfaceActionButtonProps) {
  const chrome = getToneChrome(tone);

  return (
    <button
      {...rest}
      type={type}
      disabled={disabled}
      onClick={onClick}
      className={joinClasses(
        "inline-flex items-center justify-center rounded-xl border font-medium transition-colors disabled:cursor-not-allowed",
        compact ? "px-2 py-1 text-[10px]" : "px-3 py-2 text-xs",
        className,
      )}
      style={{
        color: tone === "neutral" ? "var(--th-text-muted)" : chrome.accent,
        borderColor: tone === "neutral"
          ? "color-mix(in srgb, var(--th-border) 72%, transparent)"
          : chrome.borderColor,
        background: tone === "neutral"
          ? "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)"
          : chrome.background,
        opacity: disabled ? 0.45 : 1,
        ...style,
      }}
    >
      {children}
    </button>
  );
}

interface SurfaceListItemProps {
  children: ReactNode;
  trailing?: ReactNode;
  tone?: SurfaceTone;
  className?: string;
  style?: CSSProperties;
}

export function SurfaceListItem({
  children,
  trailing,
  tone = "neutral",
  className,
  style,
}: SurfaceListItemProps) {
  const chrome = getToneChrome(tone);

  return (
    <SurfaceCard
      className={joinClasses("p-3", className)}
      style={{
        borderColor: tone === "neutral"
          ? "color-mix(in srgb, var(--th-border) 68%, transparent)"
          : chrome.borderColor,
        background: tone === "neutral"
          ? "color-mix(in srgb, var(--th-card-bg) 90%, transparent)"
          : chrome.background,
        ...style,
      }}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0 flex-1">{children}</div>
        {trailing ? <div className="shrink-0">{trailing}</div> : null}
      </div>
    </SurfaceCard>
  );
}

interface SurfaceMetaBadgeProps {
  children: ReactNode;
  tone?: SurfaceTone;
  className?: string;
  style?: CSSProperties;
}

export function SurfaceMetaBadge({
  children,
  tone = "neutral",
  className,
  style,
}: SurfaceMetaBadgeProps) {
  const chrome = getToneChrome(tone);

  return (
    <span
      className={joinClasses(
        "inline-flex items-center rounded-full border px-2 py-1 text-[11px] leading-none",
        className,
      )}
      style={{
        borderColor: tone === "neutral"
          ? "color-mix(in srgb, var(--th-border) 70%, transparent)"
          : chrome.borderColor,
        background: tone === "neutral"
          ? "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)"
          : chrome.background,
        color: tone === "neutral" ? "var(--th-text-muted)" : chrome.accent,
        ...style,
      }}
    >
      {children}
    </span>
  );
}

export {
  SurfaceCallout as SettingsCallout,
  SurfaceCard as SettingsCard,
  SurfaceEmptyState as SettingsEmptyState,
  SurfaceFieldCard as SettingsFieldCard,
  SurfaceSection as SettingsSection,
  SurfaceSubsection as SettingsSubsection,
};
