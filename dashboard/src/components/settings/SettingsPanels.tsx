import { ChevronDown } from "lucide-react";
import { useState, type ReactNode } from "react";
import type { SettingFlag, SettingRowMeta } from "./SettingsModel";
import { SurfaceCard as SettingsCard } from "../common/SurfacePrimitives";

function flagTone(flag: SettingFlag): { bg: string; fg: string; border: string } {
  return flag === "alert" || flag === "restart_required"
    ? { bg: "var(--th-badge-amber-bg)", fg: "var(--th-badge-amber-text)", border: "var(--th-border-subtle)" }
    : { bg: "var(--th-overlay-subtle)", fg: "var(--th-text-muted)", border: "var(--th-border-subtle)" };
}

function flagLabel(flag: SettingFlag, isKo: boolean): string {
  const labels = {
    kv_meta: ["저장됨", "Stored"], live_override: ["즉시 반영", "Live override"],
    alert: ["주의", "Attention"], read_only: ["읽기 전용", "Read-only"], restart_required: ["재시작 필요", "Restart required"],
  };
  return labels[flag][isKo ? 0 : 1];
}

interface SettingRowProps {
  meta: SettingRowMeta;
  isKo: boolean;
  onChange?: (key: string, value: string | boolean | number) => void;
  renderControl?: (meta: SettingRowMeta) => ReactNode;
  controlOverlay?: ReactNode;
  trailingMeta?: ReactNode;
}

export function SettingRow({
  meta,
  isKo,
  onChange,
  renderControl,
  controlOverlay,
  trailingMeta,
}: SettingRowProps) {
  const [open, setOpen] = useState(false);
  const tr = (ko: string, en: string) => (isKo ? ko : en);

  const labelText = isKo ? meta.labelKo ?? meta.key : meta.labelEn ?? meta.key;
  const hintText = isKo ? meta.hintKo : meta.hintEn;
  const readOnly = !meta.editable;
  const visibleFlags = meta.flags.filter((flag) => flag !== "kv_meta");

  const renderDefaultControl = () => {
    if (renderControl) return renderControl(meta);
    if (readOnly) {
      return (
        <div
          className="w-full rounded-md px-3 py-2 text-xs leading-5 [overflow-wrap:anywhere]"
          style={{
            background: "color-mix(in srgb, var(--th-bg-surface) 60%, transparent)",
            border: "1px dashed color-mix(in srgb, var(--th-border) 70%, transparent)",
            color: "var(--th-text-muted)",
          }}
        >
          {String(meta.effectiveValue ?? "")}
        </div>
      );
    }
    if (meta.inputKind === "toggle") {
      const enabled = Boolean(
        meta.effectiveValue === true ||
          meta.effectiveValue === "true" ||
          meta.effectiveValue === 1 ||
          meta.effectiveValue === "1",
      );
      return (
        <button
          type="button"
          role="switch"
          aria-checked={enabled}
          onClick={() => onChange?.(meta.key, !enabled)}
          aria-label={labelText}
          className="relative inline-flex h-11 w-11 items-center justify-center rounded-md"
        >
          <span className="inline-flex h-6 w-11 items-center rounded-full" style={{ background: enabled ? "var(--th-accent-primary)" : "var(--th-border)" }}>
            <span className="inline-block h-5 w-5 rounded-full bg-white transition-transform" style={{ transform: enabled ? "translateX(1.4rem)" : "translateX(0.15rem)" }} />
          </span>
        </button>
      );
    }
    if (meta.inputKind === "select" && meta.selectOptions) {
      return (
        <select
          aria-label={labelText}
          value={String(meta.effectiveValue ?? "")}
          onChange={(event) => onChange?.(meta.key, event.target.value)}
          className="min-h-11 w-full min-w-0 rounded-md px-3 py-2 text-sm"
          style={{
            background: "var(--th-bg-surface)",
            border: "1px solid color-mix(in srgb, var(--th-border) 70%, transparent)",
            color: "var(--th-text)",
          }}
        >
          {meta.selectOptions.map((opt) => (
            <option key={opt.value} value={opt.value}>
              {isKo ? opt.labelKo : opt.labelEn}
            </option>
          ))}
        </select>
      );
    }
    return (
      <input
        aria-label={labelText}
        type={meta.inputKind === "number" ? "number" : "text"}
        inputMode={meta.inputKind === "number" ? "numeric" : undefined}
        min={meta.numericRange?.min}
        max={meta.numericRange?.max}
        step={meta.numericRange?.step}
        value={String(meta.effectiveValue ?? "")}
        onChange={(event) =>
          onChange?.(
            meta.key,
            meta.inputKind === "number" ? Number(event.target.value) : event.target.value,
          )
        }
        className="min-h-11 w-full min-w-0 rounded-md px-3 py-2 text-sm"
        style={{
          background: "var(--th-bg-surface)",
          border: "1px solid color-mix(in srgb, var(--th-border) 70%, transparent)",
          color: "var(--th-text)",
        }}
      />
    );
  };

  return (
    <div
      className="setting-row border-b last:border-b-0"
      style={{ borderColor: "color-mix(in srgb, var(--th-border) 60%, transparent)" }}
      data-testid={`setting-row-${meta.key}`}
    >
      <div className="grid min-w-0 grid-cols-[minmax(0,1fr)_2.75rem] items-start gap-x-3 gap-y-2 py-4 sm:grid-cols-[minmax(0,1fr)_minmax(12rem,0.9fr)_2.75rem] sm:items-center sm:gap-x-4">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-1.5">
            <span className="text-sm font-medium [overflow-wrap:anywhere]" style={{ color: "var(--th-text)" }}>
              {labelText}
            </span>
            {visibleFlags.map((flag) => {
              const tone = flagTone(flag);
              return (
                <span
                  key={flag}
                  className="inline-flex items-center rounded-full border px-1.5 py-px text-[10px] font-medium uppercase tracking-wide"
                  style={{ background: tone.bg, color: tone.fg, borderColor: tone.border }}
                >
                  {flagLabel(flag, isKo)}
                </span>
              );
            })}
          </div>
          {hintText ? (
            <div className="mt-1 text-xs leading-5 [overflow-wrap:anywhere]" style={{ color: "var(--th-text-muted)" }}>
              {hintText}
            </div>
          ) : null}
        </div>
        <div className="col-start-1 row-start-2 min-w-0 sm:col-start-2 sm:row-start-1">
          {controlOverlay ?? renderDefaultControl()}
        </div>
        <button
          type="button"
          aria-expanded={open}
          aria-label={tr("자세히 보기", "Show details")}
          onClick={() => setOpen((current) => !current)}
          className="col-start-2 row-start-1 grid h-11 w-11 place-items-center rounded-md border sm:col-start-3"
          style={{
            borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
            color: "var(--th-text-muted)",
            background: "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)",
          }}
        >
          <ChevronDown
            size={14}
            style={{
              transform: open ? "rotate(180deg)" : "none",
              transition: "transform 0.2s",
            }}
          />
        </button>
      </div>
      {open ? (
        <div
          className="mb-4 grid min-w-0 gap-2 rounded-md p-3 text-xs [overflow-wrap:anywhere] sm:grid-cols-2 sm:gap-3 sm:p-4"
          style={{
            background: "color-mix(in srgb, var(--th-overlay-medium) 70%, transparent)",
            border: "1px solid color-mix(in srgb, var(--th-border) 60%, transparent)",
            color: "var(--th-text-muted)",
          }}
        >
          <div>
            <span style={{ color: "var(--th-text-muted)" }}>{tr("기본값:", "Default:")} </span>
            <span style={{ color: "var(--th-text)" }}>
              {meta.defaultValue === undefined || meta.defaultValue === null
                ? tr("없음", "—")
                : String(meta.defaultValue)}
            </span>
          </div>
          <div>
            <span style={{ color: "var(--th-text-muted)" }}>{tr("상태:", "Status:")} </span>
            <span style={{ color: "var(--th-text)" }}>
              {meta.editable ? tr("수정 가능", "Editable") : tr("읽기 전용", "Read-only")}
            </span>
          </div>
          <div>
            <span style={{ color: "var(--th-text-muted)" }}>{tr("적용:", "Applies:")} </span>
            <span style={{ color: "var(--th-text)" }}>
              {meta.restartRequired ? tr("저장 후 재시작 필요", "After restart") : tr("저장 후 반영", "After save")}
            </span>
          </div>
          {meta.restartNoteKo || meta.restartNoteEn ? (
            <div className="sm:col-span-2" style={{ color: "var(--th-text-muted)" }}>
              {tr(meta.restartNoteKo ?? "", meta.restartNoteEn ?? "")}
            </div>
          ) : null}
          {meta.validation && meta.validation.ok === false ? (
            <div className="sm:col-span-2" style={{ color: "rgba(252,165,165,0.95)" }}>
              {tr(meta.validation.messageKo, meta.validation.messageEn)}
            </div>
          ) : null}
          {trailingMeta ? <div className="sm:col-span-2">{trailingMeta}</div> : null}
        </div>
      ) : null}
    </div>
  );
}

export function CompactFieldCard({
  label,
  description,
  children,
  footer,
}: {
  label: string;
  description: string;
  children: ReactNode;
  footer?: ReactNode;
}) {
  return (
    <div className="min-w-0 border-b border-th-border-subtle py-4 last:border-b-0">
      <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
        {label}
      </div>
      <p className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
        {description}
      </p>
      <div className="mt-3">{children}</div>
      {footer && (
        <div className="mt-3 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
          {footer}
        </div>
      )}
    </div>
  );
}

export function GroupLabel({ title }: { title: string }) {
  return (
    <div
      className="text-[11px] font-semibold uppercase tracking-[0.18em]"
      style={{ color: "var(--th-text-muted)" }}
    >
      {title}
    </div>
  );
}

export function StorageSurfaceCard({
  title,
  body,
  footer,
}: {
  title: string;
  body: string;
  footer: string;
}) {
  return (
    <SettingsCard
      className="rounded-lg p-4"
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
      }}
    >
      <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
        {title}
      </div>
      <p className="mt-2 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
        {body}
      </p>
      <div className="mt-3 text-[11px] font-medium uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
        {footer}
      </div>
    </SettingsCard>
  );
}
