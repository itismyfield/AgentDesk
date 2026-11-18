import { ChevronDown, Info } from "lucide-react";
import { useId, type ReactNode } from "react";
import { Button, type ButtonProps } from "../../kit/free/button-clean";
import { TextField, type TextFieldProps } from "../../kit/free/text-field-clean";

// Settings frame pieces laid out like the kit settings screens (section head, card, row group, notice, skeleton),
// drawn with dashboard tokens so they follow the shell theme.

/** Card surface: a hairline edge instead of a border, rows separated by inset hairlines. */
export const SETTINGS_CARD_CLASS =
  "min-w-0 rounded-[var(--r-md)] bg-[var(--th-card-bg)] text-th-text-primary shadow-[0_0_0_0.5px_var(--line),0_1px_2px_rgb(0_0_0/0.06)]";
const CARD_ROWS_CLASS = "[&>*+*]:shadow-[inset_0_0.5px_0_var(--line)]";
/** Quiet tray inside a card (details, pending sign-in, code). */
export const SETTINGS_TRAY_CLASS =
  "rounded-[var(--r-sm)] bg-[var(--bg-1)] shadow-[inset_0_0_0_0.5px_var(--line)]";
/** Padding of one row in a card: 60px tall rows, tighter on narrow panels. */
export const SETTINGS_ROW_PAD_CLASS = "px-3.5 py-3 @min-[37.5rem]:py-2.5 @min-[37.5rem]:pl-4 @min-[37.5rem]:pr-3";

/** Small kit-style action: a secondary button at the compact size (44px on coarse pointers). */
export function SettingsAction({ variant = "secondary", size = "sm", ...props }: ButtonProps) {
  return <Button variant={variant} size={size} {...props} />;
}

// Keyboard focus rings the rounded field box (as the kit settings screens do) instead of the bare input inside it.
const FIELD_FOCUS_CLASS =
  "[&_div:has(>input:focus-visible)]:outline-2 [&_div:has(>input:focus-visible)]:outline-offset-2 [&_div:has(>input:focus-visible)]:outline-solid [&_div:has(>input:focus-visible)]:outline-[color:var(--th-focus-ring,var(--accent))]";

/** Kit text field with the settings focus ring. */
export function SettingsTextField({ className = "", inputClassName = "", ...props }: TextFieldProps) {
  return <TextField {...props} className={`${FIELD_FOCUS_CLASS} ${className}`} inputClassName={`focus-visible:outline-none! ${inputClassName}`} />;
}

export function SettingsSectionHead({ title, description, id, actions }: {
  title: string;
  description?: ReactNode;
  id?: string;
  actions?: ReactNode;
}) {
  return (
    <div className="flex flex-wrap items-end justify-between gap-x-4 gap-y-2">
      <div className="min-w-[12rem] flex-1">
        <h3 id={id} className="text-sm font-medium leading-5 break-keep text-th-text-primary">{title}</h3>
        {description ? <p className="mt-0.5 max-w-[39rem] text-[13px] leading-5 break-keep text-th-text-muted">{description}</p> : null}
      </div>
      {actions ? <div className="flex shrink-0 flex-wrap items-center gap-2">{actions}</div> : null}
    </div>
  );
}

export function SettingsCard({ label, children, className = "", rows = true }: {
  label?: string;
  children: ReactNode;
  className?: string;
  /** Rows get hairline separators; off for content that lays itself out. */
  rows?: boolean;
}) {
  return (
    <div role={label ? "group" : undefined} aria-label={label} className={`${SETTINGS_CARD_CLASS} ${rows ? CARD_ROWS_CLASS : ""} ${className}`}>
      {children}
    </div>
  );
}

interface SettingsSectionProps {
  title: string;
  description?: string;
  actions?: ReactNode;
  children: ReactNode;
  /** "rows": a card of rows · "plain": a padded card · "open": no card, for content that brings its own cards. */
  variant?: "rows" | "plain" | "open";
  testId?: string;
}

/** A section head with its card underneath. */
export function SettingsSection({ title, description, actions, children, variant = "rows", testId }: SettingsSectionProps) {
  return (
    <section className="min-w-0" data-testid={testId}>
      <SettingsSectionHead title={title} description={description} actions={actions} />
      {variant === "open" ? (
        <div className="mt-3 min-w-0">{children}</div>
      ) : (
        <SettingsCard className="mt-3" rows={variant === "rows"}>
          {variant === "plain" ? <div className="min-w-0 px-4 py-4">{children}</div> : children}
        </SettingsCard>
      )}
    </section>
  );
}

interface SettingsDisclosureProps {
  title: string;
  description?: string;
  openLabel: string;
  closeLabel: string;
  open: boolean;
  onToggle: () => void;
  children: ReactNode;
  variant?: "rows" | "plain";
  testId?: string;
}

/** A section whose card stays hidden until asked for, for settings most people never touch. */
export function SettingsDisclosure({
  title, description, openLabel, closeLabel, open, onToggle, children, variant = "rows", testId,
}: SettingsDisclosureProps) {
  const bodyId = useId();
  return (
    <section className="min-w-0" data-testid={testId}>
      <SettingsSectionHead
        title={title}
        description={description}
        actions={(
          <SettingsAction
            aria-expanded={open}
            aria-controls={bodyId}
            onClick={onToggle}
            iconRight={<ChevronDown className={`transition-transform motion-reduce:transition-none ${open ? "rotate-180" : ""}`} />}
          >
            {open ? closeLabel : openLabel}
          </SettingsAction>
        )}
      />
      {open ? (
        <div id={bodyId} className="mt-3">
          <SettingsCard rows={variant === "rows"}>
            {variant === "plain" ? <div className="min-w-0 px-4 py-4">{children}</div> : children}
          </SettingsCard>
        </div>
      ) : null}
    </section>
  );
}

/** Soft notice strip with a glyph, for alerts and empty search results inside a panel. */
export function SettingsNotice({ children, tone = "muted", role, icon, testId }: {
  children: ReactNode;
  tone?: "muted" | "warn";
  role?: "alert" | "status";
  icon?: ReactNode;
  testId?: string;
}) {
  const warn = tone === "warn";
  return (
    <div
      role={role}
      data-testid={testId}
      className={`flex items-start gap-2 rounded-[var(--r-sm)] px-3 py-2.5 text-[13px] leading-5 break-keep ${
        warn
          ? "bg-[var(--th-badge-amber-bg)] text-th-text-primary shadow-[inset_0_0_0_0.5px_color-mix(in_oklch,var(--warn)_45%,transparent)]"
          : "bg-[var(--bg-1)] text-th-text-secondary shadow-[inset_0_0_0_0.5px_var(--line)]"
      }`}
    >
      <span aria-hidden className={`inline-flex h-5 shrink-0 items-center ${warn ? "text-[var(--warn)]" : "text-th-text-muted"}`}>
        {icon ?? <Info size={15} />}
      </span>
      <div className="min-w-0">{children}</div>
    </div>
  );
}

export function SettingsStatusMessage({ children, tone = "muted" }: { children: ReactNode; tone?: "muted" | "error" }) {
  return (
    <p
      role={tone === "error" ? "alert" : "status"}
      className={`px-4 py-4 text-[13px] leading-5 break-keep ${tone === "error" ? "text-th-text-secondary" : "text-th-text-muted"}`}
    >
      {children}
    </p>
  );
}

/** Pill for row state ("Changed", "Read-only") and counts. */
export function SettingsBadge({ children, tone = "neutral", testId, label }: {
  children: ReactNode;
  tone?: "neutral" | "accent" | "warn" | "ok";
  testId?: string;
  label?: string;
}) {
  const toneClass = {
    neutral: "bg-[var(--bg-1)] text-th-text-muted shadow-[inset_0_0_0_0.5px_var(--line)]",
    accent: "bg-[var(--accent-soft)] text-[var(--accent)]",
    warn: "bg-[var(--th-badge-amber-bg)] text-[var(--th-badge-amber-text)]",
    ok: "bg-[var(--th-badge-emerald-bg)] text-[var(--th-badge-emerald-text)]",
  }[tone];
  return (
    <span
      className={`inline-flex shrink-0 items-center whitespace-nowrap rounded-full px-1.5 py-px text-[11px] font-medium leading-4 tabular-nums ${toneClass}`}
      data-testid={testId}
      aria-label={label}
    >
      {children}
    </span>
  );
}

/** Loading slots shaped like the rows they stand in for; one status line announces the wait. */
export function SettingsSkeletonRows({ label, count = 2 }: { label: string; count?: number }) {
  return (
    <div role="status" aria-label={label} className="min-w-0">
      <span className="sr-only">{label}</span>
      {Array.from({ length: count }, (_, index) => (
        <div
          key={index}
          aria-hidden
          className={`flex min-h-[60px] flex-col gap-2.5 @min-[37.5rem]:flex-row @min-[37.5rem]:items-center @min-[37.5rem]:justify-between @min-[37.5rem]:gap-6 ${SETTINGS_ROW_PAD_CLASS} ${index ? "shadow-[inset_0_0.5px_0_var(--line)]" : ""}`}
        >
          <span className="grid min-w-0 flex-1 gap-1.5">
            <span className="block h-2.5 w-2/5 rounded-full bg-[var(--bg-3)] animate-pulse motion-reduce:animate-none" />
            <span className="block h-2 w-3/5 rounded-full bg-[var(--bg-3)] opacity-70 animate-pulse motion-reduce:animate-none" />
          </span>
          <span className="block h-8 w-full shrink-0 rounded-[var(--r-sm)] bg-[var(--bg-3)] animate-pulse motion-reduce:animate-none @min-[37.5rem]:w-60 pointer-coarse:h-11" />
        </div>
      ))}
    </div>
  );
}
