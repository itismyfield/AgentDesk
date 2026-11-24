import { PageFrame } from "../app/PageFrame";
import { RoutinesTimelineWidget } from "./dashboard/RoutinesTimelineWidget";
import { useSettings } from "../contexts/SettingsContext";
import { useI18n } from "../i18n";

export default function RoutinesPageView() {
  const { settings } = useSettings();
  const { language, locale, t } = useI18n(settings.language);

  return (
    <PageFrame
      testId="routines-page"
      title={t({ ko: "루틴", en: "Routines", ja: "ルーチン", zh: "例程" })}
      description={t({
        ko: "주간·시간대별 예약 배치와 최근 실행 결과를 확인하고 필요할 때 바로 실행해요.",
        en: "Review weekly scheduled slots, latest outcomes, and trigger routines on demand.",
        ja: "週間スケジュールと直近の結果を確認し、必要に応じて即時実行します。",
        zh: "查看每周排程与最近运行结果，并可按需立即运行。",
      })}
    >
      <RoutinesTimelineWidget t={t} localeTag={locale} language={language} />
    </PageFrame>
  );
}
