import { ChevronLeft, ChevronRight, Minus, Plus } from "lucide-react";
import { useState, type ReactNode } from "react";
import { Button } from "../../kit/free/button-clean";
import { Switch } from "../../kit/free/switch-clean";
import { BottomSheet } from "../common/overlay/BottomSheet";
import type { SettingsCategory } from "./SettingsCopy";
import { baselineText, draftProblem, formatSettingValue, withRo, type CatalogItem, type CategoryFilter } from "./SettingsModel";
import { ChoiceControl, ItemChip, ItemFacts, ItemHeading, ItemValue, RelatedSettings, currentValue, editableItem, resetLabel } from "./SettingsPanels";
import { SETTINGS_CARD_CLASS, SETTINGS_TRAY_CLASS, SettingsSkeletonRows, SettingsTextField } from "./SettingsPrimitives";

type Tr = (ko: string, en: string) => string;

interface SettingsPhoneCatalogProps {
  categories: Array<{ id: SettingsCategory; label: string; count: number }>;
  category: CategoryFilter;
  onCategory: (id: CategoryFilter) => void;
  /** Rows that differ from their default, for the home view. */
  differing: CatalogItem[];
  /** Rows of the open category or the search result. */
  rows: CatalogItem[];
  searching: boolean;
  loading: boolean;
  notice: ReactNode;
  blocks: ReactNode;
  onOpen: (key: string) => void;
  tr: Tr;
}

const ROW_CLASS = "flex min-h-14 w-full min-w-0 items-center gap-3 px-3 py-2 text-left";

function PhoneRow({ item, onOpen, tr }: { item: CatalogItem; onOpen: (key: string) => void; tr: Tr }) {
  const value = formatSettingValue(currentValue(item), item, tr);
  const baseline = baselineText(item, tr);
  return (
    <button type="button" className={ROW_CLASS} data-testid={`setting-row-${item.key}`} onClick={() => onOpen(item.key)}>
      <span className="min-w-0 flex-1">
        <span className="block text-[15px] leading-5 break-keep text-th-text-primary">{item.label}</span>
        {item.input === "status" ? <span className="block text-xs"><ItemValue item={item} tr={tr} /></span> : (
          <span className="block text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">{baseline ? `${value} · ${baseline}` : value}</span>
        )}
      </span>
      <ItemChip item={item} tr={tr} />
    </button>
  );
}

/** Phone catalog: search above (page frame), then the differing rows and the category list, or one category's rows. */
export function SettingsPhoneCatalog({ categories, category, onCategory, differing, rows, searching, loading, notice, blocks, onOpen, tr }: SettingsPhoneCatalogProps) {
  const divided = "[&>*+*]:border-t [&>*+*]:border-[var(--line-soft)]";
  if (category === "all" && !searching) {
    return (
      <div className="flex min-w-0 flex-col gap-3" data-testid="settings-catalog">
        {notice}
        {loading ? <div className={SETTINGS_CARD_CLASS}><SettingsSkeletonRows label={tr("설정을 불러오는 중…", "Loading settings…")} /></div> : null}
        {differing.length > 0 ? (
          <section aria-label={tr("기본값과 다름", "Differs from default")} className={`${SETTINGS_CARD_CLASS} overflow-hidden ${divided}`} data-testid="settings-differing">
            <div className="flex min-h-[52px] items-center gap-2 px-3">
              <h2 className="m-0 flex-1 text-sm font-semibold text-th-text-primary">{tr("기본값과 다름", "Differs from default")}</h2>
              <span className="rounded-full bg-[var(--th-badge-amber-bg)] px-2 text-xs font-medium tabular-nums text-[var(--th-badge-amber-text)]">{differing.length}</span>
            </div>
            {differing.map((item) => <PhoneRow key={item.key} item={item} onOpen={onOpen} tr={tr} />)}
          </section>
        ) : null}
        <nav aria-label={tr("분류", "Categories")} className={`${SETTINGS_CARD_CLASS} overflow-hidden ${divided}`}>
          {categories.map((entry) => (
            <button key={entry.id} type="button" className={`${ROW_CLASS} min-h-[52px]`} data-testid={`settings-category-${entry.id}`} onClick={() => onCategory(entry.id)}>
              <span className="min-w-0 flex-1 text-[15px] text-th-text-primary">{entry.label}</span>
              <span className="text-xs tabular-nums text-th-text-muted">{entry.count}</span>
              <ChevronRight size={16} aria-hidden className="text-th-text-muted" />
            </button>
          ))}
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
          <button type="button" aria-label={tr("분류 목록으로", "Back to categories")} onClick={() => onCategory("all")}
            className="-ml-2 inline-flex size-11 shrink-0 items-center justify-center rounded-[var(--r-sm)] text-th-text-secondary hover:bg-surface-subtle">
            <ChevronLeft size={20} aria-hidden />
          </button>
        ) : null}
        <h2 className="m-0 min-w-0 flex-1 text-base font-semibold text-th-text-primary" data-testid="settings-phone-title">{title}</h2>
      </div>
      {loading ? <div className={SETTINGS_CARD_CLASS}><SettingsSkeletonRows label={tr("설정을 불러오는 중…", "Loading settings…")} /></div> : null}
      {rows.length > 0 ? (
        <div className={`${SETTINGS_CARD_CLASS} overflow-hidden ${divided}`}>
          {rows.map((item) => <PhoneRow key={item.key} item={item} onOpen={onOpen} tr={tr} />)}
        </div>
      ) : loading ? null : (
        <p className="m-0 px-1 text-[13px] text-th-text-muted">{tr("맞는 설정이 없어요.", "No matching settings.")}</p>
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

/** Phone edit sheet: − / value / + (or a switch or field), the stored and default values, then save this one row. */
export function SettingsEditSheet({ item, allItems, saving, onClose, onSave, onSelect, tr }: SettingsEditSheetProps) {
  const [value, setValue] = useState(currentValue(item));
  const problem = draftProblem(item, value, tr);
  const editable = editableItem(item);
  const step = (delta: number) => {
    const next = (Number(value) || 0) + delta;
    setValue(String(Math.min(item.max ?? Infinity, Math.max(item.min ?? -Infinity, next))));
  };
  const shown = formatSettingValue(value, item, tr);
  // The kit button sizes itself in unlayered CSS, so the 48px sheet buttons set their height inline.
  const big = { minHeight: 48, minWidth: 48 };
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
            <Button variant="secondary" className="w-12 shrink-0" style={big} aria-label={tr("1 줄이기", "Decrease by 1")} icon={<Minus />} disabled={saving} onClick={() => step(-1)} />
            <SettingsTextField
              aria-label={item.label}
              type="number"
              inputMode="numeric"
              size="lg"
              value={value}
              min={item.min}
              max={item.max}
              disabled={saving}
              className="min-w-0 flex-1"
              inputClassName="text-center text-xl tabular-nums"
              onChange={(event) => setValue(event.target.value)}
            />
            <Button variant="secondary" className="w-12 shrink-0" style={big} aria-label={tr("1 늘리기", "Increase by 1")} icon={<Plus />} disabled={saving} onClick={() => step(1)} />
          </div>
        ) : item.input === "toggle" ? (
          <div className={`flex min-h-12 items-center px-3 ${SETTINGS_TRAY_CLASS}`}>
            <Switch placement="end" className="w-full" label={shown} checked={value === "true"} disabled={saving} onChange={(event) => setValue(String(event.target.checked))} />
          </div>
        ) : item.input === "choice" ? (
          <ChoiceControl item={item} value={value} label={item.label} size="lg" className="w-full" disabled={saving} onChange={setValue} />
        ) : item.input === "text" ? (
          <SettingsTextField aria-label={item.label} size="lg" value={value} disabled={saving} inputClassName="font-mono" onChange={(event) => setValue(event.target.value)} />
        ) : null}
        <div className={`px-3 py-2.5 ${SETTINGS_TRAY_CLASS}`}>
          <ItemFacts item={item} pending={editable ? value : null} problem={problem} tr={tr} />
        </div>
        <RelatedSettings item={item} items={allItems} onSelect={onSelect} disabled={saving} tr={tr} />
        {editable ? (
          <div className="flex gap-2">
            {item.resetTo != null ? (
              <Button variant="secondary" className="flex-1" style={big} disabled={saving} onClick={() => setValue(item.resetTo ?? "")}>
                {resetLabel(item, tr, true)}
              </Button>
            ) : null}
            <Button
              variant="primary"
              className="flex-[2]"
              style={big}
              loading={saving}
              disabled={saving || Boolean(problem) || value === item.saved}
              onClick={() => onSave(item, value)}
            >
              {item.input === "text" || item.input === "choice" ? tr("저장", "Save") : tr(`${withRo(shown)} 저장`, `Save ${shown}`)}
            </Button>
          </div>
        ) : null}
      </div>
    </BottomSheet>
  );
}
