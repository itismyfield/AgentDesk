import {
  BUTTON_INFO_STYLE,
  BUTTON_NEUTRAL_STYLE,
  INPUT_CLASS,
  INPUT_STYLE,
  MUTED_TEXT_STYLE,
} from "./pipeline-visual-editor-ui";

interface Props {
  ctx: any;
  actions: any;
}

export default function PipelineVisualEditorPhaseGatePanel({ ctx, actions }: Props) {
  const tr = ctx.tr;
  const pipelineDraft = ctx.pipelineDraft;

  return (
    <div className="space-y-3">
      <p className="text-xs" style={MUTED_TEXT_STYLE}>
        {tr(
          "visual editor 안에서 override의 phase gate 블록도 함께 편집합니다.",
          "The visual editor also edits the override phase gate block in place.",
        )}
      </p>
      <div className="grid gap-3 sm:grid-cols-2">
        <TextField
          label={tr("dispatch_to", "dispatch_to")}
          value={pipelineDraft.phase_gate.dispatch_to}
          onChange={(value) => actions.updatePhaseGate({ dispatch_to: value })}
        />
        <TextField
          label={tr("dispatch_type", "dispatch_type")}
          value={pipelineDraft.phase_gate.dispatch_type}
          onChange={(value) => actions.updatePhaseGate({ dispatch_type: value })}
        />
        <div className="sm:col-span-2">
          <TextField
            label={tr("pass_verdict", "pass_verdict")}
            value={pipelineDraft.phase_gate.pass_verdict}
            onChange={(value) => actions.updatePhaseGate({ pass_verdict: value })}
          />
        </div>
      </div>
      <div>
        <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
          {tr("checks", "checks")}
        </label>
        <div className="flex flex-wrap gap-1.5">
          {Array.from(new Set([
            "merge_verified",
            "issue_closed",
            "build_passed",
            ...(pipelineDraft.phase_gate.checks ?? []),
          ])).map((checkName: string) => {
            const active = (pipelineDraft.phase_gate.checks ?? []).includes(checkName);
            return (
              <button
                key={checkName}
                type="button"
                onClick={() => {
                  const current = pipelineDraft.phase_gate.checks ?? [];
                  const next = active
                    ? current.filter((check: string) => check !== checkName)
                    : [...current, checkName];
                  actions.updatePhaseGate({ checks: next });
                }}
                className="rounded-lg border px-2 py-1 text-xs font-mono transition-colors"
                style={
                  active
                    ? BUTTON_INFO_STYLE
                    : { ...BUTTON_NEUTRAL_STYLE, background: "transparent", color: "var(--th-text-muted)" }
                }
              >
                {checkName}
              </button>
            );
          })}
        </div>
      </div>
    </div>
  );
}

function TextField(props: { label: string; value: string; onChange: (value: string) => void }) {
  return (
    <div>
      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
        {props.label}
      </label>
      <input
        value={props.value}
        onChange={(event) => props.onChange(event.target.value)}
        className={INPUT_CLASS}
        style={INPUT_STYLE}
      />
    </div>
  );
}
