import { ChevronDown, ChevronRight, RotateCcw, SearchX } from "lucide-react";
import { Fragment, type ReactNode } from "react";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { Cap, CardSkeleton, Chip, Eyebrow, InspSection, KitCard, ROW_HOVER_CLASS, Row } from "../common/KitPrimitives";
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
  onRelated: (key: string) => void;
  onChange: (item: CatalogItem, value: string) => void;
  onReset: (item: CatalogItem) => void;
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
  essentialItems?: CatalogItem[];
  advancedGroups?: CatalogGroup[];
  advancedCount?: number;
  twoTier?: boolean;
  advancedOpen?: boolean;
  onToggleAdvanced?: () => void;
  caption: string;
  selected: CatalogItem | null;
  wide: boolean;
  loading: boolean;
  notice: ReactNode;
  saveBar: ReactNode;
  blocks: ReactNode;
}

export function resettable(item: CatalogItem): boolean {
  return editableItem(item) && item.resetTo != null && !isResetValue(item, currentValue(item));
}

function GroupCard({ group, selected, wide, props }: { group: CatalogGroup; selected: CatalogItem | null; wide: boolean; props: SettingsDesktopCatalogProps }) {
  return (
    <KitCard as="section" aria-label={group.label} data-testid={`settings-group-${group.id}`}>
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
  );
}

export function SettingsDesktopCatalog(props: SettingsDesktopCatalogProps) {
  const {
    categories,
    category,
    onCategory,
    filters,
    onFilter,
    groups,
    essentialItems = [],
    advancedGroups = [],
    advancedCount = 0,
    twoTier = false,
    advancedOpen = false,
    onToggleAdvanced,
    caption,
    selected,
    wide,
    loading,
    notice,
    saveBar,
    blocks,
    tr,
  } = props;

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
            ) : twoTier ? (
              <>
                {essentialItems.length > 0 ? (
                  <KitCard as="section" aria-label={tr("필수 운영 설정", "Essential operational settings")} data-testid="settings-tier-essential">
                    <Eyebrow className="flex items-center justify-between gap-2 border-b border-[var(--line-soft)] px-4 py-2.5">
                      <h3 className="m-0 text-inherit font-inherit">{tr("필수 운영 설정", "Essential settings")}</h3>
                      <Chip tone="accent">{essentialItems.length}</Chip>
                    </Eyebrow>
                    <div>
                      {essentialItems.map((item) => {
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
                ) : null}

                {advancedCount > 0 ? (
                  <section className="flex min-w-0 flex-col gap-3" data-testid="settings-tier-advanced">
                    <KitCard>
                      <button
                        type="button"
                        aria-expanded={advancedOpen}
                        data-testid="settings-advanced-toggle"
                        onClick={onToggleAdvanced}
                        className={`flex min-h-12 w-full items-center justify-between gap-3 border-0 bg-transparent px-4 py-3 text-left ${ROW_HOVER_CLASS}`}
                      >
                        <span className="flex min-w-0 flex-col">
                          <span className="text-sm leading-5 font-semibold text-[var(--fg)]">
                            {tr("고급 런타임 설정", "Advanced runtime settings")}
                          </span>
                          <Cap className="[word-break:keep-all]">
                            {tr("세부 타임아웃·복구 정책·알림 채널·내부 키를 펼쳐서 조정해요.", "Expand granular recovery policies, alert channels, and internal runtime keys.")}
                          </Cap>
                        </span>
                        <span className="flex shrink-0 items-center gap-2">
                          <Chip tone="neutral">{advancedCount}</Chip>
                          {advancedOpen ? <ChevronDown size={16} aria-hidden className="text-[var(--fg-muted)]" /> : <ChevronRight size={16} aria-hidden className="text-[var(--fg-muted)]" />}
                        </span>
                      </button>
                    </KitCard>
                    <div className={advancedOpen ? "flex min-w-0 flex-col gap-3" : "hidden"} hidden={!advancedOpen}>
                      {advancedGroups.map((group) => (
                        <GroupCard key={group.id} group={group} selected={selected} wide={wide} props={props} />
                      ))}
                    </div>
                  </section>
                ) : null}
              </>
            ) : (
              groups.map((group) => (
                <GroupCard key={group.id} group={group} selected={selected} wide={wide} props={props} />
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
    <Row
      selected={selected}
      data-testid={`setting-row-${item.key}`}
      data-selected={selected || undefined}
      onClick={() => onSelect(item.key)}
      onFocusCapture={() => onSelect(item.key)}
      className="min-h-[60px] flex-wrap gap-x-3 gap-y-1.5"
    >
      <button
        type="button"
        aria-current={selected ? "true" : undefined}
        className="min-w-[12rem] flex-1 text-left focus-visible:rounded-[var(--r-sm)]"
        onClick={() => onSelect(item.key)}
      >
        <span className="block text-sm leading-5 font-medium break-keep text-[var(--fg)]">{item.label}</span>
        <Cap title={item.key} className="block max-w-full truncate font-mono text-[11.5px] leading-[18px]">
          {item.key}
        </Cap>
      </button>
      <div className="ml-auto flex shrink-0 justify-end">
        <InlineControl
          item={item}
          label={item.label}
          invalid={Boolean(problemFor(item.key))}
          disabled={locked(item)}
          onChange={(value) => onChange(item, value)}
          tr={tr}
        />
      </div>
      <div className="flex w-full min-w-0 items-center justify-between gap-2 @min-[48rem]:w-auto @min-[48rem]:justify-end" data-testid={`setting-meta-${item.key}`}>
        <Cap className="min-w-0 truncate break-keep">{baselineText(item, tr)}</Cap>
        <span className="inline-flex shrink-0 flex-nowrap items-center justify-end gap-1" data-testid={`setting-chips-${item.key}`}>
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
