import { Suspense, lazy, useEffect, useRef } from "react";
import { SettingsButton } from "./SettingsPrimitives";

const OnboardingWizard = lazy(() => import("../OnboardingWizard"));

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
  const onboardingDialogRef = useRef<HTMLDivElement | null>(null);
  const onboardingCloseButtonRef = useRef<HTMLButtonElement | null>(null);

  useEffect(() => {
    if (!open || typeof window === "undefined") return;
    const previousActiveElement =
      document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const focusCloseButton = window.setTimeout(() => {
      onboardingCloseButtonRef.current?.focus();
    }, 0);
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        onClose();
        return;
      }
      if (event.key !== "Tab") return;
      const dialog = onboardingDialogRef.current;
      if (!dialog) return;
      const focusable = Array.from(
        dialog.querySelectorAll<HTMLElement>(
          'a[href], button:not([disabled]), textarea:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])',
        ),
      );
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
    window.addEventListener("keydown", handleKeyDown);
    return () => {
      window.clearTimeout(focusCloseButton);
      window.removeEventListener("keydown", handleKeyDown);
      previousActiveElement?.focus();
    };
  }, [onClose, open]);

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-[80] overflow-y-auto bg-[var(--bg)] text-[var(--fg)]" role="dialog" aria-modal="true" aria-label={tr("초기 설정", "Initial setup")}>
      <div className="flex min-h-screen items-start justify-center pt-4 pb-8">
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
    </div>
  );
}
