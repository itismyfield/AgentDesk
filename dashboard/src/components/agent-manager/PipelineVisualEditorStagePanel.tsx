import { localeName } from "../../i18n";
import type { StageDraft } from "./pipeline-visual-editor-model";
import {
  BUTTON_DANGER_STYLE,
  BUTTON_INFO_STYLE,
  BUTTON_NEUTRAL_STYLE,
  EMPTY_PANEL_STYLE,
  INPUT_CLASS,
  INPUT_STYLE,
  MUTED_TEXT_STYLE,
  PANEL_SOFT_STYLE,
  PANEL_STYLE,
} from "./pipeline-visual-editor-ui";

interface Props {
  ctx: any;
  actions: any;
}

export default function PipelineVisualEditorStagePanel({ ctx, actions }: Props) {
  const tr = ctx.tr;

  return (
    <div className="min-w-0 rounded-[24px] border p-4 sm:p-5 space-y-4" style={PANEL_STYLE}>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h4 className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
            {tr("파이프라인 스테이지", "Pipeline Stages")}
          </h4>
          <p className="text-xs" style={MUTED_TEXT_STYLE}>
            {ctx.selectedAgentDetail
              ? tr(
                  "선택된 에이전트에 보이는 스테이지만 편집합니다. 저장 시 다른 에이전트 전용 스테이지는 유지됩니다.",
                  "You are editing only stages visible to the selected agent. Saving preserves other-agent stages.",
                )
              : tr(
                  "상태머신과 같은 카드 안에서 스테이지 실행 순서를 함께 관리합니다.",
                  "Manage stage execution order in the same card as the state machine.",
                )}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <button onClick={actions.addStage} className="rounded-xl border px-3 py-1.5 text-xs font-medium" style={BUTTON_INFO_STYLE}>
            + {tr("스테이지", "Stage")}
          </button>
          <button
            onClick={() => void actions.handleClearStages()}
            disabled={ctx.saving || (ctx.stageDrafts.length === 0 && ctx.allRepoStages.length === 0)}
            className="rounded-xl border px-3 py-1.5 text-xs"
            style={{
              ...BUTTON_DANGER_STYLE,
              opacity: ctx.saving || (ctx.stageDrafts.length === 0 && ctx.allRepoStages.length === 0) ? 0.45 : 1,
            }}
          >
            {tr("보이는 스테이지 정리", "Clear visible stages")}
          </button>
        </div>
      </div>

      {ctx.stageDrafts.length === 0 ? (
        <div className="rounded-[20px] border px-4 py-6 text-center text-sm" style={EMPTY_PANEL_STYLE}>
          {tr(
            "스테이지가 없습니다. 아래의 + 버튼으로 자동 실행 단계를 추가하세요.",
            "No stages yet. Add an automated stage with the + button.",
          )}
        </div>
      ) : (
        <div className="grid min-w-0 gap-3 xl:grid-cols-2">
          {ctx.stageDrafts.map((stage: StageDraft, index: number) => (
            <StageCard key={`${stage.stage_name}-${index}`} ctx={ctx} actions={actions} stage={stage} index={index} />
          ))}
        </div>
      )}
    </div>
  );
}

function StageCard({ ctx, actions, stage, index }: Props & { stage: StageDraft; index: number }) {
  const tr = ctx.tr;

  return (
    <div className="min-w-0 rounded-[20px] border p-4 space-y-3" style={PANEL_SOFT_STYLE}>
      <div className="flex items-center gap-2">
        <span
          className="inline-flex h-7 w-7 items-center justify-center rounded-full text-xs font-semibold"
          style={{ background: "var(--th-accent-primary-soft)", color: "var(--th-text-primary)" }}
        >
          {index + 1}
        </span>
        <input
          value={stage.stage_name}
          onChange={(event) => actions.updateStage(index, { stage_name: event.target.value })}
          className={INPUT_CLASS}
          style={INPUT_STYLE}
          placeholder={tr("스테이지 이름", "Stage name")}
        />
      </div>

      <div className="grid gap-3 sm:grid-cols-2">
        <TextField ctx={ctx} label={tr("스킬", "Skill")} value={stage.entry_skill} onChange={(value) => actions.updateStage(index, { entry_skill: value })} placeholder="claude-code-plan" />
        <TextField ctx={ctx} label={tr("프로바이더", "Provider")} value={stage.provider} onChange={(value) => actions.updateStage(index, { provider: value })} placeholder="self / counter" />
        <SelectField
          label={tr("트리거", "Trigger")}
          value={stage.trigger_after}
          onChange={(value) => actions.updateStage(index, { trigger_after: value as StageDraft["trigger_after"] })}
          options={[
            ["ready", tr("카드 준비 시", "On ready")],
            ["review_pass", tr("리뷰 통과 후", "After review pass")],
          ]}
        />
        <NumberField ctx={ctx} label={tr("타임아웃(분)", "Timeout (min)")} value={stage.timeout_minutes} min={1} onChange={(value) => actions.updateStage(index, { timeout_minutes: Math.max(1, value || 60) })} />
        <AgentSelect ctx={ctx} label={tr("담당 에이전트 override", "Agent override")} value={stage.agent_override_id} emptyLabel={tr("카드 담당자", "Card assignee")} onChange={(value) => actions.updateStage(index, { agent_override_id: value })} />
        <AgentSelect ctx={ctx} label={tr("적용 대상 에이전트", "Applies to agent")} value={stage.applies_to_agent_id} emptyLabel={tr("전체", "All agents")} onChange={(value) => actions.updateStage(index, { applies_to_agent_id: value })} />
        <SelectField
          label={tr("실패 시", "On failure")}
          value={stage.on_failure}
          onChange={(value) => actions.updateStage(index, { on_failure: value as StageDraft["on_failure"] })}
          options={[
            ["fail", tr("실패 처리", "Fail")],
            ["retry", tr("재시도", "Retry")],
            ["previous", tr("이전 스테이지", "Previous stage")],
            ["goto", tr("지정 스테이지", "Go to stage")],
          ]}
        />
        <NumberField ctx={ctx} label={tr("최대 재시도", "Max retries")} value={stage.max_retries} min={0} onChange={(value) => actions.updateStage(index, { max_retries: Math.max(0, value || 0) })} />
        {stage.on_failure === "goto" && (
          <div className="sm:col-span-2">
            <StageNameSelect ctx={ctx} label={tr("이동 대상", "Goto target")} value={stage.on_failure_target} index={index} onChange={(value) => actions.updateStage(index, { on_failure_target: value })} />
          </div>
        )}
      </div>

      <div className="grid gap-3 sm:grid-cols-2">
        <TextField ctx={ctx} label={tr("스킵 조건", "Skip condition")} value={stage.skip_condition} onChange={(value) => actions.updateStage(index, { skip_condition: value })} placeholder="label:hotfix" />
        <StageNameSelect ctx={ctx} label={tr("병렬 스테이지", "Parallel with")} value={stage.parallel_with} index={index} emptyLabel={tr("없음", "None")} onChange={(value) => actions.updateStage(index, { parallel_with: value })} />
      </div>

      <div className="flex flex-wrap gap-2">
        {index > 0 && (
          <button onClick={() => actions.moveStage(index, -1)} className="rounded-xl border px-3 py-1.5 text-xs" style={BUTTON_NEUTRAL_STYLE}>
            ↑ {tr("앞으로", "Earlier")}
          </button>
        )}
        {index < ctx.stageDrafts.length - 1 && (
          <button onClick={() => actions.moveStage(index, 1)} className="rounded-xl border px-3 py-1.5 text-xs" style={BUTTON_NEUTRAL_STYLE}>
            ↓ {tr("뒤로", "Later")}
          </button>
        )}
        <button onClick={() => actions.removeStage(index)} className="rounded-xl border px-3 py-1.5 text-xs" style={BUTTON_DANGER_STYLE}>
          {tr("삭제", "Delete")}
        </button>
      </div>
    </div>
  );
}

function TextField(props: { ctx: any; label: string; value: string; onChange: (value: string) => void; placeholder?: string }) {
  return (
    <div>
      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
        {props.label}
      </label>
      <input value={props.value} onChange={(event) => props.onChange(event.target.value)} className={INPUT_CLASS} style={INPUT_STYLE} placeholder={props.placeholder} />
    </div>
  );
}

function NumberField(props: { ctx: any; label: string; value: number; min: number; onChange: (value: number) => void }) {
  return (
    <div>
      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
        {props.label}
      </label>
      <input type="number" value={props.value} onChange={(event) => props.onChange(Number(event.target.value))} className={INPUT_CLASS} style={INPUT_STYLE} min={props.min} />
    </div>
  );
}

function SelectField(props: { label: string; value: string; onChange: (value: string) => void; options: [string, string][] }) {
  return (
    <div>
      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
        {props.label}
      </label>
      <select value={props.value} onChange={(event) => props.onChange(event.target.value)} className={INPUT_CLASS} style={INPUT_STYLE}>
        {props.options.map(([value, label]) => (
          <option key={value} value={value}>{label}</option>
        ))}
      </select>
    </div>
  );
}

function AgentSelect(props: { ctx: any; label: string; value: string; emptyLabel: string; onChange: (value: string) => void }) {
  return (
    <div>
      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
        {props.label}
      </label>
      <select value={props.value} onChange={(event) => props.onChange(event.target.value)} className={INPUT_CLASS} style={INPUT_STYLE}>
        <option value="">{props.emptyLabel}</option>
        {props.ctx.agents.map((agent: any) => (
          <option key={agent.id} value={agent.id}>
            {localeName(props.ctx.locale, agent)}
          </option>
        ))}
      </select>
    </div>
  );
}

function StageNameSelect(props: { ctx: any; label: string; value: string; index: number; onChange: (value: string) => void; emptyLabel?: string }) {
  return (
    <div>
      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
        {props.label}
      </label>
      <select value={props.value} onChange={(event) => props.onChange(event.target.value)} className={INPUT_CLASS} style={INPUT_STYLE}>
        <option value="">{props.emptyLabel ?? props.ctx.tr("선택", "Select")}</option>
        {props.ctx.stageDrafts
          .filter((_: StageDraft, stageIndex: number) => stageIndex !== props.index)
          .map((candidate: StageDraft) => (
            <option key={candidate.stage_name} value={candidate.stage_name}>
              {candidate.stage_name}
            </option>
          ))}
      </select>
    </div>
  );
}
