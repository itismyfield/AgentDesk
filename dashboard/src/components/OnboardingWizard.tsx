import { useState, useEffect, useCallback, useRef } from "react";
import {
  readOnboardingDraft,
  type AgentDef,
  type BotInfo,
  type ChannelAssignment,
  type CommandBotEntry,
  type OnboardingDraft,
  type OnboardingResumeState,
  type ProviderStatus,
} from "./onboardingDraft";
import { providerSuffix } from "./onboarding/providerConfig";
import type { CompletionChecklistItem } from "./onboarding/OnboardingWizardSections";
import { useOnboardingWizardActions } from "./onboarding/OnboardingWizardActions";
import { buildOnboardingWizardDerived } from "./onboarding/OnboardingWizardDerived";
import { useOnboardingDraftLifecycle } from "./onboarding/useOnboardingDraftLifecycle";
import {
  ONBOARDING_WIZARD_STYLES,
  OnboardingWizardLayout,
} from "./onboarding/OnboardingWizardLayout";

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
  const agentsReady =
    agents.length > 0 &&
    agents.every((agent) => Boolean(agent.prompt.trim()));
  const customAgents = agents.filter((agent) => agent.custom);
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

  const {
    addCustomAgent,
    checkProviders,
    fetchChannels,
    generateAiPrompt,
    handleComplete,
    invitePermissions,
    makeInviteUrl,
    removeAgent,
    selectTemplate,
    validateStep1,
  } = useOnboardingWizardActions({
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
  });

  const { resetDraft } = useOnboardingDraftLifecycle({
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
  });

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

  useEffect(() => {
    if (step === 2) void checkProviders();
  }, [step, checkProviders]);

  useEffect(() => {
    if (step === 4 && guilds.length === 0) void fetchChannels();
  }, [step, guilds.length, fetchChannels]);

  useEffect(() => {
    if (agents.length > 0) {
      const suffix = providerSuffix(primaryProvider);
      setChannelAssignments((prev) => {
        const previousByAgent = new Map(
          prev.map((assignment) => [assignment.agentId, assignment]),
        );
        return agents.map((agent) => {
          const existing = previousByAgent.get(agent.id);
          const recommendedName = `${agent.id}-${suffix}`;
          return {
            agentId: agent.id,
            agentName: agent.name,
            recommendedName,
            channelId: existing?.channelId ?? "",
            channelName:
              existing?.channelName || existing?.recommendedName || recommendedName,
          };
        });
      });
    } else {
      setChannelAssignments([]);
    }
  }, [agents, primaryProvider]);

  const guild = guilds.find((g) => g.id === selectedGuild);
  const {
    applySummary,
    draftNoticeDetail,
    draftNoticeTitle,
    step1Checklist,
    step2Checklist,
    step3Checklist,
    step4Checklist,
    step5Checklist,
    stepStatusItems,
  } = buildOnboardingWizardDerived({
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
  });

  // ── Render ──────────────────────────────────────────

  return (
    <OnboardingWizardLayout
      addCustomAgent={addCustomAgent}
      agents={agents}
      announceBotInfo={announceBotInfo}
      announceReady={announceReady}
      announceToken={announceToken}
      applySummary={applySummary}
      channelAssignments={channelAssignments}
      channelAssignmentsReady={channelAssignmentsReady}
      checkProviders={checkProviders}
      checkingProviders={checkingProviders}
      commandBots={commandBots}
      commandBotsReady={commandBotsReady}
      completing={completing}
      completionChecklist={completionChecklist}
      completionReady={completionReady}
      confirmRerunOverwrite={confirmRerunOverwrite}
      customDesc={customDesc}
      customDescEn={customDescEn}
      customName={customName}
      customNameEn={customNameEn}
      draftNoticeDetail={draftNoticeDetail}
      draftNoticeTitle={draftNoticeTitle}
      draftNoticeVisible={draftNoticeVisible}
      error={error}
      expandedAgent={expandedAgent}
      generateAiPrompt={generateAiPrompt}
      generatingPrompt={generatingPrompt}
      goToStep={goToStep}
      guild={guild}
      guilds={guilds}
      handleComplete={handleComplete}
      hasExistingSetup={hasExistingSetup}
      hasSelectedGuild={hasSelectedGuild}
      invitePermissions={invitePermissions}
      isKo={isKo}
      makeInviteUrl={makeInviteUrl}
      notifyBotInfo={notifyBotInfo}
      notifyToken={notifyToken}
      onComplete={onComplete}
      ownerId={ownerId}
      providerStatuses={providerStatuses}
      providersReady={providersReady}
      resetDraft={resetDraft}
      removeAgent={removeAgent}
      selectTemplate={selectTemplate}
      selectedGuild={selectedGuild}
      selectedTemplate={selectedTemplate}
      setAgents={setAgents}
      setAnnounceToken={setAnnounceToken}
      setChannelAssignments={setChannelAssignments}
      setCommandBots={setCommandBots}
      setConfirmRerunOverwrite={setConfirmRerunOverwrite}
      setCustomDesc={setCustomDesc}
      setCustomDescEn={setCustomDescEn}
      setCustomName={setCustomName}
      setCustomNameEn={setCustomNameEn}
      setDraftNoticeVisible={setDraftNoticeVisible}
      setExpandedAgent={setExpandedAgent}
      setItemRef={setRailItemRef}
      setNotifyToken={setNotifyToken}
      setOwnerId={setOwnerId}
      setSelectedGuild={setSelectedGuild}
      step={step}
      step1Checklist={step1Checklist}
      step2Checklist={step2Checklist}
      step3Checklist={step3Checklist}
      step4Checklist={step4Checklist}
      step5Checklist={step5Checklist}
      stepHeadingRef={stepHeadingRef}
      stepStatusItems={stepStatusItems}
      styles={ONBOARDING_WIZARD_STYLES}
      totalSteps={TOTAL_STEPS}
      tr={tr}
      validating={validating}
      validateStep1={validateStep1}
    />
  );

}
