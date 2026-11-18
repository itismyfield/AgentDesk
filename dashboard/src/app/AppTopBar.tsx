import { Bell, BellRing, Moon, Palette, Search, Sun, X, WifiOff } from "lucide-react";
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

const FOCUS_CLASS = "focus-visible:outline-2 focus-visible:outline-[var(--th-accent-primary)]";
// Desktop: 36px bordered buttons, 44px on touch screens. Phone: 44px ghost buttons.
const DESKTOP_ICON_BUTTON_CLASS = `relative grid h-9 w-9 shrink-0 place-items-center rounded-[var(--r-sm)] border border-th-border bg-th-card-bg text-th-text-primary transition-colors hover:bg-[var(--bg-3)] pointer-coarse:h-11 pointer-coarse:w-11 ${FOCUS_CLASS}`;
const PHONE_ICON_BUTTON_CLASS = `relative grid h-11 w-11 shrink-0 place-items-center rounded-[var(--r-sm)] text-th-text-primary transition-colors hover:bg-surface-subtle ${FOCUS_CLASS}`;

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
  const iconButtonClass = isMobileViewport ? PHONE_ICON_BUTTON_CLASS : DESKTOP_ICON_BUTTON_CLASS;
  const notificationLabel = notificationBadgeCount > 0
    ? tr(`알림 ${notificationBadgeCount}개`, `${notificationBadgeCount} notifications`)
    : tr("알림", "Notifications");

  return (
    <header
      data-testid="topbar"
      className="relative shrink-0 bg-th-bg-primary sm:border-b sm:border-th-border-subtle"
      style={{ zIndex: headerZIndex }}
    >
      <div className={`flex h-14 w-full items-center ${isMobileViewport ? "gap-1 pr-2 pl-4" : "gap-3 px-6"}`}>
        {isMobileViewport ? (
          <h1
            data-testid="topbar-title"
            className="min-w-0 flex-1 truncate text-xl leading-7 font-semibold text-th-text-heading"
          >
            {routeLabel}
          </h1>
        ) : (
          <div
            data-testid="topbar-breadcrumb"
            className="min-w-0 flex-1 truncate text-[13px] font-medium text-th-text-primary"
          >
            {routeLabel}
          </div>
        )}

        {isMobileViewport && wsConnected ? (
          <span className="mr-1 flex shrink-0 items-center gap-1.5 text-xs text-th-text-muted">
            <span aria-hidden className="h-2 w-2 rounded-full bg-[var(--ok)]" />
            {tr("실시간", "Live")}
          </span>
        ) : null}

        {isMobileViewport ? (
          <button
            type="button"
            data-testid="topbar-search"
            onClick={() => setShowCommandPalette(true)}
            className={iconButtonClass}
            aria-label={tr("찾기", "Search")}
          >
            <Search size={20} aria-hidden />
          </button>
        ) : (
          <button
            type="button"
            data-testid="topbar-search"
            onClick={() => setShowCommandPalette(true)}
            aria-label={tr("찾기", "Search")}
            aria-keyshortcuts="Meta+K Control+K"
            className={`flex h-9 w-[280px] min-w-0 shrink items-center gap-2 rounded-[var(--r-sm)] border border-th-border bg-th-card-bg px-3 text-[13px] text-th-text-muted transition-colors hover:bg-[var(--bg-3)] pointer-coarse:h-11 ${FOCUS_CLASS}`}
          >
            <Search size={16} aria-hidden className="shrink-0" />
            <span className="min-w-0 flex-1 truncate text-left">{tr("찾기", "Search")}</span>
            <kbd className="shrink-0 rounded-[5px] border border-th-border px-1.5 font-mono text-[11px] leading-4">⌘K</kbd>
          </button>
        )}

        <div className={`flex shrink-0 items-center justify-end ${isMobileViewport ? "gap-0" : "gap-2"}`}>
          <DashboardLogout />
          {!isMobileViewport ? (
            <button
              type="button"
              onClick={toggleShellTheme}
              className={iconButtonClass}
              aria-label={themeLabel}
              title={themeLabel}
            >
              {resolvedTheme === "dark" ? <Sun size={18} aria-hidden /> : <Moon size={18} aria-hidden />}
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
              className={iconButtonClass}
              aria-label={notificationLabel}
              aria-expanded={showNotificationPanel}
              title={notificationLabel}
            >
              {notificationBadgeCount > 0 ? <BellRing size={18} aria-hidden /> : <Bell size={18} aria-hidden />}
              {notificationBadgeCount > 0 && (
                <span aria-hidden className="absolute -top-1 -right-1 grid h-4 min-w-4 place-items-center rounded-full bg-[var(--accent-soft)] px-1 text-[10px] font-semibold text-[var(--th-badge-blue-text)]">
                  {notificationBadgeCount > 9 ? "9+" : notificationBadgeCount}
                </span>
              )}
            </button>

            {showNotificationPanel && (
              <div
                role="dialog" aria-label={tr("알림", "Notifications")}
                className="fixed inset-x-4 top-16 max-h-[calc(100dvh-10rem)] overflow-auto rounded-lg border p-4 shadow-lg sm:absolute sm:inset-x-auto sm:right-0 sm:top-11 sm:w-[22rem]"
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
              className={iconButtonClass}
              aria-label={tr("화면 꾸미기", "Appearance")}
              title={tr("화면 꾸미기", "Appearance")}
            >
              <Palette size={18} aria-hidden />
            </button>
          ) : null}
        </div>
      </div>
      {!wsConnected ? (
        <div data-testid="connection-status" role="status" className="flex w-full items-start gap-2 border-t border-th-border-subtle px-4 py-2 text-xs leading-5 text-[var(--th-badge-amber-text)] sm:px-6">
          <WifiOff size={14} className="mt-0.5 shrink-0" aria-hidden="true" />
          <span>{tr("서버 연결 끊김 · 재연결 중입니다.", "Server disconnected · Reconnecting.")}</span>
        </div>
      ) : null}
    </header>
  );
}
