import { useCallback, useEffect, useRef, useState } from "react";
import { X } from "lucide-react";

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
      return id;
    },
    [maxItems],
  );

  const updateNotification = useCallback(
    (id: string, message: string, type?: Notification["type"]) => {
      setNotifications((prev) =>
        prev.map((notification) =>
          notification.id === id
            ? {
                ...notification,
                message,
                type: type ?? notification.type,
                ts: Date.now(),
              }
            : notification,
        ),
      );
    },
    [],
  );

  const dismissNotification = useCallback((id: string) => {
    setNotifications((prev) => prev.filter((notification) => notification.id !== id));
  }, []);

  return { notifications, pushNotification, updateNotification, dismissNotification } as const;
}

const TOAST_TTL_MS = 5000;

export const NOTIFICATION_TYPE_COLORS: Record<Notification["type"], string> = {
  info: "#60a5fa",
  success: "#34d399",
  warning: "#fbbf24",
  error: "#f87171",
};

const NOTIFICATION_TYPE_BACKGROUNDS: Record<Notification["type"], string> = {
  info: "rgba(96,165,250,0.14)",
  success: "rgba(52,211,153,0.14)",
  warning: "rgba(251,191,36,0.14)",
  error: "rgba(248,113,113,0.14)",
};

interface ToastOverlayProps {
  notifications: Notification[];
  onDismiss: (id: string) => void;
}

export function ToastOverlay({ notifications, onDismiss }: ToastOverlayProps) {
  const recent = notifications.filter((notification) => Date.now() - notification.ts < TOAST_TTL_MS);

  useEffect(() => {
    if (recent.length === 0) return;
    const timer = window.setInterval(() => {
      const now = Date.now();
      for (const notification of recent) {
        if (now - notification.ts >= TOAST_TTL_MS) {
          onDismiss(notification.id);
        }
      }
    }, 1000);
    return () => window.clearInterval(timer);
  }, [recent, onDismiss]);

  if (recent.length === 0) return null;

  return (
    <div className="fixed right-3 z-[100] flex max-w-sm flex-col gap-2 bottom-[calc(4.5rem+env(safe-area-inset-bottom))] sm:bottom-4 sm:right-4">
      {recent.slice(0, 4).map((notification) => (
        <div
          key={notification.id}
          className="flex items-start gap-2 rounded-xl border px-3 py-2 text-sm shadow-lg"
          style={{
            borderColor: `${NOTIFICATION_TYPE_COLORS[notification.type]}55`,
            background: `linear-gradient(135deg, ${NOTIFICATION_TYPE_BACKGROUNDS[notification.type]}, color-mix(in srgb, var(--th-surface) 92%, transparent))`,
            color: "var(--th-text)",
          }}
        >
          <span
            className="mt-1 h-2.5 w-2.5 shrink-0 rounded-full"
            style={{ background: NOTIFICATION_TYPE_COLORS[notification.type] }}
          />
          <div className="min-w-0 flex-1">
            <p className="break-words text-xs leading-relaxed">{notification.message}</p>
          </div>
          <button
            onClick={() => onDismiss(notification.id)}
            className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border text-[var(--th-text-muted)] transition-opacity hover:opacity-100"
            style={{
              background: "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
              borderColor: "color-mix(in srgb, var(--th-border) 64%, transparent)",
            }}
            aria-label="Dismiss notification"
          >
            <X size={12} />
          </button>
        </div>
      ))}
    </div>
  );
}
