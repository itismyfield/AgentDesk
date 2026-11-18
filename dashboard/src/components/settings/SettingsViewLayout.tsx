import { AlertTriangle, Check, SearchX } from "lucide-react";
import type { Dispatch, ReactNode, SetStateAction } from "react";
import { Button } from "../../kit/free/button-clean";
import { Modal } from "../common/overlay/Modal";
import type { SettingsNavItem } from "./SettingsMetaCatalog";
import type { PendingDangerousConfigSave, SettingsPanel } from "./SettingsModel";
import { SettingsNavigation } from "./SettingsNavigation";
import { SettingsOnboardingOverlay } from "./SettingsOnboardingOverlay";
import { SETTINGS_TRAY_CLASS, SettingsNotice } from "./SettingsPrimitives";
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
  /** Lifts the reading-width cap, for the visual workflow editor. */
  wide?: boolean;
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

/** Page frame: title block, panel list beside a reading-width content column, the save bar at the bottom. */
export function SettingsViewLayout({
  activePanel, activeNavItem, navItems, matchingCount, onPanelChange, query, queryActive, setQuery,
  showNoMatches, loadFailed, wide = false, children, saveBar, pendingDangerousSave, saving, onCancelDangerousSave,
  onConfirmDangerousSave, showOnboarding, onCloseOnboarding, isKo, tr,
}: SettingsViewLayoutProps) {
  const dangerousLabels = pendingDangerousSave?.keys.map((key) => getDangerousConfigLabel(key, isKo)).join(", ") ?? "";

  return (
    <div
      data-testid="settings-page"
      className="flex h-full w-full min-w-0 flex-col overflow-y-auto text-th-text-primary [overflow-wrap:anywhere]"
    >
      <div className="mx-auto w-full max-w-[72rem] min-w-0 flex-1 px-4 pt-6 pb-10 md:px-8 md:pt-11 md:pb-14">
        <header className="min-w-0">
          <h1 className="text-2xl font-medium leading-8 tracking-[-0.01em] text-th-text-primary">{tr("설정", "Settings")}</h1>
          <p className="mt-1 text-[13px] leading-5 break-keep text-th-text-muted">
            {tr("대시보드 표시, 작업 흐름, AI 계정과 연결을 관리해요.", "Manage display, workflow, AI accounts and connections.")}
          </p>
        </header>

        <div className="mt-6 grid min-w-0 gap-6 md:mt-8 md:grid-cols-[13rem_minmax(0,1fr)] md:gap-10">
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
            className={`@container min-w-0 outline-none ${wide ? "" : "max-w-[48rem]"}`}
          >
            <div className="min-w-0">
              <h2 className="text-base font-medium leading-6 break-keep text-th-text-primary">{activeNavItem.title}</h2>
              <p className="mt-0.5 text-[13px] leading-5 break-keep text-th-text-muted">{activeNavItem.detail}</p>
            </div>
            <div className="mt-6 min-w-0">
              {loadFailed ? (
                <div className="mb-8">
                  <SettingsNotice role="alert" tone="warn" icon={<AlertTriangle size={15} />}>
                    {tr(
                      "일부 설정을 불러오지 못했어요. 불러오지 못한 항목은 숨겼으니 페이지를 새로고침해 다시 확인해 주세요.",
                      "Some settings could not be loaded and are hidden. Refresh the page to try again.",
                    )}
                  </SettingsNotice>
                </div>
              ) : null}
              {showNoMatches ? (
                <SettingsNotice role="status" icon={<SearchX size={15} />}>
                  {tr(`이 탭에는 “${query.trim()}” 검색 결과가 없어요. 남은 탭에서 찾아보세요.`, `Nothing in this tab matches “${query.trim()}”. Try one of the remaining tabs.`)}
                </SettingsNotice>
              ) : children}
            </div>
          </section>
        </div>
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
          <SettingsNotice tone="warn" icon={<AlertTriangle size={15} />}>
            {tr(
              "저장하면 진행 중인 카드의 리뷰·병합·대화 정책이 바로 달라질 수 있어요.",
              "Saving can immediately change review, merge, or conversation policy for active cards.",
            )}
          </SettingsNotice>
          <div>
            <div className="text-xs font-medium text-th-text-muted">{tr("바뀌는 설정", "Changing")}</div>
            <div className={`mt-2 px-3 py-2 text-[13px] leading-5 break-keep ${SETTINGS_TRAY_CLASS}`}>{dangerousLabels}</div>
          </div>
          <div className="flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
            <Button variant="secondary" onClick={onCancelDangerousSave}>
              {tr("취소", "Cancel")}
            </Button>
            <Button variant="primary" icon={<Check />} loading={saving} disabled={saving} onClick={onConfirmDangerousSave}>
              {saving ? tr("저장 중…", "Saving…") : tr("확인하고 저장", "Confirm and save")}
            </Button>
          </div>
        </div>
      </Modal>

      <SettingsOnboardingOverlay isKo={isKo} onClose={onCloseOnboarding} open={showOnboarding} tr={tr} />
    </div>
  );
}
