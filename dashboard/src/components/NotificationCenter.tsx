import { useState, useCallback, useEffect, useRef } from "react";
import { Bell, X } from "lucide-react";
import { useI18n } from "../i18n";

export interface Notification {
  id: string;
  message: string;
  type: "info" | "success" | "warning" | "error";
  ts: number;
}

export function useNotifications(maxItems = 50) {
  const [notifications, setNotifications] = useState<Notification[]>([]);
  const idRef = useRef(0);

  const pushNotification = useCallback(
    (message: string, type: Notification["type"] = "info") => {
      const id = `n-${++idRef.current}`;
      setNotifications((prev) => [{ id, message, type, ts: Date.now() }, ...prev].slice(0, maxItems));
    },
    [maxItems],
  );

  const dismissNotification = useCallback((id: string) => {
    setNotifications((prev) => prev.filter((n) => n.id !== id));
  }, []);

  return { notifications, pushNotification, dismissNotification } as const;
}

// ── Toast overlay: auto-dismiss ephemeral notifications ──

const TOAST_TTL_MS = 5000;

interface ToastOverlayProps {
  notifications: Notification[];
  onDismiss: (id: string) => void;
}

export function ToastOverlay({ notifications, onDismiss }: ToastOverlayProps) {
  const recent = notifications.filter((n) => Date.now() - n.ts < TOAST_TTL_MS);

  // Auto-dismiss timer
  useEffect(() => {
    if (recent.length === 0) return;
    const timer = setInterval(() => {
      const now = Date.now();
      for (const n of recent) {
        if (now - n.ts >= TOAST_TTL_MS) onDismiss(n.id);
      }
    }, 1000);
    return () => clearInterval(timer);
  }, [recent, onDismiss]);

  if (recent.length === 0) return null;

  return (
    <div className="fixed bottom-4 right-4 z-[100] flex flex-col gap-2 max-w-sm">
      {recent.slice(0, 5).map((n) => (
        <div
          key={n.id}
          className="flex items-start gap-2 rounded-lg px-3 py-2 shadow-lg text-sm animate-[toast-in_0.2s_ease-out]"
          style={{
            background: "var(--th-card-bg)",
            border: `1px solid ${TYPE_COLORS[n.type]}40`,
            color: "var(--th-text-primary)",
          }}
        >
          <span
            className="mt-1 w-2 h-2 rounded-full shrink-0"
            style={{ background: TYPE_COLORS[n.type] }}
          />
          <span className="flex-1 min-w-0 break-words text-xs">{n.message}</span>
          <button onClick={() => onDismiss(n.id)} className="shrink-0 w-11 h-11 flex items-center justify-center text-th-text-muted hover:text-th-text-primary" aria-label="Dismiss">
            <X size={12} />
          </button>
        </div>
      ))}
    </div>
  );
}

interface NotificationCenterProps {
  notifications: Notification[];
  onDismiss: (id: string) => void;
}

const TYPE_COLORS: Record<Notification["type"], string> = {
  info: "#60a5fa",
  success: "#34d399",
  warning: "#fbbf24",
  error: "#f87171",
};

export default function NotificationCenter({ notifications, onDismiss }: NotificationCenterProps) {
  const { t, locale } = useI18n();
  const [open, setOpen] = useState(false);
  const unread = notifications.filter((n) => Date.now() - n.ts < 60_000).length;

  return (
    <div className="relative">
      <button
        onClick={() => setOpen((o) => !o)}
        className="relative w-11 h-11 rounded-lg flex items-center justify-center text-th-text-muted hover:text-th-text-primary hover:bg-surface-hover transition-colors"
        title={t({ ko: "알림", en: "Notifications" })}
      >
        <Bell size={20} />
        {unread > 0 && (
          <span className="absolute -top-1 -right-1 bg-red-500 text-white text-xs w-4 h-4 rounded-full flex items-center justify-center">
            {unread > 9 ? "9+" : unread}
          </span>
        )}
      </button>

      {open && (
        <div
          className="absolute left-12 bottom-0 w-80 max-h-96 overflow-auto rounded-xl border border-th-border bg-th-bg-primary shadow-2xl z-50"
          style={{ minHeight: 100 }}
        >
          <div className="sticky top-0 bg-th-bg-primary border-b border-th-border px-3 py-2 flex items-center justify-between">
            <span className="text-sm font-semibold text-th-text-primary">{t({ ko: "알림 센터", en: "Notification Center" })}</span>
            <button onClick={() => setOpen(false)} className="w-11 h-11 flex items-center justify-center text-th-text-muted hover:text-th-text-primary" aria-label="Close">
              <X size={14} />
            </button>
          </div>
          {notifications.length === 0 ? (
            <div className="px-3 py-6 text-center text-th-text-muted text-sm">{t({ ko: "알림이 없습니다", en: "No notifications" })}</div>
          ) : (
            <ul className="divide-y divide-th-border">
              {notifications.slice(0, 30).map((n) => (
                <li key={n.id} className="px-3 py-2 flex items-start gap-2 hover:bg-surface-hover/50">
                  <span
                    className="mt-1.5 w-2 h-2 rounded-full shrink-0"
                    style={{ background: TYPE_COLORS[n.type] }}
                  />
                  <div className="flex-1 min-w-0">
                    <div className="text-xs text-th-text-primary break-words">{n.message}</div>
                    <div className="text-xs text-th-text-muted mt-0.5">
                      {new Date(n.ts).toLocaleTimeString(locale, { hour: "2-digit", minute: "2-digit" })}
                    </div>
                  </div>
                  <button
                    onClick={() => onDismiss(n.id)}
                    className="w-11 h-11 flex items-center justify-center text-th-text-muted hover:text-th-text-primary shrink-0"
                    aria-label="Dismiss"
                  >
                    <X size={12} />
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  );
}
