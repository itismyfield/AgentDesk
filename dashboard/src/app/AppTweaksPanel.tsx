import { X } from "lucide-react";
import type { Dispatch, SetStateAction } from "react";
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

  return (
    <div
      className="pointer-events-none fixed right-4 top-[5.25rem] w-[min(22rem,calc(100vw-2rem))]"
      style={{ zIndex: popoverZIndex }}
    >
      <div
        className="pointer-events-auto rounded-[1.75rem] border p-4 shadow-2xl"
        style={{
          borderColor: "var(--th-border-subtle)",
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 95%, transparent) 100%)",
        }}
      >
        <div className="flex items-center justify-between gap-3">
          <div>
            <div
              className="text-sm font-semibold"
              style={{ color: "var(--th-text-heading)" }}
            >
              Tweaks
            </div>
            <div
              className="mt-1 text-xs"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("셸 테마와 강조색을 조정합니다.", "Tune shell theme and accent.")}
            </div>
          </div>
          <button
            type="button"
            onClick={() => setShowTweaksPanel(false)}
            className="flex h-8 w-8 items-center justify-center rounded-xl text-[var(--th-text-muted)]"
            aria-label={tr("패널 닫기", "Close panel")}
          >
            <X size={14} />
          </button>
        </div>

        <div className="mt-4 space-y-4">
          <div>
            <div
              className="mb-2 text-[11px] font-semibold uppercase tracking-[0.18em]"
              style={{ color: "var(--th-text-muted)" }}
            >
              Theme
            </div>
            <div
              className="flex items-center gap-1 rounded-full p-1"
              style={{ background: "var(--th-overlay-subtle)" }}
            >
              {THEME_OPTIONS.map((option) => {
                const active = themePreference === option.id;
                return (
                  <button
                    key={option.id}
                    type="button"
                    onClick={() => setThemePreference(option.id)}
                    aria-pressed={active}
                    className="flex-1 rounded-full px-3 py-1.5 text-xs font-medium transition-colors"
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
              className="mb-2 text-[11px] font-semibold uppercase tracking-[0.18em]"
              style={{ color: "var(--th-text-muted)" }}
            >
              Accent
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
                    className="dash-accent-swatch flex h-9 w-9 items-center justify-center rounded-full transition-transform"
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
  );
}
