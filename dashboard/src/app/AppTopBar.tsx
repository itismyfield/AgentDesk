import {
  Bell,
  BellRing,
  ChevronRight,
  Moon,
  Palette,
  Search,
  Sun,
  X,
  WifiOff,
} from "lucide-react";
import type { Dispatch, SetStateAction } from "react";
import type { Notification } from "../components/NotificationCenter";
import type { ThemePreference } from "./themePreferences";
import type { AppRouteEntry } from "./routes";
import { formatRelativeTime, notificationColor } from "./shellFormatting";
import { DashboardLogout } from "./DashboardAuthGate";

interface AppTopBarProps {
  currentRoute: AppRouteEntry | null;
  wsConnected: boolean;
  dismissNotification: (id: string) => void;
  isKo: boolean;
  isMobileViewport: boolean;
  headerZIndex: number;
  notificationBadgeCount: number;
  popoverZIndex: number;
  recentNotifications: Notification[];
  resolvedTheme: Exclude<ThemePreference, "auto">;
  setShowCommandPalette: Dispatch<SetStateAction<boolean>>;
  setShowNotificationPanel: Dispatch<SetStateAction<boolean>>;
  setShowTweaksPanel: Dispatch<SetStateAction<boolean>>;
  showNotificationPanel: boolean;
  toggleShellTheme: () => void;
}

const ICON_BUTTON_CLASS =
  "relative flex h-11 w-11 shrink-0 items-center justify-center rounded-md border border-th-border-subtle transition-colors hover:bg-surface-subtle focus-visible:outline-2 focus-visible:outline-[var(--th-accent-primary)]";

export function AppTopBar({
  wsConnected,
  currentRoute,
  dismissNotification,
  isKo,
  isMobileViewport,
  headerZIndex,
  notificationBadgeCount,
  popoverZIndex,
  recentNotifications,
  resolvedTheme,
  setShowCommandPalette,
  setShowNotificationPanel,
  setShowTweaksPanel,
  showNotificationPanel,
  toggleShellTheme,
}: AppTopBarProps) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const routeLabel = currentRoute ? (isKo ? currentRoute.labelKo : currentRoute.labelEn) : tr("홈", "Home");
  const themeLabel = tr(
    resolvedTheme === "dark" ? "밝은 화면으로 전환" : "어두운 화면으로 전환",
    resolvedTheme === "dark" ? "Switch to light theme" : "Switch to dark theme",
  );

  return (
    <header
      data-testid="topbar"
      className="relative flex min-h-14 shrink-0 flex-col justify-center border-b px-4 py-1.5 sm:min-h-16 sm:px-5 sm:py-2"
      style={{
        zIndex: headerZIndex,
        borderColor: "var(--th-border-subtle)",
        background: "var(--th-bg-surface)",
      }}
    >
      <div className="flex w-full items-center gap-2">
        {isMobileViewport ? (
          <h1
            data-testid="topbar-title"
            className="min-w-0 flex-1 truncate text-base font-semibold"
            style={{ color: "var(--th-text-heading)" }}
          >
            {routeLabel}
          </h1>
        ) : (
          <div
            data-testid="topbar-breadcrumb"
            className="flex min-w-0 flex-1 items-center gap-2 overflow-hidden text-[12px] font-medium"
            style={{ color: "var(--th-text-muted)" }}
          >
            <span className="shrink-0">AgentDesk</span>
            <ChevronRight size={12} className="shrink-0" />
            <span className="truncate whitespace-nowrap">{routeLabel}</span>
          </div>
        )}

        {isMobileViewport ? (
          <button
            type="button"
            data-testid="topbar-search"
            onClick={() => setShowCommandPalette(true)}
            className={ICON_BUTTON_CLASS}
            aria-label={tr("검색 열기", "Open search")}
          >
            <Search size={18} />
          </button>
        ) : (
          <label
            data-testid="topbar-search"
            className="flex min-h-11 w-full max-w-[18rem] items-center gap-2 rounded-md border px-3 text-sm"
            style={{
              borderColor: "var(--th-border-subtle)",
              background: "color-mix(in srgb, var(--th-bg-surface) 82%, transparent)",
            }}
          >
            <Search size={15} style={{ color: "var(--th-text-muted)" }} />
            <input
              type="search"
              readOnly
              value=""
              onFocus={() => setShowCommandPalette(true)}
              onClick={() => setShowCommandPalette(true)}
              placeholder={tr("검색…", "Search…")}
              className="h-11 w-full min-w-0 bg-transparent text-sm outline-none"
              style={{ color: "var(--th-text-primary)" }}
              aria-label={tr("검색 열기", "Open search")}
            />
            <kbd
              className="inline-flex rounded-lg px-2 py-1 text-[11px]"
              style={{ background: "var(--th-overlay-subtle)", color: "var(--th-text-muted)" }}
            >
              ⌘K
            </kbd>
          </label>
        )}

        <div className="flex shrink-0 items-center justify-end gap-2">
          <DashboardLogout />
          {!isMobileViewport ? (
            <button
              type="button"
              onClick={toggleShellTheme}
              className={ICON_BUTTON_CLASS}
              aria-label={themeLabel}
              title={themeLabel}
            >
              {resolvedTheme === "dark" ? <Sun size={18} /> : <Moon size={18} />}
            </button>
          ) : null}

          <div className="relative">
            <button
              type="button"
              onClick={() =>
                setShowNotificationPanel((prev) => {
                  if (!prev) setShowTweaksPanel(false);
                  return !prev;
                })
              }
              className={ICON_BUTTON_CLASS}
              aria-label={tr("알림 보기", "View notifications")}
              title={tr("알림 보기", "View notifications")}
            >
              {notificationBadgeCount > 0 ? <BellRing size={18} /> : <Bell size={18} />}
              {notificationBadgeCount > 0 && (
                <span className="absolute -right-1 -top-1 flex h-5 min-w-5 items-center justify-center rounded-full bg-emerald-500 px-1 text-[10px] font-semibold text-white">
                  {notificationBadgeCount > 9 ? "9+" : notificationBadgeCount}
                </span>
              )}
            </button>

            {showNotificationPanel && (
              <div
                role="dialog" aria-label={tr("알림", "Notifications")}
                className="fixed inset-x-4 top-16 max-h-[calc(100dvh-10rem)] overflow-auto rounded-lg border p-4 shadow-lg sm:absolute sm:inset-x-auto sm:right-0 sm:top-14 sm:w-[22rem]"
                style={{
                  zIndex: popoverZIndex,
                  borderColor: "var(--th-border-subtle)",
                  background: "var(--th-bg-surface)",
                }}
              >
                <div className="flex items-center justify-between gap-3 px-1 pb-2">
                  <div>
                    <div
                      className="text-sm font-semibold"
                      style={{ color: "var(--th-text-heading)" }}
                    >
                      {tr("알림", "Notifications")}
                    </div>
                    <div
                      className="text-xs"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {tr("최근 1분 동안의 이벤트", "Events from the last minute")}
                    </div>
                  </div>
                  <button
                    type="button"
                    onClick={() => setShowNotificationPanel(false)}
                    className="flex h-11 w-11 items-center justify-center rounded-xl text-[var(--th-text-muted)]"
                    aria-label={tr("알림 창 닫기", "Close notification panel")}
                  >
                    <X size={14} />
                  </button>
                </div>

                <div className="mt-3 space-y-2">
                  {recentNotifications.length === 0 ? (
                    <div
                      className="rounded-md border px-3 py-4 text-sm"
                      style={{
                        borderColor: "var(--th-border-subtle)",
                        color: "var(--th-text-muted)",
                        background: "var(--th-overlay-subtle)",
                      }}
                    >
                      {tr("새 알림이 없습니다.", "No recent notifications.")}
                    </div>
                  ) : (
                    recentNotifications.map((notification) => (
                      <div
                        key={notification.id}
                        className="rounded-md border px-3 py-3"
                        style={{
                          borderColor: "var(--th-border-subtle)",
                          background: "var(--th-overlay-subtle)",
                        }}
                      >
                        <div className="flex items-start gap-3">
                          <span
                            className="mt-1 h-2.5 w-2.5 rounded-full"
                            style={{ background: notificationColor(notification.type) }}
                          />
                          <div className="min-w-0 flex-1">
                            <div
                              className="text-sm leading-relaxed"
                              style={{ color: "var(--th-text-primary)" }}
                            >
                              {notification.message}
                            </div>
                            <div
                              className="mt-1 text-[11px]"
                              style={{ color: "var(--th-text-muted)" }}
                            >
                              {formatRelativeTime(notification.ts, isKo)}
                            </div>
                          </div>
                          <button
                            type="button"
                            onClick={() => dismissNotification(notification.id)}
                            className="flex h-11 w-11 items-center justify-center rounded-xl text-[var(--th-text-muted)]"
                            aria-label={tr("알림 지우기", "Dismiss notification")}
                          >
                            <X size={12} />
                          </button>
                        </div>
                      </div>
                    ))
                  )}
                </div>

              </div>
            )}
          </div>

          {!isMobileViewport ? (
            <button
              type="button"
              onClick={() =>
                setShowTweaksPanel((prev) => {
                  if (!prev) setShowNotificationPanel(false);
                  return !prev;
                })
              }
              className={ICON_BUTTON_CLASS}
              aria-label={tr("화면 꾸미기", "Appearance")}
              title={tr("화면 꾸미기", "Appearance")}
            >
              <Palette size={18} />
            </button>
          ) : null}
        </div>
      </div>
      {!wsConnected ? (
        <div data-testid="connection-status" role="status" className="mt-2 flex w-full items-start gap-2 border-t border-th-border-subtle pt-2 pb-1 text-xs leading-5 text-[var(--th-badge-amber-text)]">
          <WifiOff size={14} className="mt-0.5 shrink-0" aria-hidden="true" />
          <span>{tr("서버 연결 끊김 · 재연결 중입니다.", "Server disconnected · Reconnecting.")}</span>
        </div>
      ) : null}
    </header>
  );
}
