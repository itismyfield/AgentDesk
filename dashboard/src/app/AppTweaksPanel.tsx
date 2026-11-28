import { X } from "lucide-react";
import { useEffect, useRef, type Dispatch, type SetStateAction } from "react";
import type { AccentPreset, ThemePreference } from "./themePreferences";

const THEME_OPTIONS: Array<{
  id: ThemePreference;
  labelKo: string;
  labelEn: string;
}> = [
  { id: "auto", labelKo: "자동", labelEn: "Auto" },
  { id: "dark", labelKo: "다크", labelEn: "Dark" },
  { id: "light", labelKo: "라이트", labelEn: "Light" },
];

const ACCENT_OPTIONS: Array<{
  id: AccentPreset;
  label: string;
  token: string;
}> = [
  { id: "indigo", label: "Indigo", token: "--accent-indigo" },
  { id: "violet", label: "Violet", token: "--accent-violet" },
  { id: "amber", label: "Amber", token: "--accent-amber" },
  { id: "rose", label: "Rose", token: "--accent-rose" },
  { id: "cyan", label: "Cyan", token: "--accent-cyan" },
  { id: "lime", label: "Lime", token: "--accent-lime" },
];

interface AppTweaksPanelProps {
  accentPreset: AccentPreset;
  isKo: boolean;
  popoverZIndex: number;
  setAccentPreset: Dispatch<SetStateAction<AccentPreset>>;
  setShowTweaksPanel: Dispatch<SetStateAction<boolean>>;
  setThemePreference: Dispatch<SetStateAction<ThemePreference>>;
  themePreference: ThemePreference;
}

export function AppTweaksPanel({
  accentPreset,
  isKo,
  popoverZIndex,
  setAccentPreset,
  setShowTweaksPanel,
  setThemePreference,
  themePreference,
}: AppTweaksPanelProps) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const panelRef = useRef<HTMLDivElement>(null);
  const closeButtonRef = useRef<HTMLButtonElement>(null);
  const openerRef = useRef<HTMLElement | null>(null);
  if (openerRef.current === null && typeof document !== "undefined") {
    openerRef.current = document.activeElement as HTMLElement | null;
  }

  useEffect(() => {
    const initialTarget =
      panelRef.current?.querySelector<HTMLElement>('button[data-theme-option][aria-pressed="true"]') ??
      closeButtonRef.current;
    initialTarget?.focus({ preventScroll: true });
    const opener = openerRef.current;
    return () => {
      if (opener?.isConnected) {
        opener.focus({ preventScroll: true });
      }
    };
  }, []);

  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        setShowTweaksPanel(false);
        return;
      }
      if (event.key === "Tab" && panelRef.current) {
        const focusable = Array.from(
          panelRef.current.querySelectorAll<HTMLElement>(
            'a[href], button:not([disabled]), textarea:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])',
          ),
        );
        if (focusable.length === 0) {
          event.preventDefault();
          return;
        }
        const first = focusable[0];
        const last = focusable[focusable.length - 1];
        const activeEl = document.activeElement;
        if (event.shiftKey && (activeEl === first || !panelRef.current.contains(activeEl))) {
          event.preventDefault();
          last.focus();
        } else if (!event.shiftKey && (activeEl === last || !panelRef.current.contains(activeEl))) {
          event.preventDefault();
          first.focus();
        }
      }
    };
    const handlePointerDown = (event: PointerEvent) => {
      const target = event.target as Element | null;
      if (!target || panelRef.current?.contains(target)) return;
      if (
        target.closest?.(
          '[data-testid="topbar"] button[aria-label="화면 꾸미기"], [data-testid="topbar"] button[aria-label="Appearance"], [data-testid="app-mobile-more-tweaks"]',
        )
      ) {
        return;
      }
      setShowTweaksPanel(false);
    };
    document.addEventListener("keydown", handleKeyDown);
    document.addEventListener("pointerdown", handlePointerDown);
    return () => {
      document.removeEventListener("keydown", handleKeyDown);
      document.removeEventListener("pointerdown", handlePointerDown);
    };
  }, [setShowTweaksPanel]);

  return (
    <>
      <div
        aria-hidden="true"
        data-testid="tweaks-panel-backdrop"
        onClick={() => setShowTweaksPanel(false)}
        className="fixed inset-0 bg-black/25 sm:hidden"
        style={{ zIndex: popoverZIndex }}
      />
      <div
        className="pointer-events-none fixed right-4 top-24 sm:top-20 w-[min(22rem,calc(100vw-2rem))] max-[899px]:inset-x-4 max-[899px]:bottom-[calc(4.5rem+env(safe-area-inset-bottom))] max-[899px]:top-auto max-[899px]:w-auto"
        style={{ zIndex: popoverZIndex }}
      >
        <div
          ref={panelRef}
          role="dialog"
          aria-label={tr("디자인", "Appearance")}
          data-testid="tweaks-panel"
          className="pointer-events-auto max-h-[calc(100dvh-10rem)] overflow-auto rounded-lg border p-4 shadow-lg"
          style={{
            borderColor: "var(--th-border-subtle)",
            background: "var(--th-bg-surface)",
          }}
        >
          <div className="flex items-center justify-between gap-3">
            <div>
              <div
                className="text-sm font-semibold"
                style={{ color: "var(--th-text-heading)" }}
              >
                {tr("디자인", "Appearance")}
              </div>
              <div
                className="mt-1 text-xs"
                style={{ color: "var(--th-text-muted)" }}
              >
                {tr("셸 테마와 강조색을 조정합니다.", "Tune shell theme and accent.")}
              </div>
            </div>
            <button
              ref={closeButtonRef}
              type="button"
              onClick={() => setShowTweaksPanel(false)}
              className="flex h-11 w-11 items-center justify-center rounded-md text-[var(--th-text-muted)]"
              aria-label={tr("패널 닫기", "Close panel")}
            >
              <X size={14} />
            </button>
          </div>

          <div className="mt-4 space-y-4">
            <div>
              <div
                className="mb-2 text-xs font-medium"
                style={{ color: "var(--th-text-muted)" }}
              >
                {tr("테마", "Theme")}
              </div>
              <div
                className="flex items-center gap-1 rounded-md p-1"
                style={{ background: "var(--th-overlay-subtle)" }}
              >
                {THEME_OPTIONS.map((option) => {
                  const active = themePreference === option.id;
                  return (
                    <button
                      key={option.id}
                      type="button"
                      data-theme-option={option.id}
                      onClick={() => setThemePreference(option.id)}
                      aria-pressed={active}
                      className="flex-1 min-h-11 rounded-md px-3 py-2 text-xs font-medium transition-colors"
                      style={
                        active
                          ? {
                              background: "var(--th-accent-primary-soft)",
                              color: "var(--th-accent-primary)",
                            }
                          : { color: "var(--th-text-muted)" }
                      }
                    >
                      {isKo ? option.labelKo : option.labelEn}
                    </button>
                  );
                })}
              </div>
            </div>

            <div>
              <div
                className="mb-2 text-xs font-medium"
                style={{ color: "var(--th-text-muted)" }}
              >
                {tr("강조색", "Accent")}
              </div>
              <div className="flex flex-wrap items-center gap-2">
                {ACCENT_OPTIONS.map((option) => {
                  const active = accentPreset === option.id;
                  return (
                    <button
                      key={option.id}
                      type="button"
                      title={option.label}
                      aria-label={`${option.label} accent`}
                      aria-pressed={active}
                      data-accent-preset={option.id}
                      onClick={() => setAccentPreset(option.id)}
                      className="dash-accent-swatch flex h-11 w-11 items-center justify-center rounded-full transition-transform"
                      style={{
                        border: active
                          ? "2px solid var(--th-text-heading)"
                          : "1px solid color-mix(in srgb, var(--th-border-subtle) 80%, transparent)",
                        background:
                          "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
                        transform: active ? "translateY(-1px)" : undefined,
                      }}
                    >
                      <span
                        className="h-4.5 w-4.5 rounded-full"
                        style={{ background: `var(${option.token})` }}
                      />
                    </button>
                  );
                })}
              </div>
            </div>
          </div>
        </div>
      </div>
    </>
  );
}
