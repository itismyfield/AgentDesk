import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import { X } from "lucide-react";
import { useI18n } from "../../../i18n";

export type ToastVariant = "info" | "success" | "warning" | "error";

export interface ToastInput {
  message: string;
  variant?: ToastVariant;
  durationMs?: number;
}

export interface ToastItem extends ToastInput {
  id: string;
  ts: number;
}

interface ToastContextValue {
  show: (input: ToastInput | string) => string;
  dismiss: (id: string) => void;
  clear: () => void;
}

const ToastContext = createContext<ToastContextValue | null>(null);

const DEFAULT_DURATION_MS = 5000;
const MAX_VISIBLE = 4;

const VARIANT_TONE: Record<ToastVariant, string> = {
  info: "var(--info)",
  success: "var(--ok)",
  warning: "var(--warn)",
  error: "var(--err)",
};

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<ToastItem[]>([]);
  const counterRef = useRef(0);

  const dismiss = useCallback((id: string) => {
    setToasts((prev) => prev.filter((toast) => toast.id !== id));
  }, []);

  const show = useCallback((input: ToastInput | string) => {
    const normalized: ToastInput = typeof input === "string" ? { message: input } : input;
    counterRef.current += 1;
    const id = `t${Date.now().toString(36)}-${counterRef.current}`;
    const item: ToastItem = {
      id,
      ts: Date.now(),
      message: normalized.message,
      variant: normalized.variant ?? "info",
      durationMs: normalized.durationMs ?? DEFAULT_DURATION_MS,
    };
    setToasts((prev) => [...prev, item]);
    return id;
  }, []);

  const clear = useCallback(() => setToasts([]), []);

  useEffect(() => {
    if (toasts.length === 0) return;
    const timer = window.setInterval(() => {
      const now = Date.now();
      setToasts((prev) =>
        prev.filter((toast) => now - toast.ts < (toast.durationMs ?? DEFAULT_DURATION_MS)),
      );
    }, 500);
    return () => window.clearInterval(timer);
  }, [toasts.length]);

  const value = useMemo<ToastContextValue>(() => ({ show, dismiss, clear }), [show, dismiss, clear]);

  return (
    <ToastContext.Provider value={value}>
      {children}
      <ToastViewport toasts={toasts} onDismiss={dismiss} />
    </ToastContext.Provider>
  );
}

export function useToast(): ToastContextValue {
  const ctx = useContext(ToastContext);
  if (!ctx) {
    throw new Error("useToast must be used within <ToastProvider> (typically wired in OverlayProvider)");
  }
  return ctx;
}

interface ToastViewportProps {
  toasts: ToastItem[];
  onDismiss: (id: string) => void;
}

function ToastViewport({ toasts, onDismiss }: ToastViewportProps) {
  const { t } = useI18n();
  if (typeof document === "undefined") return null;
  const visible = toasts.slice(-MAX_VISIBLE);
  if (visible.length === 0) return null;

  return createPortal(
    <div
      className="pointer-events-none fixed left-3 right-3 z-[110] flex max-w-sm flex-col gap-2 bottom-[calc(4.5rem+env(safe-area-inset-bottom))] sm:bottom-4 sm:left-auto sm:right-4"
      role="region"
      aria-label={t({ ko: "알림", en: "Notifications", ja: "通知", zh: "通知" })}
      aria-live="polite"
    >
      {visible.map((toast) => {
        const variant = toast.variant ?? "info";
        return (
          <div
            key={toast.id}
            role="status"
            className="pointer-events-auto flex items-start gap-2 rounded-xl border px-3 py-2 text-sm shadow-lg"
            style={{
              borderColor: "color-mix(in oklch, " + VARIANT_TONE[variant] + " 45%, var(--line))",
              background:
                "linear-gradient(135deg, color-mix(in oklch, " +
                VARIANT_TONE[variant] +
                " 14%, transparent), color-mix(in oklch, var(--bg-2) 94%, transparent))",
              color: "var(--fg)",
            }}
          >
            <span
              className="mt-1 h-2.5 w-2.5 shrink-0 rounded-full"
              style={{ background: VARIANT_TONE[variant] }}
              aria-hidden="true"
            />
            <p className="min-w-0 flex-1 break-words text-xs leading-relaxed">{toast.message}</p>
            <button
              type="button"
              onClick={() => onDismiss(toast.id)}
              className="flex h-7 w-7 max-[899px]:h-11 max-[899px]:w-11 max-[899px]:-my-1.5 max-[899px]:-mr-1 pointer-coarse:h-11 pointer-coarse:w-11 pointer-coarse:-my-1.5 pointer-coarse:-mr-1 shrink-0 items-center justify-center rounded-lg border text-[var(--fg-muted)] hover:bg-[var(--th-overlay-light)] hover:text-[var(--fg)] transition-colors"
              style={{
                borderColor: "color-mix(in oklch, var(--line) 64%, transparent)",
              }}
              aria-label={t({ ko: "닫기", en: "Dismiss", ja: "閉じる", zh: "关闭" })}
            >
              <X size={12} />
            </button>
          </div>
        );
      })}
    </div>,
    document.body,
  );
}
