import { ChevronDown } from "lucide-react";
import { useId, useState, type CSSProperties, type ReactNode } from "react";
import type { SettingFlag, SettingRowMeta } from "./SettingsModel";

export const SETTINGS_INPUT_STYLE: CSSProperties = {
  background: "var(--th-bg-surface)",
  border: "1px solid color-mix(in srgb, var(--th-border) 70%, transparent)",
  color: "var(--th-text)",
};

export const SETTINGS_PRIMARY_BUTTON =
  "inline-flex min-h-11 shrink-0 items-center justify-center gap-1.5 whitespace-nowrap rounded-md bg-[var(--th-accent-primary)] px-4 py-2 text-sm font-medium text-white transition-opacity disabled:opacity-50";
export const SETTINGS_SECONDARY_BUTTON =
  "inline-flex min-h-11 shrink-0 items-center justify-center gap-1.5 whitespace-nowrap rounded-md border border-th-border px-4 py-2 text-sm font-medium text-th-text-secondary transition-colors hover:bg-surface-subtle hover:text-th-text-primary disabled:opacity-50";
export const SETTINGS_SUBTLE_BUTTON =
  "inline-flex min-h-11 shrink-0 items-center justify-center gap-1 whitespace-nowrap rounded-md px-2.5 text-xs font-medium text-th-text-muted transition-colors hover:bg-surface-subtle hover:text-th-text-primary disabled:opacity-50";

const BADGE_CLASS = "inline-flex shrink-0 items-center whitespace-nowrap rounded-full border px-2 py-px text-[11px] font-medium";

function FlagBadge({ flag, isKo }: { flag: SettingFlag; isKo: boolean }) {
  const labels: Record<SettingFlag, [string, string]> = {
    alert: ["저장 전 확인", "Confirm on save"],
    read_only: ["읽기 전용", "Read-only"],
    restart_required: ["다시 시작 후 적용", "Applies after restart"],
  };
  const warn = flag !== "read_only";
  return (
    <span
      className={BADGE_CLASS}
      style={{
        background: warn ? "var(--th-badge-amber-bg)" : "var(--th-overlay-subtle)",
        color: warn ? "var(--th-badge-amber-text)" : "var(--th-text-muted)",
        borderColor: "var(--th-border-subtle)",
      }}
    >
      {labels[flag][isKo ? 0 : 1]}
    </span>
  );
}

function formatValue(value: unknown, unit: string | undefined, tr: (ko: string, en: string) => string): string {
  if (value === undefined || value === null || value === "") return tr("없음", "None");
  if (value === true || value === "true") return tr("켜짐", "On");
  if (value === false || value === "false") return tr("꺼짐", "Off");
  return unit ? `${String(value)}${unit}` : String(value);
}

interface SettingRowProps {
  meta: SettingRowMeta;
  isKo: boolean;
  onChange: (key: string, value: string | boolean | number) => void;
  onReset?: (key: string) => void;
  /** Extra line under the control, e.g. a resolved channel name. */
  extra?: ReactNode;
}

export function SettingRow({ meta, isKo, onChange, onReset, extra }: SettingRowProps) {
  const [open, setOpen] = useState(false);
  const detailsId = useId();
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const label = isKo ? meta.labelKo : meta.labelEn;
  const hint = isKo ? meta.hintKo : meta.hintEn;
  const invalid = meta.validation && meta.validation.ok === false ? meta.validation : null;
  const applyNote = isKo ? meta.applyNoteKo : meta.applyNoteEn;

  const control = (() => {
    if (!meta.editable) {
      return (
        <div
          className="w-full rounded-md px-3 py-2 text-sm tabular-nums [overflow-wrap:anywhere]"
          style={{
            background: "color-mix(in srgb, var(--th-bg-surface) 60%, transparent)",
            border: "1px dashed color-mix(in srgb, var(--th-border) 70%, transparent)",
            color: "var(--th-text-secondary)",
          }}
        >
          {formatValue(meta.effectiveValue, meta.valueUnit, tr)}
        </div>
      );
    }
    if (meta.inputKind === "toggle") {
      const enabled = meta.effectiveValue === true || meta.effectiveValue === "true" || meta.effectiveValue === 1 || meta.effectiveValue === "1";
      return (
        <button
          type="button"
          role="switch"
          aria-checked={enabled}
          aria-label={label}
          onClick={() => onChange(meta.key, !enabled)}
          className="inline-flex min-h-11 items-center gap-2 rounded-md text-sm text-th-text-secondary"
        >
          <span className="inline-flex h-6 w-11 shrink-0 items-center rounded-full transition-colors" style={{ background: enabled ? "var(--th-accent-primary)" : "var(--th-border)" }}>
            <span className="inline-block h-5 w-5 rounded-full bg-white transition-transform" style={{ transform: enabled ? "translateX(1.4rem)" : "translateX(0.15rem)" }} />
          </span>
          {enabled ? tr("켜짐", "On") : tr("꺼짐", "Off")}
        </button>
      );
    }
    if (meta.inputKind === "select" && meta.selectOptions) {
      return (
        <select
          aria-label={label}
          aria-invalid={invalid ? true : undefined}
          value={String(meta.effectiveValue ?? "")}
          onChange={(event) => onChange(meta.key, event.target.value)}
          className="min-h-11 w-full min-w-0 rounded-md px-3 py-2 text-sm"
          style={SETTINGS_INPUT_STYLE}
        >
          {meta.selectOptions.map((option) => (
            <option key={option.value} value={option.value}>
              {isKo ? option.labelKo : option.labelEn}
            </option>
          ))}
        </select>
      );
    }
    if (meta.inputKind === "range" && meta.numericRange) {
      const { min, max, step } = meta.numericRange;
      const value = Number(meta.effectiveValue) || 0;
      return (
        <div className="flex items-center gap-3">
          <input
            type="range"
            aria-label={label}
            min={min}
            max={max}
            step={step}
            value={value}
            onChange={(event) => onChange(meta.key, Number(event.target.value))}
            className="h-11 min-w-0 flex-1 cursor-pointer"
            style={{ accentColor: "var(--th-accent-primary)" }}
          />
          <input
            type="number"
            aria-label={tr(`${label} 직접 입력`, `${label} value`)}
            min={min}
            max={max}
            step={step}
            value={value}
            onChange={(event) => {
              const next = Number(event.target.value);
              if (Number.isFinite(next) && next >= min && next <= max) onChange(meta.key, next);
            }}
            className="min-h-11 w-20 shrink-0 rounded-md px-2 py-1.5 text-right text-sm tabular-nums"
            style={SETTINGS_INPUT_STYLE}
          />
        </div>
      );
    }
    return (
      <input
        aria-label={label}
        aria-invalid={invalid ? true : undefined}
        type={meta.inputKind === "number" ? "number" : "text"}
        inputMode={meta.inputKind === "number" ? "numeric" : undefined}
        value={String(meta.effectiveValue ?? "")}
        onChange={(event) =>
          onChange(meta.key, meta.inputKind === "number" ? Number(event.target.value) : event.target.value)
        }
        className="min-h-11 w-full min-w-0 rounded-md px-3 py-2 text-sm"
        style={{ ...SETTINGS_INPUT_STYLE, ...(invalid ? { borderColor: "var(--th-accent-danger)" } : null) }}
      />
    );
  })();

  return (
    <div className="setting-row min-w-0 py-4" data-testid={`setting-row-${meta.key}`}>
      <div className="grid min-w-0 grid-cols-[minmax(0,1fr)_auto] items-start gap-x-3 gap-y-3 sm:grid-cols-[minmax(0,1fr)_minmax(13rem,0.9fr)_auto] sm:items-center sm:gap-x-5">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-1.5">
            <span className="text-sm font-medium break-keep text-th-text-primary">{label}</span>
            {meta.changed ? (
              <span
                className={BADGE_CLASS}
                style={{
                  background: "color-mix(in srgb, var(--th-accent-primary) 14%, transparent)",
                  color: "var(--th-accent-primary)",
                  borderColor: "color-mix(in srgb, var(--th-accent-primary) 30%, transparent)",
                }}
              >
                {tr("바뀜", "Changed")}
              </span>
            ) : null}
            {meta.flags.map((flag) => <FlagBadge key={flag} flag={flag} isKo={isKo} />)}
          </div>
          {hint ? <p className="mt-1 text-xs leading-5 break-keep text-th-text-muted">{hint}</p> : null}
        </div>
        <div className="col-span-2 row-start-2 min-w-0 sm:col-span-1 sm:col-start-2 sm:row-start-1">
          {control}
          {invalid ? (
            <p role="alert" className="mt-1.5 text-xs leading-5 break-keep" style={{ color: "var(--th-accent-danger)" }}>
              {tr(invalid.messageKo, invalid.messageEn)}
            </p>
          ) : null}
          {extra}
        </div>
        <button
          type="button"
          aria-expanded={open}
          aria-controls={detailsId}
          onClick={() => setOpen((current) => !current)}
          className={`${SETTINGS_SUBTLE_BUTTON} col-start-2 row-start-1 sm:col-start-3`}
        >
          {tr("자세히", "Details")}
          <ChevronDown size={14} aria-hidden className={`transition-transform ${open ? "rotate-180" : ""}`} />
        </button>
      </div>
      {open ? (
        <div
          id={detailsId}
          className="mt-3 grid min-w-0 gap-2 rounded-md px-3 py-3 text-xs leading-5 sm:grid-cols-2 sm:gap-3 sm:px-4"
          style={{
            background: "color-mix(in srgb, var(--th-overlay-medium) 70%, transparent)",
            border: "1px solid color-mix(in srgb, var(--th-border) 60%, transparent)",
            color: "var(--th-text-muted)",
          }}
        >
          <div>
            {tr("기본값", "Default")}:{" "}
            <span className="tabular-nums text-th-text-primary">{formatValue(meta.defaultValue, meta.valueUnit, tr)}</span>
          </div>
          <div className="break-keep">
            {tr("적용", "Applies")}:{" "}
            <span className="text-th-text-primary">
              {!meta.editable
                ? tr("여기서는 바꿀 수 없어요", "Cannot be changed here")
                : meta.restartRequired
                  ? tr("저장한 뒤 다시 시작해야 적용돼요", "After saving and restarting")
                  : tr("저장하면 적용돼요", "When you save")}
            </span>
          </div>
          {applyNote ? <div className="break-keep sm:col-span-2">{applyNote}</div> : null}
          {meta.resettable && onReset ? (
            <div className="sm:col-span-2">
              <button type="button" onClick={() => onReset(meta.key)} className={`${SETTINGS_SECONDARY_BUTTON} min-h-11 px-3 text-xs`}>
                {tr("기본값으로 되돌리기", "Reset to default")}
              </button>
            </div>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

const SECTION_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
  background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
};

interface SettingsSectionProps {
  title: string;
  description?: string;
  actions?: ReactNode;
  children: ReactNode;
  /** Rows get dividers and side padding; "plain" pads other content; "open" drops the frame for content that brings its own cards. */
  variant?: "rows" | "plain" | "open";
  testId?: string;
}

export function SettingsSection({ title, description, actions, children, variant = "rows", testId }: SettingsSectionProps) {
  if (variant === "open") {
    return (
      <section className="min-w-0" data-testid={testId}>
        <header className="mb-3 flex flex-wrap items-start justify-between gap-3">
          <div className="min-w-[12rem] flex-1">
            <h3 className="text-sm font-semibold break-keep text-th-text-primary">{title}</h3>
            {description ? <p className="mt-1 text-xs leading-5 break-keep text-th-text-muted">{description}</p> : null}
          </div>
          {actions ? <div className="flex shrink-0 flex-wrap items-center gap-2">{actions}</div> : null}
        </header>
        {children}
      </section>
    );
  }
  return (
    <section className="min-w-0 overflow-hidden rounded-lg border" style={SECTION_STYLE} data-testid={testId}>
      <header
        className="flex flex-wrap items-start justify-between gap-3 border-b px-4 py-4 sm:px-5"
        style={{ borderColor: "color-mix(in srgb, var(--th-border) 60%, transparent)" }}
      >
        <div className="min-w-[12rem] flex-1">
          <h3 className="text-sm font-semibold break-keep text-th-text-primary">{title}</h3>
          {description ? <p className="mt-1 text-xs leading-5 break-keep text-th-text-muted">{description}</p> : null}
        </div>
        {actions ? <div className="flex shrink-0 flex-wrap items-center gap-2">{actions}</div> : null}
      </header>
      {variant === "rows" ? (
        <div className="divide-y divide-th-border-subtle px-4 sm:px-5">{children}</div>
      ) : (
        <div className="min-w-0 px-4 py-4 sm:px-5">{children}</div>
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

/** A section whose body stays hidden until asked for, for settings most people never touch. */
export function SettingsDisclosure({
  title, description, openLabel, closeLabel, open, onToggle, children, variant = "rows", testId,
}: SettingsDisclosureProps) {
  const bodyId = useId();
  return (
    <section className="min-w-0 overflow-hidden rounded-lg border" style={SECTION_STYLE} data-testid={testId}>
      <header className="flex flex-wrap items-center justify-between gap-3 px-4 py-4 sm:px-5">
        <div className="min-w-[12rem] flex-1">
          <h3 className="text-sm font-semibold break-keep text-th-text-primary">{title}</h3>
          {description ? <p className="mt-1 text-xs leading-5 break-keep text-th-text-muted">{description}</p> : null}
        </div>
        <button type="button" aria-expanded={open} aria-controls={bodyId} onClick={onToggle} className={SETTINGS_SECONDARY_BUTTON}>
          {open ? closeLabel : openLabel}
          <ChevronDown size={14} aria-hidden className={`transition-transform ${open ? "rotate-180" : ""}`} />
        </button>
      </header>
      {open ? (
        <div
          id={bodyId}
          className={variant === "rows" ? "divide-y divide-th-border-subtle border-t px-4 sm:px-5" : "min-w-0 border-t px-4 py-4 sm:px-5"}
          style={{ borderColor: "color-mix(in srgb, var(--th-border) 60%, transparent)" }}
        >
          {children}
        </div>
      ) : null}
    </section>
  );
}

export function SettingsStatusMessage({ children, tone = "muted" }: { children: ReactNode; tone?: "muted" | "error" }) {
  return (
    <p
      role={tone === "error" ? "alert" : "status"}
      className="py-4 text-sm leading-6 break-keep"
      style={{ color: tone === "error" ? "var(--th-text-secondary)" : "var(--th-text-muted)" }}
    >
      {children}
    </p>
  );
}
