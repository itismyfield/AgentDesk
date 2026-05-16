import type { PipelineConfigFull } from "../../types";
import {
  BUTTON_DANGER_STYLE,
  BUTTON_NEUTRAL_STYLE,
  BUTTON_WARN_STYLE,
  FSM_DETAIL_PANEL_STYLE,
  FSM_INPUT_STYLE,
  INPUT_CLASS,
  INPUT_STYLE,
  MUTED_TEXT_STYLE,
  PANEL_SOFT_STYLE,
} from "./pipeline-visual-editor-ui";

interface Props {
  ctx: any;
  actions: any;
}

export default function PipelineVisualEditorTransitionPanel({ ctx, actions }: Props) {
  return (
    <div className="space-y-3">
      {ctx.isFsmVariant ? (
        <FsmTransitionPanel ctx={ctx} actions={actions} />
      ) : (
        <AdvancedTransitionPanel ctx={ctx} actions={actions} />
      )}
    </div>
  );
}

function FsmTransitionPanel({ ctx, actions }: Props) {
  const tr = ctx.tr;
  const selectedTransition = ctx.selectedTransition;

  return (
    <div className="rounded-[20px] border p-4 space-y-4" style={FSM_DETAIL_PANEL_STYLE}>
      <div className="flex items-start justify-between gap-3">
        <div>
          <h5
            className="text-[11px] font-semibold uppercase tracking-[0.18em]"
            style={{ ...MUTED_TEXT_STYLE, fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace" }}
          >
            {tr("선택된 전환", "Selected transition")}
          </h5>
          <p className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
            {selectedTransition.from} → {selectedTransition.to}
          </p>
        </div>
        <span
          className="rounded-md border px-2 py-1 text-[10px] font-semibold uppercase tracking-[0.16em]"
          style={{
            borderColor: "color-mix(in srgb, var(--th-border) 80%, transparent)",
            background: "color-mix(in srgb, var(--th-overlay-subtle) 82%, transparent)",
            color: "var(--th-text-secondary)",
            fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
          }}
        >
          edge
        </span>
      </div>

      <div className="grid gap-3">
        <SelectField
          label={tr("Event", "Event")}
          value={ctx.selectedFsmEvent}
          onChange={(value) => actions.updateFsmTransitionEvent(ctx.selectedTransitionIndex, value)}
          options={ctx.fsmEventOptions}
          fsm
        />
        <div>
          <SelectField
            label={tr("Hook", "Hook")}
            value={ctx.selectedFsmHook}
            onChange={(value) => actions.updateFsmEventHook(ctx.selectedFsmEvent, value)}
            options={ctx.fsmHookOptions}
            emptyLabel={tr("없음", "None")}
            fsm
          />
          <p className="mt-1 text-[11px]" style={MUTED_TEXT_STYLE}>
            {tr(
              "FSM 모드에서는 선택된 event에 연결된 대표 hook 1개를 빠르게 편집합니다.",
              "FSM mode edits a single representative hook for the selected event.",
            )}
          </p>
        </div>
        <SelectField
          label={tr("Policy", "Policy")}
          value={selectedTransition.type}
          onChange={(value) =>
            actions.updateTransition(ctx.selectedTransitionIndex, {
              type: value as PipelineConfigFull["transitions"][number]["type"],
            })
          }
          options={["free", "gated", "force_only"]}
          fsm
        />
        {selectedTransition.type === "gated" && (
          <GateToggleList ctx={ctx} actions={actions} fsm />
        )}
      </div>

      <div className="flex flex-wrap gap-2 pt-1">
        <button
          onClick={() => actions.removeTransition(ctx.selectedTransitionIndex)}
          className="rounded-lg border px-3 py-1.5 text-[11px] font-medium"
          style={BUTTON_DANGER_STYLE}
        >
          {tr("전환 삭제", "Delete edge")}
        </button>
      </div>
    </div>
  );
}

function AdvancedTransitionPanel({ ctx, actions }: Props) {
  const tr = ctx.tr;
  const selectedTransition = ctx.selectedTransition;

  return (
    <>
      <div className="grid gap-3 sm:grid-cols-2">
        <SelectField
          label={tr("시작 상태", "From")}
          value={selectedTransition.from}
          onChange={(value) => actions.updateTransition(ctx.selectedTransitionIndex, { from: value })}
          options={ctx.pipelineDraft.states.map((state: any) => state.id)}
        />
        <SelectField
          label={tr("도착 상태", "To")}
          value={selectedTransition.to}
          onChange={(value) => actions.updateTransition(ctx.selectedTransitionIndex, { to: value })}
          options={ctx.pipelineDraft.states.map((state: any) => state.id)}
        />
      </div>

      <div className="grid gap-3 sm:grid-cols-2">
        <SelectField
          label={tr("전환 타입", "Transition type")}
          value={selectedTransition.type}
          onChange={(value) =>
            actions.updateTransition(ctx.selectedTransitionIndex, {
              type: value as PipelineConfigFull["transitions"][number]["type"],
            })
          }
          options={["free", "gated", "force_only"]}
        />
        <GateToggleList ctx={ctx} actions={actions} />
      </div>

      <div className="rounded-[20px] border p-4 space-y-3" style={PANEL_SOFT_STYLE}>
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div>
            <h5 className="text-xs font-semibold uppercase tracking-wider" style={MUTED_TEXT_STYLE}>
              {tr("게이트 정의", "Gate definitions")}
            </h5>
            <p className="text-xs" style={MUTED_TEXT_STYLE}>
              {tr(
                "전환 클릭 시 조건과 트리거를 이 영역에서 편집합니다.",
                "Edit transition conditions and triggers here.",
              )}
            </p>
          </div>
          <button onClick={() => actions.addGate(ctx.selectedTransitionIndex)} className="rounded-xl border px-3 py-1.5 text-xs" style={BUTTON_WARN_STYLE}>
            + {tr("게이트", "Gate")}
          </button>
        </div>
        <GateDefinitionList ctx={ctx} actions={actions} />
      </div>

      <button
        onClick={() => actions.removeTransition(ctx.selectedTransitionIndex)}
        className="rounded-xl border px-3 py-1.5 text-xs font-medium"
        style={BUTTON_DANGER_STYLE}
      >
        {tr("이 전환 삭제", "Delete transition")}
      </button>
    </>
  );
}

function GateToggleList({ ctx, actions, fsm }: Props & { fsm?: boolean }) {
  const tr = ctx.tr;
  const names = Array.from(new Set([...Object.keys(ctx.pipelineDraft.gates), ...ctx.selectedTransitionGates]));

  return (
    <div>
      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
        {tr("게이트 / 조건", "Gates / conditions")}
      </label>
      <div className="flex flex-wrap gap-1.5">
        {names.map((name: string) => {
          const active = ctx.selectedTransitionGates.includes(name);
          return (
            <button
              key={name}
              type="button"
              onClick={() => {
                const next = active
                  ? ctx.selectedTransitionGates.filter((gate: string) => gate !== name)
                  : [...ctx.selectedTransitionGates, name];
                actions.updateTransitionGates(ctx.selectedTransitionIndex, next.join(", "));
              }}
              className="rounded-lg border px-2 py-1 text-xs font-mono transition-colors"
              style={
                active
                  ? BUTTON_WARN_STYLE
                  : fsm
                    ? { ...FSM_DETAIL_PANEL_STYLE, color: "var(--th-text-muted)" }
                    : { ...BUTTON_NEUTRAL_STYLE, background: "transparent", color: "var(--th-text-muted)" }
              }
            >
              {name}
            </button>
          );
        })}
      </div>
    </div>
  );
}

function GateDefinitionList({ ctx, actions }: Props) {
  const tr = ctx.tr;

  if (ctx.selectedTransitionGates.length === 0) {
    return (
      <p className="text-xs" style={MUTED_TEXT_STYLE}>
        {tr(
          "이 전환에는 연결된 게이트가 없습니다. gated 타입이면 게이트를 추가하세요.",
          "This transition has no gates. Add one if the transition should be gated.",
        )}
      </p>
    );
  }

  return (
    <>
      {ctx.selectedTransitionGates.map((gateName: string) => (
        <div key={gateName} className="rounded-xl border p-3 space-y-2" style={PANEL_SOFT_STYLE}>
          <div className="text-xs font-mono" style={{ color: "var(--th-text-primary)" }}>
            {gateName}
          </div>
          <div className="grid gap-3 sm:grid-cols-2">
            <SelectField
              label={tr("게이트 타입", "Gate type")}
              value={ctx.pipelineDraft.gates[gateName]?.type ?? ""}
              onChange={(value) => actions.updateGate(gateName, { type: value })}
              options={Array.from(new Set(["builtin", ...Object.values(ctx.pipelineDraft.gates).map((gate: any) => gate?.type).filter(Boolean)]))}
              emptyLabel="-"
            />
            <SelectField
              label={tr("체크", "Check")}
              value={ctx.pipelineDraft.gates[gateName]?.check ?? ""}
              onChange={(value) => actions.updateGate(gateName, { check: value || undefined })}
              options={Array.from(new Set([
                "has_active_dispatch",
                "review_verdict_pass",
                "review_verdict_rework",
                ...Object.values(ctx.pipelineDraft.gates).map((gate: any) => gate?.check).filter(Boolean),
              ]))}
              emptyLabel="-"
            />
          </div>
          <div>
            <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
              {tr("설명", "Description")}
            </label>
            <input
              value={ctx.pipelineDraft.gates[gateName]?.description ?? ""}
              onChange={(event) => actions.updateGate(gateName, { description: event.target.value || undefined })}
              className={INPUT_CLASS}
              style={INPUT_STYLE}
              placeholder={tr("게이트 설명", "Gate description")}
            />
          </div>
        </div>
      ))}
    </>
  );
}

function SelectField(props: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  options: string[];
  emptyLabel?: string;
  fsm?: boolean;
}) {
  return (
    <div>
      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
        {props.label}
      </label>
      <select
        value={props.value}
        onChange={(event) => props.onChange(event.target.value)}
        className={INPUT_CLASS}
        style={props.fsm ? FSM_INPUT_STYLE : INPUT_STYLE}
      >
        {props.emptyLabel !== undefined && <option value="">{props.emptyLabel}</option>}
        {props.options.map((option) => (
          <option key={option} value={option}>
            {option}
          </option>
        ))}
      </select>
    </div>
  );
}
