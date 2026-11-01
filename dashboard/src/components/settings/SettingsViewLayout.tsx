import { Check } from "lucide-react";
import type { Dispatch, ReactNode, SetStateAction } from "react";
import { Modal } from "../common/overlay/Modal";
import type { SettingsNavItem } from "./SettingsMetaCatalog";
import type { PendingDangerousConfigSave, SettingsPanel } from "./SettingsModel";
import { SettingsNavigation } from "./SettingsNavigation";
import { SettingsOnboardingOverlay } from "./SettingsOnboardingOverlay";
import { SETTINGS_PRIMARY_BUTTON, SETTINGS_SECONDARY_BUTTON } from "./SettingsPanels";
import { getDangerousConfigLabel } from "./settingsDangerousConfig";

interface SettingsViewLayoutProps {
  activePanel: SettingsPanel;
  activeNavItem: SettingsNavItem;
  navItems: SettingsNavItem[];
  matchingCount: number;
  onPanelChange: (panel: SettingsPanel) => void;
  query: string;
  queryActive: boolean;
  setQuery: Dispatch<SetStateAction<string>>;
  showNoMatches: boolean;
  loadFailed: boolean;
  children: ReactNode;
  saveBar: ReactNode;
  pendingDangerousSave: PendingDangerousConfigSave | null;
  saving: boolean;
  onCancelDangerousSave: () => void;
  onConfirmDangerousSave: () => void;
  showOnboarding: boolean;
  onCloseOnboarding: () => void;
  isKo: boolean;
  tr: (ko: string, en: string) => string;
}

export function SettingsViewLayout({
  activePanel, activeNavItem, navItems, matchingCount, onPanelChange, query, queryActive, setQuery,
  showNoMatches, loadFailed, children, saveBar, pendingDangerousSave, saving, onCancelDangerousSave,
  onConfirmDangerousSave, showOnboarding, onCloseOnboarding, isKo, tr,
}: SettingsViewLayoutProps) {
  const dangerousLabels = pendingDangerousSave?.keys.map((key) => getDangerousConfigLabel(key, isKo)).join(", ") ?? "";

  return (
    <div
      data-testid="settings-page"
      className="mx-auto flex h-full w-full max-w-[88rem] min-w-0 flex-col overflow-y-auto px-4 pt-5 text-th-text-primary sm:px-8 sm:pt-7"
    >
      <div className="border-b border-th-border-subtle pb-5">
        <h1 className="text-2xl font-semibold tracking-tight">{tr("설정", "Settings")}</h1>
        <p className="mt-2 text-xs leading-5 break-keep text-th-text-muted">
          {tr("대시보드 표시, 작업 흐름, AI 계정과 연결을 관리해요.", "Manage display, workflow, AI accounts and connections.")}
        </p>
      </div>

      <div className="mt-5 grid min-w-0 gap-5 pb-8 md:grid-cols-[12rem_minmax(0,1fr)] md:gap-7">
        <SettingsNavigation
          activePanel={activePanel}
          items={navItems}
          matchingCount={matchingCount}
          onPanelChange={onPanelChange}
          query={query}
          queryActive={queryActive}
          setQuery={setQuery}
          tr={tr}
        />

        <section
          id="settings-panel-content"
          role="tabpanel"
          aria-labelledby={`settings-tab-${activePanel}`}
          tabIndex={-1}
          className="min-w-0 outline-none"
        >
          <div className="border-b border-th-border-subtle pb-4">
            <h2 className="text-base font-semibold break-keep text-th-text-primary">{activeNavItem.title}</h2>
            <p className="mt-1 text-xs leading-5 break-keep text-th-text-muted">{activeNavItem.detail}</p>
          </div>
          <div className="mt-5 min-w-0">
            {loadFailed ? (
              <p role="alert" className="mb-5 rounded-lg border border-th-border-subtle px-4 py-4 text-sm leading-6 break-keep text-th-text-secondary">
                {tr(
                  "일부 설정을 불러오지 못했어요. 불러오지 못한 항목은 숨겼으니 페이지를 새로고침해 다시 확인해 주세요.",
                  "Some settings could not be loaded and are hidden. Refresh the page to try again.",
                )}
              </p>
            ) : null}
            {showNoMatches ? (
              <p role="status" className="rounded-lg border border-th-border-subtle px-4 py-6 text-sm break-keep text-th-text-muted">
                {tr(`이 탭에는 “${query.trim()}” 검색 결과가 없어요. 남은 탭에서 찾아보세요.`, `Nothing in this tab matches “${query.trim()}”. Try one of the remaining tabs.`)}
              </p>
            ) : children}
          </div>
        </section>
      </div>

      {saveBar}

      <Modal
        open={Boolean(pendingDangerousSave)}
        onClose={onCancelDangerousSave}
        title={tr("저장하기 전에 확인해 주세요", "Confirm before saving")}
        description={tr(
          "자동화, 리뷰 단계, 대화 초기화에 영향을 주는 설정이 바뀌어요.",
          "These changes affect automation, review steps, or conversation resets.",
        )}
        size="sm"
      >
        <div className="space-y-4">
          <p className="rounded-lg border px-4 py-3 text-sm leading-6 break-keep" style={{ borderColor: "rgba(251, 191, 36, 0.35)", background: "rgba(251, 191, 36, 0.10)" }}>
            {tr(
              "저장하면 진행 중인 카드의 리뷰·병합·대화 정책이 바로 달라질 수 있어요.",
              "Saving can immediately change review, merge, or conversation policy for active cards.",
            )}
          </p>
          <div>
            <div className="text-xs font-semibold text-th-text-muted">{tr("바뀌는 설정", "Changing")}</div>
            <div className="mt-2 rounded-md border border-th-border-subtle px-3 py-2 text-sm">{dangerousLabels}</div>
          </div>
          <div className="flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
            <button type="button" onClick={onCancelDangerousSave} className={SETTINGS_SECONDARY_BUTTON}>
              {tr("취소", "Cancel")}
            </button>
            <button type="button" onClick={onConfirmDangerousSave} disabled={saving} className={SETTINGS_PRIMARY_BUTTON}>
              <Check size={14} aria-hidden />
              {saving ? tr("저장 중…", "Saving…") : tr("확인하고 저장", "Confirm and save")}
            </button>
          </div>
        </div>
      </Modal>

      <SettingsOnboardingOverlay isKo={isKo} onClose={onCloseOnboarding} open={showOnboarding} tr={tr} />
    </div>
  );
}
