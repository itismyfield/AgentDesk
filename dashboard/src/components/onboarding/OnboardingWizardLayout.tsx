import { StepStatusRail } from "./OnboardingWizardSections";
import { Step1BotConnection } from "./Step1BotConnection";
import { Step2ProviderVerification } from "./Step2ProviderVerification";
import { Step3AgentSelection } from "./Step3AgentSelection";
import { Step4ChannelSetup } from "./Step4ChannelSetup";
import { Step5OwnerConfirm } from "./Step5OwnerConfirm";

export const ONBOARDING_WIZARD_STYLES = {
  stepBox: "rounded-2xl border p-6 space-y-5",
  inputStyle: "w-full rounded-xl px-4 py-3 text-sm bg-surface-subtle border",
  btnPrimary:
    "px-6 py-3 rounded-xl text-sm font-medium bg-emerald-600 text-white hover:bg-emerald-500 disabled:opacity-50 transition-colors",
  btnSecondary:
    "px-6 py-3 rounded-xl text-sm font-medium border bg-surface-subtle text-th-text-secondary hover:text-th-text-primary hover:opacity-100 disabled:opacity-50 transition-[opacity,color]",
  btnSmall:
    "px-3 py-1.5 rounded-lg text-xs font-medium border bg-surface-subtle text-th-text-secondary hover:text-th-text-primary hover:opacity-100 transition-[opacity,color]",
  labelStyle: "text-xs font-medium block mb-1",
  actionRow: "flex flex-col sm:flex-row gap-3 pt-2",
  borderLight: "rgba(148,163,184,0.2)",
  borderInput: "rgba(148,163,184,0.24)",
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
    <div className="mx-auto w-full max-w-2xl min-w-0 space-y-6 p-4 sm:p-8">
      {draftNoticeVisible && (
        <div
          className="rounded-xl border px-4 py-3"
          style={{
            borderColor:
              "color-mix(in srgb, var(--th-accent-primary) 26%, var(--th-border) 74%)",
            background:
              "color-mix(in srgb, var(--th-accent-primary-soft) 74%, transparent)",
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
                borderColor: "rgba(148,163,184,0.3)",
                color: "var(--th-text-secondary)",
              }}
            >
              {tr("계속 진행", "Keep going")}
            </button>
            <button
              type="button"
              onClick={resetDraft}
              className={btnSmall}
              style={{ borderColor: "rgba(248,113,113,0.3)", color: "#fca5a5" }}
            >
              {tr("임시 저장 비우기", "Clear draft")}
            </button>
          </div>
        </div>
      )}

      <div className="text-center space-y-2">
        <h1
          className="text-2xl font-bold"
          style={{ color: "var(--th-text-heading)" }}
        >
          {tr("AgentDesk 설정", "AgentDesk Setup")}
        </h1>
        <p className="text-sm" style={{ color: "var(--th-text-muted)" }}>
          Step {step}/{TOTAL_STEPS}
        </p>
        <div className="flex gap-1 justify-center">
          {Array.from({ length: TOTAL_STEPS }, (_, index) => index + 1).map(
            (itemStep) => (
              <div
                key={itemStep}
                className="h-1.5 rounded-full transition-all"
                style={{
                  width: itemStep <= step ? 40 : 20,
                  backgroundColor:
                    itemStep <= step
                      ? "var(--th-accent-primary)"
                      : "rgba(148,163,184,0.3)",
                }}
              />
            ),
          )}
        </div>
      </div>

      <StepStatusRail items={stepStatusItems} isKo={isKo} setItemRef={setItemRef} />

      {error && (
        <div
          className="rounded-xl px-4 py-3 text-sm border"
          style={{
            borderColor: "rgba(248,113,113,0.4)",
            color: "#fca5a5",
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
