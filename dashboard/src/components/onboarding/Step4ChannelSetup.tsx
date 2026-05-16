import type { Dispatch, RefObject, SetStateAction } from "react";

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
  inputStyle,
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
        <h2 ref={stepHeadingRef} tabIndex={-1} className="text-lg font-semibold outline-none" style={{ color: "var(--th-text-heading)" }}>
          {tr("채널 설정", "Channel Setup")}
        </h2>
        <p className="text-sm mt-1" style={{ color: "var(--th-text-muted)" }}>
          {tr(
            "각 에이전트가 사용할 Discord 채널을 실제 서버 기준으로 설정합니다. 기존 채널을 재사용하거나, 추천 이름으로 새 채널을 만들 수 있습니다.",
            "Configure real Discord channels for each agent. Reuse existing channels or create new ones from the recommended names.",
          )}
        </p>
      </div>

      {guilds.length > 0 && (
        <div>
          <label className={labelStyle} style={{ color: "var(--th-text-secondary)" }}>
            {tr("Discord 서버", "Discord Server")}
          </label>
          {guilds.length === 1 ? (
            <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
              {guilds[0].name}
            </div>
          ) : (
            <select
              value={selectedGuild}
              onChange={(event) => setSelectedGuild(event.target.value)}
              className={inputStyle}
              style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
            >
              <option value="">{tr("서버 선택", "Select server")}</option>
              {guilds.map((item) => (
                <option key={item.id} value={item.id}>{item.name}</option>
              ))}
            </select>
          )}
        </div>
      )}

      {guilds.length === 0 && (
        <div className="rounded-xl p-4 text-sm" style={{ backgroundColor: "rgba(251,191,36,0.08)", border: "1px solid rgba(251,191,36,0.2)" }}>
          <div style={{ color: "#fde68a" }}>
            {tr(
              "통신 봇이 어떤 서버에도 초대되지 않았거나 토큰 검증이 끝나지 않았습니다. 실제 채널 생성 보장을 위해 Step 1로 돌아가 봇 초대부터 완료하세요.",
              "The communication bot is not invited to any server yet, or token validation is incomplete. Go back to Step 1 and finish the invite before continuing.",
            )}
          </div>
        </div>
      )}

      <div className="space-y-2">
        {channelAssignments.map((assignment, index) => (
          <div key={assignment.agentId} className="rounded-xl p-3 border space-y-2" style={{ borderColor: "rgba(148,163,184,0.15)" }}>
            <div className="flex items-center gap-2">
              <span className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>{assignment.agentName}</span>
              <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>-&gt;</span>
            </div>
            <div className="flex flex-col sm:flex-row gap-2">
              {guild && guild.channels.length > 0 ? (
                <select
                  value={assignment.channelId}
                  onChange={(event) => {
                    const channel = guild.channels.find((candidate) => candidate.id === event.target.value);
                    setChannelAssignments((current) => {
                      const copy = [...current];
                      copy[index] = {
                        ...assignment,
                        channelId: event.target.value,
                        channelName: channel?.name || assignment.recommendedName,
                      };
                      return copy;
                    });
                  }}
                  className="flex-1 rounded-lg px-3 py-2 text-sm bg-surface-subtle border"
                  style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
                >
                  <option value="">{tr(`새 채널: #${assignment.recommendedName}`, `New: #${assignment.recommendedName}`)}</option>
                  {guild.channels.map((channel) => (
                    <option key={channel.id} value={channel.id}>#{channel.name}</option>
                  ))}
                </select>
              ) : (
                <input
                  type="text"
                  value={assignment.channelName || assignment.recommendedName}
                  readOnly
                  className="flex-1 rounded-lg px-3 py-2 text-sm bg-surface-subtle border"
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
        <button type="button" onClick={() => goToStep(3)} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
          {tr("이전", "Back")}
        </button>
        <button type="button" onClick={() => goToStep(5)} disabled={!hasSelectedGuild || !channelAssignmentsReady} className={btnPrimary}>
          {tr("다음", "Next")}
        </button>
      </div>
    </div>
  );
}
