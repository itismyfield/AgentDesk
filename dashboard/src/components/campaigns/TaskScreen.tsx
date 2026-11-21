import * as Dialog from "@radix-ui/react-dialog";
import { ArrowLeft, Check, Copy } from "lucide-react";
import { createContext, useCallback, useContext, useEffect, useLayoutEffect, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { Button } from "../../kit/free/button-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { Eyebrow, KitCard, Num, StageTrack, StatusDot, type DotTone, type TrackSegment } from "../common/KitPrimitives";
import { useReturnFocus } from "../common/overlay/useReturnFocus";
import { CAMPAIGN_SCOPE, type Tr } from "./campaignPresentation";

const TASK_ENTRY = "campaignTaskScreen";
// The task detail fills it in, so the screen's top bar can carry a title and number only the detail knows.
const ScreenTitle = createContext<((title: string, label: string | null) => void) | null>(null);

/** Puts the task's title (and a better number than the screen was given) in the phone screen's top bar.
 * False outside that screen, where the detail keeps its own heading. */
export function useScreenTitle(title: string, label: string | null = null): boolean {
  const setTitle = useContext(ScreenTitle);
  useLayoutEffect(() => { setTitle?.(title, label); }, [setTitle, title, label]);
  return setTitle !== null;
}

/** Phones show an open task as its own screen. Opening adds a history entry, so the back button and the browser's
 * Back both return to the list. */
export function TaskScreen({ campaign, label, onClose, tr, children }: { campaign: string; label: string; onClose: () => void; tr: Tr; children: ReactNode }) {
  const [heading, setHeading] = useState<{ title: string; label: string | null }>({ title: "", label: null });
  const setTitle = useCallback((title: string, own: string | null) => setHeading({ title, label: own }), []);
  const [copied, setCopied] = useState(false);
  const returnFocus = useReturnFocus(true);
  const close = useRef(onClose);
  close.current = onClose;
  const closing = useRef(false);
  useEffect(() => {
    // The entry this screen pushed keeps a mark, so a reload with the task open does not push a second one.
    if (!window.history.state?.[TASK_ENTRY]) window.history.pushState({ ...window.history.state, [TASK_ENTRY]: true }, "", window.location.href);
    const onPop = () => close.current();
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);
  // One Back per close: a quick second tap would otherwise leave the page as well.
  const back = () => {
    if (closing.current) return;
    closing.current = true;
    window.history.back();
  };
  const copy = () => void globalThis.navigator?.clipboard?.writeText(window.location.href).then(() => {
    setCopied(true);
    window.setTimeout(() => setCopied(false), 2000);
  }, () => undefined);
  // z-60: over the app's bars (45 and below), under the sheets and dialogs it opens (70/80).
  return <Dialog.Root open onOpenChange={(open) => { if (!open) back(); }}>
    <Dialog.Portal>
      <Dialog.Content aria-describedby={undefined} onCloseAutoFocus={returnFocus} className="fixed inset-0 z-[60] overflow-y-auto overscroll-contain bg-[var(--bg)] text-[var(--fg)] outline-none"
        style={{ paddingBottom: "env(safe-area-inset-bottom)" }}>
        <div className={`${CAMPAIGN_SCOPE} campaign-inspector !block min-w-0 text-sm`}>
          <header className="sticky top-0 z-10 flex h-14 min-w-0 items-center gap-1 border-b border-[var(--line-soft)] bg-[var(--bg)] px-2">
            <IconButton icon={<ArrowLeft aria-hidden />} label={tr("목록으로", "Back to list")} onClick={back} className="!size-11 shrink-0" />
            <div className="grid min-w-0 flex-1">
              <Dialog.Title className="!m-0 truncate !text-[15px] !font-semibold !leading-5 !tracking-normal text-[var(--fg)]">{heading.title}</Dialog.Title>
              <span className="flex min-w-0 gap-1 text-xs tabular-nums text-[var(--fg-muted)]"><span className="truncate">{campaign}</span> <span className="shrink-0">· {heading.label ?? label}</span></span>
            </div>
            <IconButton icon={copied ? <Check aria-hidden /> : <Copy aria-hidden />} label={copied ? tr("링크를 복사했어요", "Link copied") : tr("링크 복사", "Copy link")}
              onClick={copy} className="!size-11 shrink-0" />
          </header>
          <div className="grid min-w-0 gap-3.5 p-4 [overflow-wrap:anywhere]">
            <ScreenTitle.Provider value={setTitle}>{children}</ScreenTitle.Provider>
          </div>
        </div>
      </Dialog.Content>
    </Dialog.Portal>
  </Dialog.Root>;
}

const WARN_CARD_STYLE = {
  borderColor: "color-mix(in srgb, var(--warn) 45%, var(--line))",
  background: "color-mix(in srgb, var(--warn) 8%, var(--bg-2))",
} as CSSProperties;

/** Where the task stands: a `KitCard` surface with optional warning tint for gates and blockers. */
export function StatusCard({ label, warn = false, children }: { label: string; warn?: boolean; children: ReactNode }) {
  return <KitCard as="section" aria-label={label} pad="sm" style={warn ? WARN_CARD_STYLE : undefined}>
    <div className="grid min-w-0 justify-items-start gap-1.5 text-[13px] leading-[18px] text-[var(--fg-muted)] [&_p]:!text-[13px] [&_p]:!leading-[18px]">
      {children}
    </div>
  </KitCard>;
}

export interface Step { key: string; label: string; state: "done" | "now" | "next"; value?: string; tone?: "warn" | "err" }
const STEP_DOT: Record<Step["state"] | NonNullable<Step["tone"]>, DotTone> = { done: "ok", now: "info", next: "neutral", warn: "warn", err: "err" };

/** 단계 rows: stage track, finished steps, the current one in bold, then the steps still to come in one muted row. */
export function StepList({ steps, tr }: { steps: Step[]; tr: Tr }) {
  const later = steps.filter((step) => step.state === "next");
  const rows: Step[] = [...steps.filter((step) => step.state !== "next"), ...later.length ? [{ key: "later", label: later.map((step) => step.label).join(" · "), state: "next" as const }] : []];
  const segments: TrackSegment[] = steps.map((step) => step.state === "done" ? "done" : step.state === "now" ? (step.tone === "err" ? "fail" : step.tone === "warn" ? "wait" : "now") : "todo");
  const words = { done: tr("끝남", "done"), now: tr("지금", "now"), next: tr("대기", "waiting") };
  return <section aria-label={tr("단계", "Stages")} className="grid min-w-0 gap-2">
    <Eyebrow>{tr("단계", "Stages")}</Eyebrow>
    <StageTrack segments={segments} />
    <ol className="!m-0 grid !list-none !p-0 !text-sm">
      {rows.map((step) => <li key={step.key} aria-current={step.state === "now" ? "step" : undefined}
        className={`flex min-h-8 min-w-0 items-center gap-2.5 !whitespace-normal ${step.state === "next" ? "text-[var(--fg-faint)]" : "text-[var(--fg)]"}`}>
        <StatusDot tone={STEP_DOT[step.tone ?? step.state]} />
        <span className="sr-only">{words[step.state]}</span>
        <span className={`min-w-0 flex-1 ${step.state === "now" ? "font-semibold" : ""}`}>{step.label}</span>
        {step.value && <Num className={`shrink-0 text-xs ${step.tone === "warn" ? "text-[var(--warn)]" : step.tone === "err" ? "text-[var(--err)]" : "text-[var(--fg-muted)]"}`}>{step.value}</Num>}
      </li>)}
    </ol>
  </section>;
}

const PREVIEW = 3;
const isLong = (text: string) => text.length > 160 || text.split("\n").length > 3;

/** The newest three entries, each cut to three lines, until the reader opens all of them in full. */
export function RecentList({ label, items, tr }: { label: string; items: string[]; tr: Tr }) {
  const [all, setAll] = useState(false);
  const newest = [...items].reverse();
  const shown = all ? newest : newest.slice(0, PREVIEW);
  const hidden = newest.length - shown.length;
  return <section aria-label={label} className="grid min-w-0 gap-2">
    <Eyebrow>{label}</Eyebrow>
    <ul className="!m-0 grid gap-2 !list-none !p-0">
      {shown.map((text, index) => <li key={index} className={`border-l-2 border-[var(--line)] pl-3 !text-xs leading-[18px] text-[var(--fg)] ${all ? "" : "line-clamp-3"}`}>{text}</li>)}
    </ul>
    {(newest.length > PREVIEW || newest.some(isLong)) && <Button variant="ghost" size="sm" className="justify-self-start" aria-expanded={all} onClick={() => setAll(!all)}>
      {all ? tr("접기", "Show less") : hidden ? tr(`${hidden}개 더 보기`, `Show ${hidden} more`) : tr("전체 보기", "Show in full")}
    </Button>}
  </section>;
}
