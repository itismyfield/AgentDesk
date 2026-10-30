import { StepStatusRail } from "./OnboardingWizardSections";
import { Step1BotConnection } from "./Step1BotConnection";
import { Step2ProviderVerification } from "./Step2ProviderVerification";
import { Step3AgentSelection } from "./Step3AgentSelection";
import { Step4ChannelSetup } from "./Step4ChannelSetup";
import { Step5OwnerConfirm } from "./Step5OwnerConfirm";

export const ONBOARDING_WIZARD_STYLES = {
  stepBox: "min-w-0 rounded-lg border bg-th-bg-surface p-4 space-y-5 sm:p-5",
  inputStyle: "min-h-11 w-full min-w-0 rounded-md px-3 py-2 text-sm bg-th-bg-primary border",
  btnPrimary:
    "min-h-11 px-4 py-2.5 rounded-md text-sm font-medium bg-th-text-primary text-th-bg-primary hover:opacity-90 disabled:opacity-50 transition-colors",
  btnSecondary:
    "min-h-11 px-4 py-2.5 rounded-md text-sm font-medium border bg-surface-subtle text-th-text-secondary hover:text-th-text-primary hover:opacity-100 disabled:opacity-50 transition-[opacity,color]",
  btnSmall:
    "min-h-11 min-w-11 px-3 py-2 rounded-md text-xs font-medium border bg-surface-subtle text-th-text-secondary hover:text-th-text-primary hover:opacity-100 transition-[opacity,color]",
  labelStyle: "text-xs font-medium block mb-1",
  actionRow: "flex flex-col sm:flex-row gap-3 pt-2",
  borderLight: "var(--th-border-subtle)",
  borderInput: "var(--th-input-border)",
};

type OnboardingWizardLayoutProps = Record<string, any>;

export function OnboardingWizardLayout(props: OnboardingWizardLayoutProps) {
  const {
    addCustomAgent,
    agents,
    announceBotInfo,
    announceReady,
    announceToken,
    applySummary,
    channelAssignments,
    channelAssignmentsReady,
    checkProviders,
    checkingProviders,
    commandBots,
    commandBotsReady,
    completing,
    completionChecklist,
    completionReady,
    confirmRerunOverwrite,
    customDesc,
    customDescEn,
    customName,
    customNameEn,
    draftNoticeDetail,
    draftNoticeTitle,
    draftNoticeVisible,
    error,
    expandedAgent,
    generateAiPrompt,
    generatingPrompt,
    goToStep,
    guild,
    guilds,
    handleComplete,
    hasExistingSetup,
    hasSelectedGuild,
    inputStyle: ignoredInputStyle,
    invitePermissions,
    isKo,
    makeInviteUrl,
    notifyBotInfo,
    notifyToken,
    onComplete,
    ownerId,
    resetDraft,
    removeAgent,
    selectTemplate,
    selectedGuild,
    selectedTemplate,
    setAgents,
    setAnnounceToken,
    setChannelAssignments,
    setCommandBots,
    setConfirmRerunOverwrite,
    setCustomDesc,
    setCustomDescEn,
    setCustomName,
    setCustomNameEn,
    setDraftNoticeVisible,
    setExpandedAgent,
    setItemRef,
    setNotifyToken,
    setOwnerId,
    setSelectedGuild,
    step,
    step1Checklist,
    step2Checklist,
    step3Checklist,
    step4Checklist,
    step5Checklist,
    stepHeadingRef,
    stepStatusItems,
    styles,
    totalSteps,
    tr,
    validating,
    validateStep1,
  } = props;
  void ignoredInputStyle;

  const {
    actionRow,
    borderInput,
    borderLight,
    btnPrimary,
    btnSecondary,
    btnSmall,
    inputStyle,
    labelStyle,
    stepBox,
  } = styles;
  const PERMS = invitePermissions;
  const TOTAL_STEPS = totalSteps;

  return (
    <div className="mx-auto w-full max-w-3xl min-w-0 space-y-5 p-4 sm:px-6">
      <header className="border-b border-th-border-subtle pb-5">
        <h1 className="text-2xl font-semibold tracking-tight text-th-text-heading">{tr("AgentDesk 설정", "AgentDesk Setup")}</h1>
        <p className="mt-2 text-xs leading-5 text-th-text-muted">{tr(`초기 연결 · ${step}/${TOTAL_STEPS}단계`, `Initial setup · Step ${step}/${TOTAL_STEPS}`)}</p>
      </header>
      {draftNoticeVisible && (
        <div
          className="rounded-xl border px-4 py-3"
          style={{
            borderColor: "var(--th-border-subtle)",
            background: "var(--th-bg-surface)",
          }}
        >
          <div
            className="text-sm font-medium"
            style={{ color: "var(--th-text-primary)" }}
          >
            {draftNoticeTitle}
          </div>
          <div
            className="mt-1 text-xs leading-5"
            style={{ color: "var(--th-text-secondary)" }}
          >
            {draftNoticeDetail}
          </div>
          <div className="mt-3 flex flex-wrap gap-2">
            <button
              type="button"
              onClick={() => setDraftNoticeVisible(false)}
              className={btnSmall}
              style={{
                borderColor: "var(--th-border-subtle)",
                color: "var(--th-text-secondary)",
              }}
            >
              {tr("계속 진행", "Keep going")}
            </button>
            <button
              type="button"
              onClick={resetDraft}
              className={btnSmall}
              style={{ borderColor: "rgba(248,113,113,0.3)", color: "var(--th-accent-danger)" }}
            >
              {tr("임시 저장 비우기", "Clear draft")}
            </button>
          </div>
        </div>
      )}

      <StepStatusRail items={stepStatusItems} isKo={isKo} setItemRef={setItemRef} />

      {error && (
        <div
          className="rounded-xl px-4 py-3 text-sm border"
          style={{
            borderColor: "rgba(248,113,113,0.4)",
            color: "var(--th-accent-danger)",
            backgroundColor: "rgba(127,29,29,0.2)",
          }}
        >
          {error}
        </div>
      )}

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
          providerStatuses={props.providerStatuses}
          providersReady={props.providersReady}
          step2Checklist={step2Checklist}
          stepBox={stepBox}
          stepHeadingRef={stepHeadingRef}
          tr={tr}
        />
      )}

      {step === 3 && (
        <Step3AgentSelection
          actionRow={actionRow}
          addCustomAgent={addCustomAgent}
          agents={agents}
          borderInput={borderInput}
          borderLight={borderLight}
          btnPrimary={btnPrimary}
          btnSecondary={btnSecondary}
          btnSmall={btnSmall}
          customDesc={customDesc}
          customDescEn={customDescEn}
          customName={customName}
          customNameEn={customNameEn}
          expandedAgent={expandedAgent}
          generateAiPrompt={generateAiPrompt}
          generatingPrompt={generatingPrompt}
          goToStep={goToStep}
          labelStyle={labelStyle}
          removeAgent={removeAgent}
          selectTemplate={selectTemplate}
          selectedTemplate={selectedTemplate}
          setAgents={setAgents}
          setCustomDesc={setCustomDesc}
          setCustomDescEn={setCustomDescEn}
          setCustomName={setCustomName}
          setCustomNameEn={setCustomNameEn}
          setExpandedAgent={setExpandedAgent}
          step3Checklist={step3Checklist}
          stepBox={stepBox}
          stepHeadingRef={stepHeadingRef}
          tr={tr}
        />
      )}

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
