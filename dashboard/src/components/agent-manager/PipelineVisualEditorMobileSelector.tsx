import { fsmStateTone, transitionAccent } from "./pipeline-visual-editor-styles";
import {
  FSM_DETAIL_PANEL_STYLE,
  MUTED_TEXT_STYLE,
} from "./pipeline-visual-editor-ui";

interface Props {
  ctx: any;
  actions: any;
}

export default function PipelineVisualEditorMobileSelector({ ctx, actions }: Props) {
  const tr = ctx.tr;

  return (
    <div
      className="fsm-stack-mobile rounded-[20px] border p-3 space-y-4"
      style={FSM_DETAIL_PANEL_STYLE}
      data-testid="fsm-mobile-selector"
    >
      <div className="space-y-1">
        <div
          className="text-[11px] font-semibold uppercase tracking-[0.18em]"
          style={{
            ...MUTED_TEXT_STYLE,
            fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
          }}
        >
          {tr("모바일 빠른 편집", "Mobile quick edit")}
        </div>
        <p className="text-xs leading-5" style={MUTED_TEXT_STYLE}>
          {tr(
            "모바일에서는 그래프를 직접 누르기보다 아래 목록에서 전환/상태를 고른 뒤 바로 편집합니다.",
            "On mobile, pick a transition or state from the list below instead of targeting the graph directly.",
          )}
        </p>
      </div>

      <div className="space-y-2">
        <div className="flex items-center justify-between gap-2">
          <h5 className="text-xs font-semibold uppercase tracking-wider" style={MUTED_TEXT_STYLE}>
            {tr("전환", "Transitions")}
          </h5>
          <span className="text-[11px]" style={MUTED_TEXT_STYLE}>
            {`${ctx.fsmQuickTransitions.length}`}
          </span>
        </div>
        <div className="grid gap-2">
          {ctx.fsmQuickTransitions.map((transition: any) => {
            const accent = transitionAccent(transition.type);
            const isSelected = ctx.selection?.kind === "transition" && ctx.selection.index === transition.index;
            return (
              <button
                key={`${transition.from}-${transition.to}-${transition.index}`}
                type="button"
                onClick={() => actions.setSelection({ kind: "transition", index: transition.index })}
                aria-pressed={isSelected}
                data-testid={`fsm-mobile-transition-button-${transition.index}`}
                className="w-full rounded-[18px] border px-3 py-3 text-left transition-colors"
                style={
                  isSelected
                    ? {
                        borderColor: "color-mix(in srgb, var(--th-accent-primary) 50%, var(--th-border) 50%)",
                        background: "color-mix(in srgb, var(--th-accent-primary-soft) 84%, #11141b 16%)",
                        color: "var(--th-text-primary)",
                      }
                    : {
                        borderColor: "color-mix(in srgb, var(--th-border) 82%, transparent)",
                        background: "#11141b",
                        color: "var(--th-text-primary)",
                      }
                }
              >
                <div className="flex items-start justify-between gap-3">
                  <div>
                    <div className="text-sm font-medium">
                      {transition.from} → {transition.to}
                    </div>
                    <div className="mt-1 text-[11px]" style={MUTED_TEXT_STYLE}>
                      {transition.event}
                    </div>
                  </div>
                  <span
                    className="rounded-md border px-2 py-1 text-[10px] font-semibold uppercase tracking-[0.16em]"
                    style={{
                      borderColor: accent.stroke,
                      background: accent.background,
                      color: accent.text,
                      fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
                    }}
                  >
                    {transition.type}
                  </span>
                </div>
              </button>
            );
          })}
        </div>
      </div>

      <div className="space-y-2">
        <div className="flex items-center justify-between gap-2">
          <h5 className="text-xs font-semibold uppercase tracking-wider" style={MUTED_TEXT_STYLE}>
            {tr("상태", "States")}
          </h5>
          <span className="text-[11px]" style={MUTED_TEXT_STYLE}>
            {`${ctx.pipelineDraft.states.length}`}
          </span>
        </div>
        <div className="grid grid-cols-2 gap-2">
          {ctx.pipelineDraft.states.map((state: any) => {
            const tone = fsmStateTone(state.id);
            const isSelected = ctx.selection?.kind === "state" && ctx.selection.stateId === state.id;
            return (
              <button
                key={state.id}
                type="button"
                onClick={() => actions.setSelection({ kind: "state", stateId: state.id })}
                aria-pressed={isSelected}
                data-testid={`fsm-mobile-state-button-${state.id}`}
                className="rounded-[18px] border px-3 py-3 text-left transition-colors"
                style={
                  isSelected
                    ? { borderColor: tone.stroke, background: tone.glow, color: "var(--th-text-primary)" }
                    : {
                        borderColor: "color-mix(in srgb, var(--th-border) 82%, transparent)",
                        background: "#11141b",
                        color: "var(--th-text-primary)",
                      }
                }
              >
                <div className="text-sm font-medium">{state.label}</div>
                <div className="mt-1 text-[11px]" style={MUTED_TEXT_STYLE}>
                  {state.id}
                </div>
              </button>
            );
          })}
        </div>
      </div>
    </div>
  );
}
