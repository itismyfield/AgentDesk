import { useId } from "react";
import type { PipelineEditorActions, PipelineEditorCtx } from "./PipelineVisualEditor";
import { INPUT_CLASS, INPUT_STYLE, MUTED_TEXT_STYLE } from "./pipeline-visual-editor-ui";

interface Props {
  ctx: PipelineEditorCtx;
  actions: PipelineEditorActions;
}

export default function PipelineVisualEditorPhaseGatePanel({ ctx, actions }: Props) {
  const { pipelineDraft, tr } = ctx;
  if (!pipelineDraft) return null;

  return (
    <div className="space-y-3">
      <p className="text-xs" style={MUTED_TEXT_STYLE}>
        {tr(
          "자동큐가 페이즈를 넘어가기 전에 확인 작업을 누구에게 보낼지 정합니다. 통과 조건(PR 머지, 이슈 종료, 빌드 통과)은 게이트 종류에 고정돼 있습니다.",
          "Choose who runs the check before the auto-queue moves to the next phase. Pass conditions (PR merged, issue closed, build passed) are fixed by the gate kind.",
        )}
      </p>
      <div className="grid gap-3 sm:grid-cols-2">
        <TextField
          label={tr("확인 담당 (self = 해당 에이전트)", "Checked by (self = the entry's agent)")}
          value={pipelineDraft.phase_gate.dispatch_to}
          onChange={(value) => actions.updatePhaseGate({ dispatch_to: value })}
        />
        <TextField
          label={tr("디스패치 종류", "Dispatch type")}
          value={pipelineDraft.phase_gate.dispatch_type}
          onChange={(value) => actions.updatePhaseGate({ dispatch_type: value })}
        />
      </div>
    </div>
  );
}

function TextField(props: { label: string; value: string; onChange: (value: string) => void }) {
  const inputId = useId();
  return (
    <div>
      <label htmlFor={inputId} className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
        {props.label}
      </label>
      <input
        id={inputId}
        value={props.value}
        onChange={(event) => props.onChange(event.target.value)}
        className={INPUT_CLASS}
        style={INPUT_STYLE}
      />
    </div>
  );
}
