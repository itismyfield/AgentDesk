// @vitest-environment happy-dom

import { act, createRef } from "react";
import { createRoot, type Root } from "react-dom/client";
import { MemoryRouter } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AppTopBar } from "./AppTopBar";
import { RouteErrorBoundary } from "./RouteErrorBoundary";
import { findRouteByPath } from "./routes";
import { Step1BotConnection } from "../components/onboarding/Step1BotConnection";

describe("RouteErrorBoundary & shell audit fixes", () => {
  let container: HTMLDivElement;
  let root: Root;
  let consoleErrorSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
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
    consoleErrorSpy.mockRestore();
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

  it("renders unclipped Step1 IntegrationFlow with localized AgentDesk hub CSS variables", async () => {
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
    expect(flowWrap.style.getPropertyValue("--adk-hub-title")).toBe('"AgentDesk 허브"');
    expect(flowWrap.style.getPropertyValue("--adk-hub-sub")).toBe('"Discord 실행·통신 봇 오케스트레이션"');
    expect(flowWrap.textContent).toContain("Antigravity");
  });
});
