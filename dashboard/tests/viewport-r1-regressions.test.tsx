// @vitest-environment happy-dom

import { act, useState } from "react";
import { createRoot, type Root } from "react-dom/client";
import { MemoryRouter } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { AppTweaksPanel } from "../src/app/AppTweaksPanel";
import type { AccentPreset, ThemePreference } from "../src/app/themePreferences";
import CampaignDetail from "../src/components/campaigns/CampaignDetail";
import { Drawer } from "../src/components/common/overlay/Drawer";
import { Modal } from "../src/components/common/overlay/Modal";
import { SettingsOnboardingOverlay } from "../src/components/settings/SettingsOnboardingOverlay";
import { SkillUsageCard } from "../src/components/stats/StatsAgentCards";

vi.mock("../src/components/OnboardingWizard", () => ({
  default: () => (
    <div data-testid="mock-onboarding-wizard">
      <input data-testid="onboarding-input" type="text" />
    </div>
  ),
}));

const tr = (ko: string, _en: string) => ko;

function mockMatchMedia(mobile: boolean) {
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    writable: true,
    value: vi.fn().mockImplementation((query: string) => ({
      matches: query.includes("max-width") ? mobile : !mobile,
      media: query,
      onchange: null,
      addListener: vi.fn(),
      removeListener: vi.fn(),
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    })),
  });
}

describe("Dashboard Round 1 — Multi-Viewport & Overlay Regressions", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    mockMatchMedia(false);
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    container.remove();
    document.body.style.overflow = "";
  });

  it("renders Modal overlay at z-[90] and content at z-[100] above Drawer (z-[80])", async () => {
    await act(async () => {
      root.render(
        <Drawer open onClose={() => {}} title="작업 드로어">
          <button type="button">드로어 액션</button>
          <Modal open onClose={() => {}} title="삭제 확인">
            <p>정말 삭제할까요?</p>
          </Modal>
        </Drawer>,
      );
    });

    const drawerContent = document.body.querySelector<HTMLElement>('[aria-label="작업 드로어"]')!;
    const modalContent = document.body.querySelector<HTMLElement>('[aria-label="삭제 확인"]')!;
    expect(drawerContent).not.toBeNull();
    expect(modalContent).not.toBeNull();
    expect(drawerContent.className).toContain("z-[80]");
    expect(modalContent.className).toContain("z-[100]");

    const overlays = Array.from(document.body.querySelectorAll<HTMLElement>("div.fixed.inset-0"));
    expect(overlays.some((el) => el.className.includes("z-[70]"))).toBe(true);
    expect(overlays.some((el) => el.className.includes("z-[90]"))).toBe(true);
  });

  it("portals SettingsOnboardingOverlay into document.body at z-[90], marks sibling body children inert, and blocks / keydown", async () => {
    const onClose = vi.fn();
    const windowBubbleKeySpy = vi.fn();
    window.addEventListener("keydown", windowBubbleKeySpy);

    try {
      await act(async () => {
        root.render(
          <div data-testid="settings-page-shell">
            <SettingsOnboardingOverlay isKo open onClose={onClose} tr={tr} />
          </div>,
        );
      });
      // Allow lazy OnboardingWizard chunk resolution inside Suspense
      await act(async () => {
        await Promise.resolve();
      });

      expect(container.querySelector('[role="dialog"]')).toBeNull();
      const portalDialog = document.body.querySelector<HTMLElement>(
        '[role="dialog"][aria-label="초기 설정"]',
      )!;
      expect(portalDialog).not.toBeNull();
      expect(portalDialog.parentElement).toBe(document.body);
      expect(portalDialog.className).toContain("z-[90]");
      expect(portalDialog.firstElementChild?.className).toContain("min-h-dvh");
      expect(container.inert).toBe(true);
      expect(document.body.style.overflow).toBe("hidden");

      const closeButton = portalDialog.querySelector("button")!;
      act(() => {
        closeButton.dispatchEvent(
          new KeyboardEvent("keydown", { key: "/", bubbles: true, cancelable: true }),
        );
      });
      expect(windowBubbleKeySpy).not.toHaveBeenCalled();

      const wizardInput = portalDialog.querySelector<HTMLInputElement>('[data-testid="onboarding-input"]');
      if (wizardInput) {
        act(() => {
          wizardInput.dispatchEvent(
            new KeyboardEvent("keydown", { key: "/", bubbles: true, cancelable: true }),
          );
        });
        expect(windowBubbleKeySpy).toHaveBeenCalledTimes(1);
      }

      await act(async () => {
        root.render(
          <div data-testid="settings-page-shell">
            <SettingsOnboardingOverlay isKo open={false} onClose={onClose} tr={tr} />
          </div>,
        );
      });
      expect(container.inert).toBe(false);
      expect(document.body.style.overflow).toBe("");
    } finally {
      window.removeEventListener("keydown", windowBubbleKeySpy);
    }
  });

  it("moves focus inside AppTweaksPanel on open and restores focus to trigger button on close and Escape", async () => {
    function Harness() {
      const [open, setOpen] = useState(false);
      const [theme, setTheme] = useState<ThemePreference>("dark");
      const [accent, setAccent] = useState<AccentPreset>("indigo");
      return (
        <div>
          <button
            type="button"
            data-testid="tweaks-trigger"
            onClick={() => setOpen(true)}
          >
            화면 꾸미기
          </button>
          {open ? (
            <AppTweaksPanel
              accentPreset={accent}
              isKo
              popoverZIndex={60}
              setAccentPreset={setAccent}
              setShowTweaksPanel={setOpen}
              setThemePreference={setTheme}
              themePreference={theme}
            />
          ) : null}
        </div>
      );
    }

    await act(async () => {
      root.render(<Harness />);
    });

    const trigger = container.querySelector<HTMLButtonElement>('[data-testid="tweaks-trigger"]')!;
    trigger.focus();
    expect(document.activeElement).toBe(trigger);

    await act(async () => {
      trigger.click();
    });

    const panel = container.querySelector<HTMLElement>('[data-testid="tweaks-panel"]')!;
    expect(panel).not.toBeNull();
    expect(panel.contains(document.activeElement)).toBe(true);
    expect((document.activeElement as HTMLElement).getAttribute("data-theme-option")).toBe("dark");

    await act(async () => {
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    });

    expect(container.querySelector('[data-testid="tweaks-panel"]')).toBeNull();
    expect(document.activeElement).toBe(trigger);

    await act(async () => {
      trigger.click();
    });
    const closeBtn = container.querySelector<HTMLButtonElement>('button[aria-label="패널 닫기"]')!;
    await act(async () => {
      closeBtn.click();
    });
    expect(container.querySelector('[data-testid="tweaks-panel"]')).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it("merges sentence and actions into one header row in CampaignDetail when showTitle is false and chips is falsy", async () => {
    mockMatchMedia(true);
    await act(async () => {
      root.render(
        <CampaignDetail
          title="캠페인 A"
          showTitle={false}
          sentence="활성 · 전체 4건 중 2건 완료"
          actions={
            <button type="button" data-testid="mobile-campaign-action">
              삭제
            </button>
          }
          overview={() => <div data-testid="campaign-ov">overview</div>}
          task={null}
          onCloseTask={() => {}}
          tr={tr}
        />,
      );
    });

    expect(container.querySelector(".campaign-detail-title")).toBeNull();
    const statusEl = container.querySelector<HTMLElement>(".campaign-detail-status")!;
    const actionBtn = container.querySelector<HTMLElement>('[data-testid="mobile-campaign-action"]')!;
    expect(statusEl).not.toBeNull();
    expect(actionBtn).not.toBeNull();
    expect(statusEl.parentElement).not.toBeNull();
    expect(statusEl.parentElement?.contains(actionBtn)).toBe(true);
  });

  it("renders stats-skill-catalog-link with min-h-11 touch target class in SkillUsageCard", async () => {
    await act(async () => {
      root.render(
        <MemoryRouter>
          <SkillUsageCard
            t={(input) => (typeof input === "string" ? input : input.ko)}
            loading={false}
            rows={[]}
            byAgentRows={[]}
            windowCalls={24}
          />
        </MemoryRouter>,
      );
    });

    const link = container.querySelector<HTMLElement>('[data-testid="stats-skill-catalog-link"]')!;
    expect(link).not.toBeNull();
    expect(link.className).toContain("min-h-11");
    expect(link.className).toContain("sm:pointer-fine:min-h-7");
  });
});
