import { Search } from "lucide-react";
import { useEffect, useState, type RefObject } from "react";
import { PIPELINE_KINDS, type PipelineDefinition } from "../../api/serverCampaignPipeline";
import { Popover } from "../../kit/free/popover-clean";
import { Cap, Desc, Eyebrow, ROW_HOVER_CLASS, cn } from "../common/KitPrimitives";
import type { Tr } from "./campaignPresentation";
import { kindIcon } from "./PipelineCanvas";
import { kindName, stateName } from "./pipelineModel";

/** 상태 찾기·만들기: where a pin goes, an existing state found by what was typed or a new one of the kind chosen and
 * named by it; with no pin (the canvas tools' 상태 추가) only the new one. Enter takes the first state found. */
export default function PipelineStatePicker({ open, anchor, draft, from, onPick, onCreate, onClose, tr }: {
  open: boolean; anchor: RefObject<HTMLElement | null>; draft: PipelineDefinition; from: { name: string; label: string } | null;
  onPick: (id: string) => void; onCreate: (kind: string, title: string) => void; onClose: () => void; tr: Tr;
}) {
  const [query, setQuery] = useState("");
  useEffect(() => { if (!open) setQuery(""); }, [open]);
  const words = query.trim().toLowerCase();
  const found = from ? draft.states.filter((state) => [stateName(state, state.id, tr), state.id, kindName(state.kind, tr)].some((text) => text.toLowerCase().includes(words))) : [];
  const title = from ? tr("상태 찾기·만들기", "Find or make a state") : tr("상태 추가", "Add a state");
  return <Popover open={open} onClose={onClose} anchor={anchor} side="right" width={300} title={title}
    description={from ? tr(`${from.name} · '${from.label}' 다음에 갈 상태예요.`, `Where '${from.label}' from ${from.name} goes.`) : undefined}><div className="grid gap-2.5">
    <label className="flex min-w-0 items-center gap-2 rounded-lg border border-th-input-border bg-th-input-bg px-2.5 focus-within:outline-2 focus-within:outline-[var(--accent)]">
      <Search aria-hidden className="size-4 shrink-0 text-[var(--fg-muted)]" />
      <input aria-label={from ? tr("상태 찾기", "Find a state") : tr("새 상태 이름", "New state's name")} value={query} placeholder={from ? tr("이름으로 찾거나 새 이름", "A name to find or give") : tr("이름(없어도 돼요)", "A name (optional)")}
        className="min-[900px]:pointer-fine:!min-h-10 max-[899px]:!min-h-11 pointer-coarse:!min-h-11 min-w-0 flex-1 border-0 bg-transparent py-2 text-sm max-[899px]:!text-base pointer-coarse:!text-base text-[var(--fg)] !outline-none" onChange={(event) => setQuery(event.target.value)}
        onKeyDown={(event) => { if (event.key === "Enter" && found[0]) { event.preventDefault(); onPick(found[0].id); } }} />
    </label>
    {from && <section aria-label={tr("기존 상태 잇기", "Connect a state")} className="grid gap-1">
      <Eyebrow>{tr("기존 상태 잇기", "Connect a state")}</Eyebrow>
      {found.length ? <ul className="m-0 grid max-h-56 list-none gap-0.5 overflow-y-auto p-0">{found.map((state) => {
        const Icon = kindIcon(state.kind);
        return <li key={state.id}><button type="button" onClick={() => onPick(state.id)}
          className={cn("!m-0 flex min-[900px]:pointer-fine:!min-h-9 max-[899px]:!min-h-11 pointer-coarse:!min-h-11 w-full min-w-0 items-center gap-2 !rounded-lg !border-0 !bg-transparent px-2 text-left text-[13px] text-[var(--fg)] focus-visible:outline-2 focus-visible:outline-[var(--accent)]", ROW_HOVER_CLASS)}>
          <Icon aria-hidden className="size-4 shrink-0 text-[var(--fg-dim)]" /><span className="min-w-0 flex-1 truncate">{stateName(state, state.id, tr)}</span><Cap className="shrink-0">{kindName(state.kind, tr)}</Cap>
        </button></li>;
      })}</ul> : <Desc>{tr("맞는 상태가 없어요. 아래에서 새로 만들어요.", "No state matches. Make one below.")}</Desc>}
    </section>}
    <section aria-label={tr("새 상태 만들기", "Make a state")} className="grid gap-1.5">
      <Eyebrow>{tr("새 상태 만들기", "Make a state")}{words && <span className="font-normal normal-case"> · {query.trim()}</span>}</Eyebrow>
      <div className="flex flex-wrap gap-1.5">{PIPELINE_KINDS.map((kind) => <button key={kind} type="button" onClick={() => onCreate(kind, query)}
        className="!m-0 min-[900px]:pointer-fine:!min-h-8 max-[899px]:!min-h-11 pointer-coarse:!min-h-11 !rounded-full border border-[var(--line)] !bg-[var(--bg-2)] px-3 text-xs text-[var(--fg)] hover:!bg-[var(--bg-3)] focus-visible:outline-2 focus-visible:outline-[var(--accent)]">{kindName(kind, tr)}</button>)}</div>
    </section>
    <Cap>{tr("Esc로 닫아요.", "Esc closes.")}</Cap>
  </div></Popover>;
}
