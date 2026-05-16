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
  activePanel,
  inputStyle,
  items,
  matchingCount,
  onPanelChange,
  query,
  queryActive,
  setQuery,
  tr,
}: SettingsNavigationProps<TPanel>) {
  return (
    <aside className="settings-nav min-w-0 md:sticky md:top-4 md:self-start">
      <div className="relative mb-3">
        <Search
          size={13}
          className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2"
          style={{ color: "var(--th-text-muted)" }}
        />
        <input
          type="search"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder={tr("설정 검색", "Search settings")}
          aria-label={tr("설정 검색", "Search settings")}
          className="w-full rounded-xl py-2.5 pl-9 pr-3 text-sm"
          style={inputStyle}
          data-testid="settings-search-input"
        />
        {query ? (
          <button
            type="button"
            onClick={() => setQuery("")}
            className="absolute right-2 top-1/2 grid h-7 w-7 -translate-y-1/2 place-items-center rounded-lg"
            style={{ color: "var(--th-text-muted)" }}
            aria-label={tr("검색 지우기", "Clear search")}
          >
            <X size={13} />
          </button>
        ) : null}
      </div>

      {queryActive ? (
        <div
          className="settings-search-summary mb-3 rounded-xl border px-3 py-2 text-[11px] leading-5"
          style={{
            borderColor: "color-mix(in srgb, var(--th-border) 64%, transparent)",
            background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
            color: "var(--th-text-muted)",
          }}
          data-testid="settings-search-summary"
        >
          {tr(
            `현재 패널 ${matchingCount}개 항목 일치`,
            `${matchingCount} matches in this panel`,
          )}
        </div>
      ) : null}

      <div
        role="tablist"
        aria-label={tr("설정 패널", "Settings panels")}
        className="settings-nav-items -mx-1 flex gap-2 overflow-x-auto pb-1 md:mx-0 md:block md:space-y-1 md:overflow-visible md:pb-0"
      >
        {items.length > 0 ? (
          items.map((item) => (
            <PanelNavButton
              key={item.id}
              id={`settings-tab-${item.id}`}
              active={activePanel === item.id}
              title={item.title}
              detail={item.detail}
              count={item.count}
              ariaControls="settings-panel-content"
              onClick={() => onPanelChange(item.id)}
            />
          ))
        ) : (
          <SettingsEmptyState className="text-sm">
            {tr("검색 결과가 없습니다.", "No groups match the search.")}
          </SettingsEmptyState>
        )}
      </div>
    </aside>
  );
}

function PanelNavButton({
  id,
  active,
  title,
  detail,
  count,
  ariaControls,
  onClick,
}: {
  id: string;
  active: boolean;
  title: string;
  detail: string;
  count?: string;
  ariaControls?: string;
  onClick: () => void;
}) {
  return (
    <button
      id={id}
      type="button"
      onClick={onClick}
      role="tab"
      aria-selected={active}
      aria-current={active ? "page" : undefined}
      aria-controls={ariaControls}
      className="settings-nav-button min-h-[44px] w-[min(46vw,176px)] shrink-0 rounded-xl px-3 py-2.5 text-left transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[color:var(--th-accent-primary)] focus-visible:ring-offset-2 focus-visible:ring-offset-[color:var(--th-card-bg)] md:w-full md:px-2.5"
      style={{
        borderColor: "transparent",
        background: active
          ? "color-mix(in srgb, var(--th-overlay-medium) 92%, transparent)"
          : "transparent",
      }}
    >
      <div className="flex items-center gap-2 md:items-start md:gap-3">
        <span
          className="h-2 w-2 shrink-0 rounded-full md:mt-1"
          style={{
            background: active
              ? "var(--th-accent-primary)"
              : "color-mix(in srgb, var(--th-text-muted) 50%, transparent)",
          }}
        />
        <div className="min-w-0 flex-1">
          <div className="flex items-start justify-between gap-3">
            <div
              className="truncate text-sm font-semibold md:whitespace-normal"
              style={{
                color: active
                  ? "var(--th-accent-primary)"
                  : "var(--th-text-heading)",
              }}
            >
              {title}
            </div>
            {count && (
              <span
                className="settings-nav-count shrink-0 rounded-full border px-2 py-0.5 text-[10px] font-medium"
                style={{
                  borderColor:
                    "color-mix(in srgb, var(--th-border) 72%, transparent)",
                  background:
                    "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
                  color: active ? "var(--th-text)" : "var(--th-text-muted)",
                }}
              >
                {count}
              </span>
            )}
          </div>
          <div
            className="settings-nav-detail mt-1 hidden text-[11px] leading-5 md:block"
            style={{ color: "var(--th-text-muted)" }}
          >
            {detail}
          </div>
        </div>
      </div>
    </button>
  );
}
