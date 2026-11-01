import type { UiLanguage } from "../../i18n";

export type Locale = UiLanguage;
export type TFunction = (messages: Record<Locale, string>) => string;
