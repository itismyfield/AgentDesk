import PipelineFlowCanvas from "./PipelineFlowCanvas";
import PipelineVisualEditorInspector from "./PipelineVisualEditorInspector";
import PipelineVisualEditorStagePanel from "./PipelineVisualEditorStagePanel";
import {
  FSM_INSPECTOR_STYLE,
  FSM_PANEL_STYLE,
  PANEL_STYLE,
  BUTTON_ACCENT_STYLE,
  BUTTON_INFO_STYLE,
  BUTTON_WARN_STYLE,
} from "./pipeline-visual-editor-ui";

interface Props {
  ctx: any;
  actions: any;
}

export default function PipelineVisualEditorContent({ ctx, actions }: Props) {
  const tr = ctx.tr;

  return (
    <>
      <div className={ctx.graphGridClass}>
        <div
          className={`min-w-0 rounded-[24px] border p-4 sm:p-5 space-y-4 ${ctx.useScrollableMobileFsmCanvas ? "order-2 xl:order-1" : ""}`}
          style={ctx.isFsmVariant ? FSM_PANEL_STYLE : PANEL_STYLE}
        >
          {!ctx.isFsmVariant && (
            <div className="flex flex-wrap items-center gap-2">
              <button onClick={actions.addState} className="rounded-xl border px-3 py-1.5 text-xs font-medium" style={BUTTON_INFO_STYLE}>
                + {tr("상태", "State")}
              </button>
              <button onClick={actions.addTransition} className="rounded-xl border px-3 py-1.5 text-xs font-medium" style={BUTTON_ACCENT_STYLE}>
                + {tr("전환", "Transition")}
              </button>
              <button
                onClick={() => actions.setSelection({ kind: "phase_gate" })}
                className="rounded-xl border px-3 py-1.5 text-xs font-medium"
                style={BUTTON_WARN_STYLE}
              >
                {tr("Phase Gate", "Phase Gate")}
              </button>
            </div>
          )}

          <PipelineFlowCanvas
            compactGraph={ctx.compactGraph}
            fsmEdgeBindings={ctx.fsmEdgeBindings}
            graph={ctx.graph}
            graphPanelNote={ctx.graphPanelNote}
            isFsmVariant={ctx.isFsmVariant}
            onConnectTransition={actions.addTransitionBetween}
            onSelectionChange={actions.setSelection}
            selection={ctx.selection}
            tr={tr}
            useScrollableMobileFsmCanvas={ctx.useScrollableMobileFsmCanvas}
          />
        </div>

        <div
          className={`min-w-0 rounded-[24px] border p-4 sm:p-5 space-y-4 ${ctx.useScrollableMobileFsmCanvas ? "order-1 xl:order-2" : ""}`}
          style={ctx.isFsmVariant ? FSM_INSPECTOR_STYLE : PANEL_STYLE}
        >
          <PipelineVisualEditorInspector ctx={ctx} actions={actions} />
        </div>
      </div>

      {!ctx.isFsmVariant && (
        <PipelineVisualEditorStagePanel ctx={ctx} actions={actions} />
      )}
    </>
  );
}
