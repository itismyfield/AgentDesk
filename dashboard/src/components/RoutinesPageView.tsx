import { RoutinesTimelineWidget } from "./dashboard/RoutinesTimelineWidget";
import { useSettings } from "../contexts/SettingsContext";
import { useI18n } from "../i18n";

export default function RoutinesPageView() {
  const { settings } = useSettings();
  const { language, locale, t } = useI18n(settings.language);

  return (
    <div data-testid="routines-page" className="h-full overflow-auto bg-[var(--bg)] px-4 pb-10 pt-5 sm:px-8 sm:pt-7">
      <div className="mx-auto flex w-full max-w-6xl flex-col gap-5">
        <header className="flex flex-col gap-1.5">
          <h1 className="m-0 text-2xl font-semibold tracking-tight text-[var(--fg)]">{t({ ko: "루틴", en: "Routines" })}</h1>
          <p className="m-0 max-w-2xl text-xs leading-5 text-[var(--fg-muted)] [word-break:keep-all]">
            {t({
              ko: "예약된 자동 작업과 최근 결과를 봐요. 행 오른쪽 말풍선을 누르면 디스코드 스레드가 열려요.",
              en: "Scheduled jobs and their latest results. The bubble on each row opens its Discord thread.",
            })}
          </p>
        </header>

        <RoutinesTimelineWidget t={t} localeTag={locale} language={language} />
      </div>
    </div>
  );
}
