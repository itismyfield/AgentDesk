import { Plus } from "lucide-react";
import type { I18nContextValue } from "../i18n";
import {
  SurfaceActionButton,
  SurfaceMetricPill,
  SurfaceNotice,
  SurfaceSection,
} from "./common/SurfacePrimitives";

interface MeetingOverviewSectionProps {
  t: I18nContextValue["t"];
  embedded: boolean;
  meetingsCount: number;
  showStartForm: boolean;
  activeMeetingCount: number;
  completedMeetingCount: number;
  unresolvedIssueCount: number;
  onToggleStartForm: () => void;
}

export default function MeetingOverviewSection({
  t,
  embedded,
  meetingsCount,
  showStartForm,
  activeMeetingCount,
  completedMeetingCount,
  unresolvedIssueCount,
  onToggleStartForm,
}: MeetingOverviewSectionProps) {
  return (
    <SurfaceSection
      eyebrow={t({ ko: "Round Table", en: "Round Table" })}
      title={t({ ko: "회의 기록", en: "Meeting Records" })}
      description={
        embedded
          ? undefined
          : t({
              ko: "라운드 테이블 상세, 교차검증 흐름, 후속 일감 상태를 한 화면에서 관리합니다.",
              en: "Manage round-table detail, cross-review flow, and follow-up issue state in one place.",
            })
      }
      badge={t({
        ko: `${meetingsCount}개 기록`,
        en: `${meetingsCount} records`,
      })}
      actions={
        <SurfaceActionButton
          tone={showStartForm ? "neutral" : "accent"}
          onClick={onToggleStartForm}
        >
          <span className="inline-flex items-center gap-1.5">
            <Plus size={14} />
            {showStartForm
              ? t({ ko: "입력 닫기", en: "Close Form" })
              : t({ ko: "새 회의", en: "New Meeting" })}
          </span>
        </SurfaceActionButton>
      }
    >
      <div className="mt-4 flex flex-wrap gap-3">
        <SurfaceMetricPill
          label={t({ ko: "활성 회의", en: "Active Meetings" })}
          value={t({
            ko: `${activeMeetingCount}건 진행 중`,
            en: `${activeMeetingCount} in progress`,
          })}
          tone={activeMeetingCount > 0 ? "accent" : "neutral"}
        />
        <SurfaceMetricPill
          label={t({ ko: "완료 기록", en: "Completed" })}
          value={t({
            ko: `${completedMeetingCount}건`,
            en: `${completedMeetingCount} records`,
          })}
          tone="success"
        />
        <SurfaceMetricPill
          label={t({ ko: "후속 정리", en: "Follow-ups" })}
          value={t({
            ko: `${unresolvedIssueCount}건 미해결`,
            en: `${unresolvedIssueCount} unresolved`,
          })}
          tone={unresolvedIssueCount > 0 ? "warn" : "info"}
        />
      </div>

      {(!embedded || unresolvedIssueCount > 0) && (
        <SurfaceNotice
          className="mt-4"
          tone={unresolvedIssueCount > 0 ? "warn" : "info"}
        >
          <div className="text-sm leading-6">
            {unresolvedIssueCount > 0
              ? t({
                  ko: `생성 대기 또는 실패한 후속 일감 ${unresolvedIssueCount}건을 이 화면에서 바로 정리할 수 있습니다.`,
                  en: `You can resolve ${unresolvedIssueCount} pending or failed follow-up issues directly from this screen.`,
                })
              : t({
                  ko: "현재 미해결 후속 일감이 없습니다. 새 라운드 테이블을 시작하거나 최근 회의 흐름을 검토하세요.",
                  en: "There are no unresolved follow-up issues. Start a new round table or review recent meeting flow.",
                })}
          </div>
        </SurfaceNotice>
      )}
    </SurfaceSection>
  );
}
