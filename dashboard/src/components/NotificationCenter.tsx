import { useState, useCallback, useRef } from "react";
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

  return { notifications, pushNotification, dismissNotification };
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
        className="relative w-10 h-10 rounded-lg flex items-center justify-center text-gray-500 hover:text-gray-300 hover:bg-gray-800 transition-colors"
        title={t({ ko: "알림", en: "Notifications" })}
      >
        <Bell size={20} />
        {unread > 0 && (
          <span className="absolute -top-1 -right-1 bg-red-500 text-white text-[10px] w-4 h-4 rounded-full flex items-center justify-center">
            {unread > 9 ? "9+" : unread}
          </span>
        )}
      </button>

      {open && (
        <div
          className="absolute left-12 bottom-0 w-80 max-h-96 overflow-auto rounded-xl border border-gray-700 bg-gray-900 shadow-2xl z-50"
          style={{ minHeight: 100 }}
        >
          <div className="sticky top-0 bg-gray-900 border-b border-gray-700 px-3 py-2 flex items-center justify-between">
            <span className="text-sm font-semibold text-gray-300">{t({ ko: "알림 센터", en: "Notification Center" })}</span>
            <button onClick={() => setOpen(false)} className="text-gray-500 hover:text-gray-300">
              <X size={14} />
            </button>
          </div>
          {notifications.length === 0 ? (
            <div className="px-3 py-6 text-center text-gray-500 text-sm">{t({ ko: "알림이 없습니다", en: "No notifications" })}</div>
          ) : (
            <ul className="divide-y divide-gray-800">
              {notifications.slice(0, 30).map((n) => (
                <li key={n.id} className="px-3 py-2 flex items-start gap-2 hover:bg-gray-800/50">
                  <span
                    className="mt-1.5 w-2 h-2 rounded-full shrink-0"
                    style={{ background: TYPE_COLORS[n.type] }}
                  />
                  <div className="flex-1 min-w-0">
                    <div className="text-xs text-gray-300 break-words">{n.message}</div>
                    <div className="text-[10px] text-gray-600 mt-0.5">
                      {new Date(n.ts).toLocaleTimeString(locale, { hour: "2-digit", minute: "2-digit" })}
                    </div>
                  </div>
                  <button
                    onClick={() => onDismiss(n.id)}
                    className="text-gray-600 hover:text-gray-400 shrink-0"
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
