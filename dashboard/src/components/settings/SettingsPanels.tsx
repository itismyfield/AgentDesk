import { ChevronDown, RotateCcw } from "lucide-react";
import { useId, useState, type ReactNode } from "react";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { Select } from "../../kit/free/select-clean";
import { Switch } from "../../kit/free/switch-clean";
import type { SettingFlag, SettingRowMeta } from "./SettingsModel";
import { SETTINGS_ROW_PAD_CLASS, SETTINGS_TRAY_CLASS, SettingsAction, SettingsBadge, SettingsTextField } from "./SettingsPrimitives";

// A select this short reads better as segments (e.g. the theme row).
const SEGMENTED_OPTION_LIMIT = 3;
const FIELD_CLASS = "w-full @min-[37.5rem]:w-60";

const FLAG_LABELS: Record<SettingFlag, [string, string]> = {
  alert: ["저장 전 확인", "Confirm on save"],
  read_only: ["읽기 전용", "Read-only"],
  restart_required: ["다시 시작 후 적용", "Applies after restart"],
};

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

/** One settings row: label and hint on the left, the kit control on the right, details on demand. */
export function SettingRow({ meta, isKo, onChange, onReset, extra }: SettingRowProps) {
  const [open, setOpen] = useState(false);
  const detailsId = useId();
  const fieldId = useId();
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const label = isKo ? meta.labelKo : meta.labelEn;
  const hint = isKo ? meta.hintKo : meta.hintEn;
  const invalid = meta.validation && meta.validation.ok === false ? meta.validation : null;
  const errorText = invalid ? tr(invalid.messageKo, invalid.messageEn) : undefined;
  const applyNote = isKo ? meta.applyNoteKo : meta.applyNoteEn;
  const inline = meta.editable && meta.inputKind === "toggle";
  const labelled = meta.editable && (meta.inputKind === "text" || meta.inputKind === "number");

  const control = (() => {
    if (!meta.editable) {
      return (
        <div className={`${FIELD_CLASS} ${SETTINGS_TRAY_CLASS} px-2.5 py-1.5 text-[13px] leading-5 tabular-nums text-th-text-secondary [overflow-wrap:anywhere]`}>
          {formatValue(meta.effectiveValue, meta.valueUnit, tr)}
        </div>
      );
    }
    if (meta.inputKind === "toggle") {
      const enabled = meta.effectiveValue === true || meta.effectiveValue === "true" || meta.effectiveValue === 1 || meta.effectiveValue === "1";
      return <Switch aria-label={label} checked={enabled} onChange={(event) => onChange(meta.key, event.target.checked)} />;
    }
    if (meta.inputKind === "select" && meta.selectOptions) {
      const options = meta.selectOptions.map((option) => ({ value: option.value, label: isKo ? option.labelKo : option.labelEn }));
      const value = String(meta.effectiveValue ?? "");
      if (options.length <= SEGMENTED_OPTION_LIMIT) {
        // The kit control measures its own container, so it needs a sized slot (an auto column would collapse it).
        return (
          <div className={FIELD_CLASS}>
            <SegmentedControl
              aria-label={label}
              variant="tray"
              size="sm"
              options={options}
              value={value}
              onChange={(next) => onChange(meta.key, next)}
            />
          </div>
        );
      }
      return (
        <Select
          aria-label={label}
          className={FIELD_CLASS}
          options={options}
          value={value}
          error={errorText}
          onChange={(next) => onChange(meta.key, next)}
        />
      );
    }
    if (meta.inputKind === "range" && meta.numericRange) {
      const { min, max, step } = meta.numericRange;
      const value = Number(meta.effectiveValue) || 0;
      return (
        <div className={`flex items-center gap-3 ${FIELD_CLASS}`}>
          <input
            type="range"
            aria-label={label}
            min={min}
            max={max}
            step={step}
            value={value}
            onChange={(event) => onChange(meta.key, Number(event.target.value))}
            className="h-8 min-w-0 flex-1 cursor-pointer accent-[var(--accent)] pointer-coarse:h-11"
          />
          <SettingsTextField
            type="number"
            size="sm"
            aria-label={tr(`${label} 직접 입력`, `${label} value`)}
            min={min}
            max={max}
            step={step}
            value={value}
            className="w-20 shrink-0"
            inputClassName="text-right"
            onChange={(event) => {
              const next = Number(event.target.value);
              if (Number.isFinite(next) && next >= min && next <= max) onChange(meta.key, next);
            }}
          />
        </div>
      );
    }
    return (
      <SettingsTextField
        id={fieldId}
        aria-label={label}
        type={meta.inputKind === "number" ? "number" : "text"}
        inputMode={meta.inputKind === "number" ? "numeric" : undefined}
        value={String(meta.effectiveValue ?? "")}
        error={errorText}
        className={FIELD_CLASS}
        onChange={(event) =>
          onChange(meta.key, meta.inputKind === "number" ? Number(event.target.value) : event.target.value)
        }
      />
    );
  })();

  const detailsButton = (
    <SettingsAction
      variant="ghost"
      aria-expanded={open}
      aria-controls={detailsId}
      onClick={() => setOpen((current) => !current)}
      iconRight={<ChevronDown className={`transition-transform motion-reduce:transition-none ${open ? "rotate-180" : ""}`} />}
      className="justify-self-end"
    >
      {tr("자세히", "Details")}
    </SettingsAction>
  );

  return (
    <div className={`setting-row min-w-0 ${SETTINGS_ROW_PAD_CLASS}`} data-testid={`setting-row-${meta.key}`}>
      <div
        className={`grid min-h-9 min-w-0 items-center gap-x-3 gap-y-2.5 @min-[37.5rem]:gap-x-5 ${
          inline
            ? "grid-cols-[minmax(0,1fr)_auto_auto]"
            : "grid-cols-[minmax(0,1fr)_auto] @min-[37.5rem]:grid-cols-[minmax(0,1fr)_auto_auto]"
        }`}
      >
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-1.5">
            {labelled ? (
              <label htmlFor={fieldId} className="text-[13px] font-medium leading-5 break-keep text-th-text-primary">{label}</label>
            ) : (
              <span className="text-[13px] font-medium leading-5 break-keep text-th-text-primary">{label}</span>
            )}
            {meta.changed ? <SettingsBadge tone="accent">{tr("바뀜", "Changed")}</SettingsBadge> : null}
            {meta.flags.map((flag) => (
              <SettingsBadge key={flag} tone={flag === "read_only" ? "neutral" : "warn"}>{FLAG_LABELS[flag][isKo ? 0 : 1]}</SettingsBadge>
            ))}
          </div>
          {hint ? <p className="mt-0.5 text-xs leading-4 break-keep text-th-text-muted">{hint}</p> : null}
        </div>
        <div
          className={
            inline
              ? "flex min-w-0 justify-end"
              : "col-span-2 row-start-2 min-w-0 @min-[37.5rem]:col-span-1 @min-[37.5rem]:col-start-2 @min-[37.5rem]:row-start-1 @min-[37.5rem]:justify-self-end"
          }
        >
          {control}
          {extra}
        </div>
        <div className={inline ? "flex" : "col-start-2 row-start-1 flex justify-end @min-[37.5rem]:col-start-3"}>{detailsButton}</div>
      </div>
      {open ? (
        <div
          id={detailsId}
          className={`mt-3 grid min-w-0 gap-2 px-3 py-3 text-xs leading-5 text-th-text-muted @min-[37.5rem]:grid-cols-2 @min-[37.5rem]:gap-3 ${SETTINGS_TRAY_CLASS}`}
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
          {applyNote ? <div className="break-keep @min-[37.5rem]:col-span-2">{applyNote}</div> : null}
          {meta.resettable && onReset ? (
            <div className="@min-[37.5rem]:col-span-2">
              <SettingsAction icon={<RotateCcw />} onClick={() => onReset(meta.key)}>
                {tr("기본값으로 되돌리기", "Reset to default")}
              </SettingsAction>
            </div>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}
