import { ChevronLeft, ChevronRight, Minus, Plus } from "lucide-react";
import { useState, type CSSProperties, type ReactNode } from "react";
import { Cap, CardNote, CardSkeleton, CountBadge, KitCard, Row, RowButton, Well } from "../common/KitPrimitives";
import { BottomSheet } from "../common/overlay/BottomSheet";
import type { SettingsCategory } from "./SettingsCopy";
import { baselineText, draftProblem, formatSettingValue, withRo, type CatalogItem, type CategoryFilter } from "./SettingsModel";
import { ChoiceControl, ItemChip, ItemFacts, ItemHeading, ItemValue, RelatedSettings, currentValue, editableItem, resetLabel } from "./SettingsPanels";
import { SettingsButton, SettingsSwitch, SettingsTextField } from "./SettingsPrimitives";

type Tr = (ko: string, en: string) => string;

interface SettingsPhoneCatalogProps {
  categories: Array<{ id: SettingsCategory; label: string; count: number }>;
  category: CategoryFilter;
  onCategory: (id: CategoryFilter) => void;
  /** Rows that differ from their default, for the home view. */
  differing: CatalogItem[];
  /** Rows of the open category or the search result. */
  rows: CatalogItem[];
  selectedKey?: string | null;
  searching: boolean;
  loading: boolean;
  notice: ReactNode;
  blocks: ReactNode;
  onOpen: (key: string) => void;
  tr: Tr;
}

function PhoneRow({
  item,
  selected = false,
  onOpen,
  tr,
}: {
  item: CatalogItem;
  selected?: boolean;
  onOpen: (key: string) => void;
  tr: Tr;
}) {
  const value = formatSettingValue(currentValue(item), item, tr);
  const baseline = baselineText(item, tr);
  return (
    <RowButton selected={selected} aria-current={selected ? "true" : undefined} className="min-h-14" data-testid={`setting-row-${item.key}`} onClick={() => onOpen(item.key)}>
      <span className="min-w-0 flex-1">
        <span className="block text-sm leading-5 break-keep text-[var(--fg)]">{item.label}</span>
        {item.input === "status" ? (
          <span className="mt-0.5 block text-xs"><ItemValue item={item} tr={tr} /></span>
        ) : (
          <Cap className="mt-0.5 block [overflow-wrap:anywhere]">{baseline ? `${value} · ${baseline}` : value}</Cap>
        )}
      </span>
      <ItemChip item={item} tr={tr} />
    </RowButton>
  );
}

/** Phone catalog: search above (page frame), then the differing rows and the category list, or one category's rows. */
export function SettingsPhoneCatalog({
  categories,
  category,
  onCategory,
  differing,
  rows,
  selectedKey,
  searching,
  loading,
  notice,
  blocks,
  onOpen,
  tr,
}: SettingsPhoneCatalogProps) {
  if (category === "all" && !searching) {
    return (
      <div className="flex min-w-0 flex-col gap-3" data-testid="settings-catalog">
        {notice}
        {loading ? (
          <KitCard>
            <CardSkeleton label={tr("설정을 불러오는 중…", "Loading settings…")} />
          </KitCard>
        ) : null}
        {differing.length > 0 ? (
          <KitCard as="section" aria-label={tr("기본값과 다름", "Differs from default")} data-testid="settings-differing">
            <Row className="min-h-[52px]">
              <h2 className="m-0 flex-1 text-sm leading-5 font-semibold text-[var(--fg)]">{tr("기본값과 다름", "Differs from default")}</h2>
              <CountBadge count={differing.length} tone="warn" />
            </Row>
            {differing.map((item) => (
              <div key={item.key} className="border-t border-[var(--line-soft)]">
                <PhoneRow item={item} selected={selectedKey === item.key} onOpen={onOpen} tr={tr} />
              </div>
            ))}
          </KitCard>
        ) : null}
        <nav aria-label={tr("분류", "Categories")}>
          <KitCard>
            {categories.map((entry) => (
              <div key={entry.id} className="border-t border-[var(--line-soft)] first:border-t-0">
                <RowButton className="min-h-[52px]" data-testid={`settings-category-${entry.id}`} onClick={() => onCategory(entry.id)}>
                  <span className="min-w-0 flex-1 text-sm leading-5 text-[var(--fg)]">{entry.label}</span>
                  <Cap className="font-display tabular-nums">{entry.count}</Cap>
                  <ChevronRight size={16} aria-hidden className="shrink-0 text-[var(--fg-muted)]" />
                </RowButton>
              </div>
            ))}
          </KitCard>
        </nav>
      </div>
    );
  }

  const title = searching ? tr(`찾은 설정 ${rows.length}개`, `${rows.length} found`) : categories.find((entry) => entry.id === category)?.label ?? "";
  return (
    <div className="flex min-w-0 flex-col gap-3" data-testid="settings-catalog">
      {notice}
      <div className="flex min-w-0 items-center gap-1">
        {category !== "all" ? (
          <button
            type="button"
            aria-label={tr("분류 목록으로", "Back to categories")}
            onClick={() => onCategory("all")}
            className="-ml-2 inline-flex size-11 shrink-0 items-center justify-center rounded-[var(--r-sm)] text-[var(--fg-dim)] hover:bg-[color-mix(in_oklch,var(--fg)_5%,transparent)]"
          >
            <ChevronLeft size={20} aria-hidden />
          </button>
        ) : null}
        <h2 className="m-0 min-w-0 flex-1 text-base leading-6 font-semibold text-[var(--fg)]" data-testid="settings-phone-title">{title}</h2>
      </div>
      {loading ? (
        <KitCard>
          <CardSkeleton label={tr("설정을 불러오는 중…", "Loading settings…")} />
        </KitCard>
      ) : null}
      {rows.length > 0 ? (
        <KitCard>
          {rows.map((item) => (
            <div key={item.key} className="border-t border-[var(--line-soft)] first:border-t-0">
              <PhoneRow item={item} selected={selectedKey === item.key} onOpen={onOpen} tr={tr} />
            </div>
          ))}
        </KitCard>
      ) : loading ? null : (
        <KitCard>
          <CardNote className="border-t-0">{tr("맞는 설정이 없어요.", "No matching settings.")}</CardNote>
        </KitCard>
      )}
      {searching ? null : blocks}
    </div>
  );
}

interface SettingsEditSheetProps {
  item: CatalogItem;
  allItems: CatalogItem[];
  saving: boolean;
  onClose: () => void;
  onSave: (item: CatalogItem, value: string) => void;
  onSelect: (key: string) => void;
  tr: Tr;
}

const SHEET_BUTTON_STYLE = {
  minHeight: 48,
  minWidth: 48,
} as CSSProperties;

const SHEET_BUTTON_HEIGHT_CLASS = "[--aui-button-clean-h:48px]!";
const SHEET_FIELD_HEIGHT_CLASS = "[--h:48px]!";

/** Phone edit sheet: - / value / + (or a switch or field), the stored and default values, then save this one row. */
export function SettingsEditSheet({ item, allItems, saving, onClose, onSave, onSelect, tr }: SettingsEditSheetProps) {
  const [value, setValue] = useState(currentValue(item));
  const problem = draftProblem(item, value, tr);
  const editable = editableItem(item);
  const step = (delta: number) => {
    const next = (Number(value) || 0) + delta;
    setValue(String(Math.min(item.max ?? Infinity, Math.max(item.min ?? -Infinity, next))));
  };
  const shown = formatSettingValue(value, item, tr);
  // Saving locks every input and every way out; a blocked swipe remounts the sheet so it slides back up.
  const [held, setHeld] = useState(0);
  const close = () => (saving ? setHeld((count) => count + 1) : onClose());

  return (
    <BottomSheet
      key={held}
      open
      onClose={close}
      closeOnBackdrop={!saving}
      closeOnEsc={!saving}
      closeButtonClassName={saving ? "pointer-events-none opacity-40" : undefined}
      title={item.label}
    >
      <div className="flex min-w-0 flex-col gap-3" data-testid="settings-edit-sheet" aria-busy={saving || undefined}>
        <ItemHeading item={item} showTitle={false} />
        {item.input === "number" ? (
          <div className="flex items-center gap-2">
            <SettingsButton
              variant="secondary"
              className={`size-12 shrink-0 ${SHEET_BUTTON_HEIGHT_CLASS}`}
              style={SHEET_BUTTON_STYLE}
              aria-label={tr("1 줄이기", "Decrease by 1")}
              icon={<Minus />}
              disabled={saving}
              onClick={() => step(-1)}
            />
            <SettingsTextField
              aria-label={item.label}
              type="number"
              inputMode="numeric"
              size="lg"
              value={value}
              min={item.min}
              max={item.max}
              disabled={saving}
              className={`min-w-0 flex-1 ${SHEET_FIELD_HEIGHT_CLASS}`}
              inputClassName="text-center font-display text-xl tabular-nums"
              onChange={(event) => setValue(event.target.value)}
            />
            <SettingsButton
              variant="secondary"
              className={`size-12 shrink-0 ${SHEET_BUTTON_HEIGHT_CLASS}`}
              style={SHEET_BUTTON_STYLE}
              aria-label={tr("1 늘리기", "Increase by 1")}
              icon={<Plus />}
              disabled={saving}
              onClick={() => step(1)}
            />
          </div>
        ) : item.input === "toggle" ? (
          <Well className="flex min-h-12 items-center px-3 py-2">
            <SettingsSwitch
              sheet
              placement="end"
              className="w-full"
              label={shown}
              checked={value === "true"}
              disabled={saving}
              onChange={(event) => setValue(String(event.target.checked))}
            />
          </Well>
        ) : item.input === "choice" ? (
          <ChoiceControl
            item={item}
            value={value}
            label={item.label}
            size="lg"
            className={`w-full ${SHEET_FIELD_HEIGHT_CLASS}`}
            disabled={saving}
            onChange={setValue}
          />
        ) : item.input === "text" ? (
          <SettingsTextField
            aria-label={item.label}
            size="lg"
            value={value}
            disabled={saving}
            className={SHEET_FIELD_HEIGHT_CLASS}
            inputClassName="font-mono"
            onChange={(event) => setValue(event.target.value)}
          />
        ) : null}
        <Well className="px-3 py-2.5">
          <ItemFacts item={item} pending={editable ? value : null} problem={problem} tr={tr} />
        </Well>
        <RelatedSettings item={item} items={allItems} onSelect={onSelect} disabled={saving} tr={tr} />
        {editable ? (
          <div className="flex gap-2">
            {item.resetTo != null ? (
              <SettingsButton
                variant="secondary"
                className={`flex-1 ${SHEET_BUTTON_HEIGHT_CLASS}`}
                style={SHEET_BUTTON_STYLE}
                disabled={saving}
                onClick={() => setValue(item.resetTo ?? "")}
              >
                {resetLabel(item, tr, true)}
              </SettingsButton>
            ) : null}
            <SettingsButton
              variant="primary"
              className={`flex-[2] ${SHEET_BUTTON_HEIGHT_CLASS}`}
              style={SHEET_BUTTON_STYLE}
              loading={saving}
              disabled={saving || Boolean(problem) || value === item.saved}
              onClick={() => onSave(item, value)}
            >
              {item.input === "text" || item.input === "choice" ? tr("저장", "Save") : tr(`${withRo(shown)} 저장`, `Save ${shown}`)}
            </SettingsButton>
          </div>
        ) : null}
      </div>
    </BottomSheet>
  );
}
