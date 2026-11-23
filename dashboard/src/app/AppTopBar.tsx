import { Bell, Moon, Palette, Search, Sun, WifiOff, X } from "lucide-react";
import type { CSSProperties, Dispatch, MouseEvent, SetStateAction } from "react";
import { Cap, CardNote, CountBadge, Row, StatusDot } from "../components/common/KitPrimitives";
import type { Notification } from "../components/NotificationCenter";
import { IconButton } from "../kit/free/icon-button-clean";
import { Kbd } from "../kit/free/kbd-clean";
import { DashboardLogout } from "./DashboardAuthGate";
import type { AppRouteEntry } from "./routes";
import { formatRelativeTime, notificationTone } from "./shellFormatting";
import type { ThemePreference } from "./themePreferences";

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

const FOCUS_CLASS = "focus-visible:outline-2 focus-visible:outline-[var(--accent)]";

// Desktop: 36px bordered buttons (44px on coarse pointers via kit media rule). Phone: 44px ghost buttons.
const DESKTOP_ICON_BUTTON_STYLE = {
  "--size": "36px",
  "--icon": "16px",
  "--radius": "var(--r-sm)",
  "--ui-icbc-surface": "var(--bg-2)",
  "--ui-icbc-icon": "var(--fg)",
  "--ui-icbc-ink": "var(--fg)",
  "--ui-icbc-shadow": "inset 0 0 0 1px var(--line)",
  "--ui-icbc-shadow-hover":
    "inset 0 0 0 1px var(--line), inset 0 0 0 99px color-mix(in oklch, var(--fg) 5%, transparent)",
  "--ui-icbc-line": "var(--line)",
  "--ui-icbc-pressed": "var(--bg-3)",
  "--ui-icbc-focus": "var(--accent)",
} as CSSProperties;

const PHONE_ICON_BUTTON_STYLE = {
  "--size": "44px",
  "--icon": "16px",
  "--radius": "var(--r-sm)",
  "--ui-icbc-icon": "var(--fg-dim)",
  "--ui-icbc-ink": "var(--fg)",
  "--ui-icbc-wash": "color-mix(in oklch, var(--fg) 5%, transparent)",
  "--ui-icbc-pressed": "var(--bg-3)",
  "--ui-icbc-focus": "var(--accent)",
} as CSSProperties;

const SEARCH_KBD_STYLE = {
  "--ui-kbdc-bg": "transparent",
  "--ui-kbdc-edge": "var(--line)",
  "--h": "18px",
  "--pad": "5px",
  "--aui-radius-sm": "6px",
  "--aui-font": "var(--font-mono)",
  boxShadow: "inset 0 0 0 1px var(--line)",
  fontSize: "11px",
  color: "var(--fg-muted)",
} as CSSProperties;

const TOPBAR_COUNT_BADGE_STYLE = {
  "--h": "16px",
  padding: "0 4px",
  fontSize: "10px",
  fontWeight: 600,
  boxShadow: "0 0 0 2px var(--bg)",
} as CSSProperties;

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
  const iconButtonVariant = isMobileViewport ? "ghost" : "secondary";
  const iconButtonStyle = isMobileViewport ? PHONE_ICON_BUTTON_STYLE : DESKTOP_ICON_BUTTON_STYLE;
  const notificationLabel =
    notificationBadgeCount > 0
      ? tr(`알림 ${notificationBadgeCount}개`, `${notificationBadgeCount} notifications`)
      : tr("알림", "Notifications");

  // Safari does not focus a clicked button; focus it so closing the palette can return here.
  const openCommandPalette = (event: MouseEvent<HTMLButtonElement>) => {
    event.currentTarget.focus();
    setShowCommandPalette(true);
  };

  return (
    <header
      data-testid="topbar"
      className="relative shrink-0 bg-[var(--bg)] sm:border-b sm:border-[var(--line-soft)]"
      style={{ zIndex: headerZIndex }}
    >
      <div className={`flex h-14 w-full items-center ${isMobileViewport ? "gap-1 pr-2 pl-4" : "gap-3 px-6"}`}>
        {isMobileViewport ? (
          <h1
            data-testid="topbar-title"
            className="m-0 min-w-0 flex-1 truncate text-xl leading-7 font-semibold tracking-[-0.01em] text-[var(--fg)]"
          >
            {routeLabel}
          </h1>
        ) : (
          <div
            data-testid="topbar-breadcrumb"
            className="flex min-w-0 flex-1 items-center gap-2 truncate text-[13px] leading-5 text-[var(--fg-muted)]"
          >
            <b className="truncate font-medium text-[var(--fg)]">{routeLabel}</b>
          </div>
        )}

        {isMobileViewport && wsConnected ? (
          <Cap className="mr-1 flex shrink-0 items-center gap-1.5">
            <StatusDot tone="ok" />
            {tr("실시간", "Live")}
          </Cap>
        ) : null}

        {isMobileViewport ? (
          <IconButton
            data-testid="topbar-search"
            variant="ghost"
            label={tr("찾기", "Search")}
            icon={<Search />}
            style={PHONE_ICON_BUTTON_STYLE}
            onClick={openCommandPalette}
          />
        ) : (
          <button
            type="button"
            data-testid="topbar-search"
            onClick={openCommandPalette}
            aria-label={tr("찾기", "Search")}
            aria-keyshortcuts="Meta+K Control+K"
            className={`ml-auto flex h-9 w-[280px] min-w-0 shrink items-center gap-2 rounded-[var(--r-sm)] border border-[var(--line)] bg-[var(--bg-2)] px-3 text-[13px] leading-5 text-[var(--fg-muted)] transition-colors hover:bg-[var(--bg-3)] pointer-coarse:h-11 ${FOCUS_CLASS}`}
          >
            <Search size={16} aria-hidden className="shrink-0" />
            <span className="min-w-0 flex-1 truncate text-left">{tr("찾기", "Search")}</span>
            <Kbd keys={["mod", "K"]} style={SEARCH_KBD_STYLE} />
          </button>
        )}

        <DashboardLogout />

        {!isMobileViewport ? (
          <IconButton
            variant="secondary"
            label={themeLabel}
            icon={resolvedTheme === "dark" ? <Sun /> : <Moon />}
            style={DESKTOP_ICON_BUTTON_STYLE}
            onClick={toggleShellTheme}
          />
        ) : null}

        <div className="relative flex shrink-0">
          <IconButton
            variant={iconButtonVariant}
            label={notificationLabel}
            aria-expanded={showNotificationPanel}
            icon={<Bell />}
            style={iconButtonStyle}
            onClick={() =>
              setShowNotificationPanel((prev) => {
                if (!prev) setShowTweaksPanel(false);
                return !prev;
              })
            }
          />
          {notificationBadgeCount > 0 ? (
            <span aria-hidden className="pointer-events-none absolute -top-1 -right-1 flex">
              <CountBadge count={notificationBadgeCount} max={9} tone="warn" style={TOPBAR_COUNT_BADGE_STYLE} />
            </span>
          ) : null}

          {showNotificationPanel ? (
            <div
              role="dialog"
              aria-label={tr("알림", "Notifications")}
              className="fixed inset-x-4 top-16 max-h-[calc(100dvh-10rem)] overflow-auto rounded-[var(--r-md)] border border-[var(--line-soft)] bg-[var(--bg-2)] text-[var(--fg)] shadow-[var(--shadow-md)] sm:absolute sm:inset-x-auto sm:top-11 sm:right-0 sm:w-[22rem]"
              style={{ zIndex: popoverZIndex }}
            >
              <div className="flex items-center justify-between gap-3 px-4 py-3">
                <div className="min-w-0">
                  <div className="text-sm leading-5 font-semibold text-[var(--fg)]">
                    {tr("알림", "Notifications")}
                  </div>
                  <Cap className="block">
                    {tr("최근 1분 동안의 이벤트", "Events from the last minute")}
                  </Cap>
                </div>
                <IconButton
                  variant="ghost"
                  label={tr("알림 창 닫기", "Close notification panel")}
                  icon={<X />}
                  style={PHONE_ICON_BUTTON_STYLE}
                  onClick={() => setShowNotificationPanel(false)}
                />
              </div>

              {recentNotifications.length === 0 ? (
                <CardNote>{tr("새 알림이 없어요.", "No recent notifications.")}</CardNote>
              ) : (
                <div>
                  {recentNotifications.map((notification) => (
                    <Row key={notification.id} className="items-start border-t border-[var(--line-soft)] first:border-t">
                      <StatusDot tone={notificationTone(notification.type)} className="mt-1.5" />
                      <div className="min-w-0 flex-1">
                        <div className="text-sm leading-5 text-[var(--fg)]">{notification.message}</div>
                        <Cap className="mt-1 block">{formatRelativeTime(notification.ts, isKo)}</Cap>
                      </div>
                      <IconButton
                        variant="ghost"
                        label={tr("알림 지우기", "Dismiss notification")}
                        icon={<X />}
                        style={PHONE_ICON_BUTTON_STYLE}
                        className="-my-1.5 -mr-2"
                        onClick={() => dismissNotification(notification.id)}
                      />
                    </Row>
                  ))}
                </div>
              )}
            </div>
          ) : null}
        </div>

        {!isMobileViewport ? (
          <IconButton
            variant="secondary"
            label={tr("화면 꾸미기", "Appearance")}
            icon={<Palette />}
            style={DESKTOP_ICON_BUTTON_STYLE}
            onClick={() =>
              setShowTweaksPanel((prev) => {
                if (!prev) setShowNotificationPanel(false);
                return !prev;
              })
            }
          />
        ) : null}
      </div>
      {!wsConnected ? (
        <div
          data-testid="connection-status"
          role="status"
          className="flex w-full items-start gap-2 border-t border-[var(--line-soft)] px-4 py-2 text-xs leading-5 text-[var(--warn)] sm:px-6"
        >
          <WifiOff size={14} className="mt-0.5 shrink-0" aria-hidden="true" />
          <span>{tr("서버 연결 끊김 · 다시 연결하고 있어요.", "Server disconnected · Reconnecting.")}</span>
        </div>
      ) : null}
    </header>
  );
}
