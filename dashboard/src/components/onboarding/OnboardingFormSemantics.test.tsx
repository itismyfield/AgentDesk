// @vitest-environment happy-dom

import { createRef, type ReactNode } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { Step1BotConnection } from "./Step1BotConnection";
import { Step3AgentSelection } from "./Step3AgentSelection";
import { Step4ChannelSetup } from "./Step4ChannelSetup";
import { Step5OwnerConfirm } from "./Step5OwnerConfirm";

const english = (_ko: string, en: string) => en;
const korean = (ko: string, _en: string) => ko;
const noop = () => undefined;
const asyncNoop = async () => undefined;

function render(element: ReactNode): HTMLDivElement {
  const container = document.createElement("div");
  container.innerHTML = renderToStaticMarkup(element);
  return container;
}

function renderStep1(tr = english): HTMLDivElement {
  return render(
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
      commandBots={[
        { provider: "claude", token: "", botInfo: null },
        { provider: "codex", token: "", botInfo: null },
      ]}
      commandBotsReady={false}
      goToStep={noop}
      inputStyle=""
      makeInviteUrl={() => ""}
      notifyBotInfo={null}
      notifyToken=""
      permissions={{ announce: "", command: "", notify: "" }}
      setAnnounceToken={noop}
      setCommandBots={noop}
      setNotifyToken={noop}
      step1Checklist={[]}
      stepBox=""
      stepHeadingRef={createRef<HTMLHeadingElement>()}
      tr={tr}
      validateStep1={asyncNoop}
      validating={false}
    />,
  );
}

describe("onboarding form semantics", () => {
  it("gives every token input a unique name and shared credential-manager hints", () => {
    const tokenInputs = Array.from(
      renderStep1().querySelectorAll<HTMLInputElement>('input[type="password"]'),
    );

    expect(tokenInputs.map((input) => input.getAttribute("aria-label"))).toEqual([
      "Command bot 1 token",
      "Command bot 2 token",
      "Communication bot token",
      "Notification bot token (optional)",
    ]);
    for (const input of tokenInputs) {
      expect(input.getAttribute("autocomplete")).toBe("new-password");
      expect(input.getAttribute("autocapitalize")).toBe("none");
      expect(input.getAttribute("autocorrect")).toBe("off");
      expect(input.getAttribute("spellcheck")).toBe("false");
      expect(input.getAttribute("data-1p-ignore")).toBe("true");
    }

    const koreanLabels = Array.from(
      renderStep1(korean).querySelectorAll<HTMLInputElement>('input[type="password"]'),
      (input) => input.getAttribute("aria-label"),
    );
    expect(koreanLabels).toEqual([
      "실행 봇 1 토큰",
      "실행 봇 2 토큰",
      "통신 봇 토큰",
      "알림 봇 토큰 (선택)",
    ]);
  });

  it("names agent prompt and custom-agent fields from their rendered context", () => {
    const target = render(
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
        customName=""
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
        tr={english}
      />,
    );

    expect(target.querySelector("textarea")?.getAttribute("aria-label")).toBe(
      "System prompt for Reviewer",
    );
    expect(
      Array.from(target.querySelectorAll<HTMLInputElement>('input[type="text"]'), (input) =>
        input.getAttribute("aria-label"),
      ),
    ).toEqual([
      "Agent name",
      "Brief description",
      "English name (optional)",
      "English description (optional)",
    ]);
  });

  it("names server and per-agent channel controls in both channel modes", () => {
    const commonProps = {
      actionRow: "",
      borderInput: "",
      borderLight: "",
      btnPrimary: "",
      btnSecondary: "",
      channelAssignments: [
        {
          agentId: "reviewer",
          agentName: "Reviewer",
          recommendedName: "reviewer",
          channelId: "",
          channelName: "reviewer",
        },
      ],
      channelAssignmentsReady: true,
      goToStep: noop,
      guilds: [
        { id: "guild-1", name: "Primary", channels: [] },
        { id: "guild-2", name: "Secondary", channels: [] },
      ],
      hasSelectedGuild: true,
      inputStyle: "",
      labelStyle: "",
      selectedGuild: "guild-1",
      setChannelAssignments: noop,
      setSelectedGuild: noop,
      step4Checklist: [],
      stepBox: "",
      stepHeadingRef: createRef<HTMLHeadingElement>(),
      tr: english,
    };
    const selectedGuild = {
      id: "guild-1",
      name: "Primary",
      channels: [{ id: "channel-1", name: "existing" }],
    };

    const selectMode = render(
      <Step4ChannelSetup {...commonProps} guild={selectedGuild} />,
    );
    expect(
      Array.from(selectMode.querySelectorAll("select"), (select) =>
        select.getAttribute("aria-label"),
      ),
    ).toEqual(["Discord server", "Channel for Reviewer"]);

    const createMode = render(
      <Step4ChannelSetup {...commonProps} guild={undefined} />,
    );
    expect(
      createMode.querySelector('input[readonly]')?.getAttribute("aria-label"),
    ).toBe("Channel name for Reviewer");
  });

  it("names the owner identifier independently of its numeric placeholder", () => {
    const target = render(
      <Step5OwnerConfirm
        actionRow=""
        announceBotInfo={null}
        announceToken=""
        applySummary={[]}
        borderInput=""
        borderLight=""
        btnPrimary=""
        btnSecondary=""
        channelAssignments={[]}
        commandBots={[]}
        completing={false}
        completionChecklist={null}
        completionReady={false}
        confirmRerunOverwrite={false}
        goToStep={noop}
        guilds={[]}
        handleComplete={asyncNoop}
        hasExistingSetup={false}
        inputStyle=""
        notifyToken=""
        onComplete={noop}
        ownerId=""
        selectedGuild=""
        setConfirmRerunOverwrite={noop}
        setOwnerId={noop}
        step5Checklist={[]}
        stepBox=""
        stepHeadingRef={createRef<HTMLHeadingElement>()}
        tr={english}
      />,
    );

    expect(target.querySelector('input[type="text"]')?.getAttribute("aria-label")).toBe(
      "Owner Discord ID",
    );
  });
});
