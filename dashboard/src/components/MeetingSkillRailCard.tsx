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
    <div data-testid="meetings-page-skills" className="skill-rail card">
      <div className="section-head">
        <div className="min-w-0">
          <div className="section-kicker">
            {t({ ko: "관련 스킬", en: "Related Skills" })}
          </div>
          <div className="section-title">
            {t({ ko: "회의 후속 자동화", en: "Meeting follow-up automation" })}
          </div>
          <div className="section-copy">
            {t({
              ko: "회의에서 나온 후속 액션을 실행·정리할 때 연결되는 스킬만 한곳에 모았습니다.",
              en: "Skills connected to meeting follow-up actions stay close to the meeting detail.",
            })}
          </div>
        </div>
        <div className="section-icon">
          <BookOpen size={17} />
        </div>
      </div>
      <SkillCatalogView embedded />
    </div>
  );
}
