import { BookOpen } from "lucide-react";
import type { I18nContextValue } from "../i18n";
import SkillCatalogView from "./SkillCatalogView";

interface MeetingSkillRailCardProps {
  t: I18nContextValue["t"];
}

export default function MeetingSkillRailCard({
  t,
}: MeetingSkillRailCardProps) {
  return (
    <div data-testid="meetings-page-skills" className="p-4 sm:p-5 min-w-0 overflow-hidden rounded-lg border border-th-border-subtle bg-th-bg-surface">
      <div className="flex min-w-0 items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="text-xs leading-5 text-th-text-muted">
            {t({ ko: "관련 스킬", en: "Related Skills" })}
          </div>
          <div className="mt-1 text-base font-semibold leading-6 text-th-text-heading">
            {t({ ko: "회의 후속 자동화", en: "Meeting follow-up automation" })}
          </div>
          <div className="mt-2 text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">
            {t({
              ko: "회의에서 나온 후속 액션을 실행·정리할 때 연결되는 스킬만 한곳에 모았습니다.",
              en: "Skills connected to meeting follow-up actions stay close to the meeting detail.",
            })}
          </div>
        </div>
        <div className="hidden">
          <BookOpen size={17} />
        </div>
      </div>
      <SkillCatalogView embedded />
    </div>
  );
}
