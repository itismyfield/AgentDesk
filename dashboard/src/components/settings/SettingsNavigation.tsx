import { Bot, Plug, Search, Server, SlidersHorizontal, Workflow, X, type LucideIcon } from "lucide-react";
import { useEffect, useRef, type Dispatch, type KeyboardEvent, type SetStateAction } from "react";
import type { SettingsNavItem } from "./SettingsMetaCatalog";
import type { SettingsPanel } from "./SettingsModel";
import { SettingsAction, SettingsTextField } from "./SettingsPrimitives";

const PANEL_ICONS: Record<SettingsPanel, LucideIcon> = {
  general: SlidersHorizontal,
  workflow: Workflow,
  ai: Bot,
  connections: Plug,
  machine: Server,
};

interface SettingsNavigationProps {
  activePanel: SettingsPanel;
  items: SettingsNavItem[];
  matchingCount: number;
  onPanelChange: (panel: SettingsPanel) => void;
  query: string;
  queryActive: boolean;
  setQuery: Dispatch<SetStateAction<string>>;
  tr: (ko: string, en: string) => string;
}

/** Settings search and the panel list: a vertical list on wide screens, a sideways-scrolling tab row under 900px. */
export function SettingsNavigation({
  activePanel, items, matchingCount, onPanelChange, query, queryActive, setQuery, tr,
}: SettingsNavigationProps) {
  const listRef = useRef<HTMLDivElement | null>(null);
  const focusIndex = items.findIndex((item) => item.id === activePanel);

  // Under 900px the list scrolls sideways; keep the open panel's tab in view without moving the page.
  useEffect(() => {
    const list = listRef.current;
    const tab = list?.querySelector<HTMLElement>(`#settings-tab-${activePanel}`);
    if (!list || !tab || list.scrollWidth <= list.clientWidth) return;
    const box = list.getBoundingClientRect();
    const rect = tab.getBoundingClientRect();
    if (rect.left < box.left) list.scrollLeft -= box.left - rect.left + 16;
    else if (rect.right > box.right) list.scrollLeft += rect.right - box.right + 16;
  }, [activePanel]);

  // Arrow keys, Home and End move between panels (both axes, since the list turns sideways on phones).
  const onListKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const keys = ["ArrowDown", "ArrowRight", "ArrowUp", "ArrowLeft", "Home", "End"];
    if (!keys.includes(event.key) || items.length === 0) return;
    event.preventDefault();
    const tabs = Array.from(listRef.current?.querySelectorAll<HTMLButtonElement>('[role="tab"]') ?? []);
    const from = Math.max(0, tabs.indexOf(document.activeElement as HTMLButtonElement));
    const next = event.key === "Home" ? 0
      : event.key === "End" ? tabs.length - 1
        : (from + (event.key === "ArrowDown" || event.key === "ArrowRight" ? 1 : -1) + tabs.length) % tabs.length;
    tabs[next]?.focus();
    onPanelChange(items[next].id);
  };

  return (
    <aside className="min-w-0 md:sticky md:top-0 md:self-start">
      <SettingsTextField
        type="search"
        size="lg"
        value={query}
        onChange={(event) => setQuery(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Escape" && query) {
            event.preventDefault();
            setQuery("");
          }
        }}
        placeholder={tr("설정 검색", "Search settings")}
        aria-label={tr("설정 검색", "Search settings")}
        data-testid="settings-search-input"
        prefix={<Search aria-hidden />}
        suffix={query ? (
          <SettingsAction variant="ghost" icon={<X />} aria-label={tr("검색 지우기", "Clear search")} onClick={() => setQuery("")} />
        ) : undefined}
        className="mb-3"
      />
      {queryActive ? (
        <p className="mb-3 text-xs leading-5 break-keep text-th-text-muted" role="status" data-testid="settings-search-summary">
          {tr(`이 탭에서 ${matchingCount}개 항목이 일치해요`, `${matchingCount} matches in this tab`)}
        </p>
      ) : null}
      <div
        ref={listRef}
        role="tablist"
        aria-label={tr("설정 패널", "Settings panels")}
        onKeyDown={onListKeyDown}
        className="-mx-4 -my-1 flex gap-1.5 overflow-x-auto px-4 py-1 [scrollbar-width:none] md:mx-0 md:my-0 md:flex-col md:gap-0.5 md:overflow-visible md:px-0 md:py-0"
      >
        {items.length ? items.map((item, index) => {
          const active = activePanel === item.id;
          const Icon = PANEL_ICONS[item.id];
          return (
            <button
              key={item.id}
              id={`settings-tab-${item.id}`}
              type="button"
              role="tab"
              aria-selected={active}
              aria-current={active ? "page" : undefined}
              aria-controls="settings-panel-content"
              tabIndex={index === (focusIndex >= 0 ? focusIndex : 0) ? 0 : -1}
              title={item.detail}
              onClick={() => onPanelChange(item.id)}
              className={`flex min-h-11 shrink-0 items-center gap-2 whitespace-nowrap break-keep rounded-full px-3 text-[13px] leading-5 transition-colors motion-reduce:transition-none focus-visible:-outline-offset-2 md:rounded-[var(--r-sm)] md:px-2.5 ${
                active
                  ? "bg-[var(--th-overlay-medium)] font-medium text-th-text-primary shadow-[inset_0_0_0_0.5px_var(--line)] md:shadow-none"
                  : "font-normal text-th-text-secondary shadow-[inset_0_0_0_0.5px_var(--line)] hover:bg-[var(--th-overlay-subtle)] hover:text-th-text-primary md:shadow-none"
              }`}
            >
              <Icon size={16} aria-hidden className={`shrink-0 ${active ? "text-th-text-primary" : "text-th-text-muted"}`} />
              <span className="min-w-0 md:flex-1 md:text-left">{item.title}</span>
              {item.attention > 0 ? (
                <span
                  className="inline-flex shrink-0 items-center rounded-full bg-[var(--th-badge-amber-bg)] px-1.5 text-[11px] font-medium leading-4 tabular-nums text-[var(--th-badge-amber-text)]"
                  aria-label={tr(`확인 필요 ${item.attention}개`, `${item.attention} need attention`)}
                  data-testid={`settings-tab-attention-${item.id}`}
                >
                  {tr(`확인 ${item.attention}`, `${item.attention} to check`)}
                </span>
              ) : null}
            </button>
          );
        }) : <p className="py-2 text-[13px] break-keep text-th-text-muted">{tr("일치하는 설정이 없어요.", "No settings match.")}</p>}
      </div>
    </aside>
  );
}
