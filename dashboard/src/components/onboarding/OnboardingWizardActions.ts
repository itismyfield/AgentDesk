import { useCallback, type Dispatch, type SetStateAction } from "react";
import {
  clearOnboardingDraft,
  type AgentDef,
  type BotInfo,
  type ChannelAssignment,
  type CommandBotEntry,
  type OnboardingResumeState,
  type ProviderStatus,
} from "../onboardingDraft";
import { TEMPLATES } from "./templates";
import type { CompletionChecklistItem } from "./OnboardingWizardSections";

interface Guild {
  id: string;
  name: string;
  channels: Array<{ id: string; name: string; category_id?: string }>;
}

type Tr = (ko: string, en: string) => string;
type Setter<T> = Dispatch<SetStateAction<T>>;

export const ONBOARDING_INVITE_PERMISSIONS = {
  command: (2048 + 65536 + 8192 + 17179869184 + 274877906944).toString(),
  announce: "8",
  notify: "2048",
};

export const makeOnboardingInviteUrl = (botId: string, permissions: string) =>
  `https://discord.com/oauth2/authorize?client_id=${botId}&scope=bot&permissions=${permissions}`;

interface UseOnboardingWizardActionsArgs {
  agents: AgentDef[];
  announceToken: string;
  channelAssignments: ChannelAssignment[];
  commandBots: CommandBotEntry[];
  completionReady: boolean;
  confirmRerunOverwrite: boolean;
  customDesc: string;
  customDescEn: string;
  customName: string;
  customNameEn: string;
  hasExistingSetup: boolean;
  notifyToken: string;
  onComplete: () => void;
  ownerId: string;
  primaryProvider: string;
  selectedGuild: string;
  selectedTemplate: string | null;
  setAgents: Setter<AgentDef[]>;
  setAnnounceBotInfo: Setter<BotInfo | null>;
  setChannelAssignments: Setter<ChannelAssignment[]>;
  setCheckingProviders: Setter<boolean>;
  setCommandBots: Setter<CommandBotEntry[]>;
  setCompleting: Setter<boolean>;
  setCompletionChecklist: Setter<CompletionChecklistItem[] | null>;
  setCustomDesc: Setter<string>;
  setCustomDescEn: Setter<string>;
  setCustomName: Setter<string>;
  setCustomNameEn: Setter<string>;
  setDraftNoticeVisible: Setter<boolean>;
  setError: Setter<string>;
  setExpandedAgent: Setter<string | null>;
  setGeneratingPrompt: Setter<boolean>;
  setGuilds: Setter<Guild[]>;
  setNotifyBotInfo: Setter<BotInfo | null>;
  setProviderStatuses: Setter<Record<string, ProviderStatus>>;
  setResumeState: Setter<OnboardingResumeState>;
  setSelectedGuild: Setter<string>;
  setSelectedTemplate: Setter<string | null>;
  setValidating: Setter<boolean>;
  tr: Tr;
}

export function useOnboardingWizardActions({
  agents,
  announceToken,
  channelAssignments,
  commandBots,
  completionReady,
  confirmRerunOverwrite,
  customDesc,
  customDescEn,
  customName,
  customNameEn,
  hasExistingSetup,
  notifyToken,
  onComplete,
  ownerId,
  primaryProvider,
  selectedGuild,
  selectedTemplate,
  setAgents,
  setAnnounceBotInfo,
  setChannelAssignments,
  setCheckingProviders,
  setCommandBots,
  setCompleting,
  setCompletionChecklist,
  setCustomDesc,
  setCustomDescEn,
  setCustomName,
  setCustomNameEn,
  setDraftNoticeVisible,
  setError,
  setExpandedAgent,
  setGeneratingPrompt,
  setGuilds,
  setNotifyBotInfo,
  setProviderStatuses,
  setResumeState,
  setSelectedGuild,
  setSelectedTemplate,
  setValidating,
  tr,
}: UseOnboardingWizardActionsArgs) {
  const validateBotToken = useCallback(async (token: string): Promise<BotInfo> => {
    const response = await fetch("/api/onboarding/validate-token", {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ token }),
    });
    return response.json();
  }, []);

  const validateStep1 = useCallback(async () => {
    setValidating(true);
    setError("");
    try {
      for (let i = 0; i < commandBots.length; i += 1) {
        if (!commandBots[i].token) {
          setError(
            tr(
              `실행 봇 ${i + 1}의 토큰을 입력하세요.`,
              `Enter token for Command Bot ${i + 1}.`,
            ),
          );
          return;
        }
        const info = await validateBotToken(commandBots[i].token);
        setCommandBots((prev) => {
          const copy = [...prev];
          copy[i] = { ...copy[i], botInfo: info };
          return copy;
        });
        if (!info.valid) {
          setError(
            tr(
              `실행 봇 ${i + 1} 토큰이 유효하지 않습니다.`,
              `Command Bot ${i + 1} token is invalid.`,
            ),
          );
          return;
        }
      }

      if (!announceToken) {
        setError(tr("통신 봇 토큰을 입력하세요.", "Enter communication bot token."));
        return;
      }
      const announceInfo = await validateBotToken(announceToken);
      setAnnounceBotInfo(announceInfo);
      if (!announceInfo.valid) {
        setError(tr("통신 봇 토큰이 유효하지 않습니다.", "Communication bot token is invalid."));
        return;
      }

      if (notifyToken) {
        const notifyInfo = await validateBotToken(notifyToken);
        setNotifyBotInfo(notifyInfo);
        if (!notifyInfo.valid) {
          setError(tr("알림 봇 토큰이 유효하지 않습니다.", "Notification bot token is invalid."));
        }
      }
    } catch {
      setError(tr("검증 실패", "Validation failed"));
    } finally {
      setValidating(false);
    }
  }, [
    announceToken,
    commandBots,
    notifyToken,
    setAnnounceBotInfo,
    setCommandBots,
    setError,
    setNotifyBotInfo,
    setValidating,
    tr,
    validateBotToken,
  ]);

  const checkProviders = useCallback(async () => {
    setCheckingProviders(true);
    const providers = [...new Set(commandBots.map((bot) => bot.provider))];
    const statuses: Record<string, ProviderStatus> = {};
    for (const provider of providers) {
      try {
        const response = await fetch("/api/onboarding/check-provider", {
          method: "POST",
          credentials: "include",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ provider }),
        });
        statuses[provider] = await response.json();
      } catch {
        statuses[provider] = { installed: false, logged_in: false };
      }
    }
    setProviderStatuses(statuses);
    setCheckingProviders(false);
  }, [commandBots, setCheckingProviders, setProviderStatuses]);

  const fetchChannels = useCallback(async () => {
    const token = announceToken || commandBots[0]?.token;
    if (!token) return;
    try {
      const response = await fetch("/api/onboarding/channels", {
        method: "POST",
        credentials: "include",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ token }),
      });
      const data = (await response.json()) as { guilds?: Guild[] };
      setGuilds(data.guilds || []);
      if (data.guilds?.length === 1) setSelectedGuild(data.guilds[0].id);
    } catch {
      setError(tr("채널 조회 실패", "Failed to fetch channels"));
    }
  }, [announceToken, commandBots, setError, setGuilds, setSelectedGuild, tr]);

  const selectTemplate = useCallback(
    (key: string) => {
      const template = TEMPLATES.find((item) => item.key === key);
      if (!template) return;
      setSelectedTemplate(key);
      setAgents(template.agents.map((agent) => ({ ...agent, custom: false })));
    },
    [setAgents, setSelectedTemplate],
  );

  const addCustomAgent = useCallback(() => {
    if (!customName.trim()) return;
    const name = customName.trim();
    const desc = customDesc.trim();
    const nameEn = customNameEn.trim() || name;
    const descEn = customDescEn.trim() || desc || nameEn;
    const id =
      name
        .toLowerCase()
        .replace(/[^a-z0-9가-힣]/g, "-")
        .replace(/-+/g, "-")
        .replace(/^-|-$/g, "") || `agent-${agents.length + 1}`;
    const prompt = `당신은 ${name}입니다. ${
      desc || `${name}의 역할을 수행합니다`
    }.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 간결하고 명확하게 답변합니다\n- 필요시 확인 질문을 합니다`;
    setAgents((prev) => [
      ...prev,
      { id, name, nameEn, description: desc, descriptionEn: descEn, prompt, custom: true },
    ]);
    setExpandedAgent(id);
    setCustomName("");
    setCustomDesc("");
    setCustomNameEn("");
    setCustomDescEn("");
  }, [
    agents.length,
    customDesc,
    customDescEn,
    customName,
    customNameEn,
    setAgents,
    setCustomDesc,
    setCustomDescEn,
    setCustomName,
    setCustomNameEn,
    setExpandedAgent,
  ]);

  const removeAgent = useCallback(
    (id: string) => {
      setAgents((prev) => prev.filter((agent) => agent.id !== id));
    },
    [setAgents],
  );

  const generateAiPrompt = useCallback(
    async (agentId: string) => {
      const agent = agents.find((item) => item.id === agentId);
      if (!agent) return;
      setGeneratingPrompt(true);
      try {
        const response = await fetch("/api/onboarding/generate-prompt", {
          method: "POST",
          credentials: "include",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            name: agent.name,
            description: agent.description,
            provider: primaryProvider,
          }),
        });
        const data = (await response.json()) as { prompt?: string };
        if (data.prompt) {
          setAgents((prev) =>
            prev.map((item) => (item.id === agentId ? { ...item, prompt: data.prompt ?? item.prompt } : item)),
          );
        }
      } catch {
        setError(tr("프롬프트 생성 실패", "Failed to generate prompt"));
      }
      setGeneratingPrompt(false);
    },
    [agents, primaryProvider, setAgents, setError, setGeneratingPrompt, tr],
  );

  const handleComplete = useCallback(async () => {
    if (!completionReady) {
      setError(
        tr(
          "완료 전 체크리스트의 실패 항목을 먼저 해결하세요.",
          "Resolve the failed checklist items before completing setup.",
        ),
      );
      return;
    }

    setCompleting(true);
    setCompletionChecklist(null);
    setError("");
    try {
      const response = await fetch("/api/onboarding/complete", {
        method: "POST",
        credentials: "include",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          token: commandBots[0]?.token || "",
          announce_token: announceToken || null,
          notify_token: notifyToken || null,
          command_token_2: commandBots.length > 1 ? commandBots[1].token : null,
          command_provider_2: commandBots.length > 1 ? commandBots[1].provider : null,
          guild_id: selectedGuild,
          owner_id: ownerId || null,
          provider: primaryProvider,
          template: selectedTemplate || null,
          rerun_policy: hasExistingSetup && confirmRerunOverwrite ? "replace_existing" : "reuse_existing",
          channels: channelAssignments.map((assignment) => ({
            channel_id: assignment.channelId || assignment.channelName,
            channel_name: assignment.channelName,
            role_id: assignment.agentId,
            description: agents.find((agent) => agent.id === assignment.agentId)?.description || null,
            system_prompt: agents.find((agent) => agent.id === assignment.agentId)?.prompt || null,
          })),
        }),
      });
      const data = await response.json();
      if (data.ok) {
        setResumeState("none");
        clearOnboardingDraft();
        if (Array.isArray(data.checklist)) {
          setDraftNoticeVisible(false);
          setCompletionChecklist(data.checklist);
        } else {
          onComplete();
        }
      } else {
        const retryHint =
          data.partial_apply || data.completion_state?.partial_apply
            ? tr(
                "일부 적용이 남았습니다. 같은 payload로 다시 실행하면 기존 Discord 채널을 재사용합니다.",
                "Setup was partially applied. Retrying with the same payload will reuse the existing Discord channels.",
              )
            : "";
        setError([data.error || tr("설정 저장 실패", "Failed to save"), retryHint].filter(Boolean).join(" "));
      }
    } catch {
      setError(tr("완료 실패", "Failed to complete"));
    }
    setCompleting(false);
  }, [
    agents,
    announceToken,
    channelAssignments,
    commandBots,
    completionReady,
    confirmRerunOverwrite,
    hasExistingSetup,
    notifyToken,
    onComplete,
    ownerId,
    primaryProvider,
    selectedGuild,
    selectedTemplate,
    setCompleting,
    setCompletionChecklist,
    setDraftNoticeVisible,
    setError,
    setResumeState,
    tr,
  ]);

  return {
    addCustomAgent,
    checkProviders,
    fetchChannels,
    generateAiPrompt,
    handleComplete,
    invitePermissions: ONBOARDING_INVITE_PERMISSIONS,
    makeInviteUrl: makeOnboardingInviteUrl,
    removeAgent,
    selectTemplate,
    validateStep1,
  };
}
