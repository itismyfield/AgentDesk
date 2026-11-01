import { Search, X } from "lucide-react";
import type { Dispatch, SetStateAction } from "react";
import type { SettingsNavItem } from "./SettingsMetaCatalog";
import { SETTINGS_INPUT_STYLE } from "./SettingsPanels";
import type { SettingsPanel } from "./SettingsModel";

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

export function SettingsNavigation({
  activePanel, items, matchingCount, onPanelChange, query, queryActive, setQuery, tr,
}: SettingsNavigationProps) {
  return (
    <aside className="min-w-0 md:sticky md:top-0 md:self-start">
      <div className="relative mb-3">
        <Search size={14} className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-th-text-muted" />
        <input type="search" value={query} onChange={event => setQuery(event.target.value)}
          placeholder={tr("설정 검색", "Search settings")} aria-label={tr("설정 검색", "Search settings")}
          className="h-11 w-full rounded-md pl-9 pr-12 text-sm" style={SETTINGS_INPUT_STYLE} data-testid="settings-search-input" />
        {query ? <button type="button" onClick={() => setQuery("")}
          className="absolute right-0 top-0 grid h-11 w-11 place-items-center rounded-md text-th-text-muted hover:text-th-text-primary"
          aria-label={tr("검색 지우기", "Clear search")}><X size={14} /></button> : null}
      </div>
      {queryActive ? <p className="mb-3 text-xs leading-5 break-keep text-th-text-muted" role="status" data-testid="settings-search-summary">
        {tr(`이 탭에서 ${matchingCount}개 항목이 일치해요`, `${matchingCount} matches in this tab`)}
      </p> : null}
      <div role="tablist" aria-label={tr("설정 패널", "Settings panels")}
        className="-mx-4 flex gap-1.5 overflow-x-auto px-4 pb-1 [scrollbar-width:none] md:mx-0 md:flex-col md:gap-1 md:overflow-visible md:px-0 md:pb-0">
        {items.length ? items.map(item => {
          const active = activePanel === item.id;
          return <button key={item.id} id={`settings-tab-${item.id}`} type="button" role="tab"
            aria-selected={active} aria-current={active ? "page" : undefined} aria-controls="settings-panel-content"
            title={item.detail} onClick={() => onPanelChange(item.id)}
            className={`flex min-h-11 shrink-0 items-center gap-2 whitespace-nowrap break-keep rounded-full border px-3 text-sm transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--th-accent-primary)] md:justify-between md:rounded-md md:border-transparent md:px-3 ${active ? "border-th-border bg-surface-medium font-semibold text-th-text-primary" : "border-th-border-subtle font-medium text-th-text-muted hover:bg-surface-subtle hover:text-th-text-primary"}`}>
            <span>{item.title}</span>
            {item.attention > 0 ? <span
              className="inline-flex shrink-0 items-center rounded-full px-1.5 text-[11px] font-medium tabular-nums"
              style={{ background: "var(--th-badge-amber-bg)", color: "var(--th-badge-amber-text)" }}
              aria-label={tr(`확인 필요 ${item.attention}개`, `${item.attention} need attention`)}
              data-testid={`settings-tab-attention-${item.id}`}>
              {tr(`확인 ${item.attention}`, `${item.attention} to check`)}
            </span> : null}
          </button>;
        }) : <p className="py-2 text-sm break-keep text-th-text-muted">{tr("일치하는 설정이 없어요.", "No settings match.")}</p>}
      </div>
    </aside>
  );
}
