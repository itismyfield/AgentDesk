import { ChevronDown, Info } from "lucide-react";
import { useId, type CSSProperties, type ReactNode } from "react";
import { Button, type ButtonProps } from "../../kit/free/button-clean";
import { Switch, type SwitchProps } from "../../kit/free/switch-clean";
import { TextField, type TextFieldProps } from "../../kit/free/text-field-clean";
import { CardNote, Desc, KitCard } from "../common/KitPrimitives";

// Settings v3 wrappers over KitPrimitives and vendored free kit controls.
// Promotion candidates for common/: SettingsButton, SettingsSwitch, SettingsTextField.

const join = (...classes: Array<string | undefined | false>) => classes.filter(Boolean).join(" ");

/** Shared card class kept for cross-slice callers until all v3 slices land. */
export const SETTINGS_CARD_CLASS =
  "min-w-0 rounded-[var(--r-md)] border border-[var(--line-soft)] bg-[var(--bg-2)] text-[var(--fg)]";

/** Shared recessed tray class kept for cross-slice callers until all v3 slices land. */
export const SETTINGS_TRAY_CLASS =
  "rounded-[var(--r-md)] border border-[var(--line-soft)] bg-[var(--bg-1)]";

const BUTTON_TOKEN_VARS = {
  "--aui-button-clean-solid": "var(--accent)",
  "--aui-button-clean-solid-hover": "color-mix(in oklab, var(--accent) 88%, white)",
  "--aui-button-clean-on-solid": "#ffffff",
  "--aui-button-clean-surface": "var(--bg-2)",
  "--aui-button-clean-title": "var(--fg)",
  "--aui-button-clean-ink": "var(--fg)",
  "--aui-button-clean-muted": "var(--fg-dim)",
  "--aui-button-clean-icon-color": "var(--fg-muted)",
  "--aui-button-clean-shadow": "inset 0 0 0 1px var(--line)",
  "--aui-button-clean-shadow-hover": "inset 0 0 0 1px color-mix(in oklab, var(--line) 72%, var(--fg))",
  "--aui-button-clean-focus": "var(--accent)",
} as CSSProperties;

const SWITCH_TOKEN_CLASS = [
  "[--ui-swc-ink:var(--fg)]!",
  "[--ui-swc-subtle:var(--fg-muted)]!",
  "[--ui-swc-off:var(--bg-3)]!",
  "[--ui-swc-off-hover:color-mix(in_oklab,var(--bg-3)_86%,var(--fg))]!",
  "[--ui-swc-on:var(--accent)]!",
  "[--ui-swc-on-hover:color-mix(in_oklab,var(--accent)_88%,white)]!",
  "[--ui-swc-focus:var(--accent)]!",
].join(" ");

export const SELECT_TOKEN_CLASS = [
  "[--ui-selc-surface:var(--bg-2)]!",
  "[--ui-selc-menu:var(--bg-2)]!",
  "[--ui-selc-ink:var(--fg)]!",
  "[--ui-selc-title:var(--fg)]!",
  "[--ui-selc-subtle:var(--fg-muted)]!",
  "[--ui-selc-faint:var(--fg-muted)]!",
  "[--ui-selc-icon:var(--fg-muted)]!",
  "[--ui-selc-glow:color-mix(in_oklch,var(--fg)_6%,transparent)]!",
  "[--ui-selc-shadow:inset_0_0_0_1px_var(--line)]!",
  "[--ui-selc-shadow-hover:inset_0_0_0_1px_color-mix(in_oklab,var(--line)_72%,var(--fg))]!",
  "[--ui-selc-shadow-open:inset_0_0_0_1px_var(--accent)]!",
  "[--ui-selc-focus:var(--accent)]!",
  "[--radius:var(--r-sm)]!",
].join(" ");

const TEXT_FIELD_TOKEN_CLASS = [
  "[--ui-tfc-surface:var(--bg-2)]!",
  "[--ui-tfc-ink:var(--fg)]!",
  "[--ui-tfc-title:var(--fg)]!",
  "[--ui-tfc-subtle:var(--fg-muted)]!",
  "[--ui-tfc-faint:var(--fg-muted)]!",
  "[--ui-tfc-icon:var(--fg-muted)]!",
  "[--ui-tfc-shadow:inset_0_0_0_1px_var(--line)]!",
  "[--ui-tfc-shadow-hover:inset_0_0_0_1px_color-mix(in_oklab,var(--line)_72%,var(--fg))]!",
  "[--ui-tfc-shadow-focus:inset_0_0_0_1px_var(--accent)]!",
  "[--ui-tfc-focus:var(--accent)]!",
  "[--radius:var(--r-sm)]!",
].join(" ");

/** Kit Button mapped to v3 `.btn` (36px / 10px radius) and `.btn-sm` (30px / 8px radius) tokens, 44px on touch. */
export function SettingsButton({ size = "md", className, style, ...props }: ButtonProps) {
  const sizeClass =
    size === "sm"
      ? "[--aui-button-clean-h:30px]! [--aui-button-clean-px:10px]! [--aui-button-clean-radius:8px]! max-[899px]:[--aui-button-clean-h:44px]! pointer-coarse:[--aui-button-clean-h:44px]!"
      : "[--aui-button-clean-h:36px]! [--aui-button-clean-px:14px]! [--aui-button-clean-radius:var(--r-sm)]! max-[899px]:[--aui-button-clean-h:44px]! pointer-coarse:[--aui-button-clean-h:44px]!";
  return (
    <Button
      size={size}
      className={join(sizeClass, className)}
      style={{ ...BUTTON_TOKEN_VARS, ...style }}
      {...props}
    />
  );
}

/** Compact secondary kit action button (`.btn-sm`, 44px on coarse pointers). */
export function SettingsAction({ variant = "secondary", size = "sm", ...props }: ButtonProps) {
  return <SettingsButton variant={variant} size={size} {...props} />;
}

/** Kit Switch mapped to v3 `.toggle` (36x20px, or 48x28px in phone sheets) with `--accent` active fill and white thumb. */
export function SettingsSwitch({
  sheet = false,
  className,
  ...props
}: SwitchProps & { sheet?: boolean }) {
  return (
    <Switch
      {...props}
      className={join(
        SWITCH_TOKEN_CLASS,
        sheet ? "[--h:28px]! [--w:48px]!" : "[--h:20px]! [--w:36px]!",
        "[&_.aui-switch-clean__track]:box-border [&_.aui-switch-clean__track]:border! [&_.aui-switch-clean__track]:border-[var(--line)]!",
        "[&_input:not(:checked)+.aui-switch-clean__track_.aui-switch-clean__thumb]:bg-[var(--fg-muted)]! [&_input:not(:checked)+.aui-switch-clean__track_.aui-switch-clean__thumb]:shadow-none!",
        "[&_input:checked+.aui-switch-clean__track]:border-[var(--accent)]!",
        "[&_input:checked+.aui-switch-clean__track_.aui-switch-clean__thumb]:bg-white! [&_input:checked+.aui-switch-clean__track_.aui-switch-clean__thumb]:shadow-none!",
        className,
      )}
    />
  );
}

// Keyboard focus rings the rounded field box instead of the bare input inside it.
const FIELD_FOCUS_CLASS =
  "[&_div:has(>input:focus-visible)]:outline-2 [&_div:has(>input:focus-visible)]:outline-offset-2 [&_div:has(>input:focus-visible)]:outline-solid [&_div:has(>input:focus-visible)]:outline-[color:var(--th-focus-ring,var(--accent))]";

/** Kit TextField mapped onto v3 `.input` surface tokens (`--bg-2`, `1px solid var(--line)`, 10px radius). */
export function SettingsTextField({ className, inputClassName, ...props }: TextFieldProps) {
  return (
    <TextField
      {...props}
      className={join(FIELD_FOCUS_CLASS, TEXT_FIELD_TOKEN_CLASS, "max-[899px]:[--h:44px]! pointer-coarse:[--h:44px]!", className)}
      inputClassName={join("focus-visible:outline-none!", inputClassName)}
    />
  );
}

export function SettingsSectionHead({
  title,
  description,
  id,
  actions,
}: {
  title: string;
  description?: ReactNode;
  id?: string;
  actions?: ReactNode;
}) {
  return (
    <div className="flex flex-wrap items-end justify-between gap-x-4 gap-y-2">
      <div className="min-w-[12rem] flex-1">
        <h3 id={id} className="m-0 text-sm leading-5 font-semibold break-keep text-[var(--fg)]">{title}</h3>
        {description ? <Desc className="mt-0.5 block max-w-[39rem] break-keep">{description}</Desc> : null}
      </div>
      {actions ? <div className="flex shrink-0 flex-wrap items-center gap-2">{actions}</div> : null}
    </div>
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

/** Collapsible section card used by the deferred pipeline editor block. */
export function SettingsDisclosure({
  title,
  description,
  openLabel,
  closeLabel,
  open,
  onToggle,
  children,
  variant = "rows",
  testId,
}: SettingsDisclosureProps) {
  const bodyId = useId();
  return (
    <section className="min-w-0" data-testid={testId}>
      <SettingsSectionHead
        title={title}
        description={description}
        actions={
          <SettingsAction
            aria-expanded={open}
            aria-controls={bodyId}
            onClick={onToggle}
            iconRight={<ChevronDown className={`transition-transform motion-reduce:transition-none ${open ? "rotate-180" : ""}`} />}
          >
            {open ? closeLabel : openLabel}
          </SettingsAction>
        }
      />
      {open ? (
        <div id={bodyId} className="mt-3">
          <KitCard pad={variant === "plain" ? "md" : "none"} className={variant === "rows" ? "[&>*+*]:border-t [&>*+*]:border-[var(--line-soft)]" : undefined}>
            {children}
          </KitCard>
        </div>
      ) : null}
    </section>
  );
}

/** Soft notice strip with a glyph, for alerts and status notices inside a panel. */
export function SettingsNotice({
  children,
  tone = "muted",
  role,
  icon,
  testId,
}: {
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
      className={join(
        "flex items-start gap-2 rounded-[var(--r-md)] border px-3 py-2.5 text-[13px] leading-5 break-keep",
        warn
          ? "border-[color-mix(in_oklch,var(--warn)_40%,transparent)] bg-[var(--warn-soft)] text-[var(--fg)]"
          : "border-[var(--line-soft)] bg-[var(--bg-1)] text-[var(--fg-dim)]",
      )}
    >
      <span aria-hidden className={join("inline-flex h-5 shrink-0 items-center", warn ? "text-[var(--warn)]" : "text-[var(--fg-muted)]")}>
        {icon ?? <Info size={15} />}
      </span>
      <div className="min-w-0">{children}</div>
    </div>
  );
}

/** Status line inside a card (kept for SettingsPipelineEditorSection). */
export function SettingsStatusMessage({ children, tone = "muted" }: { children: ReactNode; tone?: "muted" | "error" }) {
  return (
    <CardNote className={join("border-t-0", tone === "error" && "text-[var(--fg-dim)]")}>
      <span role={tone === "error" ? "alert" : "status"}>{children}</span>
    </CardNote>
  );
}
