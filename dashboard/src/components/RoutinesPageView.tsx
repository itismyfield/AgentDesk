import { RoutinesTimelineWidget } from "./dashboard/RoutinesTimelineWidget";
import { useSettings } from "../contexts/SettingsContext";
import { useI18n } from "../i18n";

export default function RoutinesPageView() {
  const { settings } = useSettings();
  const { language, locale, t } = useI18n(settings.language);

  return (
    <div
      data-testid="routines-page"
      className="h-full overflow-auto px-4 pb-28 pt-4 sm:px-6 sm:pb-12 sm:pt-6"
      style={{ background: "var(--th-bg)" }}
    >
      <div className="mx-auto flex w-full max-w-6xl flex-col gap-5">
        <header>
          <p
            className="text-[11px] font-semibold uppercase tracking-[0.18em]"
            style={{ color: "var(--th-text-muted)" }}
          >
            {t({ ko: "Routines", en: "Routines" })}
          </p>
          <h1
            className="mt-2 text-2xl font-semibold tracking-normal sm:text-3xl"
            style={{ color: "var(--th-text-heading)" }}
          >
            {t({ ko: "루틴 시간표", en: "Routines Timeline" })}
          </h1>
          <p
            className="mt-2 max-w-2xl text-sm leading-6"
            style={{ color: "var(--th-text-muted)" }}
          >
            {t({
              ko: "등록된 자동 작업을 다음 실행 시간 기준으로 정리합니다.",
              en: "Registered jobs are sorted by the next scheduled run.",
            })}
          </p>
        </header>

        <RoutinesTimelineWidget
          t={t}
          localeTag={locale}
          language={language}
        />
      </div>
    </div>
  );
}
