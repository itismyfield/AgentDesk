import {
  Bell,
  BellRing,
  ChevronRight,
  Moon,
  Search,
  Settings,
  Sparkles,
  Sun,
  X,
} from "lucide-react";
import type { Dispatch, SetStateAction } from "react";
import type { Notification } from "../components/NotificationCenter";
import type { ThemePreference } from "./themePreferences";
import type { AppRouteEntry } from "./routes";
import { formatRelativeTime, notificationColor } from "./shellFormatting";

interface AppTopBarProps {
  currentRoute: AppRouteEntry | null;
  dismissNotification: (id: string) => void;
  isKo: boolean;
  navigateToRoute: (path: string) => void;
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
  unresolvedMeetingsCount: number;
}

function NotificationSummaryRow({
  label,
  value,
  accent,
}: {
  label: string;
  value: number;
  accent: string;
}) {
  return (
    <div
      className="flex items-center justify-between rounded-2xl border px-3 py-2 text-sm"
      style={{
        borderColor: "var(--th-border-subtle)",
        background: "var(--th-overlay-subtle)",
      }}
    >
      <span style={{ color: "var(--th-text-muted)" }}>{label}</span>
      <span className="font-semibold" style={{ color: accent }}>
        {value}
      </span>
    </div>
  );
}

export function AppTopBar({
  currentRoute,
  dismissNotification,
  isKo,
  navigateToRoute,
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
  unresolvedMeetingsCount,
}: AppTopBarProps) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);

  return (
    <header
      data-testid="topbar"
      className="relative flex min-h-16 shrink-0 items-center border-b px-4 py-2 sm:px-5"
      style={{
        zIndex: headerZIndex,
        borderColor: "var(--th-border-subtle)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 93%, transparent) 100%)",
        backdropFilter: "blur(14px)",
      }}
    >
      <div className="flex w-full flex-wrap items-center gap-2 sm:flex-nowrap">
        <div className="min-w-0 basis-full sm:flex-1">
          <div
            data-testid="topbar-breadcrumb"
            className="flex min-w-0 items-center gap-2 overflow-hidden text-[12px] font-medium"
            style={{ color: "var(--th-text-muted)" }}
          >
            <span className="shrink-0">AgentDesk</span>
            <ChevronRight size={12} className="shrink-0" />
            <span className="truncate whitespace-nowrap">
              {currentRoute
                ? isKo
                  ? currentRoute.labelKo
                  : currentRoute.labelEn
                : isKo
                  ? "홈"
                  : "Home"}
            </span>
          </div>
        </div>

        <label
          data-testid="topbar-search"
          className="order-3 flex min-w-0 flex-1 items-center gap-2 rounded-2xl border px-3 py-2 text-sm sm:order-none sm:max-w-[18rem]"
          style={{
            borderColor: "var(--th-border-subtle)",
            background:
              "color-mix(in srgb, var(--th-bg-surface) 82%, transparent)",
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
            className="w-full bg-transparent text-sm outline-none"
            style={{ color: "var(--th-text-primary)" }}
            aria-label={tr("검색 열기", "Open search")}
          />
          <kbd
            className="hidden rounded-lg px-2 py-1 text-[11px] sm:inline-flex"
            style={{
              background: "var(--th-overlay-subtle)",
              color: "var(--th-text-muted)",
            }}
          >
            ⌘K
          </kbd>
        </label>

        <div className="ml-auto flex shrink-0 items-center justify-end gap-2 sm:ml-0">
          <button
            type="button"
            onClick={toggleShellTheme}
            className="flex h-9 w-9 items-center justify-center rounded-2xl border transition-colors hover:bg-white/5"
            style={{ borderColor: "var(--th-border-subtle)" }}
            aria-label={tr(
              resolvedTheme === "dark" ? "라이트 테마로 전환" : "다크 테마로 전환",
              resolvedTheme === "dark" ? "Switch to light theme" : "Switch to dark theme",
            )}
            title={tr(
              resolvedTheme === "dark" ? "라이트 테마로 전환" : "다크 테마로 전환",
              resolvedTheme === "dark" ? "Switch to light theme" : "Switch to dark theme",
            )}
          >
            {resolvedTheme === "dark" ? <Sun size={18} /> : <Moon size={18} />}
          </button>

          <div className="relative">
            <button
              type="button"
              onClick={() =>
                setShowNotificationPanel((prev) => {
                  if (!prev) setShowTweaksPanel(false);
                  return !prev;
                })
              }
              className="relative flex h-9 w-9 items-center justify-center rounded-2xl border transition-colors hover:bg-white/5"
              style={{ borderColor: "var(--th-border-subtle)" }}
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
                className="absolute left-0 top-12 w-[min(22rem,calc(100vw-1.5rem))] max-w-[calc(100vw-1.5rem)] rounded-3xl border p-3 shadow-2xl"
                style={{
                  zIndex: popoverZIndex,
                  borderColor: "var(--th-border-subtle)",
                  background:
                    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 95%, transparent) 100%)",
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
                      {tr(
                        "최근 이벤트와 회의 후속 상태",
                        "Recent events and meeting follow-ups",
                      )}
                    </div>
                  </div>
                  <button
                    type="button"
                    onClick={() => setShowNotificationPanel(false)}
                    className="flex h-8 w-8 items-center justify-center rounded-xl text-[var(--th-text-muted)]"
                    aria-label={tr("알림 창 닫기", "Close notification panel")}
                  >
                    <X size={14} />
                  </button>
                </div>

                <div className="space-y-2">
                  <NotificationSummaryRow
                    label={tr("미해결 회의", "Open meetings")}
                    value={unresolvedMeetingsCount}
                    accent="var(--th-accent-warn)"
                  />
                  <NotificationSummaryRow
                    label={tr("최근 알림", "Recent notifications")}
                    value={recentNotifications.length}
                    accent="var(--th-accent-info)"
                  />
                </div>

                <div className="mt-3 space-y-2">
                  {recentNotifications.length === 0 ? (
                    <div
                      className="rounded-2xl border px-3 py-4 text-sm"
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
                        className="rounded-2xl border px-3 py-3"
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
                            className="flex h-8 w-8 items-center justify-center rounded-xl text-[var(--th-text-muted)]"
                            aria-label={tr("알림 지우기", "Dismiss notification")}
                          >
                            <X size={12} />
                          </button>
                        </div>
                      </div>
                    ))
                  )}
                </div>

                <button
                  type="button"
                  onClick={() => {
                    setShowNotificationPanel(false);
                    navigateToRoute("/meetings");
                  }}
                  className="mt-3 flex w-full items-center justify-center gap-2 rounded-2xl border px-3 py-2 text-sm font-medium transition-colors hover:bg-white/5"
                  style={{ borderColor: "var(--th-border-subtle)" }}
                >
                  <Sparkles size={15} />
                  {tr("회의 페이지로 이동", "Open meetings page")}
                </button>
              </div>
            )}
          </div>

          <button
            type="button"
            onClick={() =>
              setShowTweaksPanel((prev) => {
                if (!prev) setShowNotificationPanel(false);
                return !prev;
              })
            }
            className="flex h-9 w-9 items-center justify-center rounded-2xl border transition-colors hover:bg-white/5"
            style={{ borderColor: "var(--th-border-subtle)" }}
            aria-label={tr("디자인 설정 열기", "Open tweaks")}
            title={tr("디자인 설정 열기", "Open tweaks")}
          >
            <Settings size={18} />
          </button>
        </div>
      </div>
    </header>
  );
}
