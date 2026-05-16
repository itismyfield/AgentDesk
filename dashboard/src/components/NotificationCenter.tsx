import { useCallback, useRef, useState } from "react";
import type { ReactNode } from "react";
import { CheckCircle2, Info, TriangleAlert, XCircle } from "lucide-react";
import { Toaster, toast } from "sonner";

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
      showToast(id, message, type);
      return id;
    },
    [maxItems],
  );

  const updateNotification = useCallback(
    (id: string, message: string, type?: Notification["type"]) => {
      let toastType = type;
      setNotifications((prev) =>
        prev.map((notification) =>
          notification.id === id
            ? (() => {
                toastType = toastType ?? notification.type;
                return {
                ...notification,
                message,
                type: type ?? notification.type,
                ts: Date.now(),
                };
              })()
            : notification,
        ),
      );
      showToast(id, message, toastType ?? "info");
    },
    [],
  );

  const dismissNotification = useCallback((id: string) => {
    setNotifications((prev) => prev.filter((notification) => notification.id !== id));
    toast.dismiss(id);
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

const NOTIFICATION_TYPE_ICONS: Record<Notification["type"], ReactNode> = {
  info: <Info size={16} aria-hidden="true" />,
  success: <CheckCircle2 size={16} aria-hidden="true" />,
  warning: <TriangleAlert size={16} aria-hidden="true" />,
  error: <XCircle size={16} aria-hidden="true" />,
};

function showToast(
  id: string,
  message: string,
  type: Notification["type"],
) {
  const options = {
    id,
    duration: TOAST_TTL_MS,
    closeButton: true,
    icon: NOTIFICATION_TYPE_ICONS[type],
    style: {
      borderColor: `${NOTIFICATION_TYPE_COLORS[type]}55`,
      background: `linear-gradient(135deg, ${NOTIFICATION_TYPE_BACKGROUNDS[type]}, color-mix(in srgb, var(--th-surface) 94%, transparent))`,
      color: "var(--th-text)",
    },
  } as const;

  if (type === "success") {
    toast.success(message, options);
  } else if (type === "warning") {
    toast.warning(message, options);
  } else if (type === "error") {
    toast.error(message, options);
  } else {
    toast.info(message, options);
  }
}

interface ToastOverlayProps {
  notifications: Notification[];
  onDismiss: (id: string) => void;
}

export function ToastOverlay({ notifications: _notifications, onDismiss: _onDismiss }: ToastOverlayProps) {
  return (
    <Toaster
      closeButton
      expand
      richColors={false}
      position="bottom-right"
      visibleToasts={4}
      duration={TOAST_TTL_MS}
      mobileOffset={{
        bottom: "calc(4.75rem + env(safe-area-inset-bottom))",
        left: "0.75rem",
        right: "0.75rem",
      }}
      toastOptions={{
        closeButtonAriaLabel: "Dismiss notification",
        classNames: {
          toast: "agentdesk-toast",
          title: "agentdesk-toast-title",
          closeButton: "agentdesk-toast-close",
        },
      }}
      containerAriaLabel="Notifications"
    />
  );
}
