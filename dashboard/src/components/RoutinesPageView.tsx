import { RoutinesTimelineWidget } from "./dashboard/RoutinesTimelineWidget";
import { useSettings } from "../contexts/SettingsContext";
import { useI18n } from "../i18n";

export default function RoutinesPageView() {
  const { settings } = useSettings();
  const { language, locale, t } = useI18n(settings.language);

  return (
    <div
      data-testid="routines-page"
      className="h-full overflow-auto bg-th-bg-primary px-4 py-5 sm:px-8 sm:py-7"
    >
      <div className="mx-auto flex w-full max-w-6xl flex-col gap-5">
        <header className="border-b border-th-border-subtle pb-5">
          <h1
            className="text-2xl font-semibold tracking-tight"
            style={{ color: "var(--th-text-heading)" }}
          >
            {t({ ko: "루틴", en: "Routines" })}
          </h1>
          <p
            className="mt-2 max-w-2xl text-xs leading-5"
            style={{ color: "var(--th-text-muted)" }}
          >
            {t({
              ko: "등록된 자동 작업을 다음 실행 시간 기준으로 정리합니다.",
              en: "Registered jobs are sorted by the next scheduled run.",
            })}
          </p>
        </header>

        <RoutinesTimelineWidget
          variant="home"
          t={t}
          localeTag={locale}
          language={language}
        />
      </div>
    </div>
  );
}
