import { Check, Eye, Info } from "lucide-react";
import { Modal } from "../common/overlay/Modal";
import { SurfaceCard as SettingsCard } from "../common/SurfacePrimitives";
import { SettingsGeneralPanel } from "./SettingsGeneralPanel";
import { SettingsGlossary } from "./SettingsKnowledge";
import { SettingsNavigation } from "./SettingsNavigation";
import { SettingsOnboardingOverlay } from "./SettingsOnboardingOverlay";
import { SettingsOnboardingPanel } from "./SettingsOnboardingPanel";
import { SettingsPipelinePanel } from "./SettingsPipelinePanel";
import { SettingsRuntimePanel } from "./SettingsRuntimePanel";
import { SettingsVoicePanel } from "./SettingsVoicePanel";
import { getDangerousConfigLabel } from "./settingsDangerousConfig";

export function SettingsViewLayout({ ctx }: { ctx: any }) {
  const {
    activeNavItem,
    activePanel,
    activeRuntimeCategoryId,
    companyDirty,
    configDirty,
    configEntries,
    configSaving,
    filteredNavItems,
    generalFormInvalid,
    generalMetas,
    groupedConfigEntries,
    handleConfigSave,
    handleDangerousConfigConfirm,
    handlePanelChange,
    handleRcChange,
    handleRcReset,
    handleRcSave,
    handleSave,
    handleVoiceSave,
    inputStyle,
    isKo,
    isRowVisible,
    loadVoiceConfig,
    matchingKeysInActivePanel,
    onboardingMetas,
    openOnboarding,
    panelQuery,
    panelQueryNormalized,
    pendingDangerousConfigSave,
    pipelineAgents,
    pipelineMetas,
    pipelineRepos,
    pipelineSelectorError,
    pipelineSelectorLoading,
    primaryActionClass,
    primaryActionStyle,
    rcDirty,
    rcLoaded,
    rcSaving,
    renderSettingGroupCard,
    renderSettingRow,
    runtimeMetas,
    saving,
    secondaryActionClass,
    secondaryActionStyle,
    selectedPipelineAgentId,
    selectedPipelineRepo,
    setActiveRuntimeCategoryId,
    setPanelQuery,
    setPendingDangerousConfigSave,
    setSelectedPipelineAgentId,
    setSelectedPipelineRepo,
    setShowOnboarding,
    showOnboarding,
    subtleButtonClass,
    subtleButtonStyle,
    tr,
    updateVoiceAgent,
    updateVoiceGlobal,
    voiceAliasConflict,
    voiceDirty,
    voiceDraft,
    voiceError,
    voiceLoaded,
    voiceSaving,
  } = ctx;

  const renderActivePanel = () => {
    switch (activePanel) {
      case "runtime":
        return (
          <SettingsRuntimePanel
            activeRuntimeCategoryId={activeRuntimeCategoryId}
            inputStyle={inputStyle}
            onCategoryChange={setActiveRuntimeCategoryId}
            onRuntimeChange={handleRcChange}
            onRuntimeReset={handleRcReset}
            onRuntimeSave={handleRcSave}
            panelQueryNormalized={panelQueryNormalized}
            primaryActionClass={primaryActionClass}
            primaryActionStyle={primaryActionStyle}
            rcDirty={rcDirty}
            rcLoaded={rcLoaded}
            rcSaving={rcSaving}
            renderSettingRow={renderSettingRow}
            runtimeMetas={runtimeMetas}
            subtleButtonClass={subtleButtonClass}
            subtleButtonStyle={subtleButtonStyle}
            tr={tr}
          />
        );
      case "pipeline":
        return (
          <SettingsPipelinePanel
            configDirty={configDirty}
            configEntries={configEntries}
            configSaving={configSaving}
            groupedConfigEntries={groupedConfigEntries}
            inputStyle={inputStyle}
            isKo={isKo}
            onConfigSave={handleConfigSave}
            pipelineAgents={pipelineAgents}
            pipelineMetas={pipelineMetas}
            pipelineRepos={pipelineRepos}
            pipelineSelectorError={pipelineSelectorError}
            pipelineSelectorLoading={pipelineSelectorLoading}
            primaryActionClass={primaryActionClass}
            primaryActionStyle={primaryActionStyle}
            renderSettingGroupCard={renderSettingGroupCard}
            renderSettingRow={renderSettingRow}
            selectedPipelineAgentId={selectedPipelineAgentId}
            selectedPipelineRepo={selectedPipelineRepo}
            setSelectedPipelineAgentId={setSelectedPipelineAgentId}
            setSelectedPipelineRepo={setSelectedPipelineRepo}
            tr={tr}
          />
        );
      case "voice":
        return (
          <SettingsVoicePanel
            inputStyle={inputStyle}
            isKo={isKo}
            isRowVisible={isRowVisible}
            loadVoiceConfig={loadVoiceConfig}
            onVoiceSave={handleVoiceSave}
            primaryActionClass={primaryActionClass}
            primaryActionStyle={primaryActionStyle}
            renderSettingGroupCard={renderSettingGroupCard}
            secondaryActionClass={secondaryActionClass}
            secondaryActionStyle={secondaryActionStyle}
            tr={tr}
            updateVoiceAgent={updateVoiceAgent}
            updateVoiceGlobal={updateVoiceGlobal}
            voiceAliasConflict={voiceAliasConflict}
            voiceDirty={voiceDirty}
            voiceDraft={voiceDraft}
            voiceError={voiceError}
            voiceLoaded={voiceLoaded}
            voiceSaving={voiceSaving}
          />
        );
      case "onboarding":
        return (
          <SettingsOnboardingPanel
            onboardingMetas={onboardingMetas}
            renderSettingGroupCard={renderSettingGroupCard}
            renderSettingRow={renderSettingRow}
            tr={tr}
          />
        );
      case "general":
      default:
        return (
          <SettingsGeneralPanel
            companyDirty={companyDirty}
            generalFormInvalid={generalFormInvalid}
            generalMetas={generalMetas}
            onSave={handleSave}
            primaryActionClass={primaryActionClass}
            primaryActionStyle={primaryActionStyle}
            renderSettingGroupCard={renderSettingGroupCard}
            renderSettingRow={renderSettingRow}
            saving={saving}
            tr={tr}
          />
        );
    }
  };

  const renderHeaderActions = () => {
    if (activePanel === "onboarding") {
      return (
        <button
          onClick={openOnboarding}
          className={secondaryActionClass}
          style={secondaryActionStyle}
        >
          {tr("온보딩 다시 실행", "Re-run onboarding")}
        </button>
      );
    }

    if (activePanel === "pipeline") {
      return (
        <>
          <button
            type="button"
            onClick={() =>
              document
                .getElementById("settings-audit-notes")
                ?.scrollIntoView({ behavior: "smooth", block: "start" })
            }
            className={secondaryActionClass}
            style={secondaryActionStyle}
          >
            <Eye size={12} />
            {tr("audit 노트", "Audit notes")}
          </button>
          <button
            onClick={handleConfigSave}
            disabled={configSaving || !configDirty}
            className={primaryActionClass}
            style={primaryActionStyle}
          >
            <Check size={12} />
            {configSaving ? tr("저장 중...", "Saving...") : tr("저장", "Save")}
          </button>
        </>
      );
    }

    if (activePanel === "runtime") {
      return (
        <button
          onClick={handleRcSave}
          disabled={rcSaving || !rcDirty}
          className={primaryActionClass}
          style={primaryActionStyle}
        >
          <Check size={12} />
          {rcSaving ? tr("저장 중...", "Saving...") : tr("저장", "Save")}
        </button>
      );
    }

    if (activePanel === "voice") {
      return (
        <>
          <button
            type="button"
            onClick={() => void loadVoiceConfig()}
            className={secondaryActionClass}
            style={secondaryActionStyle}
          >
            {tr("다시 불러오기", "Reload")}
          </button>
          <button
            type="button"
            onClick={() => void handleVoiceSave()}
            disabled={voiceSaving || !voiceDirty || Boolean(voiceAliasConflict)}
            className={primaryActionClass}
            style={primaryActionStyle}
          >
            <Check size={12} />
            {voiceSaving ? tr("저장 중...", "Saving...") : tr("저장", "Save")}
          </button>
        </>
      );
    }

    return (
      <button
        onClick={() => void handleSave()}
        disabled={saving || generalFormInvalid || !companyDirty}
        className={primaryActionClass}
        style={primaryActionStyle}
      >
        <Check size={12} />
        {saving ? tr("저장 중...", "Saving...") : tr("저장", "Save")}
      </button>
    );
  };

  const settingsInfoNotice = (
    <div
      className="flex items-start gap-3 rounded-[18px] border px-4 py-4 sm:px-5"
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
      }}
    >
      <div
        className="grid h-7 w-7 shrink-0 place-items-center rounded-[10px]"
        style={{
          background: "var(--th-accent-primary-soft)",
          color: "var(--th-accent-primary)",
        }}
      >
        <Info size={14} />
      </div>
      <div className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
        {tr("whitelist된 ", "Only whitelisted ")}
        <code
          className="rounded px-1.5 py-0.5 text-[12px]"
          style={{
            fontFamily: "var(--font-mono)",
            color: "var(--th-text)",
            background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
          }}
        >
          kv_meta
        </code>{" "}
        {tr(
          "키와 agentdesk.yaml 음성 설정만 편집합니다. read-only 항목도 숨기지 않고 현재 상태를 그대로 보여줍니다.",
          "keys and agentdesk.yaml voice settings are editable. Read-only items stay visible so the current state remains explicit.",
        )}
      </div>
    </div>
  );
  const dangerousConfigLabels =
    pendingDangerousConfigSave?.keys.map((key: string) => getDangerousConfigLabel(key, isKo)).join(", ") ?? "";

  return (
    <div
      data-testid="settings-page"
      className="page fade-in mx-auto h-full w-full max-w-[1600px] min-w-0 overflow-x-hidden overflow-y-auto px-4 py-4 pb-40 sm:px-6"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <div className="page-header">
        <div className="min-w-0">
          <div className="page-title">{tr("설정", "Settings")}</div>
          <div className="page-sub">
            {tr(
              "카탈로그에서 꺼내 쓰는 kv_meta 설정",
              "Catalog-driven kv_meta configuration",
            )}
          </div>
        </div>
        <div className="flex flex-wrap gap-2">{renderHeaderActions()}</div>
      </div>

      <div className="settings-grid mt-4 grid gap-4 md:grid-cols-[220px_minmax(0,1fr)]">
        <SettingsNavigation
          activePanel={activePanel}
          inputStyle={inputStyle}
          items={filteredNavItems}
          matchingCount={matchingKeysInActivePanel.size}
          onPanelChange={handlePanelChange}
          query={panelQuery}
          queryActive={Boolean(panelQueryNormalized)}
          setQuery={setPanelQuery}
          tr={tr}
        />

        <div className="min-w-0 space-y-4">
          {settingsInfoNotice}
          <SettingsGlossary isKo={isKo} />

          <SettingsCard
            id="settings-panel-content"
            role="tabpanel"
            aria-labelledby={`settings-tab-${activePanel}`}
            tabIndex={-1}
            className="min-w-0 rounded-[28px] border px-4 py-4 outline-none sm:px-5 sm:py-5"
            style={{
              borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
              background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
            }}
          >
            <div className="flex flex-wrap items-start justify-between gap-3 border-b pb-4" style={{ borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)" }}>
              <div className="min-w-0">
                <div className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                  {activeNavItem.title}
                </div>
                <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {activeNavItem.detail}
                </div>
              </div>
              {activeNavItem.count ? (
                <span
                  className="inline-flex items-center rounded-full border px-2.5 py-1 text-[10px] font-medium"
                  style={{
                    borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
                    background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
                    color: "var(--th-text-muted)",
                  }}
                >
                  {activeNavItem.count}
                </span>
              ) : null}
            </div>
            <div className="mt-5 min-w-0">
              {renderActivePanel()}
            </div>
          </SettingsCard>
        </div>
      </div>

      <Modal
        open={Boolean(pendingDangerousConfigSave)}
        onClose={() => setPendingDangerousConfigSave(null)}
        title={tr("위험 설정 저장 확인", "Confirm risky settings")}
        description={tr(
          "자동화, 리뷰 게이트, 컨텍스트 초기화에 영향을 주는 설정입니다.",
          "These settings affect automation, review gates, or context clearing.",
        )}
        size="sm"
      >
        <div className="space-y-4">
          <div className="rounded-2xl border px-4 py-3 text-sm leading-6"
            style={{
              borderColor: "rgba(251, 191, 36, 0.35)",
              background: "rgba(251, 191, 36, 0.10)",
              color: "var(--th-text)",
            }}
          >
            {tr(
              "저장하면 진행 중인 카드의 리뷰/머지/컨텍스트 정책이 즉시 달라질 수 있습니다.",
              "Saving can immediately change review, merge, or context policy for active cards.",
            )}
          </div>
          <div>
            <div className="text-xs font-semibold uppercase tracking-wide" style={{ color: "var(--th-text-muted)" }}>
              {tr("변경 대상", "Changing")}
            </div>
            <div className="mt-2 rounded-xl border px-3 py-2 text-sm" style={{
              borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
              background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
              color: "var(--th-text)",
            }}>
              {dangerousConfigLabels}
            </div>
          </div>
          <div className="flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
            <button
              type="button"
              onClick={() => setPendingDangerousConfigSave(null)}
              className={secondaryActionClass}
              style={secondaryActionStyle}
            >
              {tr("취소", "Cancel")}
            </button>
            <button
              type="button"
              onClick={() => void handleDangerousConfigConfirm()}
              disabled={configSaving}
              className={primaryActionClass}
              style={primaryActionStyle}
            >
              <Check size={12} />
              {configSaving ? tr("저장 중...", "Saving...") : tr("확인 후 저장", "Confirm and save")}
            </button>
          </div>
        </div>
      </Modal>

      <SettingsOnboardingOverlay
        isKo={isKo}
        onClose={() => setShowOnboarding(false)}
        open={showOnboarding}
        tr={tr}
      />
    </div>
  );
}
