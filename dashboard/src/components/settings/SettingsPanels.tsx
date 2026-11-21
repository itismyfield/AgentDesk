import { useQuery } from "@tanstack/react-query";
import { RefreshCw } from "lucide-react";
import { getSettingsChannelName } from "../../api/settingsRuntime";
import { Select } from "../../kit/free/select-clean";
import { FreshnessIndicator } from "../common/FreshnessIndicator";
import { Cap, Chip, Desc, Eyebrow, KeyValue, Num, ROW_HOVER_CLASS, RuleItem, StatusDot, Well, toneText, type DotTone } from "../common/KitPrimitives";
import { STALE_AFTER_MS } from "../OpsPageModel";
import { applyChip, applyText, defaultText, formatSettingValue, withRo, type CatalogItem } from "./SettingsModel";
import { SELECT_TOKEN_CLASS, SettingsAction, SettingsSwitch, SettingsTextField } from "./SettingsPrimitives";

// Pieces shared by the desktop catalog and the phone sheet: the row control, its chip and the detail lists.

type Tr = (ko: string, en: string) => string;

const DISCORD_ID_PATTERN = /^\d{17,20}$/;

export const editableItem = (item: CatalogItem) => ["toggle", "number", "text", "choice"].includes(item.input);

/** Option picker for "choice" rows: four languages do not fit a row as segments, so rows and the sheet use the kit Select. */
export function ChoiceControl({
  item,
  value,
  label,
  size,
  className,
  disabled,
  onChange,
}: {
  item: CatalogItem;
  value: string;
  label: string;
  size: "sm" | "md" | "lg";
  className: string;
  disabled?: boolean;
  onChange: (value: string) => void;
}) {
  return (
    <Select
      aria-label={label}
      size={size}
      className={`${SELECT_TOKEN_CLASS} ${className}`}
      value={value}
      disabled={disabled}
      onChange={onChange}
      options={item.options ?? []}
    />
  );
}

/** "기본값 3으로 되돌리기" (short: "기본값 3"), or the inherit wording when reset stores "". */
export function resetLabel(item: CatalogItem, tr: Tr, short = false): string {
  if (item.resetTo === "" && item.baseline === null) {
    return short ? tr("공통 값 따르기", "Use shared value") : tr("공통 값을 따르게 되돌리기", "Follow the shared value again");
  }
  const value = formatSettingValue(item.baseline ?? item.resetTo ?? "", item, tr);
  return short ? tr(`기본값 ${value}`, `Default ${value}`) : tr(`기본값 ${withRo(value)} 되돌리기`, `Reset to default (${value})`);
}

/** The value the page currently holds for a row: its draft, else the stored value. */
export const currentValue = (item: CatalogItem) => item.draft ?? item.saved;

const STATUS_DOT_TONE: Record<"ok" | "warn" | "unknown", DotTone> = {
  ok: "ok",
  warn: "warn",
  unknown: "neutral",
};

/** Chips for an unsaved change, an override and how the value applies; the phone row keeps only the first. */
export function ItemChip({ item, tr, all = false }: { item: CatalogItem; tr: Tr; all?: boolean }) {
  const apply = applyChip(item, tr);
  const pillClass = all ? "min-w-[76px] justify-center" : undefined;
  const chips = [
    item.draft !== null ? <Chip key="draft" tone="warn" className={pillClass}>{tr("바뀜", "Changed")}</Chip> : null,
    item.overridden ? <Chip key="override" tone="accent" className={pillClass}>{tr("덮어씀", "Overridden")}</Chip> : null,
    apply ? <Chip key="apply" tone="neutral" className={pillClass}>{apply}</Chip> : null,
  ].filter(Boolean);
  return <>{all ? chips : chips.slice(0, 1)}</>;
}

/** Plain value for read-only and status rows; status rows get their tone dot. */
export function ItemValue({ item, tr }: { item: CatalogItem; tr: Tr }) {
  const dotTone = item.status ? STATUS_DOT_TONE[item.status.tone] : "neutral";
  const color = item.status
    ? toneText(dotTone) ?? (dotTone === "ok" ? "var(--ok)" : "var(--fg-muted)")
    : "var(--fg-dim)";
  return (
    <Num className="inline-flex min-w-0 items-center gap-1.5 text-[13px] leading-5 break-keep" style={{ color }}>
      {item.input === "status" ? <StatusDot tone={dotTone} /> : null}
      {formatSettingValue(item.saved, item, tr)}
    </Num>
  );
}

interface InlineControlProps {
  item: CatalogItem;
  label: string;
  invalid?: boolean;
  disabled?: boolean;
  onChange: (value: string) => void;
  tr: Tr;
}

/** The row's own control: a switch, a number or text field, or the plain value when it cannot be edited here. */
export function InlineControl({ item, label, invalid, disabled, onChange, tr }: InlineControlProps) {
  const value = currentValue(item);
  if (item.input === "toggle") {
    return <SettingsSwitch aria-label={label} checked={value === "true"} disabled={disabled} onChange={(event) => onChange(String(event.target.checked))} />;
  }
  if (item.input === "choice") {
    return <ChoiceControl item={item} value={value} label={label} size="md" className="w-44" disabled={disabled} onChange={onChange} />;
  }
  if (item.input === "number" || item.input === "text") {
    const numeric = item.input === "number";
    return (
      <SettingsTextField
        size="md"
        aria-label={label}
        aria-invalid={invalid || undefined}
        type={numeric ? "number" : "text"}
        inputMode={numeric ? "numeric" : undefined}
        min={item.min}
        max={item.max}
        value={value}
        disabled={disabled}
        className={numeric ? "w-[88px]" : "w-44"}
        inputClassName={numeric ? "text-right font-display tabular-nums" : "font-mono"}
        onChange={(event) => onChange(event.target.value)}
      />
    );
  }
  return <ItemValue item={item} tr={tr} />;
}

function ChannelName({ id, tr }: { id: string; tr: Tr }) {
  const valid = DISCORD_ID_PATTERN.test(id);
  const channel = useQuery({
    queryKey: ["settings", "discord-channel", id],
    queryFn: () => getSettingsChannelName(id),
    enabled: valid,
    staleTime: 5 * 60_000,
    retry: false,
  });
  if (!id) return <>{tr("정하지 않음", "Not set")}</>;
  if (!valid) return <span className="text-[var(--warn)]">{tr("17~20자리 숫자가 아니에요", "Not a 17–20 digit ID")}</span>;
  if (channel.isPending) return <>{tr("확인 중…", "Checking…")}</>;
  if (channel.isError || !channel.data) return <span className="text-[var(--warn)]">{tr("채널을 찾지 못했어요", "Channel not found")}</span>;
  return <>#{channel.data}</>;
}

interface ItemFactsProps {
  item: CatalogItem;
  /** Value being edited when it differs from the draft (the phone sheet). */
  pending?: string | null;
  problem?: string | null;
  tr: Tr;
}

const FACT_VALUE_CLASS = "items-baseline whitespace-normal [overflow-wrap:anywhere]";

/** Stored, pending and default values with when they apply; status rows list their facts, signals and setup steps. */
export function ItemFacts({ item, pending = item.draft, problem, tr }: ItemFactsProps) {
  const apply = applyText(item, tr);
  return (
    <div className="flex min-w-0 flex-col gap-3" data-testid={`settings-facts-${item.key}`}>
      <dl className="m-0 flex min-w-0 flex-col gap-2">
        {item.source === "status" ? (
          item.status?.facts.map(([label, value, note]) => (
            <KeyValue key={label} dl label={label} num className="items-baseline" valueClassName={FACT_VALUE_CLASS}>
              {value}
              {note ? <Cap className="mt-0.5 block font-sans">{note}</Cap> : null}
            </KeyValue>
          ))
        ) : (
          <>
            <KeyValue dl label={tr("저장된 값", "Saved")} num className="items-baseline" valueClassName={FACT_VALUE_CLASS}>
              {formatSettingValue(item.saved, item, tr)}
            </KeyValue>
            {pending !== null && pending !== item.saved ? (
              <KeyValue dl label={tr("고칠 값", "New value")} tone="warn" num className="items-baseline" valueClassName={FACT_VALUE_CLASS}>
                {formatSettingValue(pending, item, tr)}
              </KeyValue>
            ) : null}
            <KeyValue dl label={tr("기본값", "Default")} num className="items-baseline" valueClassName={FACT_VALUE_CLASS}>
              {defaultText(item, tr)}
            </KeyValue>
          </>
        )}
        {apply ? (
          <KeyValue dl label={tr("적용", "Applies")} className="items-baseline" valueClassName={FACT_VALUE_CLASS}>
            {apply}
          </KeyValue>
        ) : null}
        {item.key.endsWith("_channel_id") ? (
          <KeyValue dl label={tr("채널", "Channel")} className="items-baseline" valueClassName={FACT_VALUE_CLASS}>
            <ChannelName id={(pending ?? item.saved).trim()} tr={tr} />
          </KeyValue>
        ) : null}
      </dl>
      {problem ? <p role="alert" className="m-0 text-xs leading-5 text-[var(--err)]">{problem}</p> : null}
      {item.status?.updatedAt || item.status?.refresh ? (
        <div className="flex min-w-0 flex-wrap items-center justify-between gap-2">
          {item.status.updatedAt ? (
            <FreshnessIndicator
              timestamp={item.status.updatedAt}
              label={tr("업데이트", "Updated")}
              staleAfterSeconds={STALE_AFTER_MS / 1000}
              criticalAfterSeconds={(STALE_AFTER_MS * 3) / 1000}
            />
          ) : (
            <span />
          )}
          {item.status.refresh ? (
            <SettingsAction icon={<RefreshCw />} disabled={item.status.refresh.busy} onClick={item.status.refresh.run}>
              {tr("다시 확인", "Check again")}
            </SettingsAction>
          ) : null}
        </div>
      ) : null}
      {item.status?.failed ? (
        <p role="alert" className="m-0 text-xs leading-5 text-[var(--warn)]">
          {tr("최근 확인이 실패해서 마지막으로 받은 값을 보여 줘요.", "The latest check failed. Showing the last values received.")}
        </p>
      ) : null}
      {item.status?.notes?.length ? (
        <ul className="m-0 flex list-none flex-col gap-2 p-0" data-testid="settings-status-notes">
          {item.status.notes.map((note, index, notes) => (
            <li key={`${note.tooltip}-${note.title}`} className="min-w-0">
              <RuleItem tone={note.tone === "danger" ? "err" : "warn"}>
                <span className="flex justify-between gap-3">
                  <span className="text-[var(--fg)]" title={note.tooltip}>{note.title}</span>
                  {note.count !== undefined ? <Num className="shrink-0 text-[var(--fg-dim)]">{note.count.toLocaleString()}</Num> : null}
                </span>
                {/* Server-reported reasons share one explanation; say it once. */}
                {note.detail !== notes[index - 1]?.detail ? <Desc className="mt-0.5 block">{note.detail}</Desc> : null}
              </RuleItem>
            </li>
          ))}
        </ul>
      ) : null}
      {item.status?.steps ? (
        <div className="min-w-0">
          <Eyebrow>{tr("설정 방법", "How to set up")}</Eyebrow>
          <ul className="m-0 mt-1.5 flex list-none flex-col gap-1.5 p-0">
            {item.status.steps.lines.map((line) => (
              <li key={line}>
                <Well
                  lang={item.status?.steps?.translated ? undefined : "en"}
                  className={`rounded-[var(--r-sm)] px-3 py-2 text-xs leading-5 [overflow-wrap:anywhere] ${item.status?.steps?.translated ? "break-keep" : "font-mono"}`}
                >
                  {line}
                </Well>
              </li>
            ))}
          </ul>
        </div>
      ) : null}
    </div>
  );
}

export function hasRelatedSettings(item: CatalogItem, items: CatalogItem[]): boolean {
  return item.related.some((link) => items.some((candidate) => candidate.key === link.key));
}

/** "함께 보는 설정": linked rows with the note that ties them, each opening that row. */
export function RelatedSettings({
  item,
  items,
  onSelect,
  disabled,
  tr,
}: {
  item: CatalogItem;
  items: CatalogItem[];
  onSelect: (key: string) => void;
  disabled?: boolean;
  tr: Tr;
}) {
  const links = item.related
    .map((link) => ({ link, target: items.find((candidate) => candidate.key === link.key) }))
    .filter((entry): entry is { link: CatalogItem["related"][number]; target: CatalogItem } => Boolean(entry.target));
  if (links.length === 0) return null;
  return (
    <div className="min-w-0">
      <Eyebrow>{tr("함께 보는 설정", "Related settings")}</Eyebrow>
      <ul className="m-0 mt-1.5 flex list-none flex-col gap-1 p-0">
        {links.map(({ link, target }) => (
          <li key={link.key}>
            <button
              type="button"
              disabled={disabled}
              onClick={() => onSelect(target.key)}
              className={`-mx-1.5 flex min-h-11 w-[calc(100%+0.75rem)] min-w-0 items-center rounded-[var(--r-sm)] px-1.5 py-1 text-left ${ROW_HOVER_CLASS} disabled:cursor-not-allowed disabled:opacity-50 sm:min-h-9`}
            >
              <RuleItem tone="neutral" className="w-full">
                <span className="block text-[var(--fg)]">{target.label}</span>
                {link.noteKo ? <Desc className="block">{tr(link.noteKo, link.noteEn ?? link.noteKo)}</Desc> : null}
              </RuleItem>
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}

/** Category, name, mono key and description: the head of the inspector and the phone sheet. */
export function ItemHeading({ item, category, showTitle = true }: { item: CatalogItem; category?: string; showTitle?: boolean }) {
  return (
    <div className="flex min-w-0 flex-col gap-0.5">
      {category ? <Cap>{category}</Cap> : null}
      {showTitle ? <h2 className="m-0 text-base leading-6 font-semibold break-keep text-[var(--fg)]">{item.label}</h2> : null}
      <Cap className="font-mono leading-[18px] [overflow-wrap:anywhere]">{item.key}</Cap>
      {/* Leave the description blank until a verified server description is available. */}
      {item.description ? <Desc className="mt-1 block break-keep">{item.description}</Desc> : null}
    </div>
  );
}
