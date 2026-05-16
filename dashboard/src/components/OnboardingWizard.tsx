import { useState, useEffect, useCallback, useRef } from "react";
import {
  clearOnboardingDraft,
  isMeaningfulOnboardingDraft,
  pickPreferredOnboardingDraft,
  readOnboardingDraft,
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
} from "./onboardingDraft";
import {
  COMMAND_PROVIDERS,
  providerCliName,
  providerInstallHint,
  providerLabel,
  providerLoginCommand,
  providerLoginHint,
  providerSuffix,
} from "./onboarding/providerConfig";
import { TEMPLATES } from "./onboarding/templates";
import {
  ChecklistPanel,
  StepStatusRail,
  Tip,
  type ChecklistItem,
  type CompletionChecklistItem,
  type StepStatusItem,
} from "./onboarding/OnboardingWizardSections";
import { Step1BotConnection } from "./onboarding/Step1BotConnection";
import { Step2ProviderVerification } from "./onboarding/Step2ProviderVerification";
import { Step4ChannelSetup } from "./onboarding/Step4ChannelSetup";
import { Step5OwnerConfirm } from "./onboarding/Step5OwnerConfirm";

// ── Types ─────────────────────────────────────────────

interface Guild {
  id: string;
  name: string;
  channels: Array<{ id: string; name: string; category_id?: string }>;
}

interface Props {
  isKo: boolean;
  onComplete: () => void;
}

// ── Main Component ────────────────────────────────────

export default function OnboardingWizard({ isKo, onComplete }: Props) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const initialDraftRef = useRef<OnboardingDraft | null>(null);
  if (initialDraftRef.current === null) {
    initialDraftRef.current = readOnboardingDraft();
  }
  const suppressNextServerDraftSyncRef = useRef(false);
  const initialDraft = initialDraftRef.current;
  const railItemRefs = useRef<Record<number, HTMLDivElement | null>>({});
  const stepHeadingRef = useRef<HTMLHeadingElement | null>(null);

  // Step control
  const [step, setStep] = useState(initialDraft?.step ?? 1);
  const TOTAL_STEPS = 5;

  // Step 1: Bot tokens
  const [commandBots, setCommandBots] = useState<CommandBotEntry[]>(
    initialDraft?.commandBots.length
      ? initialDraft.commandBots
      : [{ provider: "claude", token: "", botInfo: null }],
  );
  const [announceToken, setAnnounceToken] = useState(initialDraft?.announceToken ?? "");
  const [notifyToken, setNotifyToken] = useState(initialDraft?.notifyToken ?? "");
  const [announceBotInfo, setAnnounceBotInfo] = useState<BotInfo | null>(initialDraft?.announceBotInfo ?? null);
  const [notifyBotInfo, setNotifyBotInfo] = useState<BotInfo | null>(initialDraft?.notifyBotInfo ?? null);
  const [validating, setValidating] = useState(false);

  // Step 2: Provider verification
  const [providerStatuses, setProviderStatuses] = useState<Record<string, ProviderStatus>>(initialDraft?.providerStatuses ?? {});
  const [checkingProviders, setCheckingProviders] = useState(false);

  // Step 3: Agent selection
  const [selectedTemplate, setSelectedTemplate] = useState<string | null>(initialDraft?.selectedTemplate ?? null);
  const [agents, setAgents] = useState<AgentDef[]>(initialDraft?.agents ?? []);
  const [customName, setCustomName] = useState(initialDraft?.customName ?? "");
  const [customDesc, setCustomDesc] = useState(initialDraft?.customDesc ?? "");
  const [customNameEn, setCustomNameEn] = useState(initialDraft?.customNameEn ?? "");
  const [customDescEn, setCustomDescEn] = useState(initialDraft?.customDescEn ?? "");
  const [generatingPrompt, setGeneratingPrompt] = useState(false);
  const [expandedAgent, setExpandedAgent] = useState<string | null>(initialDraft?.expandedAgent ?? null);

  // Step 4: Channel setup
  const [guilds, setGuilds] = useState<Guild[]>([]);
  const [selectedGuild, setSelectedGuild] = useState(initialDraft?.selectedGuild ?? "");
  const [channelAssignments, setChannelAssignments] = useState<ChannelAssignment[]>(initialDraft?.channelAssignments ?? []);

  // Step 5: Owner
  const [ownerId, setOwnerId] = useState(initialDraft?.ownerId ?? "");
  const [hasExistingSetup, setHasExistingSetup] = useState(initialDraft?.hasExistingSetup ?? false);
  const [confirmRerunOverwrite, setConfirmRerunOverwrite] = useState(initialDraft?.confirmRerunOverwrite ?? false);
  const [draftNoticeVisible, setDraftNoticeVisible] = useState(Boolean(initialDraft));
  const [completing, setCompleting] = useState(false);
  const [completionChecklist, setCompletionChecklist] = useState<CompletionChecklistItem[] | null>(null);
  const [error, setError] = useState("");
  const [draftSyncReady, setDraftSyncReady] = useState(false);
  const [resumeState, setResumeState] = useState<OnboardingResumeState>("none");

  // Get primary provider from first command bot
  const primaryProvider = commandBots[0]?.provider ?? "claude";
  const uniqueProviders = [...new Set(commandBots.map((bot) => bot.provider))];
  const selectedTemplateInfo = TEMPLATES.find((template) => template.key === selectedTemplate) ?? null;
  const validatedCommandCount = commandBots.filter((bot) => bot.botInfo?.valid).length;
  const commandBotsReady =
    commandBots.length > 0 &&
    commandBots.every((bot) => Boolean(bot.token.trim()) && Boolean(bot.botInfo?.valid));
  const announceReady = Boolean(announceToken.trim()) && Boolean(announceBotInfo?.valid);
  const notifyReady = !notifyToken.trim() || Boolean(notifyBotInfo?.valid);
  const providersReady =
    uniqueProviders.length > 0 &&
    uniqueProviders.every(
      (provider) => providerStatuses[provider]?.installed && providerStatuses[provider]?.logged_in,
    );
  const customAgents = agents.filter((agent) => agent.custom);
  const agentsReady =
    agents.length > 0 &&
    agents.every((agent) => Boolean(agent.prompt.trim()));
  const customAgentsReady = customAgents.every((agent) => Boolean(agent.description.trim()));
  const hasSelectedGuild = Boolean(selectedGuild.trim());
  const channelAssignmentsReady =
    agents.length > 0 &&
    channelAssignments.length === agents.length &&
    channelAssignments.every(
      (assignment) =>
        Boolean((assignment.channelId || assignment.channelName).trim()) &&
        Boolean((assignment.channelName || assignment.recommendedName).trim()),
    );
  const newChannelCount = channelAssignments.filter((assignment) => !assignment.channelId).length;
  const ownerIdValid = !ownerId.trim() || /^\d{17,20}$/.test(ownerId.trim());
  const overwriteAcknowledged = !hasExistingSetup || confirmRerunOverwrite;
  const completionReady =
    commandBotsReady &&
    announceReady &&
    providersReady &&
    agentsReady &&
    hasSelectedGuild &&
    channelAssignmentsReady &&
    ownerIdValid &&
    notifyReady &&
    overwriteAcknowledged;

  const setRailItemRef = useCallback((stepNumber: number, node: HTMLDivElement | null) => {
    railItemRefs.current[stepNumber] = node;
  }, []);

  const goToStep = useCallback((nextStep: number) => {
    setStep(nextStep);
    setError("");
  }, []);

  const applyDraft = useCallback((draft: OnboardingDraft) => {
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
  }, []);

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
  }, []);

  // Load existing config and the latest server draft, then restore the newer draft.
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
        if (!cancelled) {
          setDraftSyncReady(true);
        }
      }
    }

    void loadInitialState();
    return () => {
      cancelled = true;
    };
  }, [applyDraft, isKo]);

  useEffect(() => {
    if (!draftSyncReady || completionChecklist) {
      return;
    }
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
    expandedAgent,
    hasExistingSetup,
    notifyBotInfo,
    notifyToken,
    ownerId,
    providerStatuses,
    selectedGuild,
    selectedTemplate,
    step,
    draftSyncReady,
  ]);

  useEffect(() => {
    railItemRefs.current[step]?.scrollIntoView({
      behavior: "smooth",
      block: "nearest",
      inline: "center",
    });

    window.requestAnimationFrame(() => {
      stepHeadingRef.current?.focus();
    });
  }, [step]);

  // ── API helpers ───────────────────────────────────

  const validateBotToken = async (tkn: string): Promise<BotInfo> => {
    const r = await fetch("/api/onboarding/validate-token", {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ token: tkn }),
    });
    return r.json();
  };

  const validateStep1 = async () => {
    setValidating(true);
    setError("");
    try {
      // Validate all command bots
      for (let i = 0; i < commandBots.length; i++) {
        if (!commandBots[i].token) {
          setError(tr(`실행 봇 ${i + 1}의 토큰을 입력하세요.`, `Enter token for Command Bot ${i + 1}.`));
          setValidating(false);
          return;
        }
        const info = await validateBotToken(commandBots[i].token);
        setCommandBots((prev) => {
          const copy = [...prev];
          copy[i] = { ...copy[i], botInfo: info };
          return copy;
        });
        if (!info.valid) {
          setError(tr(`실행 봇 ${i + 1} 토큰이 유효하지 않습니다.`, `Command Bot ${i + 1} token is invalid.`));
          setValidating(false);
          return;
        }
      }

      // Validate announce bot
      if (!announceToken) {
        setError(tr("통신 봇 토큰을 입력하세요.", "Enter communication bot token."));
        setValidating(false);
        return;
      }
      const annInfo = await validateBotToken(announceToken);
      setAnnounceBotInfo(annInfo);
      if (!annInfo.valid) {
        setError(tr("통신 봇 토큰이 유효하지 않습니다.", "Communication bot token is invalid."));
        setValidating(false);
        return;
      }

      // Validate notify bot if provided
      if (notifyToken) {
        const ntfInfo = await validateBotToken(notifyToken);
        setNotifyBotInfo(ntfInfo);
        if (!ntfInfo.valid) {
          setError(tr("알림 봇 토큰이 유효하지 않습니다.", "Notification bot token is invalid."));
          setValidating(false);
          return;
        }
      }

      // Don't auto-advance — let user invite bots first
    } catch {
      setError(tr("검증 실패", "Validation failed"));
    }
    setValidating(false);
  };

  const checkProviders = useCallback(async () => {
    setCheckingProviders(true);
    const providers = [...new Set(commandBots.map((b) => b.provider))];
    const statuses: Record<string, ProviderStatus> = {};
    for (const p of providers) {
      try {
        const r = await fetch("/api/onboarding/check-provider", {
          method: "POST",
          credentials: "include",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ provider: p }),
        });
        statuses[p] = await r.json();
      } catch {
        statuses[p] = { installed: false, logged_in: false };
      }
    }
    setProviderStatuses(statuses);
    setCheckingProviders(false);
  }, [commandBots]);

  useEffect(() => {
    if (step === 2) void checkProviders();
  }, [step, checkProviders]);

  const fetchChannels = async () => {
    const token = announceToken || commandBots[0]?.token;
    if (!token) return;
    try {
      const r = await fetch("/api/onboarding/channels", {
        method: "POST",
        credentials: "include",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ token }),
      });
      const d = await r.json();
      setGuilds(d.guilds || []);
      if (d.guilds?.length === 1) setSelectedGuild(d.guilds[0].id);
    } catch {
      setError(tr("채널 조회 실패", "Failed to fetch channels"));
    }
  };

  useEffect(() => {
    if (step === 4 && guilds.length === 0) void fetchChannels();
  }, [step]);

  // When agents change or guild changes, update channel assignments
  useEffect(() => {
    if (agents.length > 0) {
      const suffix = providerSuffix(primaryProvider);
      setChannelAssignments((prev) => {
        const previousByAgent = new Map(prev.map((assignment) => [assignment.agentId, assignment]));
        return agents.map((agent) => {
          const existing = previousByAgent.get(agent.id);
          const recommendedName = `${agent.id}-${suffix}`;
          return {
            agentId: agent.id,
            agentName: agent.name,
            recommendedName,
            channelId: existing?.channelId ?? "",
            channelName: existing?.channelName || existing?.recommendedName || recommendedName,
          };
        });
      });
    } else {
      setChannelAssignments([]);
    }
  }, [agents, primaryProvider]);

  const selectTemplate = (key: string) => {
    const tpl = TEMPLATES.find((t) => t.key === key);
    if (!tpl) return;
    setSelectedTemplate(key);
    setAgents(tpl.agents.map((a) => ({ ...a, custom: false })));
  };

  const addCustomAgent = () => {
    if (!customName.trim()) return;
    const name = customName.trim();
    const desc = customDesc.trim();
    const nameEn = customNameEn.trim() || name;
    const descEn = customDescEn.trim() || desc || nameEn;
    const id = name
      .toLowerCase()
      .replace(/[^a-z0-9가-힣]/g, "-")
      .replace(/-+/g, "-")
      .replace(/^-|-$/g, "")
      || `agent-${agents.length + 1}`;
    // Generate prompt in the same format as templates
    const prompt = `당신은 ${name}입니다. ${desc || name + "의 역할을 수행합니다"}.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 간결하고 명확하게 답변합니다\n- 필요시 확인 질문을 합니다`;
    setAgents((prev) => [
      ...prev,
      {
        id,
        name,
        nameEn,
        description: desc,
        descriptionEn: descEn,
        prompt,
        custom: true,
      },
    ]);
    setExpandedAgent(id);
    setCustomName("");
    setCustomDesc("");
    setCustomNameEn("");
    setCustomDescEn("");
  };

  const removeAgent = (id: string) => {
    setAgents((prev) => prev.filter((a) => a.id !== id));
  };

  const generateAiPrompt = async (agentId: string) => {
    const agent = agents.find((a) => a.id === agentId);
    if (!agent) return;
    setGeneratingPrompt(true);
    try {
      const r = await fetch("/api/onboarding/generate-prompt", {
        method: "POST",
        credentials: "include",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          name: agent.name,
          description: agent.description,
          provider: primaryProvider,
        }),
      });
      const d = await r.json();
      if (d.prompt) {
        setAgents((prev) =>
          prev.map((a) => (a.id === agentId ? { ...a, prompt: d.prompt } : a)),
        );
      }
    } catch {
      setError(tr("프롬프트 생성 실패", "Failed to generate prompt"));
    }
    setGeneratingPrompt(false);
  };

  const handleComplete = async () => {
    if (!completionReady) {
      setError(tr("완료 전 체크리스트의 실패 항목을 먼저 해결하세요.", "Resolve the failed checklist items before completing setup."));
      return;
    }

    setCompleting(true);
    setCompletionChecklist(null);
    setError("");
    try {
      const r = await fetch("/api/onboarding/complete", {
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
          channels: channelAssignments.map((ca) => ({
            channel_id: ca.channelId || ca.channelName,
            channel_name: ca.channelName,
            role_id: ca.agentId,
            description: agents.find((a) => a.id === ca.agentId)?.description || null,
            system_prompt: agents.find((a) => a.id === ca.agentId)?.prompt || null,
          })),
        }),
      });
      const d = await r.json();
      if (d.ok) {
        setResumeState("none");
        if (Array.isArray(d.checklist)) {
          clearOnboardingDraft();
          setDraftNoticeVisible(false);
          setCompletionChecklist(d.checklist);
        } else {
          clearOnboardingDraft();
          onComplete();
        }
      } else {
        const retryHint = d.partial_apply || d.completion_state?.partial_apply
          ? tr(
              "일부 적용이 남았습니다. 같은 payload로 다시 실행하면 기존 Discord 채널을 재사용합니다.",
              "Setup was partially applied. Retrying with the same payload will reuse the existing Discord channels.",
            )
          : "";
        setError([d.error || tr("설정 저장 실패", "Failed to save"), retryHint].filter(Boolean).join(" "));
      }
    } catch {
      setError(tr("완료 실패", "Failed to complete"));
    }
    setCompleting(false);
  };

  // ── Invite link helpers ──────────────────────────────

  // Discord permission bit values
  const PERMS = {
    // Command bot: Send Messages + Read Message History + Manage Messages
    //   + Create Public Threads + Send Messages in Threads
    command: (2048 + 65536 + 8192 + 17179869184 + 274877906944).toString(),
    // Announce bot: Administrator (simplest — covers channel creation, role management, etc.)
    announce: "8",
    // Notify bot: Send Messages only
    notify: "2048",
  };

  const makeInviteUrl = (botId: string, permissions: string) =>
    `https://discord.com/oauth2/authorize?client_id=${botId}&scope=bot&permissions=${permissions}`;

  // ── Styles ──────────────────────────────────────────

  const stepBox = "rounded-2xl border p-6 space-y-5";
  const inputStyle = "w-full rounded-xl px-4 py-3 text-sm bg-surface-subtle border";
  const btnPrimary =
    "px-6 py-3 rounded-xl text-sm font-medium bg-emerald-600 text-white hover:bg-emerald-500 disabled:opacity-50 transition-colors";
  const btnSecondary =
    "px-6 py-3 rounded-xl text-sm font-medium border bg-surface-subtle text-th-text-secondary hover:text-th-text-primary hover:opacity-100 disabled:opacity-50 transition-[opacity,color]";
  const btnSmall =
    "px-3 py-1.5 rounded-lg text-xs font-medium border bg-surface-subtle text-th-text-secondary hover:text-th-text-primary hover:opacity-100 transition-[opacity,color]";
  const labelStyle = "text-xs font-medium block mb-1";
  const actionRow = "flex flex-col sm:flex-row gap-3 pt-2";
  const borderLight = "rgba(148,163,184,0.2)";
  const borderInput = "rgba(148,163,184,0.24)";

  const guild = guilds.find((g) => g.id === selectedGuild);
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
        ? tr("채널 생성과 권한 설정에 사용할 통신 봇이 준비되었습니다.", "Communication bot is ready for channel creation and permissions.")
        : tr("통신 봇이 없으면 실제 Discord 채널 생성이 진행되지 않습니다.", "Without the communication bot, real Discord channel setup cannot run."),
    },
    {
      key: "notify-bot",
      label: tr("알림 봇 상태", "Notification bot status"),
      ok: notifyReady,
      detail: notifyToken.trim()
        ? tr("알림 봇 토큰이 검증되었습니다.", "Notification bot token is validated.")
        : tr("선택 사항입니다. 비워두면 알림 봇 없이 진행합니다.", "Optional. Leave blank to continue without a notification bot."),
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
      detail: customAgents.length === 0
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
        ? tr("완료 시 Discord 채널, 설정 파일, 파이프라인 검증까지 서버에서 진행합니다.", "Completion will apply Discord channels, settings, and pipeline verification on the server.")
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

  // ── Render ──────────────────────────────────────────

  return (
    <div className="mx-auto w-full max-w-2xl min-w-0 space-y-6 p-4 sm:p-8">
      {draftNoticeVisible && (
        <div
          className="rounded-xl border px-4 py-3"
          style={{
            borderColor: "color-mix(in srgb, var(--th-accent-primary) 26%, var(--th-border) 74%)",
            background: "color-mix(in srgb, var(--th-accent-primary-soft) 74%, transparent)",
          }}
        >
          <div className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
            {draftNoticeTitle}
          </div>
          <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-secondary)" }}>
            {draftNoticeDetail}
          </div>
          <div className="mt-3 flex flex-wrap gap-2">
            <button type="button"
              onClick={() => setDraftNoticeVisible(false)}
              className={btnSmall}
              style={{ borderColor: "rgba(148,163,184,0.3)", color: "var(--th-text-secondary)" }}
            >
              {tr("계속 진행", "Keep going")}
            </button>
            <button type="button"
              onClick={resetDraft}
              className={btnSmall}
              style={{ borderColor: "rgba(248,113,113,0.3)", color: "#fca5a5" }}
            >
              {tr("임시 저장 비우기", "Clear draft")}
            </button>
          </div>
        </div>
      )}

      {/* Header */}
      <div className="text-center space-y-2">
        <h1 className="text-2xl font-bold" style={{ color: "var(--th-text-heading)" }}>
          {tr("AgentDesk 설정", "AgentDesk Setup")}
        </h1>
        <p className="text-sm" style={{ color: "var(--th-text-muted)" }}>
          Step {step}/{TOTAL_STEPS}
        </p>
        <div className="flex gap-1 justify-center">
          {Array.from({ length: TOTAL_STEPS }, (_, i) => i + 1).map((s) => (
            <div
              key={s}
              className="h-1.5 rounded-full transition-all"
              style={{
                width: s <= step ? 40 : 20,
                backgroundColor: s <= step ? "var(--th-accent-primary)" : "rgba(148,163,184,0.3)",
              }}
            />
          ))}
        </div>
      </div>

      <StepStatusRail items={stepStatusItems} isKo={isKo} setItemRef={setRailItemRef} />

      {/* Error banner */}
      {error && (
        <div
          className="rounded-xl px-4 py-3 text-sm border"
          style={{ borderColor: "rgba(248,113,113,0.4)", color: "#fca5a5", backgroundColor: "rgba(127,29,29,0.2)" }}
        >
          {error}
        </div>
      )}

      {/* ──────────────── Step 1: Bot Connection ──────────────── */}
      {step === 1 && (
        <Step1BotConnection
          actionRow={actionRow}
          announceBotInfo={announceBotInfo}
          announceReady={announceReady}
          announceToken={announceToken}
          borderInput={borderInput}
          borderLight={borderLight}
          btnPrimary={btnPrimary}
          btnSecondary={btnSecondary}
          btnSmall={btnSmall}
          commandBots={commandBots}
          commandBotsReady={commandBotsReady}
          goToStep={goToStep}
          inputStyle={inputStyle}
          makeInviteUrl={makeInviteUrl}
          notifyBotInfo={notifyBotInfo}
          notifyToken={notifyToken}
          permissions={PERMS}
          setAnnounceToken={setAnnounceToken}
          setCommandBots={setCommandBots}
          setNotifyToken={setNotifyToken}
          step1Checklist={step1Checklist}
          stepBox={stepBox}
          stepHeadingRef={stepHeadingRef}
          tr={tr}
          validating={validating}
          validateStep1={validateStep1}
        />
      )}

      {/* ──────────────── Step 2: Provider Verification ──────────────── */}
      {step === 2 && (
        <Step2ProviderVerification
          actionRow={actionRow}
          borderLight={borderLight}
          btnPrimary={btnPrimary}
          btnSecondary={btnSecondary}
          checkingProviders={checkingProviders}
          commandBots={commandBots}
          goToStep={goToStep}
          isKo={isKo}
          onCheckProviders={checkProviders}
          providerStatuses={providerStatuses}
          providersReady={providersReady}
          step2Checklist={step2Checklist}
          stepBox={stepBox}
          stepHeadingRef={stepHeadingRef}
          tr={tr}
        />
      )}

      {/* ──────────────── Step 3: Agent Selection ──────────────── */}
      {step === 3 && (
        <div className={stepBox} style={{ borderColor: borderLight }}>
          <div>
            <h2 ref={stepHeadingRef} tabIndex={-1} className="text-lg font-semibold outline-none" style={{ color: "var(--th-text-heading)" }}>
              {tr("역할 프리셋과 에이전트 구성", "Role Presets & Agents")}
            </h2>
            <p className="text-sm mt-1" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "역할별 프리셋으로 팀을 빠르게 시작하거나, 필요한 에이전트를 직접 추가할 수 있습니다.",
                "Start from a role-based preset or add the exact agents you need.",
              )}
            </p>
          </div>

          {/* Template cards */}
          <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
            {TEMPLATES.map((tpl) => (
              <button type="button"
                key={tpl.key}
                onClick={() => selectTemplate(tpl.key)}
                className="rounded-xl p-4 border text-left transition-all hover:scale-[1.02]"
                style={{
                  borderColor: selectedTemplate === tpl.key ? "var(--th-accent-primary)" : borderLight,
                  backgroundColor:
                    selectedTemplate === tpl.key
                      ? "color-mix(in srgb, var(--th-accent-primary-soft) 82%, transparent)"
                      : "transparent",
                }}
              >
                <div className="text-2xl mb-2">{tpl.icon}</div>
                <div className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>{tr(tpl.name, tpl.nameEn)}</div>
                <div className="text-xs mt-1" style={{ color: "var(--th-text-muted)" }}>{tr(tpl.description, tpl.descriptionEn)}</div>
                <div className="text-xs mt-2" style={{ color: "var(--th-text-muted)" }}>
                  {tpl.agents.map((a) => tr(a.name, a.nameEn)).join(", ")}
                </div>
              </button>
            ))}
          </div>

          <div
            className="rounded-xl border p-4 space-y-2"
            style={{ borderColor: "rgba(99,102,241,0.2)", backgroundColor: "rgba(99,102,241,0.08)" }}
          >
            <div className="text-sm font-medium" style={{ color: "#c7d2fe" }}>
              {tr("커스텀 에이전트의 AI 프롬프트 초안 만들기", "Create AI prompt drafts for custom agents")}
            </div>
            <div className="text-xs space-y-1" style={{ color: "var(--th-text-secondary)" }}>
              <div>{tr("1. 이름과 한줄 설명을 적고 에이전트를 추가합니다.", "1. Add an agent with a name and one-line description.")}</div>
              <div>{tr("2. 카드 펼치기 → `AI 초안 생성`으로 시스템 프롬프트 뼈대를 만듭니다.", "2. Expand the card and click `AI Draft` to build the first system prompt draft.")}</div>
              <div>{tr("3. 담당 업무, 금지사항, 말투를 직접 보정하면 품질이 크게 올라갑니다.", "3. Refine responsibilities, guardrails, and tone for a much better final prompt.")}</div>
            </div>
          </div>

          {/* Agent list (from template or custom) */}
          {agents.length > 0 && (
            <div className="space-y-2">
              <div className="text-xs font-medium" style={{ color: "var(--th-text-secondary)" }}>
                {tr(`${agents.length}개 에이전트`, `${agents.length} agents`)}
              </div>
              {agents.map((agent) => (
                <div key={agent.id} className="rounded-xl border overflow-hidden" style={{ borderColor: borderLight }}>
                  <div
                    className="flex items-center gap-3 px-4 py-3 cursor-pointer hover:bg-surface-subtle"
                    onClick={() => setExpandedAgent(expandedAgent === agent.id ? null : agent.id)}
                  >
                    <span className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
                      {tr(agent.name, agent.nameEn || agent.name)}
                    </span>
                    <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                      {tr(agent.description, agent.descriptionEn || agent.description)}
                    </span>
                    <span className="ml-auto text-xs" style={{ color: "var(--th-text-muted)" }}>
                      {expandedAgent === agent.id ? "▲" : "▼"}
                    </span>
                    {agent.custom && (
                      <button type="button"
                        onClick={(e) => { e.stopPropagation(); removeAgent(agent.id); }}
                        className="text-xs text-red-400 hover:text-red-300"
                      >
                        {tr("삭제", "Del")}
                      </button>
                    )}
                  </div>
                  {expandedAgent === agent.id && (
                    <div className="px-4 pb-3 space-y-2 border-t" style={{ borderColor: borderLight }}>
                      <div className="flex items-center gap-2 pt-2">
                        <label className={labelStyle} style={{ color: "var(--th-text-secondary)" }}>
                          {tr("시스템 프롬프트", "System Prompt")}
                        </label>
                        {agent.custom && (
                          <button type="button"
                            onClick={() => void generateAiPrompt(agent.id)}
                            disabled={generatingPrompt}
                            className={btnSmall}
                            style={{
                              borderColor: "color-mix(in srgb, var(--th-accent-primary) 32%, var(--th-border) 68%)",
                              color: "var(--th-text-primary)",
                            }}
                          >
                            {generatingPrompt ? tr("생성 중...", "Generating...") : tr("AI 초안 생성", "AI Draft")}
                          </button>
                        )}
                      </div>
                      <textarea
                        value={agent.prompt}
                        onChange={(e) => {
                          setAgents((prev) =>
                            prev.map((a) => (a.id === agent.id ? { ...a, prompt: e.target.value } : a)),
                          );
                        }}
                        rows={6}
                        className="w-full rounded-lg px-3 py-2 text-xs bg-surface-subtle border resize-y"
                        style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
                        placeholder={tr("에이전트의 역할과 행동 규칙을 정의합니다", "Define the agent's role and behavior")}
                      />
                    </div>
                  )}
                </div>
              ))}
            </div>
          )}

          {/* Custom agent creation — single row */}
          <div className="space-y-2">
            <div className="grid grid-cols-1 sm:grid-cols-2 gap-2">
              <input
                type="text"
                placeholder={tr("에이전트 이름", "Agent name")}
                value={customName}
                onChange={(e) => setCustomName(e.target.value)}
                className="flex-1 rounded-lg px-3 py-2 text-sm bg-surface-subtle border"
                style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
              />
              <input
                type="text"
                placeholder={tr("한줄 설명", "Brief description")}
                value={customDesc}
                onChange={(e) => setCustomDesc(e.target.value)}
                className="flex-1 rounded-lg px-3 py-2 text-sm bg-surface-subtle border"
                style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
              />
            </div>
            <div className="grid grid-cols-1 sm:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto] gap-2">
              <input
                type="text"
                placeholder={tr("영문 이름 (선택)", "English name (optional)")}
                value={customNameEn}
                onChange={(e) => setCustomNameEn(e.target.value)}
                className="flex-1 rounded-lg px-3 py-2 text-sm bg-surface-subtle border"
                style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
              />
              <input
                type="text"
                placeholder={tr("영문 설명 (선택)", "English description (optional)")}
                value={customDescEn}
                onChange={(e) => setCustomDescEn(e.target.value)}
                className="flex-1 rounded-lg px-3 py-2 text-sm bg-surface-subtle border"
                style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
              />
              <button type="button"
                onClick={addCustomAgent}
                disabled={!customName.trim()}
                className="w-full sm:w-auto px-4 py-2 rounded-lg text-sm font-medium bg-indigo-600 text-white hover:bg-indigo-500 disabled:opacity-40 transition-colors whitespace-nowrap"
              >
                + {tr("추가", "Add")}
              </button>
            </div>
            <p className="text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "영문 필드를 비워두면 현재 입력값을 그대로 사용합니다. 다국어 대시보드에서 별도 표기가 필요할 때만 채우면 됩니다.",
                "Leave the English fields empty to reuse the current values. Fill them only when you need separate wording in English mode.",
              )}
            </p>
          </div>

          <ChecklistPanel title={tr("Step 3 체크리스트", "Step 3 checklist")} items={step3Checklist} />

          <div className={actionRow}>
            <button type="button" onClick={() => goToStep(2)} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
              {tr("이전", "Back")}
            </button>
            <button type="button" onClick={() => goToStep(4)} disabled={agents.length === 0} className={btnPrimary}>
              {tr("다음", "Next")} ({agents.length}{tr("개 에이전트", " agents")})
            </button>
          </div>
        </div>
      )}

      {/* ──────────────── Step 4: Channel Setup ──────────────── */}
      {step === 4 && (
        <Step4ChannelSetup
          actionRow={actionRow}
          borderInput={borderInput}
          borderLight={borderLight}
          btnPrimary={btnPrimary}
          btnSecondary={btnSecondary}
          channelAssignments={channelAssignments}
          channelAssignmentsReady={channelAssignmentsReady}
          goToStep={goToStep}
          guild={guild}
          guilds={guilds}
          hasSelectedGuild={hasSelectedGuild}
          inputStyle={inputStyle}
          labelStyle={labelStyle}
          selectedGuild={selectedGuild}
          setChannelAssignments={setChannelAssignments}
          setSelectedGuild={setSelectedGuild}
          step4Checklist={step4Checklist}
          stepBox={stepBox}
          stepHeadingRef={stepHeadingRef}
          tr={tr}
        />
      )}

      {/* ──────────────── Step 5: Owner + Confirm ──────────────── */}
      {step === 5 && (
        <Step5OwnerConfirm
          actionRow={actionRow}
          announceBotInfo={announceBotInfo}
          announceToken={announceToken}
          applySummary={applySummary}
          borderInput={borderInput}
          borderLight={borderLight}
          btnPrimary={btnPrimary}
          btnSecondary={btnSecondary}
          channelAssignments={channelAssignments}
          commandBots={commandBots}
          completing={completing}
          completionChecklist={completionChecklist}
          completionReady={completionReady}
          confirmRerunOverwrite={confirmRerunOverwrite}
          goToStep={goToStep}
          guilds={guilds}
          handleComplete={handleComplete}
          hasExistingSetup={hasExistingSetup}
          inputStyle={inputStyle}
          notifyToken={notifyToken}
          onComplete={onComplete}
          ownerId={ownerId}
          selectedGuild={selectedGuild}
          setConfirmRerunOverwrite={setConfirmRerunOverwrite}
          setOwnerId={setOwnerId}
          step5Checklist={step5Checklist}
          stepBox={stepBox}
          stepHeadingRef={stepHeadingRef}
          tr={tr}
        />
      )}
    </div>
  );
}
