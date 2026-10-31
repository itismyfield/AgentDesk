import { Search, X } from "lucide-react";
import type { CSSProperties, Dispatch, SetStateAction } from "react";
import { SurfaceEmptyState as SettingsEmptyState } from "../common/SurfacePrimitives";

export interface SettingsNavItem<TPanel extends string> {
  id: TPanel;
  title: string;
  detail: string;
  count?: string;
}

interface SettingsNavigationProps<TPanel extends string> {
  activePanel: TPanel;
  inputStyle: CSSProperties;
  items: SettingsNavItem<TPanel>[];
  matchingCount: number;
  onPanelChange: (panel: TPanel) => void;
  query: string;
  queryActive: boolean;
  setQuery: Dispatch<SetStateAction<string>>;
  tr: (ko: string, en: string) => string;
}

export function SettingsNavigation<TPanel extends string>({
  activePanel, inputStyle, items, matchingCount, onPanelChange,
  query, queryActive, setQuery, tr,
}: SettingsNavigationProps<TPanel>) {
  return (
    <aside className="min-w-0 md:sticky md:top-0 md:self-start">
      <div className="relative mb-3">
        <Search size={14} className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-th-text-muted" />
        <input type="search" value={query} onChange={event => setQuery(event.target.value)}
          placeholder={tr("설정 검색", "Search settings")} aria-label={tr("설정 검색", "Search settings")}
          className="h-11 w-full rounded-md pl-9 pr-12 text-sm" style={inputStyle} data-testid="settings-search-input" />
        {query ? <button type="button" onClick={() => setQuery("")}
          className="absolute right-0 top-0 grid h-11 w-11 place-items-center rounded-md text-th-text-muted hover:text-th-text-primary"
          aria-label={tr("검색 지우기", "Clear search")}><X size={14} /></button> : null}
      </div>
      {queryActive ? <p className="mb-3 text-xs leading-5 text-th-text-muted" role="status" data-testid="settings-search-summary">
        {tr(`현재 패널 ${matchingCount}개 항목 일치`, `${matchingCount} matches in this panel`)}
      </p> : null}
      <div role="tablist" aria-label={tr("설정 패널", "Settings panels")}
        className="grid grid-cols-2 gap-1 border-b border-th-border-subtle pb-3 min-[23.5rem]:grid-cols-3 min-[28.75rem]:grid-cols-4 md:grid-cols-1 md:border-b-0 md:pb-0">
        {items.length ? items.map(item => {
          const active = activePanel === item.id;
          return <button key={item.id} id={`settings-tab-${item.id}`} type="button" role="tab"
            aria-selected={active} aria-current={active ? "page" : undefined} aria-controls="settings-panel-content"
            title={item.detail} onClick={() => onPanelChange(item.id)}
            className={`flex min-h-11 min-w-0 items-center justify-center gap-2 rounded-md px-2 py-2 text-xs transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--th-accent-primary)] min-[28.75rem]:px-1 md:justify-between md:px-3 ${active ? "bg-surface-medium font-semibold text-th-text-primary" : "font-medium text-th-text-muted hover:bg-surface-subtle hover:text-th-text-primary"}`}>
            <span className="min-w-0 [overflow-wrap:anywhere]">{item.title}</span>
            {item.count ? <span className="hidden shrink-0 font-normal tabular-nums text-th-text-muted md:inline">{item.count}</span> : null}
          </button>;
        }) : <SettingsEmptyState className="col-span-full text-sm">{tr("검색 결과가 없습니다.", "No groups match the search.")}</SettingsEmptyState>}
      </div>
    </aside>
  );
}
