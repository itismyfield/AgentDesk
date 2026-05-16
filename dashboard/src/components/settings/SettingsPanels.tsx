import { ChevronDown } from "lucide-react";
import { useState, type ReactNode } from "react";
import type { SettingFlag, SettingRowMeta, SettingSource } from "./SettingsModel";
import { SurfaceCard as SettingsCard } from "../common/SurfacePrimitives";

function flagTone(flag: SettingFlag): { bg: string; fg: string; border: string } {
  switch (flag) {
    case "kv_meta":
      return {
        bg: "color-mix(in srgb, var(--th-overlay-medium) 92%, transparent)",
        fg: "var(--th-text-secondary)",
        border: "color-mix(in srgb, var(--th-border) 70%, transparent)",
      };
    case "live_override":
      return {
        bg: "rgba(56, 189, 248, 0.16)",
        fg: "rgba(186, 230, 253, 0.92)",
        border: "rgba(56, 189, 248, 0.42)",
      };
    case "alert":
      return {
        bg: "rgba(251, 191, 36, 0.16)",
        fg: "rgba(253, 230, 138, 0.92)",
        border: "rgba(251, 191, 36, 0.42)",
      };
    case "read_only":
      return {
        bg: "rgba(148, 163, 184, 0.18)",
        fg: "rgba(226, 232, 240, 0.85)",
        border: "rgba(148, 163, 184, 0.40)",
      };
    case "restart_required":
      return {
        bg: "rgba(244, 114, 182, 0.16)",
        fg: "rgba(251, 207, 232, 0.92)",
        border: "rgba(244, 114, 182, 0.42)",
      };
  }
}

function flagLabel(flag: SettingFlag, isKo: boolean): string {
  if (flag === "kv_meta") return "kv_meta";
  if (flag === "live_override") return isKo ? "live override" : "live override";
  if (flag === "alert") return isKo ? "alert" : "alert";
  if (flag === "read_only") return isKo ? "read-only" : "read-only";
  if (flag === "restart_required") return isKo ? "restart" : "restart";
  return flag;
}

function settingSourceLabel(source: SettingSource, isKo: boolean): string {
  if (source === "repo_canonical") return isKo ? "repo 정본" : "repo canonical";
  if (source === "runtime_config") return isKo ? "런타임 설정" : "runtime config";
  if (source === "kv_meta") return "kv_meta";
  if (source === "live_override") return isKo ? "실시간 override" : "live override";
  if (source === "legacy_readonly") return isKo ? "legacy alias" : "legacy alias";
  if (source === "computed") return isKo ? "유도값" : "computed";
  return source;
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

  const renderDefaultControl = () => {
    if (renderControl) return renderControl(meta);
    if (readOnly) {
      return (
        <div
          className="w-full truncate rounded-xl px-3 py-2 text-xs"
          style={{
            background: "color-mix(in srgb, var(--th-bg-surface) 60%, transparent)",
            border: "1px dashed color-mix(in srgb, var(--th-border) 70%, transparent)",
            color: "var(--th-text-muted)",
            fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
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
          className="relative inline-flex h-6 w-11 items-center rounded-full transition-colors"
          style={{
            background: enabled ? "var(--th-accent-primary)" : "color-mix(in srgb, var(--th-border) 80%, transparent)",
          }}
        >
          <span
            className="inline-block h-5 w-5 rounded-full bg-white shadow transition-transform"
            style={{ transform: enabled ? "translateX(1.4rem)" : "translateX(0.15rem)" }}
          />
        </button>
      );
    }
    if (meta.inputKind === "select" && meta.selectOptions) {
      return (
        <select
          value={String(meta.effectiveValue ?? "")}
          onChange={(event) => onChange?.(meta.key, event.target.value)}
          className="w-full rounded-xl px-3 py-2 text-sm"
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
        className="w-full rounded-xl px-3 py-2 text-xs"
        style={{
          background: "var(--th-bg-surface)",
          border: "1px solid color-mix(in srgb, var(--th-border) 70%, transparent)",
          color: "var(--th-text)",
          fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
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
      <div className="setting-row-grid items-center gap-3 px-2 py-3 sm:gap-4 sm:px-3 sm:py-4">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-1.5">
            <span className="setting-row-label text-sm font-medium" style={{ color: "var(--th-text)" }}>
              {labelText}
            </span>
            {meta.flags.map((flag) => {
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
            <div className="setting-row-hint mt-1 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
              {hintText}
            </div>
          ) : null}
          <code
            className="setting-key-token mt-1 inline-block rounded px-1 py-px text-[10px]"
            title={meta.key}
            style={{
              background: "color-mix(in srgb, var(--th-overlay-medium) 80%, transparent)",
              color: "var(--th-text-muted)",
              fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
            }}
          >
            {meta.key}
          </code>
        </div>
        <div className="min-w-0">
          {controlOverlay ?? renderDefaultControl()}
        </div>
        <button
          type="button"
          aria-expanded={open}
          aria-label={tr("자세히 보기", "Show details")}
          onClick={() => setOpen((current) => !current)}
          className="grid h-8 w-8 place-items-center rounded-full border"
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
          className="mx-2 mb-3 grid gap-2 rounded-2xl p-3 text-[11px] sm:mx-3 sm:grid-cols-2 sm:gap-3 sm:p-4"
          style={{
            background: "color-mix(in srgb, var(--th-overlay-medium) 70%, transparent)",
            border: "1px solid color-mix(in srgb, var(--th-border) 60%, transparent)",
            color: "var(--th-text-muted)",
          }}
        >
          <div>
            <span style={{ color: "var(--th-text-muted)" }}>{tr("기본값:", "Default:")} </span>
            <code
              style={{
                fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
                color: "var(--th-text)",
              }}
            >
              {meta.defaultValue === undefined || meta.defaultValue === null
                ? tr("없음", "—")
                : String(meta.defaultValue)}
            </code>
          </div>
          <div>
            <span style={{ color: "var(--th-text-muted)" }}>{tr("저장 레이어:", "Storage:")} </span>
            <code
              style={{
                fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
                color: "var(--th-text)",
              }}
            >
              {settingSourceLabel(meta.source, isKo)}
            </code>
            {meta.storageLayerKo || meta.storageLayerEn ? (
              <span className="ml-1" style={{ color: "var(--th-text-muted)" }}>
                · {tr(meta.storageLayerKo ?? "", meta.storageLayerEn ?? "")}
              </span>
            ) : null}
          </div>
          <div>
            <span style={{ color: "var(--th-text-muted)" }}>{tr("편집 가능:", "Editable:")} </span>
            <span style={{ color: "var(--th-text)" }}>
              {meta.editable ? tr("예", "yes") : tr("아니오 (읽기 전용)", "no (read-only)")}
            </span>
          </div>
          <div>
            <span style={{ color: "var(--th-text-muted)" }}>{tr("재시작 필요:", "Restart required:")} </span>
            <span style={{ color: "var(--th-text)" }}>
              {meta.restartRequired ? tr("예", "yes") : tr("아니오", "no")}
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
    <SettingsCard
      className="rounded-2xl p-4"
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
      }}
    >
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
    </SettingsCard>
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
      className="rounded-2xl p-4"
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
