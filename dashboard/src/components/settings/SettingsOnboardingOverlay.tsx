import { Suspense, lazy, useEffect, useLayoutEffect, useRef } from "react";
import { createPortal } from "react-dom";
import { SettingsButton } from "./SettingsPrimitives";

const OnboardingWizard = lazy(() => import("../OnboardingWizard"));

const FOCUSABLE_SELECTOR =
  'a[href], button:not([disabled]), textarea:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])';

function isEditableTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  const tag = target.tagName;
  if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return true;
  return target.isContentEditable || target.getAttribute("contenteditable") === "true";
}

type SettingsOnboardingOverlayProps = {
  isKo: boolean;
  onClose: () => void;
  open: boolean;
  tr: (ko: string, en: string) => string;
};

export function SettingsOnboardingOverlay({
  isKo,
  onClose,
  open,
  tr,
}: SettingsOnboardingOverlayProps) {
  const rootRef = useRef<HTMLDivElement | null>(null);
  const onboardingDialogRef = useRef<HTMLDivElement | null>(null);
  const onboardingCloseButtonRef = useRef<HTMLButtonElement | null>(null);
  const closeRef = useRef(onClose);
  closeRef.current = onClose;

  useLayoutEffect(() => {
    if (!open || typeof document === "undefined") return;
    const root = rootRef.current;
    if (!root) return;
    const restore: Array<[HTMLElement, boolean]> = [];
    for (const child of Array.from(document.body.children)) {
      if (child instanceof HTMLElement && child !== root && !child.contains(root)) {
        restore.push([child, child.inert]);
        child.inert = true;
      }
    }
    return () => {
      restore.forEach(([element, inert]) => {
        element.inert = inert;
      });
    };
  }, [open]);

  useEffect(() => {
    if (!open || typeof window === "undefined") return;
    const previousOverflow = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    const previousActiveElement =
      document.activeElement instanceof HTMLElement ? document.activeElement : null;
    onboardingCloseButtonRef.current?.focus({ preventScroll: true });
    const focusCloseButton = window.setTimeout(() => {
      if (!onboardingDialogRef.current?.contains(document.activeElement)) {
        onboardingCloseButtonRef.current?.focus({ preventScroll: true });
      }
    }, 0);
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "/" && !isEditableTarget(event.target)) {
        event.preventDefault();
        event.stopImmediatePropagation();
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        closeRef.current();
        return;
      }
      if (event.key !== "Tab") return;
      const dialog = onboardingDialogRef.current;
      if (!dialog) return;
      const focusable = Array.from(dialog.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR));
      if (focusable.length === 0) {
        event.preventDefault();
        return;
      }
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };
    window.addEventListener("keydown", handleKeyDown, true);
    return () => {
      window.clearTimeout(focusCloseButton);
      window.removeEventListener("keydown", handleKeyDown, true);
      document.body.style.overflow = previousOverflow;
      if (previousActiveElement?.isConnected) {
        previousActiveElement.focus({ preventScroll: true });
      }
    };
  }, [open]);

  if (!open || typeof document === "undefined") return null;

  return createPortal(
    <div ref={rootRef} className="fixed inset-0 z-[90] overflow-y-auto bg-[var(--bg)] text-[var(--fg)]" role="dialog" aria-modal="true" aria-label={tr("초기 설정", "Initial setup")}>
      <div className="flex min-h-dvh items-start justify-center pt-[max(1rem,env(safe-area-inset-top))] pb-[max(2rem,env(safe-area-inset-bottom))] px-[max(0.75rem,env(safe-area-inset-left))]">
        <div ref={onboardingDialogRef} className="w-full max-w-3xl">
          <div className="mb-2 flex justify-end px-4">
            <SettingsButton ref={onboardingCloseButtonRef} variant="secondary" onClick={onClose}>
              {tr("닫기", "Close")}
            </SettingsButton>
          </div>
          <Suspense fallback={<div role="status" className="py-8 text-center text-[13px] text-[var(--fg-muted)]">{tr("불러오는 중…", "Loading…")}</div>}>
            <OnboardingWizard
              isKo={isKo}
              onComplete={() => {
                onClose();
                window.location.reload();
              }}
            />
          </Suspense>
        </div>
      </div>
    </div>,
    document.body,
  );
}
