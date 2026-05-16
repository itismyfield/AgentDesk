import type {
  AgentDef,
  ChannelAssignment,
  CommandBotEntry,
  OnboardingResumeState,
  ProviderStatus,
} from "../onboardingDraft";
import { providerCliName, providerInstallHint, providerLoginCommand } from "./providerConfig";
import { TEMPLATES } from "./templates";
import type {
  ChecklistItem,
  CompletionChecklistItem,
  StepStatusItem,
} from "./OnboardingWizardSections";

type Tr = (ko: string, en: string) => string;

interface BuildOnboardingWizardDerivedArgs {
  agents: AgentDef[];
  agentsReady: boolean;
  announceReady: boolean;
  channelAssignments: ChannelAssignment[];
  channelAssignmentsReady: boolean;
  commandBots: CommandBotEntry[];
  commandBotsReady: boolean;
  completionChecklist: CompletionChecklistItem[] | null;
  completionReady: boolean;
  customAgentsReady: boolean;
  hasExistingSetup: boolean;
  hasSelectedGuild: boolean;
  isKo: boolean;
  newChannelCount: number;
  notifyReady: boolean;
  notifyToken: string;
  ownerId: string;
  ownerIdValid: boolean;
  overwriteAcknowledged: boolean;
  providerStatuses: Record<string, ProviderStatus>;
  resumeState: OnboardingResumeState;
  selectedTemplate: string | null;
  step: number;
  tr: Tr;
  uniqueProviders: Array<CommandBotEntry["provider"]>;
  validatedCommandCount: number;
}

export interface OnboardingWizardDerived {
  applySummary: Array<{ key: string; label: string; detail: string }>;
  draftNoticeDetail: string;
  draftNoticeTitle: string;
  step1Checklist: ChecklistItem[];
  step2Checklist: ChecklistItem[];
  step3Checklist: ChecklistItem[];
  step4Checklist: ChecklistItem[];
  step5Checklist: ChecklistItem[];
  stepStatusItems: StepStatusItem[];
}

export function buildOnboardingWizardDerived({
  agents,
  agentsReady,
  announceReady,
  channelAssignments,
  channelAssignmentsReady,
  commandBots,
  commandBotsReady,
  completionChecklist,
  completionReady,
  customAgentsReady,
  hasExistingSetup,
  hasSelectedGuild,
  isKo,
  newChannelCount,
  notifyReady,
  notifyToken,
  ownerId,
  ownerIdValid,
  overwriteAcknowledged,
  providerStatuses,
  resumeState,
  selectedTemplate,
  step,
  tr,
  uniqueProviders,
  validatedCommandCount,
}: BuildOnboardingWizardDerivedArgs): OnboardingWizardDerived {
  const selectedTemplateInfo =
    TEMPLATES.find((template) => template.key === selectedTemplate) ?? null;
  const customAgents = agents.filter((agent) => agent.custom);
  const step1Checklist: ChecklistItem[] = [
    {
      key: "command-bots",
      label: tr("실행 봇 검증", "Command bots validated"),
      ok: commandBotsReady,
      detail: tr(
        `${validatedCommandCount}/${commandBots.length}개 실행 봇 토큰이 검증되었습니다.`,
        `${validatedCommandCount}/${commandBots.length} command bot tokens are validated.`,
      ),
    },
    {
      key: "announce-bot",
      label: tr("통신 봇 검증", "Communication bot validated"),
      ok: announceReady,
      detail: announceReady
        ? tr(
            "채널 생성과 권한 설정에 사용할 통신 봇이 준비되었습니다.",
            "Communication bot is ready for channel creation and permissions.",
          )
        : tr(
            "통신 봇이 없으면 실제 Discord 채널 생성이 진행되지 않습니다.",
            "Without the communication bot, real Discord channel setup cannot run.",
          ),
    },
    {
      key: "notify-bot",
      label: tr("알림 봇 상태", "Notification bot status"),
      ok: notifyReady,
      detail: notifyToken.trim()
        ? tr("알림 봇 토큰이 검증되었습니다.", "Notification bot token is validated.")
        : tr(
            "선택 사항입니다. 비워두면 알림 봇 없이 진행합니다.",
            "Optional. Leave blank to continue without a notification bot.",
          ),
    },
  ];

  const step2Checklist: ChecklistItem[] = uniqueProviders.map((provider) => {
    const status = providerStatuses[provider];
    const installed = Boolean(status?.installed);
    const loggedIn = Boolean(status?.installed && status?.logged_in);
    return {
      key: provider,
      label: tr(`${providerCliName(provider)} 준비`, `${providerCliName(provider)} ready`),
      ok: installed && loggedIn,
      detail: !status
        ? tr("아직 확인 전입니다. 다시 확인을 눌러 상태를 읽어오세요.", "Not checked yet. Re-run the provider check.")
        : installed && loggedIn
          ? tr("CLI 설치와 로그인 상태가 모두 확인되었습니다.", "CLI installation and login are both confirmed.")
          : !installed
            ? providerInstallHint(provider, isKo)
            : `${tr("로그인 필요:", "Login required:")} ${providerLoginCommand(provider)}`,
    };
  });

  const step3Checklist: ChecklistItem[] = [
    {
      key: "preset",
      label: tr("역할 프리셋 또는 커스텀 팀 구성", "Role preset or custom team selected"),
      ok: agents.length > 0,
      detail: selectedTemplateInfo
        ? tr(
            `${selectedTemplateInfo.name} 프리셋을 기준으로 ${agents.length}개 에이전트를 구성했습니다.`,
            `${selectedTemplateInfo.nameEn} preset selected with ${agents.length} agents.`,
          )
        : tr(
            `${agents.length}개 커스텀 에이전트를 직접 구성했습니다.`,
            `${agents.length} custom agents configured manually.`,
          ),
    },
    {
      key: "prompts",
      label: tr("모든 에이전트 프롬프트 준비", "All agent prompts prepared"),
      ok: agentsReady,
      detail: agentsReady
        ? tr("각 에이전트에 시스템 프롬프트가 채워져 있습니다.", "Every agent has a system prompt.")
        : tr("비어 있는 시스템 프롬프트가 있으면 완료할 수 없습니다.", "Blank system prompts block completion."),
    },
    {
      key: "custom-guidance",
      label: tr("커스텀 에이전트 설명 준비", "Custom agent descriptions ready"),
      ok: customAgentsReady,
      detail:
        customAgents.length === 0
          ? tr("현재는 프리셋 에이전트만 사용 중입니다.", "Only preset agents are in use right now.")
          : customAgentsReady
            ? tr("설명 기반으로 AI 프롬프트 초안을 생성할 준비가 되었습니다.", "Descriptions are ready for AI prompt generation.")
            : tr("커스텀 에이전트의 이름과 설명을 채워야 AI 초안이 더 정확해집니다.", "Fill in custom agent names and descriptions for better AI prompt drafts."),
    },
  ];

  const step4Checklist: ChecklistItem[] = [
    {
      key: "guild",
      label: tr("Discord 서버 선택", "Discord server selected"),
      ok: hasSelectedGuild,
      detail: hasSelectedGuild
        ? tr("이 서버에 채널 생성/재사용을 적용합니다.", "Channel creation and reuse will target this server.")
        : tr("실제 채널 생성을 위해 Discord 서버 선택이 필수입니다.", "Selecting a Discord server is required for real channel setup."),
    },
    {
      key: "assignments",
      label: tr("에이전트별 채널 매핑", "Agent-to-channel mapping ready"),
      ok: channelAssignmentsReady,
      detail: channelAssignmentsReady
        ? tr(
            `${channelAssignments.length}개 에이전트 채널 매핑이 준비되었습니다.`,
            `${channelAssignments.length} agent channel mappings are ready.`,
          )
        : tr("모든 에이전트에 채널 이름 또는 기존 채널을 지정해야 합니다.", "Each agent needs a channel name or existing channel."),
    },
    {
      key: "new-channels",
      label: tr("새 채널 생성 준비", "New channel creation ready"),
      ok: newChannelCount === 0 || announceReady,
      detail:
        newChannelCount === 0
          ? tr("모든 에이전트가 기존 채널에 연결됩니다.", "All agents are mapped to existing channels.")
          : tr(
              `${newChannelCount}개 채널은 완료 시 자동 생성됩니다.`,
              `${newChannelCount} channels will be created automatically on completion.`,
            ),
    },
  ];

  const step5Checklist: ChecklistItem[] = [
    {
      key: "owner-id",
      label: tr("소유자 ID 형식", "Owner ID format"),
      ok: ownerIdValid,
      detail: ownerId.trim()
        ? tr("17~20자리 Discord 사용자 ID 형식인지 확인했습니다.", "Checked that the value matches a Discord user ID format.")
        : tr("비워두면 첫 메시지 발신자가 자동 소유자가 됩니다.", "Leave blank to make the first message sender the owner."),
    },
    {
      key: "apply-ready",
      label: tr("실제 세팅 적용 준비", "Ready to apply real setup"),
      ok: completionReady,
      detail: completionReady
        ? tr(
            "완료 시 Discord 채널, 설정 파일, 파이프라인 검증까지 서버에서 진행합니다.",
            "Completion will apply Discord channels, settings, and pipeline verification on the server.",
          )
        : tr("이전 단계의 실패 항목이 남아 있어 아직 완료를 실행할 수 없습니다.", "A previous step is still failing, so completion is blocked."),
    },
    ...(hasExistingSetup
      ? [
          {
            key: "rerun-overwrite",
            label: tr("재실행 덮어쓰기 확인", "Rerun overwrite acknowledgement"),
            ok: overwriteAcknowledged,
            detail: overwriteAcknowledged
              ? tr(
                  "기존 role_id 기반 에이전트와 채널 매핑을 다시 적용할 수 있다는 점을 확인했습니다.",
                  "You acknowledged that existing role-based agents and channel mappings may be applied again.",
                )
              : tr(
                  "현재 API는 기존 에이전트 구성을 프리필하지 않습니다. 같은 role_id를 다시 적용할 수 있다는 점을 확인해야 완료할 수 있습니다.",
                  "The current API does not prefill the existing agent layout. Completion requires acknowledging that the same role IDs may be applied again.",
                ),
          },
        ]
      : []),
  ];

  const stepStatusFor = (stepNumber: number, ok: boolean): StepStatusItem["status"] => {
    if (step === stepNumber) return "active";
    if (step > stepNumber) return ok ? "complete" : "blocked";
    return "pending";
  };
  const stepStatusItems: StepStatusItem[] = [
    { step: 1, label: tr("봇", "Bots"), status: stepStatusFor(1, step1Checklist.every((item) => item.ok)) },
    { step: 2, label: tr("프로바이더", "Providers"), status: stepStatusFor(2, step2Checklist.every((item) => item.ok)) },
    { step: 3, label: tr("에이전트", "Agents"), status: stepStatusFor(3, step3Checklist.every((item) => item.ok)) },
    { step: 4, label: tr("채널", "Channels"), status: stepStatusFor(4, step4Checklist.every((item) => item.ok)) },
    {
      step: 5,
      label: tr("적용", "Apply"),
      status: stepStatusFor(5, (completionChecklist ?? step5Checklist).every((item) => item.ok)),
    },
  ];

  const applySummary = [
    {
      key: "channels",
      label: tr("Discord 채널", "Discord channels"),
      detail: hasSelectedGuild
        ? tr(
            `${channelAssignments.length}개 에이전트 채널 매핑을 적용하고, 새 채널 ${newChannelCount}개는 완료 시 실제 생성합니다. 네트워크 오류 뒤 재시도 전에는 Discord에 일부 채널이 먼저 생겼는지 확인하는 편이 안전합니다.`,
            `Applies ${channelAssignments.length} agent channel mappings and creates ${newChannelCount} new channels on completion. After a network error, check whether some channels were already created in Discord before retrying.`,
          )
        : tr(
            "서버를 선택해야 실제 채널 생성과 기존 채널 연결을 확정할 수 있습니다.",
            "Select a server before real channel creation and existing-channel reuse can be finalized.",
          ),
    },
    {
      key: "settings",
      label: tr("설정 저장", "Settings write"),
      detail: tr(
        "owner ID, command/communication bot, provider 조합을 서버에 실제로 기록합니다.",
        "Writes the owner ID, command/communication bot, and provider wiring to the server.",
      ),
    },
    {
      key: "pipeline",
      label: tr("기본 운영 파이프라인", "Default operating pipeline"),
      detail: tr(
        "기본 채널/카테고리와 함께 초기 파이프라인/설정 재생성을 같은 완료 작업에서 처리합니다.",
        "Rebuilds the initial pipeline and baseline settings alongside the default channels and categories.",
      ),
    },
    {
      key: "verification",
      label: tr("완료 후 검증", "Post-apply verification"),
      detail: tr(
        "성공 응답은 설정 산출물과 기본 파이프라인 파일 검증이 끝난 뒤에만 내려옵니다. 현재 체크리스트는 read-back 비교가 아니라 생성/검증 결과 요약입니다.",
        "A success response arrives only after settings artifacts and the default pipeline file are verified. The current checklist is a summary of creation/verification, not a read-back diff.",
      ),
    },
  ];

  const draftNoticeTitle =
    resumeState === "partial_apply"
      ? tr("중간에 멈춘 온보딩 상태를 복원했습니다.", "Restored the onboarding state from a partial apply.")
      : tr("저장된 온보딩 진행 상태를 복원했습니다.", "Restored your saved onboarding progress.");
  const draftNoticeDetail =
    resumeState === "partial_apply"
      ? tr(
          "이전 적용이 중간에 멈췄습니다. 같은 설정으로 다시 완료를 실행하면 이미 만든 Discord 채널과 draft를 기준으로 이어서 진행합니다.",
          "A previous apply stopped mid-flight. Re-running completion with the same setup will continue from the saved draft and any Discord channels that were already created.",
        )
      : tr(
          "브라우저를 바꾸거나 새로고침해도 서버에 저장된 draft를 기준으로 이어서 진행할 수 있습니다. 처음부터 다시 하려면 임시 저장을 비우세요.",
          "You can resume from the server-side draft even after switching browsers or refreshing. Clear the draft if you want to start over.",
        );

  return {
    applySummary,
    draftNoticeDetail,
    draftNoticeTitle,
    step1Checklist,
    step2Checklist,
    step3Checklist,
    step4Checklist,
    step5Checklist,
    stepStatusItems,
  };
}
