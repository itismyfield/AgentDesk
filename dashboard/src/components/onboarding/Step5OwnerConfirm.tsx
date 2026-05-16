import type { Dispatch, RefObject, SetStateAction } from "react";

import {
  clearOnboardingDraft,
  type BotInfo,
  type ChannelAssignment,
  type CommandBotEntry,
} from "../onboardingDraft";
import { providerLabel } from "./providerConfig";
import {
  ChecklistPanel,
  Tip,
  type ChecklistItem,
  type CompletionChecklistItem,
} from "./OnboardingWizardSections";

type Tr = (ko: string, en: string) => string;

interface Guild {
  id: string;
  name: string;
}

interface ApplySummaryItem {
  detail: string;
  key: string;
  label: string;
}

interface Step5OwnerConfirmProps {
  actionRow: string;
  announceBotInfo: BotInfo | null;
  announceToken: string;
  applySummary: ApplySummaryItem[];
  borderInput: string;
  borderLight: string;
  btnPrimary: string;
  btnSecondary: string;
  channelAssignments: ChannelAssignment[];
  commandBots: CommandBotEntry[];
  completing: boolean;
  completionChecklist: CompletionChecklistItem[] | null;
  completionReady: boolean;
  confirmRerunOverwrite: boolean;
  goToStep: (step: number) => void;
  guilds: Guild[];
  handleComplete: () => Promise<void>;
  hasExistingSetup: boolean;
  inputStyle: string;
  notifyToken: string;
  onComplete: () => void;
  ownerId: string;
  selectedGuild: string;
  setConfirmRerunOverwrite: Dispatch<SetStateAction<boolean>>;
  setOwnerId: Dispatch<SetStateAction<string>>;
  step5Checklist: ChecklistItem[];
  stepBox: string;
  stepHeadingRef: RefObject<HTMLHeadingElement | null>;
  tr: Tr;
}

export function Step5OwnerConfirm({
  actionRow,
  announceBotInfo,
  announceToken,
  applySummary,
  borderInput,
  borderLight,
  btnPrimary,
  btnSecondary,
  channelAssignments,
  commandBots,
  completing,
  completionChecklist,
  completionReady,
  confirmRerunOverwrite,
  goToStep,
  guilds,
  handleComplete,
  hasExistingSetup,
  inputStyle,
  notifyToken,
  onComplete,
  ownerId,
  selectedGuild,
  setConfirmRerunOverwrite,
  setOwnerId,
  step5Checklist,
  stepBox,
  stepHeadingRef,
  tr,
}: Step5OwnerConfirmProps) {
  return (
    <div className={stepBox} style={{ borderColor: borderLight }}>
      <div>
        <h2 ref={stepHeadingRef} tabIndex={-1} className="text-lg font-semibold outline-none" style={{ color: "var(--th-text-heading)" }}>
          {tr("소유자 설정 및 확인", "Owner Setup & Confirm")}
        </h2>
      </div>

      <div className="rounded-xl p-4 border space-y-3" style={{ borderColor: borderLight }}>
        <div className="flex items-center gap-2">
          <span className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
            {tr("Discord 소유자 ID", "Discord Owner ID")}
          </span>
          <Tip text={tr(
            "소유자는 에이전트에게 직접 명령할 수 있고,\n관리자 기능에 접근할 수 있습니다.\n비워두면 처음 메시지를 보내는 사람이\n자동으로 소유자가 됩니다.",
            "The owner can command agents directly\nand access admin features.\nLeave blank to auto-register\nthe first message sender.",
          )} />
        </div>

        <input
          type="text"
          placeholder="123456789012345678"
          value={ownerId}
          onChange={(event) => setOwnerId(event.target.value)}
          className={inputStyle}
          style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
        />

        <div
          className="rounded-lg p-3 text-xs space-y-2"
          style={{ background: "color-mix(in srgb, var(--th-accent-primary-soft) 64%, transparent)" }}
        >
          <div className="font-medium" style={{ color: "var(--th-text-primary)" }}>
            {tr("Discord 사용자 ID 찾는 방법", "How to find your Discord User ID")}
          </div>
          <ol className="list-decimal list-inside space-y-1" style={{ color: "var(--th-text-secondary)" }}>
            <li>{tr("Discord 앱 하단 설정 -> 고급 -> 개발자 모드 활성화", "Discord Settings -> Advanced -> Enable Developer Mode")}</li>
            <li>{tr("왼쪽 사용자 목록에서 내 이름을 우클릭", "Right-click your name in the member list")}</li>
            <li>{tr("\"사용자 ID 복사\" 클릭 -> 위 입력란에 붙여넣기", "Click \"Copy User ID\" -> Paste above")}</li>
          </ol>
          <div className="mt-1" style={{ color: "var(--th-text-muted)" }}>
            {tr(
              "17~20자리 숫자입니다 (예: 123456789012345678)",
              "It's a 17-20 digit number (e.g., 123456789012345678)",
            )}
          </div>
        </div>

        <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
          {tr(
            "소유자로 등록되면: 에이전트에게 직접 명령 가능 · 관리자 권한 활성화 · 시스템 알림 수신",
            "As owner: Direct commands to agents · Admin access · System notifications",
          )}
        </div>
      </div>

      <div className="rounded-xl p-4 border space-y-3" style={{ borderColor: borderLight }}>
        <div className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
          {tr("설정 요약", "Setup Summary")}
        </div>
        <div className="space-y-2 text-sm" style={{ color: "var(--th-text-primary)" }}>
          <div className="flex items-center gap-2">
            <span style={{ color: "var(--th-text-muted)" }}>{tr("실행 봇", "Command")}</span>
            <span>
              {commandBots.map((bot) => `${providerLabel(bot.provider)}${bot.botInfo?.bot_name ? ` (${bot.botInfo.bot_name})` : ""}`).join(", ")}
            </span>
          </div>
          <div className="flex items-center gap-2">
            <span style={{ color: "var(--th-text-muted)" }}>{tr("통신 봇", "Comm")}</span>
            <span>{announceBotInfo?.bot_name || (announceToken ? tr("설정됨", "Set") : tr("미설정", "Not set"))}</span>
          </div>
          {notifyToken && (
            <div className="flex items-center gap-2">
              <span style={{ color: "var(--th-text-muted)" }}>{tr("알림 봇", "Notify")}</span>
              <span>{tr("설정됨", "Set")}</span>
            </div>
          )}

          {selectedGuild && (
            <div className="flex items-center gap-2">
              <span style={{ color: "var(--th-text-muted)" }}>{tr("서버", "Server")}</span>
              <span>{guilds.find((guild) => guild.id === selectedGuild)?.name || selectedGuild}</span>
            </div>
          )}

          <div className="border-t pt-2 mt-2" style={{ borderColor: borderLight }}>
            <div className="text-xs font-medium mb-1" style={{ color: "var(--th-text-muted)" }}>
              {tr(`에이전트 -> 채널 (${channelAssignments.length}개)`, `Agents -> Channels (${channelAssignments.length})`)}
            </div>
            {channelAssignments.map((assignment) => (
              <div key={assignment.agentId} className="text-xs py-0.5" style={{ color: "var(--th-text-secondary)" }}>
                {assignment.agentName} {"->"} #{assignment.channelName || assignment.recommendedName}
              </div>
            ))}
          </div>
        </div>
      </div>

      <div className="rounded-xl p-4 border space-y-3" style={{ borderColor: borderLight }}>
        <div className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
          {tr("완료 시 실제로 적용되는 항목", "What completion actually applies")}
        </div>
        <div className="space-y-2">
          {applySummary.map((item) => (
            <div
              key={item.key}
              className="rounded-lg border px-3 py-2"
              style={{ borderColor: "rgba(148,163,184,0.16)", backgroundColor: "rgba(15,23,42,0.28)" }}
            >
              <div className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
                {item.label}
              </div>
              <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                {item.detail}
              </div>
            </div>
          ))}
        </div>
      </div>

      {hasExistingSetup && (
        <div
          className="rounded-xl border p-4 space-y-3"
          style={{ borderColor: "rgba(251,191,36,0.24)", backgroundColor: "rgba(251,191,36,0.08)" }}
        >
          <div className="text-sm font-medium" style={{ color: "#fde68a" }}>
            {tr("기존 구성이 감지된 상태에서 다시 실행 중입니다.", "You are re-running onboarding on top of an existing setup.")}
          </div>
          <p className="text-xs leading-5" style={{ color: "var(--th-text-secondary)" }}>
            {tr(
              "현재 API는 기존 에이전트/채널 구성을 이 화면에 프리필하지 않습니다. 같은 role_id를 다시 적용하면 기존 agent row와 채널 매핑이 갱신될 수 있으니, 아래 요약을 먼저 확인한 뒤 실행하세요.",
              "The current API does not prefill the existing agent/channel layout in this screen. Re-applying the same role IDs can update existing agent rows and channel mappings, so review the summary before running completion.",
            )}
          </p>
          <label className="flex items-start gap-3 text-sm" style={{ color: "var(--th-text-primary)" }}>
            <input
              type="checkbox"
              checked={confirmRerunOverwrite}
              onChange={(event) => setConfirmRerunOverwrite(event.target.checked)}
              className="mt-0.5 h-4 w-4 rounded border"
            />
            <span>
              {tr(
                "기존 role_id 에이전트와 채널 매핑이 다시 적용될 수 있다는 점을 이해했고, 현재 요약 기준으로 재실행합니다.",
                "I understand that existing role-based agents and channel mappings may be applied again, and I want to re-run onboarding with the current summary.",
              )}
            </span>
          </label>
        </div>
      )}

      <ChecklistPanel
        title={
          completionChecklist
            ? tr("실제 적용 결과", "Applied setup result")
            : tr("Step 5 체크리스트", "Step 5 checklist")
        }
        items={completionChecklist ?? step5Checklist}
      />

      {completionChecklist && (
        <div className="text-xs leading-5" style={{ color: "#86efac" }}>
          {tr(
            "설정이 실제로 저장되었습니다. 체크리스트를 검토한 뒤 직접 대시보드로 돌아가면 됩니다.",
            "Setup has been applied. Review the checklist, then return to the dashboard when you are ready.",
          )}
        </div>
      )}

      <div className={actionRow}>
        {completionChecklist ? (
          <button type="button"
            onClick={() => {
              clearOnboardingDraft();
              onComplete();
            }}
            className={btnPrimary}
          >
            {tr("대시보드로 돌아가기", "Return to dashboard")}
          </button>
        ) : (
          <>
            <button type="button" onClick={() => goToStep(4)} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
              {tr("이전", "Back")}
            </button>
            <button type="button" onClick={() => void handleComplete()} disabled={completing || !completionReady} className={btnPrimary}>
              {completing ? tr("설정 중...", "Setting up...") : tr("설정 완료", "Complete Setup")}
            </button>
          </>
        )}
      </div>
    </div>
  );
}
