// @vitest-environment happy-dom

import { act, createRef, useRef, useState, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { MemoryRouter } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../src/api";
import type { CampaignNode } from "../src/api/campaigns";
import type { PipelineDefinition } from "../src/api/serverCampaignPipeline";
import { AppTopBar } from "../src/app/AppTopBar";
import { PRIMARY_ROUTES } from "../src/app/routes";
import PipelineVisualEditor from "../src/components/agent-manager/PipelineVisualEditor";
import { TaskInspector } from "../src/components/campaigns/CampaignGroupInspector";
import PipelineStatePicker from "../src/components/campaigns/PipelineStatePicker";
import { BottomSheet } from "../src/components/common/overlay/BottomSheet";
import { Drawer } from "../src/components/common/overlay/Drawer";
import { Step3AgentSelection } from "../src/components/onboarding/Step3AgentSelection";
import { Step4ChannelSetup } from "../src/components/onboarding/Step4ChannelSetup";
import { I18nProvider } from "../src/i18n";
import type { PipelineConfigFull } from "../src/types";

vi.mock("../src/components/agent-manager/PipelineFlowCanvas", () => ({
  default: () => <div data-testid="mock-flow-canvas" />,
}));

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  if (!Element.prototype.scrollIntoView) {
    Element.prototype.scrollIntoView = () => {};
  }
  const dialog = HTMLDialogElement.prototype as HTMLDialogElement & {
    showModal?: () => void;
    show?: () => void;
    close?: () => void;
    showPopover?: () => void;
    hidePopover?: () => void;
  };
  if (typeof dialog.showModal !== "function") {
    dialog.showModal = function showModal(this: HTMLDialogElement) {
      this.setAttribute("open", "");
    };
  }
  if (typeof dialog.show !== "function") {
    dialog.show = function show(this: HTMLDialogElement) {
      this.setAttribute("open", "");
    };
  }
  if (typeof dialog.close !== "function") {
    dialog.close = function close(this: HTMLDialogElement) {
      this.removeAttribute("open");
    };
  }
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  container.remove();
  document.body.removeAttribute("style");
  vi.restoreAllMocks();
});

async function render(ui: ReactNode) {
  await act(async () => {
    root.render(ui);
  });
}

describe("Round 2B viewport, overlay, focus & light theme regressions", () => {
  it("moves focus into AppTopBar notification dialog on open, traps Tab/Shift+Tab, renders mobile backdrop, and restores Bell focus on close", async () => {
    function TopBarHarness() {
      const [showNotificationPanel, setShowNotificationPanel] = useState(false);
      return (
        <MemoryRouter initialEntries={["/home"]}>
          <AppTopBar
            currentRoute={PRIMARY_ROUTES[0]}
            wsConnected={true}
            dismissNotification={() => {}}
            isKo={true}
            isMobileViewport={false}
            headerZIndex={20}
            notificationBadgeCount={1}
            popoverZIndex={40}
            recentNotifications={[
              {
                id: "notif-1",
                message: "빌드 완료",
                type: "success",
                ts: Date.now() - 5_000,
              },
            ]}
            resolvedTheme="light"
            runningTasks={[]}
            setShowCommandPalette={() => {}}
            setShowNotificationPanel={setShowNotificationPanel}
            setShowTweaksPanel={() => {}}
            showNotificationPanel={showNotificationPanel}
            toggleShellTheme={() => {}}
          />
        </MemoryRouter>
      );
    }

    await render(<TopBarHarness />);

    const bellButton = container.querySelector<HTMLButtonElement>('button[aria-label="알림 1개"]')!;
    const themeButton = container.querySelector<HTMLButtonElement>('button[aria-label="어두운 화면으로 전환"]')!;
    const appearanceButton = container.querySelector<HTMLButtonElement>('button[aria-label="화면 꾸미기"]')!;

    expect(bellButton.className).toContain("pointer-coarse:[--size:44px]!");
    expect(themeButton.className).toContain("pointer-coarse:[--size:44px]!");
    expect(appearanceButton.className).toContain("pointer-coarse:[--size:44px]!");

    bellButton.focus();
    await act(async () => {
      bellButton.click();
    });

    const dialog = container.querySelector<HTMLElement>('[role="dialog"][aria-label="알림"]');
    expect(dialog).not.toBeNull();
    const backdrop = container.querySelector<HTMLElement>('div[aria-hidden="true"].fixed.inset-0');
    expect(backdrop).not.toBeNull();
    expect(backdrop?.style.zIndex).toBe("39");

    const closeButton = dialog!.querySelector<HTMLButtonElement>('button[aria-label="알림 창 닫기"]')!;
    const dismissButton = dialog!.querySelector<HTMLButtonElement>('button[aria-label="알림 지우기"]')!;
    expect(document.activeElement).toBe(closeButton);

    // Shift+Tab on first focusable wraps to last focusable
    let shiftTabPrevented = false;
    await act(async () => {
      const event = new KeyboardEvent("keydown", {
        key: "Tab",
        shiftKey: true,
        bubbles: true,
        cancelable: true,
      });
      closeButton.dispatchEvent(event);
      shiftTabPrevented = event.defaultPrevented;
    });
    expect(shiftTabPrevented).toBe(true);
    expect(document.activeElement).toBe(dismissButton);

    // Tab on last focusable wraps to first focusable
    let tabPrevented = false;
    await act(async () => {
      const event = new KeyboardEvent("keydown", {
        key: "Tab",
        shiftKey: false,
        bubbles: true,
        cancelable: true,
      });
      dismissButton.dispatchEvent(event);
      tabPrevented = event.defaultPrevented;
    });
    expect(tabPrevented).toBe(true);
    expect(document.activeElement).toBe(closeButton);

    // Escape closes dialog and restores focus to Bell trigger
    await act(async () => {
      document.dispatchEvent(
        new KeyboardEvent("keydown", {
          key: "Escape",
          bubbles: true,
          cancelable: true,
        }),
      );
    });
    expect(container.querySelector('[role="dialog"][aria-label="알림"]')).toBeNull();
    expect(document.activeElement).toBe(bellButton);

    // Reopen and close via × close button -> restores focus to Bell trigger
    await act(async () => {
      bellButton.click();
    });
    const reopenedClose = container.querySelector<HTMLButtonElement>('button[aria-label="알림 창 닫기"]')!;
    expect(document.activeElement).toBe(reopenedClose);
    await act(async () => {
      reopenedClose.click();
    });
    expect(container.querySelector('[role="dialog"][aria-label="알림"]')).toBeNull();
    expect(document.activeElement).toBe(bellButton);
  });

  it("localizes BottomSheet and Drawer close button aria-labels under I18nProvider and applies light-safe hover tokens", async () => {
    window.matchMedia = vi.fn().mockReturnValue({
      matches: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });

    await render(
      <I18nProvider language="en">
        <BottomSheet open onClose={() => {}} title="English Sheet">
          Sheet body
        </BottomSheet>
        <Drawer open onClose={() => {}} title="English Drawer">
          Drawer body
        </Drawer>
      </I18nProvider>,
    );

    const enCloseButtons = Array.from(document.querySelectorAll<HTMLButtonElement>('button[aria-label="Close"]'));
    expect(enCloseButtons).toHaveLength(2);
    for (const button of enCloseButtons) {
      expect(button.className).toContain("hover:bg-[var(--th-overlay-light)]");
      expect(button.className).toContain("hover:text-[var(--th-text)]");
      expect(button.className).not.toContain("hover:bg-white/5");
    }

    const dialogs = Array.from(document.querySelectorAll<HTMLElement>('[role="dialog"]'));
    expect(dialogs.length).toBeGreaterThanOrEqual(2);
    for (const dialog of dialogs) {
      expect(dialog.className).toContain("bg-[var(--th-card-bg,_var(--bg-2))]");
      expect(dialog.className).toContain("text-[var(--th-text,_var(--fg))]");
      expect(dialog.className).not.toContain("#0f172a");
    }

    await render(
      <I18nProvider language="ko">
        <BottomSheet open onClose={() => {}} title="한국어 시트">
          시트 본문
        </BottomSheet>
        <Drawer open onClose={() => {}} title="한국어 드로어">
          드로어 본문
        </Drawer>
      </I18nProvider>,
    );

    const koCloseButtons = Array.from(document.querySelectorAll<HTMLButtonElement>('button[aria-label="닫기"]'));
    expect(koCloseButtons).toHaveLength(2);
  });

  it("applies 44px touch heights and 16px input font classes in PipelineStatePicker and CampaignGroupInspector", async () => {
    const draft: PipelineDefinition = {
      start: "plan",
      states: [
        { id: "plan", kind: "agent_task", title: "Plan" },
        { id: "verify", kind: "auto_check", title: "Verify" },
      ],
      transitions: [],
      events: [],
    };

    function PickerHarness() {
      const anchor = useRef<HTMLButtonElement | null>(null);
      return (
        <div>
          <button ref={anchor} type="button">
            Anchor
          </button>
          <PipelineStatePicker
            open={true}
            anchor={anchor}
            draft={draft}
            from={{ name: "Plan", label: "done" }}
            onPick={() => {}}
            onCreate={() => {}}
            onClose={() => {}}
            tr={(_ko, en) => en}
          />
        </div>
      );
    }

    const sampleNode: CampaignNode = {
      id: "task-1",
      title: "Fix touch targets",
      status: "running",
      issue_url: "https://github.com/itismyfield/AgentDesk/issues/8400",
      dependencies: [],
      acceptance: [],
    } as CampaignNode;

    await render(
      <>
        <PickerHarness />
        <TaskInspector
          node={sampleNode}
          nodes={[sampleNode]}
          cycles={[]}
          onChoose={() => {}}
          onOpenTask={() => {}}
          tr={(_ko, en) => en}
        />
      </>,
    );

    const searchInput = document.querySelector<HTMLInputElement>('input[aria-label="Find a state"]')!;
    expect(searchInput).not.toBeNull();
    expect(searchInput.className).toContain("max-[899px]:!min-h-11");
    expect(searchInput.className).toContain("pointer-coarse:!min-h-11");
    expect(searchInput.className).toContain("max-[899px]:!text-base");
    expect(searchInput.className).toContain("pointer-coarse:!text-base");

    const pickerButtons = Array.from(
      document.querySelectorAll<HTMLButtonElement>('[role="dialog"] button'),
    );
    expect(pickerButtons.length).toBeGreaterThan(2);
    for (const button of pickerButtons) {
      expect(button.className).toContain("max-[899px]:!min-h-11");
      expect(button.className).toContain("pointer-coarse:!min-h-11");
    }

    const issueLink = container.querySelector<HTMLAnchorElement>('a[href="https://github.com/itismyfield/AgentDesk/issues/8400"]')!;
    expect(issueLink.className).toContain("max-[899px]:min-h-11");
    expect(issueLink.className).toContain("pointer-coarse:min-h-11");
  });

  it("renders type=button on stage & toolbar actions and aria-label on stage name inputs in PipelineVisualEditorStagePanel", async () => {
    const repo = "itismyfield/AgentDesk";
    const pipeline: PipelineConfigFull = {
      name: "default",
      version: 1,
      states: [{ id: "ready", label: "Ready" }],
      transitions: [],
      gates: {},
      hooks: {},
      events: {},
      clocks: {},
      phase_gate: { dispatch_to: "self", dispatch_type: "phase-gate" },
    };

    const storage = new Map<string, string>();
    Object.defineProperty(window, "localStorage", {
      configurable: true,
      value: {
        getItem: (key: string) => storage.get(key) ?? null,
        setItem: (key: string, value: string) => storage.set(key, value),
      },
    });

    vi.spyOn(api, "getEffectivePipeline").mockResolvedValue({
      pipeline,
      layers: { default: true, repo: false, agent: false },
    });
    vi.spyOn(api, "getRepoPipeline").mockResolvedValue({ repo, pipeline_config: null });
    vi.spyOn(api, "getPipelineStages").mockResolvedValue([
      {
        id: "stage-1",
        repo,
        stage_name: "qa",
        stage_order: 1,
        provider: null,
        skip_condition: null,
        agent_override_id: null,
        trigger_after: "review_pass",
      },
      {
        id: "stage-2",
        repo,
        stage_name: "deploy",
        stage_order: 2,
        provider: null,
        skip_condition: null,
        agent_override_id: null,
        trigger_after: "ready",
      },
    ]);

    await render(<PipelineVisualEditor tr={(_ko, en) => en} locale="en" repo={repo} agents={[]} defaultCollapsed={false} />);

    const stage1Input = container.querySelector<HTMLInputElement>('input[aria-label="Stage 1 name"]')!;
    const stage2Input = container.querySelector<HTMLInputElement>('input[aria-label="Stage 2 name"]')!;
    expect(stage1Input).not.toBeNull();
    expect(stage1Input.value).toBe("qa");
    expect(stage2Input).not.toBeNull();
    expect(stage2Input.value).toBe("deploy");

    const actionLabels = ["+ State", "+ Transition", "Phase Gate", "+ Stage", "Clear all stages", "↓ Later", "↑ Earlier", "Delete"];
    for (const label of actionLabels) {
      const button = Array.from(container.querySelectorAll<HTMLButtonElement>("button")).find(
        (candidate) => candidate.textContent?.trim() === label,
      );
      expect(button, `Expected button "${label}"`).toBeTruthy();
      expect(button!.getAttribute("type")).toBe("button");
      expect(button!.className).toContain("max-[899px]:min-h-11");
      expect(button!.className).toContain("pointer-coarse:min-h-11");
    }
  });

  it("uses theme tokens and 16px touch font classes in Step3AgentSelection and Step4ChannelSetup", async () => {
    const noop = () => undefined;
    const asyncNoop = async () => undefined;

    await render(
      <>
        <Step3AgentSelection
          actionRow=""
          addCustomAgent={noop}
          agents={[
            {
              id: "reviewer",
              name: "검토자",
              nameEn: "Reviewer",
              description: "검토",
              descriptionEn: "Review",
              prompt: "Review carefully",
            },
          ]}
          borderInput=""
          borderLight=""
          btnPrimary=""
          btnSecondary=""
          btnSmall=""
          customDesc=""
          customDescEn=""
          customName="custom"
          customNameEn=""
          expandedAgent="reviewer"
          generateAiPrompt={asyncNoop}
          generatingPrompt={false}
          goToStep={noop}
          labelStyle=""
          removeAgent={noop}
          selectTemplate={noop}
          selectedTemplate={null}
          setAgents={noop}
          setCustomDesc={noop}
          setCustomDescEn={noop}
          setCustomName={noop}
          setCustomNameEn={noop}
          setExpandedAgent={noop}
          step3Checklist={[]}
          stepBox=""
          stepHeadingRef={createRef<HTMLHeadingElement>()}
          tr={(_ko, en) => en}
        />
        <Step4ChannelSetup
          actionRow=""
          borderInput=""
          borderLight=""
          btnPrimary=""
          btnSecondary=""
          channelAssignments={[
            {
              agentId: "reviewer",
              agentName: "Reviewer",
              recommendedName: "reviewer",
              channelId: "",
              channelName: "reviewer",
            },
          ]}
          channelAssignmentsReady={true}
          goToStep={noop}
          guild={undefined}
          guilds={[]}
          hasSelectedGuild={false}
          inputStyle=""
          labelStyle=""
          selectedGuild=""
          setChannelAssignments={noop}
          setSelectedGuild={noop}
          step4Checklist={[]}
          stepBox=""
          stepHeadingRef={createRef<HTMLHeadingElement>()}
          tr={(_ko, en) => en}
        />
      </>,
    );

    const addButton = Array.from(container.querySelectorAll<HTMLButtonElement>("button")).find(
      (btn) => btn.textContent?.trim() === "+ Add",
    )!;
    expect(addButton.className).toContain("bg-[var(--accent)]");
    expect(addButton.className).toContain("text-[var(--accent-ink)]");
    expect(addButton.className).not.toContain("bg-indigo-600");

    const textarea = container.querySelector<HTMLTextAreaElement>("textarea")!;
    expect(textarea.getAttribute("aria-label")).toBe("System prompt for Reviewer");
    expect(textarea.className).toContain("max-[899px]:text-base");
    expect(textarea.className).toContain("pointer-coarse:text-base");

    const textInputs = Array.from(container.querySelectorAll<HTMLInputElement>('input[type="text"]'));
    expect(textInputs).toHaveLength(5);
    for (const input of textInputs) {
      expect(input.className).toContain("min-h-11");
      expect(input.className).toContain("max-[899px]:text-base");
      expect(input.className).toContain("pointer-coarse:text-base");
    }

    const warnBox = Array.from(container.querySelectorAll<HTMLElement>("div")).find((el) =>
      el.className.includes("bg-[var(--warn-soft)]"),
    );
    expect(warnBox).toBeTruthy();
    expect(warnBox!.className).toContain("text-[var(--warn-fg)]");
  });
});
