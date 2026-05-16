import {
  useCallback,
  useEffect,
  type Dispatch,
  type MutableRefObject,
  type SetStateAction,
} from "react";
import {
  clearOnboardingDraft,
  isMeaningfulOnboardingDraft,
  pickPreferredOnboardingDraft,
  serverDraftToLocalDraft,
  writeOnboardingDraft,
  type AgentDef,
  type BotInfo,
  type ChannelAssignment,
  type CommandBotEntry,
  type OnboardingDraft,
  type OnboardingResumeState,
  type OnboardingStatusResponse,
  type ProviderStatus,
  type ServerOnboardingDraftResponse,
} from "../onboardingDraft";
import { COMMAND_PROVIDERS } from "./providerConfig";
import type { CompletionChecklistItem } from "./OnboardingWizardSections";

type Setter<T> = Dispatch<SetStateAction<T>>;
type Tr = (ko: string, en: string) => string;

interface Guild {
  id: string;
  name: string;
  channels: Array<{ id: string; name: string; category_id?: string }>;
}

interface UseOnboardingDraftLifecycleArgs {
  agents: AgentDef[];
  announceBotInfo: BotInfo | null;
  announceToken: string;
  channelAssignments: ChannelAssignment[];
  commandBots: CommandBotEntry[];
  completionChecklist: CompletionChecklistItem[] | null;
  confirmRerunOverwrite: boolean;
  customDesc: string;
  customDescEn: string;
  customName: string;
  customNameEn: string;
  draftSyncReady: boolean;
  expandedAgent: string | null;
  hasExistingSetup: boolean;
  initialDraftRef: MutableRefObject<OnboardingDraft | null>;
  isKo: boolean;
  notifyBotInfo: BotInfo | null;
  notifyToken: string;
  ownerId: string;
  providerStatuses: Record<string, ProviderStatus>;
  selectedGuild: string;
  selectedTemplate: string | null;
  setAgents: Setter<AgentDef[]>;
  setAnnounceBotInfo: Setter<BotInfo | null>;
  setAnnounceToken: Setter<string>;
  setChannelAssignments: Setter<ChannelAssignment[]>;
  setCommandBots: Setter<CommandBotEntry[]>;
  setCompletionChecklist: Setter<CompletionChecklistItem[] | null>;
  setConfirmRerunOverwrite: Setter<boolean>;
  setCustomDesc: Setter<string>;
  setCustomDescEn: Setter<string>;
  setCustomName: Setter<string>;
  setCustomNameEn: Setter<string>;
  setDraftNoticeVisible: Setter<boolean>;
  setDraftSyncReady: Setter<boolean>;
  setError: Setter<string>;
  setExpandedAgent: Setter<string | null>;
  setGuilds: Setter<Guild[]>;
  setHasExistingSetup: Setter<boolean>;
  setNotifyBotInfo: Setter<BotInfo | null>;
  setNotifyToken: Setter<string>;
  setOwnerId: Setter<string>;
  setProviderStatuses: Setter<Record<string, ProviderStatus>>;
  setResumeState: Setter<OnboardingResumeState>;
  setSelectedGuild: Setter<string>;
  setSelectedTemplate: Setter<string | null>;
  setStep: Setter<number>;
  step: number;
  suppressNextServerDraftSyncRef: MutableRefObject<boolean>;
  tr: Tr;
}

export function useOnboardingDraftLifecycle({
  agents,
  announceBotInfo,
  announceToken,
  channelAssignments,
  commandBots,
  completionChecklist,
  confirmRerunOverwrite,
  customDesc,
  customDescEn,
  customName,
  customNameEn,
  draftSyncReady,
  expandedAgent,
  hasExistingSetup,
  initialDraftRef,
  isKo,
  notifyBotInfo,
  notifyToken,
  ownerId,
  providerStatuses,
  selectedGuild,
  selectedTemplate,
  setAgents,
  setAnnounceBotInfo,
  setAnnounceToken,
  setChannelAssignments,
  setCommandBots,
  setCompletionChecklist,
  setConfirmRerunOverwrite,
  setCustomDesc,
  setCustomDescEn,
  setCustomName,
  setCustomNameEn,
  setDraftNoticeVisible,
  setDraftSyncReady,
  setError,
  setExpandedAgent,
  setGuilds,
  setHasExistingSetup,
  setNotifyBotInfo,
  setNotifyToken,
  setOwnerId,
  setProviderStatuses,
  setResumeState,
  setSelectedGuild,
  setSelectedTemplate,
  setStep,
  step,
  suppressNextServerDraftSyncRef,
  tr,
}: UseOnboardingDraftLifecycleArgs) {
  const applyDraft = useCallback(
    (draft: OnboardingDraft) => {
      initialDraftRef.current = draft;
      setStep(draft.step);
      setCommandBots(
        draft.commandBots.length
          ? draft.commandBots
          : [{ provider: "claude", token: "", botInfo: null }],
      );
      setAnnounceToken(draft.announceToken);
      setNotifyToken(draft.notifyToken);
      setAnnounceBotInfo(draft.announceBotInfo);
      setNotifyBotInfo(draft.notifyBotInfo);
      setProviderStatuses(draft.providerStatuses);
      setSelectedTemplate(draft.selectedTemplate);
      setAgents(draft.agents);
      setCustomName(draft.customName);
      setCustomDesc(draft.customDesc);
      setCustomNameEn(draft.customNameEn);
      setCustomDescEn(draft.customDescEn);
      setExpandedAgent(draft.expandedAgent);
      setSelectedGuild(draft.selectedGuild);
      setChannelAssignments(draft.channelAssignments);
      setOwnerId(draft.ownerId);
      setHasExistingSetup(draft.hasExistingSetup);
      setConfirmRerunOverwrite(draft.confirmRerunOverwrite);
      setCompletionChecklist(null);
      setError("");
      setDraftNoticeVisible(true);
    },
    [
      initialDraftRef,
      setAgents,
      setAnnounceBotInfo,
      setAnnounceToken,
      setChannelAssignments,
      setCommandBots,
      setCompletionChecklist,
      setConfirmRerunOverwrite,
      setCustomDesc,
      setCustomDescEn,
      setCustomName,
      setCustomNameEn,
      setDraftNoticeVisible,
      setError,
      setExpandedAgent,
      setHasExistingSetup,
      setNotifyBotInfo,
      setNotifyToken,
      setOwnerId,
      setProviderStatuses,
      setSelectedGuild,
      setSelectedTemplate,
      setStep,
    ],
  );

  const resetDraft = useCallback(() => {
    clearOnboardingDraft();
    initialDraftRef.current = null;
    setStep(1);
    setCommandBots([{ provider: "claude", token: "", botInfo: null }]);
    setAnnounceToken("");
    setNotifyToken("");
    setAnnounceBotInfo(null);
    setNotifyBotInfo(null);
    setProviderStatuses({});
    setSelectedTemplate(null);
    setAgents([]);
    setCustomName("");
    setCustomDesc("");
    setCustomNameEn("");
    setCustomDescEn("");
    setExpandedAgent(null);
    setGuilds([]);
    setSelectedGuild("");
    setChannelAssignments([]);
    setOwnerId("");
    setConfirmRerunOverwrite(false);
    setCompletionChecklist(null);
    setError("");
    setResumeState("none");
    setDraftNoticeVisible(false);
    void fetch("/api/onboarding/draft", {
      method: "DELETE",
      credentials: "include",
    }).catch(() => {});
  }, [
    initialDraftRef,
    setAgents,
    setAnnounceBotInfo,
    setAnnounceToken,
    setChannelAssignments,
    setCommandBots,
    setCompletionChecklist,
    setConfirmRerunOverwrite,
    setCustomDesc,
    setCustomDescEn,
    setCustomName,
    setCustomNameEn,
    setDraftNoticeVisible,
    setError,
    setExpandedAgent,
    setGuilds,
    setNotifyBotInfo,
    setNotifyToken,
    setOwnerId,
    setProviderStatuses,
    setResumeState,
    setSelectedGuild,
    setSelectedTemplate,
    setStep,
  ]);

  useEffect(() => {
    let cancelled = false;

    async function loadInitialState() {
      try {
        const [statusResponse, draftResponse] = await Promise.all([
          fetch("/api/onboarding/status", { credentials: "include" }),
          fetch("/api/onboarding/draft", { credentials: "include" }),
        ]);
        const statusData = (await statusResponse.json()) as OnboardingStatusResponse;
        const draftData = draftResponse.ok
          ? ((await draftResponse.json()) as ServerOnboardingDraftResponse)
          : null;
        if (cancelled) return;

        const serverDraft = serverDraftToLocalDraft(draftData?.draft);
        const serverHasExistingSetup = statusData.setup_mode
          ? statusData.setup_mode === "rerun"
          : Boolean(
              statusData.owner_id ||
                statusData.guild_id ||
                statusData.bot_tokens?.command ||
                statusData.bot_tokens?.announce ||
                statusData.bot_tokens?.notify ||
                statusData.bot_tokens?.command2,
            );
        const preferredDraft = pickPreferredOnboardingDraft(
          initialDraftRef.current,
          serverDraft,
        );
        const nextResumeState =
          draftData?.resume_state ?? statusData.resume_state ?? "none";

        setHasExistingSetup(serverHasExistingSetup);
        setResumeState(nextResumeState);

        if (preferredDraft) {
          suppressNextServerDraftSyncRef.current = preferredDraft === serverDraft;
          applyDraft({
            ...preferredDraft,
            hasExistingSetup: serverHasExistingSetup || preferredDraft.hasExistingSetup,
          });
          setHasExistingSetup(serverHasExistingSetup || preferredDraft.hasExistingSetup);
          return;
        }

        suppressNextServerDraftSyncRef.current = true;
        if (statusData.owner_id) setOwnerId(statusData.owner_id);
        if (statusData.guild_id) setSelectedGuild(statusData.guild_id);
        const commandToken = statusData.bot_tokens?.command;
        const command2Token = statusData.bot_tokens?.command2;
        if (!serverHasExistingSetup && commandToken) {
          setCommandBots((prev) => {
            const copy = [...prev];
            copy[0] = {
              ...copy[0],
              provider: statusData.bot_providers?.command ?? copy[0].provider,
              token: commandToken,
            };
            return copy;
          });
        }
        if (!serverHasExistingSetup && command2Token) {
          setCommandBots((prev) => [
            ...prev,
            {
              provider:
                statusData.bot_providers?.command2 ??
                COMMAND_PROVIDERS.find((provider) => provider !== prev[0].provider) ??
                "codex",
              token: command2Token,
              botInfo: null,
            },
          ]);
        }
        if (!serverHasExistingSetup && statusData.bot_tokens?.announce) {
          setAnnounceToken(statusData.bot_tokens.announce);
        }
        if (!serverHasExistingSetup && statusData.bot_tokens?.notify) {
          setNotifyToken(statusData.bot_tokens.notify);
        }
        if (nextResumeState === "partial_apply") {
          setError(
            tr(
              "이전 온보딩 적용이 중간에 멈췄습니다. 같은 설정으로 다시 완료를 실행하면 기존 채널을 재사용합니다.",
              "A previous onboarding apply stopped mid-flight. Re-running completion with the same setup will reuse the existing channels.",
            ),
          );
        }
      } catch {
        // Ignore initial hydration failures and keep the local state fallback.
      } finally {
        if (!cancelled) setDraftSyncReady(true);
      }
    }

    void loadInitialState();
    return () => {
      cancelled = true;
    };
  }, [applyDraft, initialDraftRef, isKo, setDraftSyncReady, suppressNextServerDraftSyncRef]);

  useEffect(() => {
    if (!draftSyncReady || completionChecklist) return;
    const nextDraft: OnboardingDraft = {
      version: 1,
      updatedAtMs: Date.now(),
      step,
      commandBots,
      announceToken,
      notifyToken,
      announceBotInfo,
      notifyBotInfo,
      providerStatuses,
      selectedTemplate,
      agents,
      customName,
      customDesc,
      customNameEn,
      customDescEn,
      expandedAgent,
      selectedGuild,
      channelAssignments,
      ownerId,
      hasExistingSetup,
      confirmRerunOverwrite,
    };
    const controller = new AbortController();

    if (suppressNextServerDraftSyncRef.current) {
      suppressNextServerDraftSyncRef.current = false;
      if (isMeaningfulOnboardingDraft(nextDraft)) {
        initialDraftRef.current = nextDraft;
        writeOnboardingDraft(nextDraft);
      } else {
        initialDraftRef.current = null;
        clearOnboardingDraft();
      }
      return () => controller.abort();
    }

    if (!isMeaningfulOnboardingDraft(nextDraft)) {
      initialDraftRef.current = null;
      clearOnboardingDraft();
      void fetch("/api/onboarding/draft", {
        method: "DELETE",
        credentials: "include",
        signal: controller.signal,
      }).catch(() => {});
      return () => controller.abort();
    }

    initialDraftRef.current = nextDraft;
    writeOnboardingDraft(nextDraft);
    const timer = window.setTimeout(() => {
      void fetch("/api/onboarding/draft", {
        method: "PUT",
        credentials: "include",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          version: nextDraft.version,
          updated_at_ms: nextDraft.updatedAtMs,
          step: nextDraft.step,
          command_bots: nextDraft.commandBots.map((bot) => ({
            provider: bot.provider,
            token: bot.token,
            bot_info: bot.botInfo,
          })),
          announce_token: nextDraft.announceToken,
          notify_token: nextDraft.notifyToken,
          announce_bot_info: nextDraft.announceBotInfo,
          notify_bot_info: nextDraft.notifyBotInfo,
          provider_statuses: nextDraft.providerStatuses,
          selected_template: nextDraft.selectedTemplate,
          agents: nextDraft.agents.map((agent) => ({
            id: agent.id,
            name: agent.name,
            name_en: agent.nameEn ?? null,
            description: agent.description,
            description_en: agent.descriptionEn ?? null,
            prompt: agent.prompt,
            custom: Boolean(agent.custom),
          })),
          custom_name: nextDraft.customName,
          custom_desc: nextDraft.customDesc,
          custom_name_en: nextDraft.customNameEn,
          custom_desc_en: nextDraft.customDescEn,
          expanded_agent: nextDraft.expandedAgent,
          selected_guild: nextDraft.selectedGuild,
          channel_assignments: nextDraft.channelAssignments.map((assignment) => ({
            agent_id: assignment.agentId,
            agent_name: assignment.agentName,
            recommended_name: assignment.recommendedName,
            channel_id: assignment.channelId,
            channel_name: assignment.channelName,
          })),
          owner_id: nextDraft.ownerId,
          has_existing_setup: nextDraft.hasExistingSetup,
          confirm_rerun_overwrite: nextDraft.confirmRerunOverwrite,
        }),
        signal: controller.signal,
      }).catch(() => {});
    }, 300);

    return () => {
      controller.abort();
      window.clearTimeout(timer);
    };
  }, [
    agents,
    announceBotInfo,
    announceToken,
    channelAssignments,
    commandBots,
    completionChecklist,
    confirmRerunOverwrite,
    customDesc,
    customDescEn,
    customName,
    customNameEn,
    draftSyncReady,
    expandedAgent,
    hasExistingSetup,
    initialDraftRef,
    notifyBotInfo,
    notifyToken,
    ownerId,
    providerStatuses,
    selectedGuild,
    selectedTemplate,
    step,
    suppressNextServerDraftSyncRef,
  ]);

  return { resetDraft };
}
