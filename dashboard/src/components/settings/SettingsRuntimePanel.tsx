import * as Accordion from "@radix-ui/react-accordion";
import { ChevronDown } from "lucide-react";
import type { CSSProperties } from "react";

import {
  SurfaceCallout as SettingsCallout,
  SurfaceEmptyState as SettingsEmptyState,
} from "../common/SurfacePrimitives";
import { CATEGORIES, type SettingRowMeta } from "./SettingsModel";
import type {
  RenderSettingRow,
  SettingsActionStyles,
  SettingsTr,
} from "./SettingsPanelTypes";

interface SettingsRuntimePanelProps extends Pick<
  SettingsActionStyles,
  "primaryActionClass" | "primaryActionStyle" | "subtleButtonClass" | "subtleButtonStyle"
> {
  activeRuntimeCategoryId: string;
  inputStyle: CSSProperties;
  onCategoryChange: (categoryId: string) => void;
  onRuntimeChange: (key: string, value: number) => void;
  onRuntimeReset: (key: string) => void;
  onRuntimeSave: () => Promise<void>;
  panelQueryNormalized: string;
  rcDirty: boolean;
  rcLoaded: boolean;
  rcSaving: boolean;
  renderSettingRow: RenderSettingRow;
  runtimeMetas: SettingRowMeta[];
  tr: SettingsTr;
}

export function SettingsRuntimePanel({
  activeRuntimeCategoryId,
  inputStyle,
  onCategoryChange,
  onRuntimeChange,
  onRuntimeReset,
  onRuntimeSave,
  panelQueryNormalized,
  primaryActionClass,
  primaryActionStyle,
  rcDirty,
  rcLoaded,
  rcSaving,
  renderSettingRow,
  runtimeMetas,
  subtleButtonClass,
  subtleButtonStyle,
  tr,
}: SettingsRuntimePanelProps) {
  return (
    <div className="space-y-4">
      {!rcLoaded ? (
        <SettingsEmptyState className="text-sm">
          {tr("런타임 설정을 불러오는 중...", "Loading runtime config...")}
        </SettingsEmptyState>
      ) : (
        <div className="space-y-4">
          <Accordion.Root
            type="single"
            value={activeRuntimeCategoryId}
            onValueChange={(value) => {
              if (value) onCategoryChange(value);
            }}
            className="space-y-3"
          >
            {CATEGORIES.map((category) => {
              const categoryMetas = runtimeMetas.filter((meta) =>
                category.fields.some((field) => field.key === meta.key),
              );
              const rows = categoryMetas
                .map((meta) => {
                  const field = category.fields.find((item) => item.key === meta.key);
                  if (!field) return renderSettingRow(meta);
                  const value = Number(meta.effectiveValue) || 0;
                  const defaultValue = Number(meta.defaultValue) || 0;
                  const isDefault = value === defaultValue;
                  const controlOverlay = (
                    <div className="flex items-center gap-2">
                      <input
                        type="range"
                        min={field.min}
                        max={field.max}
                        step={field.step}
                        value={value}
                        onChange={(event) => onRuntimeChange(field.key, Number(event.target.value))}
                        className="h-1.5 flex-1 cursor-pointer appearance-none rounded-full"
                        style={{ accentColor: "var(--th-accent-primary)" }}
                      />
                      <input
                        type="number"
                        min={field.min}
                        max={field.max}
                        step={field.step}
                        value={value}
                        onChange={(event) => {
                          const next = Number(event.target.value);
                          if (Number.isFinite(next) && next >= field.min && next <= field.max) {
                            onRuntimeChange(field.key, next);
                          }
                        }}
                        className="w-20 rounded-xl px-2 py-1.5 text-right text-xs"
                        style={{
                          ...inputStyle,
                          fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
                        }}
                      />
                    </div>
                  );
                  const trailingMeta = !isDefault ? (
                    <button
                      type="button"
                      onClick={() => onRuntimeReset(field.key)}
                      className={subtleButtonClass}
                      style={subtleButtonStyle}
                    >
                      {tr("기본값 복원", "Reset to default")}
                    </button>
                  ) : null;
                  return renderSettingRow(meta, { controlOverlay, trailingMeta });
                })
                .filter(Boolean);
              const countLabel = panelQueryNormalized
                ? `${rows.length}/${categoryMetas.length}`
                : tr(`${categoryMetas.length}개`, `${categoryMetas.length} items`);
              const isOpen = activeRuntimeCategoryId === category.id;

              return (
                <Accordion.Item
                  key={category.id}
                  value={category.id}
                  className="overflow-hidden rounded-[20px] border"
                  style={{
                    borderColor: isOpen
                      ? "color-mix(in srgb, var(--th-accent-primary) 32%, var(--th-border) 68%)"
                      : "color-mix(in srgb, var(--th-border) 70%, transparent)",
                    background: isOpen
                      ? "color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-primary-soft) 18%)"
                      : "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
                  }}
                >
                  <Accordion.Header>
                    <Accordion.Trigger
                      className="flex w-full items-start justify-between gap-3 px-4 py-4 text-left sm:px-5"
                      style={{ color: "var(--th-text)" }}
                    >
                      <span className="min-w-0">
                        <span className="settings-section-title block text-sm font-semibold">
                          {tr(category.titleKo, category.titleEn)}
                        </span>
                        <span className="settings-copy mt-1 block text-[12px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                          {tr(category.descriptionKo, category.descriptionEn)}
                        </span>
                      </span>
                      <span className="flex shrink-0 items-center gap-2">
                        <span
                          className="settings-count-chip inline-flex rounded-full border px-2.5 py-1 text-[10px] font-medium"
                          style={{
                            borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
                            background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
                            color: "var(--th-text-muted)",
                          }}
                        >
                          {countLabel}
                        </span>
                        <ChevronDown
                          size={16}
                          style={{
                            color: "var(--th-text-muted)",
                            transform: isOpen ? "rotate(180deg)" : "none",
                            transition: "transform 0.2s",
                          }}
                        />
                      </span>
                    </Accordion.Trigger>
                  </Accordion.Header>
                  <Accordion.Content
                    className="px-2 pb-2 sm:px-3"
                    style={{ borderTop: "1px solid color-mix(in srgb, var(--th-border) 60%, transparent)" }}
                  >
                    {rows.length > 0 ? (
                      <div className="pt-1">{rows}</div>
                    ) : (
                      <SettingsEmptyState className="my-2 text-sm">
                        {tr("검색 결과가 없습니다.", "No matching settings.")}
                      </SettingsEmptyState>
                    )}
                  </Accordion.Content>
                </Accordion.Item>
              );
            })}
          </Accordion.Root>

          <SettingsCallout
            className="mt-0"
            action={(
              <button
                onClick={onRuntimeSave}
                disabled={rcSaving || !rcDirty}
                className={primaryActionClass}
                style={primaryActionStyle}
              >
                {rcSaving ? tr("저장 중...", "Saving...") : tr("런타임 저장", "Save runtime")}
              </button>
            )}
          >
            <p className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "런타임 설정은 저장 즉시 반영됩니다. 현재 선택한 하위 카테고리는 브라우저에 기억해 두었다가 다음 방문 때 다시 엽니다.",
                "Runtime settings apply immediately on save. The selected subcategory is remembered in the browser and restored on the next visit.",
              )}
            </p>
          </SettingsCallout>
        </div>
      )}
    </div>
  );
}
