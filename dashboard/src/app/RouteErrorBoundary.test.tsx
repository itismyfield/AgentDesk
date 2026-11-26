// @vitest-environment happy-dom

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, createRef, useEffect } from "react";
import { createRoot, type Root } from "react-dom/client";
import { MemoryRouter } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { requireAuthentication, resolveAuthentication } from "../api/authState";
import { OverlayProvider } from "../components/common/overlay";
import { Step1BotConnection } from "../components/onboarding/Step1BotConnection";
import { KanbanProvider } from "../contexts/KanbanContext";
import { SettingsProvider, useSettings } from "../contexts/SettingsContext";
import { WorkspaceProvider } from "../contexts/WorkspaceContext";
import { DEFAULT_SETTINGS } from "../types";
import AppShell from "./AppShell";
import { AppTopBar } from "./AppTopBar";
import { DashboardAuthGate } from "./DashboardAuthGate";
import { RouteErrorBoundary } from "./RouteErrorBoundary";
import { findRouteByPath } from "./routes";
import { THEME_STORAGE_KEY } from "./themePreferences";

const storageEntries = new Map<string, string>();
const memoryStorage: Storage = {
  get length() {
    return storageEntries.size;
  },
  clear() {
    storageEntries.clear();
  },
  getItem(key: string) {
    return storageEntries.get(key) ?? null;
  },
  key(index: number) {
    return Array.from(storageEntries.keys())[index] ?? null;
  },
  removeItem(key: string) {
    storageEntries.delete(key);
  },
  setItem(key: string, value: string) {
    storageEntries.set(key, String(value));
  },
};

describe("RouteErrorBoundary & shell audit fixes", () => {
  let container: HTMLDivElement;
  let root: Root;
  let consoleErrorSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(JSON.stringify({ error: "offline" }), { status: 503, headers: { "Content-Type": "application/json" } })),
    );
    Object.defineProperty(window, "localStorage", {
      configurable: true,
      writable: true,
      value: memoryStorage,
    });
    memoryStorage.clear();
    const dialog = HTMLDialogElement.prototype as HTMLDialogElement & {
      showModal?: () => void;
      show?: () => void;
      close?: () => void;
    };
    if (typeof dialog.showModal !== "function") {
      dialog.showModal = function showModal(this: HTMLDialogElement) { this.setAttribute("open", ""); };
    }
    if (typeof dialog.show !== "function") {
      dialog.show = function show(this: HTMLDialogElement) { this.setAttribute("open", ""); };
    }
    if (typeof dialog.close !== "function") {
      dialog.close = function close(this: HTMLDialogElement) { this.removeAttribute("open"); };
    }
    consoleErrorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    memoryStorage.clear();
    resolveAuthentication(0, true, false);
    consoleErrorSpy.mockRestore();
    vi.unstubAllGlobals();
  });

  it("catches route render errors and recovers on retry or home navigation", async () => {
    let shouldCrash = true;
    const onGoHome = vi.fn();

    function FlakyView() {
      if (shouldCrash) throw new Error("chunk failed");
      return <div data-testid="recovered-view">정상 화면</div>;
    }

    await act(async () => {
      root.render(
        <RouteErrorBoundary resetKey="/stats" isKo={true} onGoHome={onGoHome}>
          <FlakyView />
        </RouteErrorBoundary>,
      );
    });

    const alert = container.querySelector('[data-testid="route-error-boundary"]');
    expect(alert).not.toBeNull();
    expect(alert?.textContent).toContain("chunk failed");
    expect(alert?.textContent).toContain("다시 시도");
    expect(alert?.textContent).toContain("홈으로 이동");

    shouldCrash = false;
    await act(async () => {
      container.querySelector<HTMLButtonElement>('[data-testid="route-error-retry"]')!.click();
    });
    expect(container.querySelector('[data-testid="recovered-view"]')?.textContent).toBe("정상 화면");

    shouldCrash = true;
    await act(async () => {
      root.render(
        <RouteErrorBoundary resetKey="/stats" isKo={false} onGoHome={onGoHome}>
          <FlakyView />
        </RouteErrorBoundary>,
      );
    });
    expect(container.textContent).toContain("Try again");
    expect(container.textContent).toContain("Go to Home");

    shouldCrash = false;
    await act(async () => {
      container.querySelector<HTMLButtonElement>('[data-testid="route-error-home"]')!.click();
    });
    expect(onGoHome).toHaveBeenCalledTimes(1);
    expect(container.querySelector('[data-testid="recovered-view"]')).not.toBeNull();
  });

  it("resets error state automatically when resetKey changes on route navigation", async () => {
    function RouteView({ crash }: { crash: boolean }) {
      if (crash) throw new Error("audit crash");
      return <div data-testid="safe-route">홈 화면</div>;
    }

    await act(async () => {
      root.render(
        <RouteErrorBoundary resetKey="/audit" isKo={true} onGoHome={() => {}}>
          <RouteView crash={true} />
        </RouteErrorBoundary>,
      );
    });
    expect(container.querySelector('[data-testid="route-error-boundary"]')).not.toBeNull();

    await act(async () => {
      root.render(
        <RouteErrorBoundary resetKey="/home" isKo={true} onGoHome={() => {}}>
          <RouteView crash={false} />
        </RouteErrorBoundary>,
      );
    });
    expect(container.querySelector('[data-testid="route-error-boundary"]')).toBeNull();
    expect(container.querySelector('[data-testid="safe-route"]')?.textContent).toBe("홈 화면");
  });

  it("deduplicates desktop AppTopBar heading and closes RunningNow when opening a task", async () => {
    const onOpenTask = vi.fn();
    const agentsRoute = findRouteByPath("/agents");
    expect(agentsRoute?.descriptionKo).toBe("에이전트와 스킬, 연결 세션을 관리해요.");
    expect(agentsRoute?.descriptionEn).toBe("Manage agents, skills, and sessions.");

    await act(async () => {
      root.render(
        <MemoryRouter initialEntries={["/agents"]}>
          <AppTopBar
            currentRoute={agentsRoute}
            wsConnected={true}
            dismissNotification={() => {}}
            isKo={true}
            isMobileViewport={false}
            headerZIndex={20}
            notificationBadgeCount={0}
            popoverZIndex={40}
            recentNotifications={[]}
            resolvedTheme="dark"
            runningTasks={[{ id: "agent:ada", title: "아다", step: "코드 분석 중", status: "running" }]}
            onOpenRunningTask={onOpenTask}
            setShowCommandPalette={() => {}}
            setShowNotificationPanel={() => {}}
            setShowTweaksPanel={() => {}}
            showNotificationPanel={false}
            toggleShellTheme={() => {}}
          />
        </MemoryRouter>,
      );
    });

    const breadcrumb = container.querySelector('[data-testid="topbar-breadcrumb"]')!;
    expect(breadcrumb.querySelector(".sr-only")?.textContent).toBe("에이전트");
    expect(breadcrumb.textContent).toContain("에이전트와 스킬, 연결 세션을 관리해요.");

    const trigger = container.querySelector<HTMLButtonElement>('[data-testid="topbar-running-now"] .aui-running-now-trigger')!;
    await act(async () => {
      trigger.dispatchEvent(new MouseEvent("click", { bubbles: true, detail: 1 }));
    });
    const rowBody = container.querySelector<HTMLButtonElement>(".aui-running-now-body")!;
    await act(async () => {
      rowBody.click();
    });
    expect(onOpenTask).toHaveBeenCalledWith("agent:ada");
    expect(container.querySelector(".aui-running-now-panel")?.hasAttribute("open")).toBe(false);
  });

  it("renders unclipped Step1 IntegrationFlow with localized AgentDesk hub copy", async () => {
    await act(async () => {
      root.render(
        <Step1BotConnection
          actionRow=""
          announceBotInfo={null}
          announceReady={false}
          announceToken=""
          borderInput=""
          borderLight=""
          btnPrimary=""
          btnSecondary=""
          btnSmall=""
          commandBots={[{ provider: "claude", token: "", botInfo: null }]}
          commandBotsReady={false}
          goToStep={() => {}}
          inputStyle=""
          makeInviteUrl={() => ""}
          notifyBotInfo={null}
          notifyToken=""
          permissions={{ announce: "", command: "", notify: "" }}
          setAnnounceToken={() => {}}
          setCommandBots={() => {}}
          setNotifyToken={() => {}}
          step1Checklist={[]}
          stepBox=""
          stepHeadingRef={createRef<HTMLHeadingElement>()}
          tr={(ko) => ko}
          validateStep1={async () => {}}
          validating={false}
        />,
      );
    });

    const flowWrap = container.querySelector<HTMLElement>('[data-testid="step1-integration-flow"]')!;
    expect(flowWrap.className).not.toContain("max-h-48");
    expect(flowWrap.className).toContain("max-w-[440px]");
    expect(flowWrap.textContent).toContain("AgentDesk 허브");
    expect(flowWrap.textContent).toContain("Discord 실행·통신 봇 오케스트레이션");
    expect(flowWrap.textContent).not.toContain("Data Analysis");
    expect(flowWrap.textContent).toContain("Antigravity");
  });

  it("preserves localStorage dark theme across server hydration and dismisses TweaksPanel on Escape, outside click, and modals", async () => {
    window.localStorage.setItem(THEME_STORAGE_KEY, "dark");

    function HydrateServerLightTheme() {
      const { setSettings } = useSettings();
      useEffect(() => {
        setSettings((prev) => ({ ...prev, theme: "light" }));
      }, [setSettings]);
      return null;
    }

    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    await act(async () => {
      root.render(
        <QueryClientProvider client={client}>
          <MemoryRouter initialEntries={["/logs"]}>
            <OverlayProvider>
              <WorkspaceProvider
                initialAgents={[]}
                initialDepartments={[]}
                initialSessions={[]}
                pushNotification={() => ""}
              >
                <SettingsProvider initialSettings={DEFAULT_SETTINGS} initialStats={null}>
                  <HydrateServerLightTheme />
                  <KanbanProvider initialCards={[]} initialDispatches={[]}>
                    <AppShell
                      wsConnected={true}
                      wsLastEventTs={null}
                      notifications={[]}
                      pushNotification={() => ""}
                      updateNotification={() => {}}
                      dismissNotification={() => {}}
                    />
                  </KanbanProvider>
                </SettingsProvider>
              </WorkspaceProvider>
            </OverlayProvider>
          </MemoryRouter>
        </QueryClientProvider>,
      );
    });

    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe("dark");
    expect(document.documentElement.dataset.theme).toBe("dark");

    const tweaksTrigger = container.querySelector<HTMLButtonElement>(
      '[data-testid="topbar"] button[aria-label="화면 꾸미기"]',
    )!;
    await act(async () => {
      tweaksTrigger.click();
    });
    expect(container.querySelector('[data-testid="tweaks-panel"]')).not.toBeNull();

    await act(async () => {
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    });
    expect(container.querySelector('[data-testid="tweaks-panel"]')).toBeNull();

    await act(async () => {
      tweaksTrigger.click();
    });
    expect(container.querySelector('[data-testid="tweaks-panel"]')).not.toBeNull();

    await act(async () => {
      document.body.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
    });
    expect(container.querySelector('[data-testid="tweaks-panel"]')).toBeNull();

    await act(async () => {
      tweaksTrigger.click();
    });
    expect(container.querySelector('[data-testid="tweaks-panel"]')).not.toBeNull();

    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "?", bubbles: true }));
    });
    expect(container.querySelector('[data-testid="tweaks-panel"]')).toBeNull();
    expect(document.body.textContent).toContain("키보드 단축키");
  });

  it("renders DashboardAuthGate with semantic theme tokens instead of hardcoded dark gray classes", async () => {
    requireAuthentication("토큰을 입력해 주세요.");
    await act(async () => {
      root.render(
        <DashboardAuthGate>
          <div>authenticated</div>
        </DashboardAuthGate>,
      );
    });

    const main = container.querySelector("main")!;
    const form = container.querySelector("form")!;
    expect(main.className).toContain("bg-[var(--bg)]");
    expect(form.className).toContain("bg-[var(--bg-2)]");
    expect(form.className).toContain("border-[var(--line)]");
    expect(container.innerHTML).not.toMatch(/bg-gray-|text-gray-|border-gray-/);
  });

  it("focuses the active or first route item via [data-autofocus] instead of the close button when opening mobile More", async () => {
    const originalMatchMedia = window.matchMedia;
    const fetchSpy = vi.spyOn(globalThis, "fetch").mockImplementation(async () =>
      new Response(JSON.stringify({ error: "offline" }), {
        status: 503,
        headers: { "Content-Type": "application/json" },
      }),
    );
    window.matchMedia = vi.fn().mockImplementation((query: string) => ({
      matches: query === "(max-width: 899px)",
      media: query,
      onchange: null,
      addListener: vi.fn(),
      removeListener: vi.fn(),
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    }));

    try {
      const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
      await act(async () => {
        root.render(
          <QueryClientProvider client={client}>
            <MemoryRouter initialEntries={["/audit"]}>
              <OverlayProvider>
                <WorkspaceProvider
                  initialAgents={[]}
                  initialDepartments={[]}
                  initialSessions={[]}
                  pushNotification={() => ""}
                >
                  <SettingsProvider initialSettings={DEFAULT_SETTINGS} initialStats={null}>
                    <KanbanProvider initialCards={[]} initialDispatches={[]}>
                      <AppShell
                        wsConnected={true}
                        wsLastEventTs={null}
                        notifications={[]}
                        pushNotification={() => ""}
                        updateNotification={() => {}}
                        dismissNotification={() => {}}
                      />
                    </KanbanProvider>
                  </SettingsProvider>
                </WorkspaceProvider>
              </OverlayProvider>
            </MemoryRouter>
          </QueryClientProvider>,
        );
      });

      const moreTrigger = container.querySelector<HTMLButtonElement>('[data-testid="app-mobile-more-button"]')!;
      await act(async () => {
        moreTrigger.click();
      });

      const sheet = container.querySelector<HTMLElement>("#app-mobile-more-menu")!;
      const closeBtn = sheet.querySelector<HTMLButtonElement>('[aria-label="더 보기 닫기"]')!;
      const auditItem = sheet.querySelector<HTMLButtonElement>('[data-testid="app-mobile-more-audit"]')!;
      expect(auditItem.hasAttribute("data-autofocus")).toBe(true);
      expect(document.activeElement).toBe(auditItem);
      expect(document.activeElement).not.toBe(closeBtn);

      const homeTab = container.querySelector<HTMLButtonElement>('[data-testid="app-mobile-tab-home"]')!;
      await act(async () => {
        homeTab.click();
      });
      await act(async () => {
        moreTrigger.click();
      });

      const reopenedSheet = container.querySelector<HTMLElement>("#app-mobile-more-menu")!;
      const routinesItem = reopenedSheet.querySelector<HTMLButtonElement>('[data-testid="app-mobile-more-routines"]')!;
      expect(routinesItem.hasAttribute("data-autofocus")).toBe(true);
      expect(document.activeElement).toBe(routinesItem);
      expect(document.activeElement).not.toBe(reopenedSheet.querySelector('[aria-label="더 보기 닫기"]'));
    } finally {
      window.matchMedia = originalMatchMedia;
      fetchSpy.mockRestore();
    }
  });
});
