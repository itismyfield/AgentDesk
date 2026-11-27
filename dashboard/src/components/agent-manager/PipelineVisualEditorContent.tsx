import PipelineFlowCanvas from "./PipelineFlowCanvas";
import type { PipelineEditorActions, PipelineEditorCtx } from "./PipelineVisualEditor";
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
  ctx: PipelineEditorCtx;
  actions: PipelineEditorActions;
}

export default function PipelineVisualEditorContent({ ctx, actions }: Props) {
  const { graph, pipelineDraft, tr } = ctx;
  if (!graph || !pipelineDraft) return null;

  return (
    <>
      <div className={ctx.graphGridClass}>
        <div
          className={`min-w-0 rounded-lg border p-4 sm:p-5 space-y-4 ${ctx.useScrollableMobileFsmCanvas ? "order-2 xl:order-1" : ""}`}
          style={ctx.isFsmVariant ? FSM_PANEL_STYLE : PANEL_STYLE}
        >
          {!ctx.isFsmVariant && (
            <div className="flex flex-wrap items-center gap-2">
              <button
                type="button"
                onClick={actions.addState}
                className="inline-flex items-center justify-center max-[899px]:min-h-11 pointer-coarse:min-h-11 rounded-xl border px-3 py-1.5 text-xs font-medium"
                style={BUTTON_INFO_STYLE}
              >
                + {tr("상태", "State")}
              </button>
              <button
                type="button"
                onClick={actions.addTransition}
                className="inline-flex items-center justify-center max-[899px]:min-h-11 pointer-coarse:min-h-11 rounded-xl border px-3 py-1.5 text-xs font-medium"
                style={BUTTON_ACCENT_STYLE}
              >
                + {tr("전환", "Transition")}
              </button>
              <button
                type="button"
                onClick={() => actions.setSelection({ kind: "phase_gate" })}
                className="inline-flex items-center justify-center max-[899px]:min-h-11 pointer-coarse:min-h-11 rounded-xl border px-3 py-1.5 text-xs font-medium"
                style={BUTTON_WARN_STYLE}
              >
                {tr("단계 관문", "Phase Gate")}
              </button>
            </div>
          )}

          <PipelineFlowCanvas
            compactGraph={ctx.compactGraph}
            fsmEdgeBindings={ctx.fsmEdgeBindings}
            graph={graph}
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
          className={`min-w-0 rounded-lg border p-4 sm:p-5 space-y-4 ${ctx.useScrollableMobileFsmCanvas ? "order-1 xl:order-2" : ""}`}
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
