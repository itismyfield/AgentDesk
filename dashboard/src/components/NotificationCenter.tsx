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

const TYPE_COLORS: Record<Notification["type"], string> = {
  info: "#60a5fa",
  success: "#34d399",
  warning: "#fbbf24",
  error: "#f87171",
};

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
