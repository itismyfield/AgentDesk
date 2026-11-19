import { RotateCcw, SearchX } from "lucide-react";
import type { ReactNode } from "react";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { Switch } from "../../kit/free/switch-clean";
import type { SettingsCategory } from "./SettingsCopy";
import { baselineText, type CatalogItem, type CategoryFilter } from "./SettingsModel";
import { ItemChip, ItemFacts, ItemHeading, InlineControl, RelatedSettings, currentValue, editableItem, resetLabel } from "./SettingsPanels";
import { SETTINGS_CARD_CLASS, SettingsAction, SettingsSkeletonRows } from "./SettingsPrimitives";

type Tr = (ko: string, en: string) => string;

export interface CatalogGroup {
  id: SettingsCategory;
  label: string;
  items: CatalogItem[];
}

interface CatalogActions {
  onSelect: (key: string) => void;
  /** Opens a "함께 보는 설정" target even when the search, a filter or the category hides it. */
  onRelated: (key: string) => void;
  onChange: (item: CatalogItem, value: string) => void;
  onReset: (item: CatalogItem) => void;
  problemFor: (key: string) => string | null;
  allItems: CatalogItem[];
  categoryLabel: (id: SettingsCategory) => string;
  tr: Tr;
}

interface SettingsDesktopCatalogProps extends CatalogActions {
  categories: Array<{ id: CategoryFilter; label: string; count: number }>;
  category: CategoryFilter;
  onCategory: (id: CategoryFilter) => void;
  filters: { differs: boolean; restart: boolean; differsCount: number; restartCount: number };
  onFilter: (name: "differs" | "restart", on: boolean) => void;
  groups: CatalogGroup[];
  caption: string;
  selected: CatalogItem | null;
  /** Inspector beside the list (≥1280px); narrower desktops open details under the selected row. */
  wide: boolean;
  loading: boolean;
  notice: ReactNode;
  saveBar: ReactNode;
  blocks: ReactNode;
}

/** Whether a row can go back to its default (or to inheriting) from here. */
export function resettable(item: CatalogItem): boolean {
  return editableItem(item) && item.resetTo != null && currentValue(item) !== item.resetTo;
}

/** Desktop catalog: categories and filters, the grouped list with the pending bar, and the selected row's details. */
export function SettingsDesktopCatalog(props: SettingsDesktopCatalogProps) {
  const { categories, category, onCategory, filters, onFilter, groups, caption, selected, wide, loading, notice, saveBar, blocks, tr } = props;
  return (
    <div className="flex min-w-0 items-start gap-5" data-testid="settings-catalog">
      <nav aria-label={tr("분류", "Categories")} className="sticky top-4 flex w-[200px] shrink-0 flex-col gap-0.5">
        {categories.map((entry) => (
          <button
            key={entry.id}
            type="button"
            aria-current={entry.id === category ? "true" : undefined}
            data-testid={`settings-category-${entry.id}`}
            onClick={() => onCategory(entry.id)}
            className="flex min-h-10 w-full items-center justify-between gap-2 rounded-[var(--r-sm)] px-2.5 text-left text-[13px] text-th-text-secondary transition-colors hover:bg-surface-subtle aria-[current=true]:bg-[var(--bg-2)] aria-[current=true]:font-medium aria-[current=true]:text-th-text-primary"
          >
            <span className="min-w-0 break-keep">{entry.label}</span>
            <span className="text-xs tabular-nums text-th-text-muted">{entry.count}</span>
          </button>
        ))}
        <div className="my-2 h-px bg-[var(--line-soft)]" />
        <div className="flex flex-col gap-3 px-2.5 py-1">
          <Switch
            checked={filters.differs}
            onChange={(event) => onFilter("differs", event.target.checked)}
            label={<>{tr("기본값과 다름", "Differs from default")} <span className="tabular-nums text-th-text-muted">{filters.differsCount}</span></>}
          />
          <Switch
            checked={filters.restart}
            onChange={(event) => onFilter("restart", event.target.checked)}
            label={<>{tr("재시작 필요", "Needs restart")} <span className="tabular-nums text-th-text-muted">{filters.restartCount}</span></>}
          />
        </div>
      </nav>

      <div className="flex min-w-0 flex-1 flex-col gap-5">
        <div className="flex min-w-0 items-start gap-5">
          <section aria-label={tr("설정 목록", "Settings list")} className="@container flex min-w-0 flex-1 flex-col gap-3">
            {notice}
            <p className="m-0 text-xs text-th-text-muted" data-testid="settings-list-caption">{caption}</p>
            {loading ? (
              <div className={SETTINGS_CARD_CLASS}><SettingsSkeletonRows label={tr("설정을 불러오는 중…", "Loading settings…")} count={4} /></div>
            ) : groups.length === 0 ? (
              <EmptyState
                icon={<SearchX aria-hidden />}
                title={tr("맞는 설정이 없어요", "No matching settings")}
                text={tr("검색어나 필터를 바꿔 보세요.", "Try another search or filter.")}
              />
            ) : groups.map((group) => (
              <section key={group.id} aria-label={group.label} className={`${SETTINGS_CARD_CLASS} overflow-hidden`} data-testid={`settings-group-${group.id}`}>
                <h3 className="m-0 border-b border-[var(--line-soft)] px-4 py-2.5 text-xs font-medium text-th-text-muted">{group.label}</h3>
                <div className="[&>*+*]:border-t [&>*+*]:border-[var(--line-soft)]">
                  {group.items.map((item) => (
                    <div key={item.key}>
                      <CatalogRow {...props} item={item} selected={item.key === selected?.key} />
                      {!wide && item.key === selected?.key ? (
                        <div className="bg-[var(--bg-1)]" data-testid="settings-inline-details"><Inspector {...props} item={item} /></div>
                      ) : null}
                    </div>
                  ))}
                </div>
              </section>
            ))}
            {saveBar}
          </section>
          {wide && selected ? (
            <aside aria-label={tr("선택한 설정", "Selected setting")} data-testid="settings-inspector" className={`sticky top-4 max-h-[calc(100dvh-6rem)] w-[300px] shrink-0 overflow-y-auto min-[1400px]:w-[340px] ${SETTINGS_CARD_CLASS}`}>
              <Inspector {...props} item={selected} heading />
            </aside>
          ) : null}
        </div>
        {blocks}
      </div>
    </div>
  );
}

function CatalogRow({ item, selected, onSelect, onChange, problemFor, tr }: CatalogActions & { item: CatalogItem; selected: boolean }) {
  return (
    // Clicking anywhere or focusing the control selects the row; the name button is the keyboard way in.
    <div
      data-testid={`setting-row-${item.key}`}
      data-selected={selected || undefined}
      onClick={() => onSelect(item.key)}
      onFocusCapture={() => onSelect(item.key)}
      className={`flex min-h-[60px] min-w-0 flex-wrap items-center gap-x-3 gap-y-1.5 px-4 py-2.5 ${selected ? "bg-[color-mix(in_oklab,var(--accent)_8%,transparent)] shadow-[inset_2px_0_0_var(--accent)]" : ""}`}
    >
      <button type="button" aria-current={selected ? "true" : undefined} className="min-w-0 flex-1 text-left" onClick={() => onSelect(item.key)}>
        <span className="block text-[13px] leading-5 font-medium break-keep text-th-text-primary">{item.label}</span>
        <span className="block font-mono text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">{item.key}</span>
      </button>
      <div className="flex shrink-0 justify-end">
        <InlineControl item={item} label={item.label} invalid={Boolean(problemFor(item.key))} onChange={(value) => onChange(item, value)} tr={tr} />
      </div>
      {/* A narrow list moves the default and chips to a second line instead of hiding them. */}
      <div className="flex w-full min-w-0 items-center gap-2 @min-[34rem]:w-auto" data-testid={`setting-meta-${item.key}`}>
        <span className="min-w-0 flex-1 text-xs leading-4 text-th-text-muted break-keep @min-[34rem]:w-28 @min-[34rem]:flex-none">{baselineText(item, tr)}</span>
        <span className="flex shrink-0 flex-wrap justify-end gap-1 @min-[34rem]:w-[84px] @min-[34rem]:flex-col @min-[34rem]:items-center"><ItemChip item={item} tr={tr} all /></span>
      </div>
    </div>
  );
}

function Inspector({ item, heading = false, onRelated, onReset, problemFor, allItems, categoryLabel, tr }: CatalogActions & { item: CatalogItem; heading?: boolean }) {
  return (
    <div className="flex min-w-0 flex-col">
      {heading ? <div className="p-4"><ItemHeading item={item} category={categoryLabel(item.category)} /></div> : null}
      <div className={`flex min-w-0 flex-col gap-3 p-4 ${heading ? "border-t border-[var(--line-soft)]" : ""}`}>
        {heading ? null : <ItemHeading item={item} showTitle={false} />}
        <ItemFacts item={item} problem={problemFor(item.key)} tr={tr} />
        {resettable(item) ? (
          <SettingsAction icon={<RotateCcw />} className="self-start" onClick={() => onReset(item)}>
            {resetLabel(item, tr)}
          </SettingsAction>
        ) : null}
      </div>
      {item.related.length > 0 ? (
        <div className="border-t border-[var(--line-soft)] p-4">
          <RelatedSettings item={item} items={allItems} onSelect={onRelated} tr={tr} />
        </div>
      ) : null}
    </div>
  );
}
