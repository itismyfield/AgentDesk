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
import EmojiPicker from "../src/components/agent-manager/EmojiPicker";
import EmojiPickerLibraryPanel from "../src/components/agent-manager/EmojiPickerLibraryPanel";
import PipelineVisualEditor from "../src/components/agent-manager/PipelineVisualEditor";
import PipelineVisualEditorPhaseGatePanel from "../src/components/agent-manager/PipelineVisualEditorPhaseGatePanel";
import PipelineVisualEditorStatePanel from "../src/components/agent-manager/PipelineVisualEditorStatePanel";
import PipelineVisualEditorToolbar from "../src/components/agent-manager/PipelineVisualEditorToolbar";
import PipelineVisualEditorTransitionPanel from "../src/components/agent-manager/PipelineVisualEditorTransitionPanel";
import { FilterChipGroup } from "../src/components/audit/AuditDetailFrame";
import { TaskInspector } from "../src/components/campaigns/CampaignGroupInspector";
import PipelineStatePicker from "../src/components/campaigns/PipelineStatePicker";
import { ActionMenu } from "../src/components/common/ActionMenu";
import { BottomSheet } from "../src/components/common/overlay/BottomSheet";
import { Drawer } from "../src/components/common/overlay/Drawer";
import { ToastProvider, useToast } from "../src/components/common/overlay/ToastProvider";
import { RoutineDetailBody } from "../src/components/dashboard/RoutineDetail";
import { OnboardingWizardLayout, ONBOARDING_WIZARD_STYLES } from "../src/components/onboarding/OnboardingWizardLayout";
import { Step3AgentSelection } from "../src/components/onboarding/Step3AgentSelection";
import { Step4ChannelSetup } from "../src/components/onboarding/Step4ChannelSetup";
import { I18nProvider } from "../src/i18n";
import type { PipelineConfigFull } from "../src/types";

vi.mock("emoji-picker-react", () => ({
  default: ({ searchPlaceholder, searchClearButtonLabel }: { searchPlaceholder?: string; searchClearButtonLabel?: string }) => (
    <div data-testid="mock-epr" data-placeholder={searchPlaceholder} data-clear-label={searchClearButtonLabel}>
      <input className="epr-search" aria-label={searchPlaceholder} />
    </div>
  ),
  EmojiStyle: { NATIVE: "native" },
  Theme: { DARK: "dark", LIGHT: "light" },
}));

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

  it("renders valid color-mix border styles, z-[110] narrow containment, 44px dismiss target, and 4-locale i18n in ToastProvider", async () => {
    function ToastTrigger() {
      const { show } = useToast();
      return (
        <button
          type="button"
          data-testid="fire-toast"
          onClick={() => show({ message: "Deployment failed", variant: "error" })}
        >
          Trigger toast
        </button>
      );
    }

    await render(
      <I18nProvider language="ja">
        <ToastProvider>
          <ToastTrigger />
        </ToastProvider>
      </I18nProvider>,
    );

    const setPropertySpy = vi.spyOn(window.CSSStyleDeclaration.prototype, "setProperty");
    const trigger = container.querySelector<HTMLButtonElement>('[data-testid="fire-toast"]')!;
    await act(async () => {
      trigger.click();
    });

    const borderColorCalls = setPropertySpy.mock.calls
      .filter(([prop]) => prop === "border-color")
      .map(([, val]) => String(val));
    expect(borderColorCalls).toContain("color-mix(in oklch, var(--err) 45%, var(--line))");
    for (const val of borderColorCalls) {
      expect(val).not.toMatch(/\)\s*55/);
    }

    const region = document.querySelector<HTMLElement>('[role="region"][aria-label="通知"]')!;
    expect(region).not.toBeNull();
    expect(region.className).toContain("z-[110]");
    expect(region.className).toContain("left-3");
    expect(region.className).toContain("sm:left-auto");

    const statusCard = region.querySelector<HTMLElement>('[role="status"]')!;
    expect(statusCard.style.color).toBe("var(--fg)");
    const dot = statusCard.querySelector<HTMLElement>('span[aria-hidden="true"]')!;
    expect(dot.getAttribute("style")).toContain("var(--err)");

    const dismissButton = statusCard.querySelector<HTMLButtonElement>('button[aria-label="閉じる"]')!;
    expect(dismissButton).not.toBeNull();
    expect(dismissButton.className).toContain("max-[899px]:h-11");
    expect(dismissButton.className).toContain("pointer-coarse:h-11");
  });

  it("associates every <label> with its <input>/<textarea> across PipelineVisualEditorPhaseGatePanel, StatePanel, and TransitionPanel", async () => {
    const getByLabelText = (rootEl: HTMLElement, labelText: string): HTMLElement => {
      const label = Array.from(rootEl.querySelectorAll("label")).find(
        (el) => el.textContent?.trim() === labelText,
      );
      expect(label, `Missing <label> with text "${labelText}"`).toBeTruthy();
      const htmlFor = label!.getAttribute("htmlFor") ?? label!.getAttribute("for");
      if (htmlFor) {
        const control = document.getElementById(htmlFor);
        expect(control, `Missing control with id="${htmlFor}" for "${labelText}"`).not.toBeNull();
        return control!;
      }
      const nested = label!.querySelector<HTMLElement>("input, textarea, select");
      expect(nested, `Missing nested control inside <label> "${labelText}"`).not.toBeNull();
      return nested!;
    };

    const draft: PipelineConfigFull = {
      name: "default",
      version: 1,
      states: [{ id: "review", label: "Review", terminal: false }],
      transitions: [{ from: "review", to: "done", type: "gated", gates: ["qa_gate"] }],
      gates: { qa_gate: { type: "builtin", check: "review_verdict_pass", description: "QA check" } },
      hooks: { review: { on_enter: ["OnReviewEnter"], on_exit: ["OnStateExit"] } },
      events: {},
      clocks: { review: { set: "started_at", mode: "coalesce" } },
      phase_gate: { dispatch_to: "self", dispatch_type: "phase-gate" },
    };

    const mockCtx: any = {
      tr: (_ko: string, en: string) => en,
      isFsmVariant: false,
      pipelineDraft: draft,
      selectedState: draft.states[0],
      selectedTransition: draft.transitions[0],
      selectedTransitionIndex: 0,
      selectedTransitionGates: ["qa_gate"],
    };
    const mockActions: any = {
      updatePhaseGate: vi.fn(),
      updateState: vi.fn(),
      updateStateHooks: vi.fn(),
      clearStateHooks: vi.fn(),
      clearStateClock: vi.fn(),
      updateStateClock: vi.fn(),
      updateTransition: vi.fn(),
      updateTransitionGates: vi.fn(),
      addGate: vi.fn(),
      updateGate: vi.fn(),
    };

    await render(
      <>
        <div data-testid="phase-gate-panel">
          <PipelineVisualEditorPhaseGatePanel ctx={mockCtx} actions={mockActions} />
        </div>
        <div data-testid="state-panel">
          <PipelineVisualEditorStatePanel ctx={mockCtx} actions={mockActions} />
        </div>
        <div data-testid="transition-panel">
          <PipelineVisualEditorTransitionPanel ctx={mockCtx} actions={mockActions} />
        </div>
      </>,
    );

    const phaseGateRoot = container.querySelector<HTMLElement>('[data-testid="phase-gate-panel"]')!;
    expect((getByLabelText(phaseGateRoot, "Checked by (self = the entry's agent)") as HTMLInputElement).value).toBe("self");
    expect((getByLabelText(phaseGateRoot, "Dispatch type") as HTMLInputElement).value).toBe("phase-gate");

    const stateRoot = container.querySelector<HTMLElement>('[data-testid="state-panel"]')!;
    expect((getByLabelText(stateRoot, "Label") as HTMLInputElement).value).toBe("Review");
    expect((getByLabelText(stateRoot, "Terminal state") as HTMLInputElement).type).toBe("checkbox");
    expect((getByLabelText(stateRoot, "on_enter hooks") as HTMLTextAreaElement).value).toBe("OnReviewEnter");
    expect((getByLabelText(stateRoot, "on_exit hooks") as HTMLTextAreaElement).value).toBe("OnStateExit");
    expect((getByLabelText(stateRoot, "Clock field") as HTMLInputElement).value).toBe("started_at");
    expect((getByLabelText(stateRoot, "Clock mode") as HTMLInputElement).value).toBe("coalesce");

    for (const clearLabel of ["Clear hooks", "Clear clock"]) {
      const btn = Array.from(stateRoot.querySelectorAll("button")).find((b) => b.textContent?.trim() === clearLabel)!;
      expect(btn.getAttribute("type")).toBe("button");
      expect(btn.className).toContain("max-[899px]:min-h-11");
      expect(btn.className).toContain("pointer-coarse:min-h-11");
    }

    const transitionRoot = container.querySelector<HTMLElement>('[data-testid="transition-panel"]')!;
    expect((getByLabelText(transitionRoot, "Description") as HTMLInputElement).value).toBe("QA check");
    const gateToggle = Array.from(transitionRoot.querySelectorAll("button")).find((b) => b.textContent?.trim() === "qa_gate")!;
    expect(gateToggle.getAttribute("aria-pressed")).toBe("true");
    const addGateBtn = Array.from(transitionRoot.querySelectorAll("button")).find((b) => b.textContent?.trim() === "+ Gate")!;
    expect(addGateBtn.getAttribute("type")).toBe("button");
  });

  it("sets type=button and min-h-11 on SaveButton, aria-pressed on LevelSwitch, and role=alert on OnboardingWizardLayout error", async () => {
    const toolbarCtx: any = {
      tr: (_ko: string, en: string) => en,
      isFsmVariant: false,
      editorHelpText: "Help text",
      selectedAgentDetail: null,
      level: "repo",
      selectedAgentId: "agent-1",
      saving: false,
      overrideExists: true,
      hasVisibleChanges: true,
    };
    const toolbarActions: any = {
      setLevel: vi.fn(),
      setReloadKey: vi.fn(),
      handleClearOverride: vi.fn(),
      handleSave: vi.fn(),
    };

    await render(
      <>
        <div data-testid="toolbar-wrap">
          <PipelineVisualEditorToolbar ctx={toolbarCtx} actions={toolbarActions} />
        </div>
        <OnboardingWizardLayout
          step={0}
          totalSteps={5}
          isKo={false}
          tr={(_ko: string, en: string) => en}
          styles={ONBOARDING_WIZARD_STYLES}
          stepStatusItems={[]}
          setItemRef={() => () => {}}
          draftNoticeVisible={true}
          draftNoticeTitle="Saved draft"
          draftNoticeDetail="Resume"
          setDraftNoticeVisible={() => {}}
          resetDraft={() => {}}
          error="Token check failed"
        />
      </>,
    );

    const toolbarWrap = container.querySelector<HTMLElement>('[data-testid="toolbar-wrap"]')!;
    const buttons = Array.from(toolbarWrap.querySelectorAll<HTMLButtonElement>("button"));
    expect(buttons.length).toBeGreaterThanOrEqual(5);
    for (const btn of buttons) {
      expect(btn.getAttribute("type")).toBe("button");
    }

    const repoBtn = buttons.find((b) => b.textContent?.trim() === "Repository")!;
    const agentBtn = buttons.find((b) => b.textContent?.trim() === "Agent")!;
    expect(repoBtn.getAttribute("aria-pressed")).toBe("true");
    expect(agentBtn.getAttribute("aria-pressed")).toBe("false");

    const saveBtn = buttons.find((b) => b.textContent?.trim() === "Save changes")!;
    expect(saveBtn.getAttribute("type")).toBe("button");
    expect(saveBtn.className).toContain("min-h-11");

    const errorAlert = container.querySelector<HTMLElement>('[role="alert"]')!;
    expect(errorAlert).not.toBeNull();
    expect(errorAlert.textContent).toBe("Token check failed");
    expect(errorAlert.className).toContain("bg-[var(--err-soft)]");
    expect(errorAlert.className).toContain("text-[var(--err-fg)]");
  });

  it("syncs document.documentElement.lang across ko, en, ja, and zh in I18nProvider", async () => {
    for (const lang of ["ko", "en", "ja", "zh"] as const) {
      await render(
        <I18nProvider language={lang}>
          <div>{lang}</div>
        </I18nProvider>,
      );
      expect(document.documentElement.lang).toBe(lang);
    }
  });

  it("renders EmojiPicker dialog at z-[110] with localized labels and 16px search input classes", async () => {
    await render(
      <I18nProvider language="ja">
        <EmojiPicker value="🤖" onChange={() => {}} />
        <EmojiPickerLibraryPanel height={360} width={300} value="🤖" onSelect={() => {}} />
      </I18nProvider>,
    );

    const trigger = container.querySelector<HTMLButtonElement>("button")!;
    expect(trigger.getAttribute("aria-label")).toBe("選択中のアイコン: 🤖、アイコンを変更");

    await act(async () => {
      trigger.click();
    });

    const dialog = container.querySelector<HTMLElement>('div[role="dialog"]')!;
    expect(dialog).not.toBeNull();
    expect(dialog.getAttribute("aria-label")).toBe("アイコンを選択");
    expect(dialog.className).toContain("z-[110]");
    expect(dialog.className).toContain("max-w-[calc(100vw-1.5rem)]");

    const mockEpr = container.querySelector<HTMLElement>('[data-testid="mock-epr"]')!;
    expect(mockEpr.getAttribute("data-placeholder")).toBe("絵文字を検索");
    expect(mockEpr.getAttribute("data-clear-label")).toBe("絵文字検索をクリア");
    expect(mockEpr.parentElement?.className).toContain("max-[899px]:[&_input.epr-search]:text-base");
    expect(mockEpr.parentElement?.className).toContain("pointer-coarse:[&_input.epr-search]:text-base");

    const outside = document.createElement("div");
    document.body.appendChild(outside);
    await act(async () => {
      outside.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
    });
    expect(container.querySelector('div[role="dialog"]')).toBeNull();
    outside.remove();

    await render(
      <I18nProvider language="en">
        <EmojiPicker value="" onChange={() => {}} />
      </I18nProvider>,
    );
    expect(container.querySelector("button")?.getAttribute("aria-label")).toBe("Open icon picker");
  });

  it("moves focus into ActionMenu on open, roves enabled menuitems via ArrowDown/ArrowUp/Home/End, and closes on outside pointerdown", async () => {
    await render(
      <ActionMenu
        label="Row actions"
        items={[
          { key: "disabled-first", label: "Disabled", disabled: true, onSelect: () => {} },
          { key: "edit", label: "Edit", onSelect: () => {} },
          { key: "duplicate", label: "Duplicate", onSelect: () => {} },
          { key: "delete", label: "Delete", danger: true, onSelect: () => {} },
        ]}
      />,
    );

    const trigger = container.querySelector<HTMLButtonElement>('button[aria-label="Row actions"]')!;
    await act(async () => {
      trigger.click();
    });

    const menu = container.querySelector<HTMLElement>('[role="menu"]')!;
    expect(menu).not.toBeNull();
    expect(menu.className).toContain("max-w-[calc(100vw-1.5rem)]");

    const items = Array.from(menu.querySelectorAll<HTMLButtonElement>('[role="menuitem"]'));
    expect(document.activeElement).toBe(items[1]); // First enabled ("Edit")

    await act(async () => {
      items[1].dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true, cancelable: true }));
    });
    expect(document.activeElement).toBe(items[2]); // "Duplicate"

    await act(async () => {
      items[2].dispatchEvent(new KeyboardEvent("keydown", { key: "End", bubbles: true, cancelable: true }));
    });
    expect(document.activeElement).toBe(items[3]); // "Delete"

    await act(async () => {
      items[3].dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true, cancelable: true }));
    });
    expect(document.activeElement).toBe(items[1]); // Wraps to "Edit"

    await act(async () => {
      items[1].dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowUp", bubbles: true, cancelable: true }));
    });
    expect(document.activeElement).toBe(items[3]); // Wraps back to "Delete"

    await act(async () => {
      items[3].dispatchEvent(new KeyboardEvent("keydown", { key: "Home", bubbles: true, cancelable: true }));
    });
    expect(document.activeElement).toBe(items[1]); // "Edit"

    const outside = document.createElement("div");
    document.body.appendChild(outside);
    await act(async () => {
      outside.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
    });
    expect(container.querySelector('[role="menu"]')).toBeNull();
    outside.remove();
  });

  it("stacks RoutineDetailBody settings <dl> below 360px and scopes FilterChipGroup 30px height to fine pointers", async () => {
    await render(
      <>
        <RoutineDetailBody
          routine={{
            id: "r-1",
            agent_id: "ada",
            script_ref: "daily.sh",
            name: "daily-sync",
            status: "enabled",
            execution_strategy: "fresh",
            schedule: "0 9 * * *",
            next_due_at: null,
            last_run_at: null,
            last_result: null,
            checkpoint: null,
            discord_thread_id: null,
            timeout_secs: 300,
            in_flight_run_id: null,
            created_at: "2026-11-26T00:00:00Z",
            updated_at: "2026-11-26T00:00:00Z",
          }}
          threadUrl={null}
          runs={[]}
          runsLoading={false}
          runsError={false}
          t={(input) => (typeof input === "string" ? input : input.en)}
          localeTag="en-US"
          language="en"
          now={Date.now()}
        />
        <FilterChipGroup
          value="all"
          onChange={() => {}}
          ariaLabel="Audit filter"
          options={[
            { value: "all", label: "All", count: 3 },
            { value: "warn", label: "Warnings", count: 1 },
          ]}
        />
      </>,
    );

    const settingsDl = container.querySelector<HTMLElement>('[data-testid="routine-detail"] dl')!;
    expect(settingsDl.className).toContain("grid-cols-1");
    expect(settingsDl.className).toContain("min-[360px]:grid-cols-2");

    const chipButton = container.querySelector<HTMLButtonElement>('[role="radiogroup"] [role="radio"]')!;
    expect(chipButton.className).toContain("min-[900px]:pointer-fine:min-h-[30px]");
    expect(chipButton.className).toContain("pointer-coarse:min-h-11");
    expect(chipButton.className).not.toContain("sm:min-h-[30px]");
  });
});
