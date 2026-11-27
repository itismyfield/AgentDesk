import type { Dispatch, RefObject, SetStateAction } from "react";

import { Select } from "../../kit/free/select-clean";
import type { ChannelAssignment } from "../onboardingDraft";
import {
  ChecklistPanel,
  type ChecklistItem,
} from "./OnboardingWizardSections";

type Tr = (ko: string, en: string) => string;

interface Guild {
  id: string;
  name: string;
  channels: Array<{ id: string; name: string; category_id?: string }>;
}

interface Step4ChannelSetupProps {
  actionRow: string;
  borderInput: string;
  borderLight: string;
  btnPrimary: string;
  btnSecondary: string;
  channelAssignments: ChannelAssignment[];
  channelAssignmentsReady: boolean;
  goToStep: (step: number) => void;
  guild: Guild | undefined;
  guilds: Guild[];
  hasSelectedGuild: boolean;
  inputStyle: string;
  labelStyle: string;
  selectedGuild: string;
  setChannelAssignments: Dispatch<SetStateAction<ChannelAssignment[]>>;
  setSelectedGuild: Dispatch<SetStateAction<string>>;
  step4Checklist: ChecklistItem[];
  stepBox: string;
  stepHeadingRef: RefObject<HTMLHeadingElement | null>;
  tr: Tr;
}

export function Step4ChannelSetup({
  actionRow,
  borderInput,
  borderLight,
  btnPrimary,
  btnSecondary,
  channelAssignments,
  channelAssignmentsReady,
  goToStep,
  guild,
  guilds,
  hasSelectedGuild,
  labelStyle,
  selectedGuild,
  setChannelAssignments,
  setSelectedGuild,
  step4Checklist,
  stepBox,
  stepHeadingRef,
  tr,
}: Step4ChannelSetupProps) {
  return (
    <div className={stepBox} style={{ borderColor: borderLight }}>
      <div>
        <h2 ref={stepHeadingRef} tabIndex={-1} className="text-base font-semibold outline-none" style={{ color: "var(--th-text-heading)" }}>
          {tr("채널 설정", "Channel Setup")}
        </h2>
        <p className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
          {tr(
            "각 에이전트가 사용할 Discord 채널을 실제 서버 기준으로 설정합니다. 기존 채널을 재사용하거나, 추천 이름으로 새 채널을 만들 수 있습니다.",
            "Configure real Discord channels for each agent. Reuse existing channels or create new ones from the recommended names.",
          )}
        </p>
      </div>

      {guilds.length > 0 && (
        <div>
          {guilds.length === 1 ? (
            <>
              <div className={labelStyle} style={{ color: "var(--th-text-secondary)" }}>
                {tr("Discord 서버", "Discord Server")}
              </div>
              <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                {guilds[0].name}
              </div>
            </>
          ) : (
            <Select
              id="onboarding-discord-guild"
              label={tr("Discord 서버", "Discord Server")}
              aria-label={tr("Discord 서버", "Discord server")}
              size="lg"
              className="w-full"
              value={selectedGuild}
              placeholder={tr("서버 선택", "Select server")}
              onChange={(value) => setSelectedGuild(value)}
              options={[
                { value: "", label: tr("서버 선택", "Select server") },
                ...guilds.map((item) => ({ value: item.id, label: item.name })),
              ]}
            />
          )}
        </div>
      )}

      {guilds.length === 0 && (
        <div className="rounded-lg border border-[color-mix(in_oklch,var(--warn)_38%,var(--line))] bg-[var(--warn-soft)] p-4 text-sm text-[var(--warn-fg)]">
          <div>
            {tr(
              "통신 봇이 어떤 서버에도 초대되지 않았거나 토큰 검증이 끝나지 않았습니다. 실제 채널 생성 보장을 위해 Step 1로 돌아가 봇 초대부터 완료하세요.",
              "The communication bot is not invited to any server yet, or token validation is incomplete. Go back to Step 1 and finish the invite before continuing.",
            )}
          </div>
        </div>
      )}

      <div className="space-y-2">
        {channelAssignments.map((assignment, index) => (
          <div key={assignment.agentId} className="rounded-lg p-3 border space-y-2" style={{ borderColor: "var(--th-border-subtle)" }}>
            <div className="flex items-center gap-2">
              <span className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>{assignment.agentName}</span>
              <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>-&gt;</span>
            </div>
            <div className="flex flex-col sm:flex-row gap-2">
              {guild && guild.channels.length > 0 ? (
                <Select
                  aria-label={tr(
                    `${assignment.agentName} 채널`,
                    `Channel for ${assignment.agentName}`,
                  )}
                  size="lg"
                  className="min-w-0 flex-1"
                  value={assignment.channelId}
                  onChange={(nextChannelId) => {
                    const channel = guild.channels.find((candidate) => candidate.id === nextChannelId);
                    setChannelAssignments((current) => {
                      const copy = [...current];
                      copy[index] = {
                        ...assignment,
                        channelId: nextChannelId,
                        channelName: channel?.name || assignment.recommendedName,
                      };
                      return copy;
                    });
                  }}
                  options={[
                    {
                      value: "",
                      label: tr(`새 채널: #${assignment.recommendedName}`, `New: #${assignment.recommendedName}`),
                    },
                    ...guild.channels.map((channel) => ({
                      value: channel.id,
                      label: `#${channel.name}`,
                    })),
                  ]}
                />
              ) : (
                <input
                  type="text"
                  aria-label={tr(
                    `${assignment.agentName} 채널 이름`,
                    `Channel name for ${assignment.agentName}`,
                  )}
                  value={assignment.channelName || assignment.recommendedName}
                  readOnly
                  className="min-h-11 min-w-0 flex-1 rounded-md px-3 py-2 text-sm max-[899px]:text-base pointer-coarse:text-base bg-surface-subtle border"
                  style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
                  placeholder={assignment.recommendedName}
                />
              )}
            </div>
          </div>
        ))}
      </div>

      {guild ? (
        <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
          {tr(
            "\"새 채널\"을 선택하면 통신 봇이 온보딩 완료 시 해당 채널을 자동 생성합니다.",
            "Selecting \"New\" makes the communication bot create that channel automatically during onboarding.",
          )}
        </p>
      ) : (
        <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
          {tr(
            "서버를 선택하면 각 추천 채널명을 기존 채널에 연결하거나 새 채널로 생성할 수 있습니다.",
            "Once a server is selected, each recommended channel can be linked to an existing channel or created as a new one.",
          )}
        </p>
      )}

      <ChecklistPanel title={tr("Step 4 체크리스트", "Step 4 checklist")} items={step4Checklist} />

      <div className={actionRow}>
        <button type="button" onClick={() => goToStep(3)} className={btnSecondary} style={{ borderColor: "var(--th-border-subtle)" }}>
          {tr("이전", "Back")}
        </button>
        <button type="button" onClick={() => goToStep(5)} disabled={!hasSelectedGuild || !channelAssignmentsReady} className={btnPrimary}>
          {tr("다음", "Next")}
        </button>
      </div>
    </div>
  );
}
