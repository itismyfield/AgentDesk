import { RotateCcw, SearchX } from "lucide-react";
import { Fragment, type ReactNode } from "react";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { Cap, CardSkeleton, Eyebrow, InspSection, KitCard, ROW_HOVER_CLASS, Row } from "../common/KitPrimitives";
import type { SettingsCategory } from "./SettingsCopy";
import { baselineText, isResetValue, type CatalogItem, type CategoryFilter } from "./SettingsModel";
import { InlineControl, ItemChip, ItemFacts, ItemHeading, RelatedSettings, currentValue, editableItem, hasRelatedSettings, resetLabel } from "./SettingsPanels";
import { SettingsAction, SettingsSwitch } from "./SettingsPrimitives";

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
  /** True while this row's save is in flight; its control and reset stay disabled until it ends. */
  locked: (item: CatalogItem) => boolean;
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
  /** Inspector beside the list (>=1280px); narrower desktops open details under the selected row. */
  wide: boolean;
  loading: boolean;
  notice: ReactNode;
  saveBar: ReactNode;
  blocks: ReactNode;
}

/** Whether a row can go back to its default (or to inheriting) from here. */
export function resettable(item: CatalogItem): boolean {
  return editableItem(item) && item.resetTo != null && !isResetValue(item, currentValue(item));
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
            className={`flex min-h-10 w-full items-center justify-between gap-2 rounded-[var(--r-sm)] px-2.5 text-left text-sm leading-5 font-medium text-[var(--fg-dim)] pointer-coarse:min-h-11 ${ROW_HOVER_CLASS} aria-[current=true]:bg-[var(--bg-3)] aria-[current=true]:text-[var(--fg)]`}
          >
            <span className="min-w-0 break-keep">{entry.label}</span>
            <Cap className="font-display tabular-nums">{entry.count}</Cap>
          </button>
        ))}
        <div className="mx-1 my-2 h-px bg-[var(--line-soft)]" />
        <div className="flex flex-col gap-1 px-2.5 py-1">
          <div className="py-1">
            <SettingsSwitch
              checked={filters.differs}
              onChange={(event) => onFilter("differs", event.target.checked)}
              label={
                <span className={filters.differs ? "text-sm leading-5 text-[var(--fg)]" : "text-sm leading-5 text-[var(--fg-dim)]"}>
                  {tr("기본값과 다름", "Differs from default")}{" "}
                  <Cap className="font-display tabular-nums">{filters.differsCount}</Cap>
                </span>
              }
            />
          </div>
          <div className="py-1">
            <SettingsSwitch
              checked={filters.restart}
              onChange={(event) => onFilter("restart", event.target.checked)}
              label={
                <span className={filters.restart ? "text-sm leading-5 text-[var(--fg)]" : "text-sm leading-5 text-[var(--fg-dim)]"}>
                  {tr("재시작 필요", "Needs restart")}{" "}
                  <Cap className="font-display tabular-nums">{filters.restartCount}</Cap>
                </span>
              }
            />
          </div>
        </div>
      </nav>

      <div className="flex min-w-0 flex-1 flex-col gap-5">
        <div className="flex min-w-0 items-start gap-5">
          <section aria-label={tr("설정 목록", "Settings list")} className="@container flex min-w-0 flex-1 flex-col gap-3">
            {notice}
            <p className="m-0 text-xs leading-4 text-[var(--fg-muted)]" data-testid="settings-list-caption">{caption}</p>
            {loading ? (
              <KitCard>
                <CardSkeleton label={tr("설정을 불러오는 중…", "Loading settings…")} rows={4} />
              </KitCard>
            ) : groups.length === 0 ? (
              <EmptyState
                icon={<SearchX aria-hidden />}
                title={tr("맞는 설정이 없어요", "No matching settings")}
                text={tr("검색어나 필터를 바꿔 보세요.", "Try another search or filter.")}
              />
            ) : (
              groups.map((group) => (
                <KitCard key={group.id} as="section" aria-label={group.label} data-testid={`settings-group-${group.id}`}>
                  <Eyebrow className="border-b border-[var(--line-soft)] px-4 py-2.5">
                    <h3 className="m-0 text-inherit font-inherit">{group.label}</h3>
                  </Eyebrow>
                  <div>
                    {group.items.map((item) => {
                      const isSelected = item.key === selected?.key;
                      return (
                        <Fragment key={item.key}>
                          <CatalogRow {...props} item={item} selected={isSelected} />
                          {!wide && isSelected ? (
                            <div className="border-t border-[var(--line-soft)] bg-[var(--bg-1)]" data-testid="settings-inline-details">
                              <Inspector {...props} item={item} />
                            </div>
                          ) : null}
                        </Fragment>
                      );
                    })}
                  </div>
                </KitCard>
              ))
            )}
            {saveBar}
          </section>
          {wide && selected ? (
            <aside
              aria-label={tr("선택한 설정", "Selected setting")}
              data-testid="settings-inspector"
              className="sticky top-4 w-[300px] shrink-0 min-[1400px]:w-[340px]"
            >
              <KitCard className="max-h-[calc(100dvh-6rem)] overflow-y-auto">
                <Inspector {...props} item={selected} heading />
              </KitCard>
            </aside>
          ) : null}
        </div>
        {blocks}
      </div>
    </div>
  );
}

function CatalogRow({ item, selected, onSelect, onChange, locked, problemFor, tr }: CatalogActions & { item: CatalogItem; selected: boolean }) {
  return (
    // Clicking anywhere or focusing the control selects the row; the name button is the keyboard way in.
    <Row
      selected={selected}
      data-testid={`setting-row-${item.key}`}
      data-selected={selected || undefined}
      onClick={() => onSelect(item.key)}
      onFocusCapture={() => onSelect(item.key)}
      className="min-h-[60px] flex-wrap gap-y-1.5"
    >
      <button
        type="button"
        aria-current={selected ? "true" : undefined}
        className="min-w-0 flex-1 text-left focus-visible:rounded-[var(--r-sm)]"
        onClick={() => onSelect(item.key)}
      >
        <span className="block text-sm leading-5 font-medium break-keep text-[var(--fg)]">{item.label}</span>
        <Cap className="block font-mono leading-[18px] [overflow-wrap:anywhere]">{item.key}</Cap>
      </button>
      <div className="flex shrink-0 justify-end">
        <InlineControl
          item={item}
          label={item.label}
          invalid={Boolean(problemFor(item.key))}
          disabled={locked(item)}
          onChange={(value) => onChange(item, value)}
          tr={tr}
        />
      </div>
      {/* A narrow list moves the default and chips to a second line instead of hiding them. */}
      <div className="flex w-full min-w-0 items-center gap-3 @min-[34rem]:w-auto" data-testid={`setting-meta-${item.key}`}>
        <Cap className="min-w-0 flex-1 break-keep @min-[34rem]:w-28 @min-[34rem]:flex-none">{baselineText(item, tr)}</Cap>
        <span className="flex shrink-0 flex-wrap justify-end gap-1 @min-[34rem]:w-[84px] @min-[34rem]:flex-col @min-[34rem]:items-end">
          <ItemChip item={item} tr={tr} all />
        </span>
      </div>
    </Row>
  );
}

function Inspector({ item, heading = false, onRelated, onReset, locked, problemFor, allItems, categoryLabel, tr }: CatalogActions & { item: CatalogItem; heading?: boolean }) {
  return (
    <div className="flex min-w-0 flex-col">
      {heading ? (
        <div className="p-4">
          <ItemHeading item={item} category={categoryLabel(item.category)} />
        </div>
      ) : null}
      <InspSection className={heading ? undefined : "border-t-0"}>
        {heading ? null : <ItemHeading item={item} showTitle={false} />}
        <ItemFacts item={item} problem={problemFor(item.key)} tr={tr} />
        {resettable(item) ? (
          <SettingsAction icon={<RotateCcw />} className="mt-1 self-start" disabled={locked(item)} onClick={() => onReset(item)}>
            {resetLabel(item, tr)}
          </SettingsAction>
        ) : null}
      </InspSection>
      {hasRelatedSettings(item, allItems) ? (
        <InspSection>
          <RelatedSettings item={item} items={allItems} onSelect={onRelated} tr={tr} />
        </InspSection>
      ) : null}
    </div>
  );
}
