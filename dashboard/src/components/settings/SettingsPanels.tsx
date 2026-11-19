import { useQuery } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { getSettingsChannelName } from "../../api/settingsRuntime";
import { Switch } from "../../kit/free/switch-clean";
import { applyChip, applyText, defaultText, formatSettingValue, type CatalogItem, type StatusTone } from "./SettingsModel";
import { SETTINGS_TRAY_CLASS, SettingsBadge, SettingsTextField } from "./SettingsPrimitives";

// Pieces shared by the desktop catalog and the phone sheet: the row control, its chip and the detail lists.

type Tr = (ko: string, en: string) => string;

const DISCORD_ID_PATTERN = /^\d{17,20}$/;

export const editableItem = (item: CatalogItem) => item.input === "toggle" || item.input === "number" || item.input === "text";

/** The value the page currently holds for a row: its draft, else the stored value. */
export const currentValue = (item: CatalogItem) => item.draft ?? item.saved;

const TONE_TEXT: Record<StatusTone, string> = {
  ok: "text-[var(--th-badge-emerald-text)]",
  warn: "text-[var(--th-badge-amber-text)]",
  unknown: "text-th-text-muted",
};

/** Right-hand chip: an unsaved change, an override, or how the value applies. Status rows show none. */
export function ItemChip({ item, tr }: { item: CatalogItem; tr: Tr }) {
  if (item.draft !== null) return <SettingsBadge tone="warn">{tr("바뀜", "Changed")}</SettingsBadge>;
  if (item.overridden) return <SettingsBadge tone="accent">{tr("덮어씀", "Overridden")}</SettingsBadge>;
  const apply = applyChip(item, tr);
  return apply ? <SettingsBadge>{apply}</SettingsBadge> : null;
}

/** Plain value for read-only and status rows; status rows get their tone dot. */
export function ItemValue({ item, tr }: { item: CatalogItem; tr: Tr }) {
  const tone = item.status ? TONE_TEXT[item.status.tone] : "text-th-text-secondary";
  return (
    <span className={`inline-flex min-w-0 items-center gap-1.5 text-[13px] leading-5 tabular-nums break-keep ${tone}`}>
      {item.input === "status" ? <span aria-hidden className="size-2 shrink-0 rounded-full bg-current" /> : null}
      {formatSettingValue(item.saved, item, tr)}
    </span>
  );
}

interface InlineControlProps {
  item: CatalogItem;
  label: string;
  invalid?: boolean;
  onChange: (value: string) => void;
  tr: Tr;
}

/** The row's own control: a switch, a number or text field, or the plain value when it cannot be edited here. */
export function InlineControl({ item, label, invalid, onChange, tr }: InlineControlProps) {
  const value = currentValue(item);
  if (item.input === "toggle") {
    return <Switch aria-label={label} checked={value === "true"} onChange={(event) => onChange(String(event.target.checked))} />;
  }
  if (item.input === "number" || item.input === "text") {
    const numeric = item.input === "number";
    return (
      <SettingsTextField
        size="sm"
        aria-label={label}
        aria-invalid={invalid || undefined}
        type={numeric ? "number" : "text"}
        inputMode={numeric ? "numeric" : undefined}
        min={item.min}
        max={item.max}
        value={value}
        className={numeric ? "w-24" : "w-44"}
        inputClassName={numeric ? "text-right tabular-nums" : "font-mono"}
        onChange={(event) => onChange(event.target.value)}
      />
    );
  }
  return <ItemValue item={item} tr={tr} />;
}

function FactRow({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex min-w-0 items-baseline justify-between gap-4 text-[13px] leading-5">
      <dt className="shrink-0 text-th-text-muted">{label}</dt>
      <dd className="m-0 min-w-0 text-right tabular-nums text-th-text-primary [overflow-wrap:anywhere]">{children}</dd>
    </div>
  );
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
  if (!valid) return <span className="text-[var(--th-badge-amber-text)]">{tr("17~20자리 숫자가 아니에요", "Not a 17–20 digit ID")}</span>;
  if (channel.isPending) return <>{tr("확인 중…", "Checking…")}</>;
  if (channel.isError || !channel.data) return <span className="text-[var(--th-badge-amber-text)]">{tr("채널을 찾지 못했어요", "Channel not found")}</span>;
  return <>#{channel.data}</>;
}

interface ItemFactsProps {
  item: CatalogItem;
  /** Value being edited when it differs from the draft (the phone sheet). */
  pending?: string | null;
  problem?: string | null;
  tr: Tr;
}

/** Stored, pending and default values with when they apply; status rows list their facts, signals and setup steps. */
export function ItemFacts({ item, pending = item.draft, problem, tr }: ItemFactsProps) {
  const apply = applyText(item, tr);
  return (
    <div className="flex min-w-0 flex-col gap-3" data-testid={`settings-facts-${item.key}`}>
      <dl className="m-0 flex min-w-0 flex-col gap-1.5">
        {item.source === "status" ? item.status?.facts.map(([label, value]) => <FactRow key={label} label={label}>{value}</FactRow>) : (
          <>
            <FactRow label={tr("저장된 값", "Saved")}>{formatSettingValue(item.saved, item, tr)}</FactRow>
            {pending !== null && pending !== item.saved ? (
              <FactRow label={tr("고칠 값", "New value")}>
                <span className="text-[var(--warn)]">{formatSettingValue(pending, item, tr)}</span>
              </FactRow>
            ) : null}
            <FactRow label={tr("기본값", "Default")}>{defaultText(item, tr)}</FactRow>
          </>
        )}
        {apply ? <FactRow label={tr("적용", "Applies")}>{apply}</FactRow> : null}
        {item.key.endsWith("_channel_id") ? (
          <FactRow label={tr("채널", "Channel")}><ChannelName id={(pending ?? item.saved).trim()} tr={tr} /></FactRow>
        ) : null}
      </dl>
      {problem ? <p role="alert" className="m-0 text-xs leading-5 text-[var(--err)]">{problem}</p> : null}
      {item.status?.notes?.length ? (
        <ul className="m-0 flex list-none flex-col gap-2 p-0" data-testid="settings-status-notes">
          {item.status.notes.map((note, index, notes) => (
            <li key={`${note.tooltip}-${note.title}`} className="flex min-w-0 gap-2 text-[13px] leading-5">
              <span aria-hidden className={`mt-1.5 size-2 shrink-0 rounded-full ${note.tone === "danger" ? "bg-[var(--err)]" : "bg-[var(--warn)]"}`} />
              <span className="min-w-0">
                <span className="block text-th-text-primary" title={note.tooltip}>{note.title}</span>
                {/* Server-reported reasons share one explanation; say it once. */}
                {note.detail !== notes[index - 1]?.detail ? <span className="block text-xs text-th-text-muted">{note.detail}</span> : null}
              </span>
            </li>
          ))}
        </ul>
      ) : null}
      {item.status?.steps ? (
        <div className="min-w-0">
          <div className="text-xs font-medium text-th-text-muted">{tr("설정 방법", "How to set up")}</div>
          <ul className="m-0 mt-1.5 flex list-none flex-col gap-1.5 p-0">
            {item.status.steps.lines.map((line) => (
              <li
                key={line}
                lang={item.status?.steps?.translated ? undefined : "en"}
                className={`px-3 py-2 text-xs leading-5 [overflow-wrap:anywhere] ${SETTINGS_TRAY_CLASS} ${item.status?.steps?.translated ? "break-keep" : "font-mono"}`}
              >
                {line}
              </li>
            ))}
          </ul>
        </div>
      ) : null}
    </div>
  );
}

/** "함께 보는 설정": linked rows with the note that ties them, each opening that row. */
export function RelatedSettings({ item, items, onSelect, tr }: { item: CatalogItem; items: CatalogItem[]; onSelect: (key: string) => void; tr: Tr }) {
  const links = item.related
    .map((link) => ({ link, target: items.find((candidate) => candidate.key === link.key) }))
    .filter((entry): entry is { link: CatalogItem["related"][number]; target: CatalogItem } => Boolean(entry.target));
  if (links.length === 0) return null;
  return (
    <div className="min-w-0">
      <div className="text-xs font-medium text-th-text-muted">{tr("함께 보는 설정", "Related settings")}</div>
      <ul className="m-0 mt-1 flex list-none flex-col p-0">
        {links.map(({ link, target }) => (
          <li key={link.key}>
            <button
              type="button"
              onClick={() => onSelect(target.key)}
              className="flex min-h-11 w-full min-w-0 items-start gap-2 rounded-[var(--r-sm)] px-1 py-1.5 text-left hover:bg-surface-subtle sm:min-h-9"
            >
              <span aria-hidden className="mt-1.5 size-2 shrink-0 rounded-full bg-[var(--accent)]" />
              <span className="min-w-0 text-[13px] leading-5">
                <span className="block text-th-text-primary">{target.label}</span>
                {link.noteKo ? <span className="block text-xs text-th-text-muted">{tr(link.noteKo, link.noteEn ?? link.noteKo)}</span> : null}
              </span>
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
    <div className="min-w-0">
      {category ? <div className="text-xs text-th-text-muted">{category}</div> : null}
      {showTitle ? <h2 className="m-0 text-base leading-6 font-semibold text-th-text-heading break-keep">{item.label}</h2> : null}
      <div className="font-mono text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">{item.key}</div>
      {/* No verified description yet means a blank line, not a guess (#8249 brings server meta). */}
      {item.description ? <p className="m-0 mt-1.5 text-[13px] leading-5 text-th-text-secondary break-keep">{item.description}</p> : null}
    </div>
  );
}
