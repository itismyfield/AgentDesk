import type { Dispatch, RefObject, SetStateAction } from "react";

import type { AgentDef } from "../onboardingDraft";
import { TEMPLATES } from "./templates";
import {
  ChecklistPanel,
  type ChecklistItem,
} from "./OnboardingWizardSections";

type Tr = (ko: string, en: string) => string;

interface Step3AgentSelectionProps {
  actionRow: string;
  addCustomAgent: () => void;
  agents: AgentDef[];
  borderInput: string;
  borderLight: string;
  btnPrimary: string;
  btnSecondary: string;
  btnSmall: string;
  customDesc: string;
  customDescEn: string;
  customName: string;
  customNameEn: string;
  expandedAgent: string | null;
  generateAiPrompt: (agentId: string) => Promise<void>;
  generatingPrompt: boolean;
  goToStep: (step: number) => void;
  labelStyle: string;
  removeAgent: (agentId: string) => void;
  selectTemplate: (key: string) => void;
  selectedTemplate: string | null;
  setAgents: Dispatch<SetStateAction<AgentDef[]>>;
  setCustomDesc: Dispatch<SetStateAction<string>>;
  setCustomDescEn: Dispatch<SetStateAction<string>>;
  setCustomName: Dispatch<SetStateAction<string>>;
  setCustomNameEn: Dispatch<SetStateAction<string>>;
  setExpandedAgent: Dispatch<SetStateAction<string | null>>;
  step3Checklist: ChecklistItem[];
  stepBox: string;
  stepHeadingRef: RefObject<HTMLHeadingElement | null>;
  tr: Tr;
}

export function Step3AgentSelection({
  actionRow,
  addCustomAgent,
  agents,
  borderInput,
  borderLight,
  btnPrimary,
  btnSecondary,
  btnSmall,
  customDesc,
  customDescEn,
  customName,
  customNameEn,
  expandedAgent,
  generateAiPrompt,
  generatingPrompt,
  goToStep,
  labelStyle,
  removeAgent,
  selectTemplate,
  selectedTemplate,
  setAgents,
  setCustomDesc,
  setCustomDescEn,
  setCustomName,
  setCustomNameEn,
  setExpandedAgent,
  step3Checklist,
  stepBox,
  stepHeadingRef,
  tr,
}: Step3AgentSelectionProps) {
  return (
    <div className={stepBox} style={{ borderColor: borderLight }}>
      <div>
        <h2 ref={stepHeadingRef} tabIndex={-1} className="text-base font-semibold outline-none" style={{ color: "var(--th-text-heading)" }}>
          {tr("역할 프리셋과 에이전트 구성", "Role Presets & Agents")}
        </h2>
        <p className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
          {tr(
            "역할별 프리셋으로 팀을 빠르게 시작하거나, 필요한 에이전트를 직접 추가할 수 있습니다.",
            "Start from a role-based preset or add the exact agents you need.",
          )}
        </p>
      </div>

      <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
        {TEMPLATES.map((template) => (
          <button type="button"
            key={template.key}
            onClick={() => selectTemplate(template.key)}
            className="rounded-lg p-4 border text-left transition-colors hover:bg-surface-subtle"
            style={{
              borderColor: selectedTemplate === template.key ? "var(--th-accent-primary)" : borderLight,
              backgroundColor:
                selectedTemplate === template.key
                  ? "color-mix(in srgb, var(--th-accent-primary-soft) 82%, transparent)"
                  : "transparent",
            }}
          >
            <div className="text-base mb-2">{template.icon}</div>
            <div className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>{tr(template.name, template.nameEn)}</div>
            <div className="text-xs mt-1" style={{ color: "var(--th-text-muted)" }}>{tr(template.description, template.descriptionEn)}</div>
            <div className="text-xs mt-2" style={{ color: "var(--th-text-muted)" }}>
              {template.agents.map((agent) => tr(agent.name, agent.nameEn)).join(", ")}
            </div>
          </button>
        ))}
      </div>

      <div
        className="rounded-lg border p-4 space-y-2"
        style={{ borderColor: "var(--th-border-subtle)", backgroundColor: "var(--th-overlay-subtle)" }}
      >
        <div className="text-sm font-medium" style={{ color: "var(--th-text-secondary)" }}>
          {tr("커스텀 에이전트의 AI 프롬프트 초안 만들기", "Create AI prompt drafts for custom agents")}
        </div>
        <div className="text-xs space-y-1" style={{ color: "var(--th-text-secondary)" }}>
          <div>{tr("1. 이름과 한줄 설명을 적고 에이전트를 추가합니다.", "1. Add an agent with a name and one-line description.")}</div>
          <div>{tr("2. 카드 펼치기 → `AI 초안 생성`으로 시스템 프롬프트 뼈대를 만듭니다.", "2. Expand the card and click `AI Draft` to build the first system prompt draft.")}</div>
          <div>{tr("3. 담당 업무, 금지사항, 말투를 직접 보정하면 품질이 크게 올라갑니다.", "3. Refine responsibilities, guardrails, and tone for a much better final prompt.")}</div>
        </div>
      </div>

      {agents.length > 0 && (
        <div className="space-y-2">
          <div className="text-xs font-medium" style={{ color: "var(--th-text-secondary)" }}>
            {tr(`${agents.length}개 에이전트`, `${agents.length} agents`)}
          </div>
          {agents.map((agent) => (
            <div key={agent.id} className="rounded-lg border overflow-hidden" style={{ borderColor: borderLight }}>
              <div className="flex min-w-0 items-start gap-2 px-3 py-2">
                <button type="button" aria-expanded={expandedAgent === agent.id}
                  onClick={() => setExpandedAgent(expandedAgent === agent.id ? null : agent.id)}
                  className="flex min-h-11 min-w-0 flex-1 items-center justify-between gap-3 rounded-md px-1 py-2 text-left hover:bg-surface-subtle">
                  <span className="min-w-0">
                    <span className="block text-sm font-medium text-th-text-primary [overflow-wrap:anywhere]">{tr(agent.name, agent.nameEn || agent.name)}</span>
                    <span className="mt-1 block text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">{tr(agent.description, agent.descriptionEn || agent.description)}</span>
                  </span>
                  <span className="shrink-0 text-xs text-th-text-muted" aria-hidden="true">{expandedAgent === agent.id ? "▲" : "▼"}</span>
                </button>
                {agent.custom ? <button type="button" onClick={() => removeAgent(agent.id)} className="min-h-11 min-w-11 shrink-0 rounded-md px-2 text-xs text-[var(--th-accent-danger)]">{tr("삭제", "Del")}</button> : null}
              </div>
              {expandedAgent === agent.id && (
                <div className="px-4 pb-3 space-y-2 border-t" style={{ borderColor: borderLight }}>
                  <div className="flex items-center gap-2 pt-2">
                    <label className={labelStyle} style={{ color: "var(--th-text-secondary)" }}>
                      {tr("시스템 프롬프트", "System Prompt")}
                    </label>
                    {agent.custom && (
                      <button type="button"
                        onClick={() => void generateAiPrompt(agent.id)}
                        disabled={generatingPrompt}
                        className={btnSmall}
                        style={{
                          borderColor: "color-mix(in srgb, var(--th-accent-primary) 32%, var(--th-border) 68%)",
                          color: "var(--th-text-primary)",
                        }}
                      >
                        {generatingPrompt ? tr("생성 중...", "Generating...") : tr("AI 초안 생성", "AI Draft")}
                      </button>
                    )}
                  </div>
                  <textarea
                    aria-label={tr(
                      `${agent.name} 시스템 프롬프트`,
                      `System prompt for ${agent.nameEn || agent.name}`,
                    )}
                    value={agent.prompt}
                    onChange={(event) => {
                      setAgents((current) =>
                        current.map((candidate) => (candidate.id === agent.id ? { ...candidate, prompt: event.target.value } : candidate)),
                      );
                    }}
                    rows={6}
                    className="w-full rounded-lg px-3 py-2 text-xs bg-surface-subtle border resize-y"
                    style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
                    placeholder={tr("에이전트의 역할과 행동 규칙을 정의합니다", "Define the agent's role and behavior")}
                  />
                </div>
              )}
            </div>
          ))}
        </div>
      )}

      <div className="space-y-2">
        <div className="grid grid-cols-1 sm:grid-cols-2 gap-2">
          <input
            type="text"
            aria-label={tr("에이전트 이름", "Agent name")}
            placeholder={tr("에이전트 이름", "Agent name")}
            value={customName}
            onChange={(event) => setCustomName(event.target.value)}
            className="min-h-11 min-w-0 flex-1 rounded-md px-3 py-2 text-sm bg-surface-subtle border"
            style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
          />
          <input
            type="text"
            aria-label={tr("한줄 설명", "Brief description")}
            placeholder={tr("한줄 설명", "Brief description")}
            value={customDesc}
            onChange={(event) => setCustomDesc(event.target.value)}
            className="min-h-11 min-w-0 flex-1 rounded-md px-3 py-2 text-sm bg-surface-subtle border"
            style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
          />
        </div>
        <div className="grid grid-cols-1 sm:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto] gap-2">
          <input
            type="text"
            aria-label={tr("영문 이름 (선택)", "English name (optional)")}
            placeholder={tr("영문 이름 (선택)", "English name (optional)")}
            value={customNameEn}
            onChange={(event) => setCustomNameEn(event.target.value)}
            className="min-h-11 min-w-0 flex-1 rounded-md px-3 py-2 text-sm bg-surface-subtle border"
            style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
          />
          <input
            type="text"
            aria-label={tr("영문 설명 (선택)", "English description (optional)")}
            placeholder={tr("영문 설명 (선택)", "English description (optional)")}
            value={customDescEn}
            onChange={(event) => setCustomDescEn(event.target.value)}
            className="min-h-11 min-w-0 flex-1 rounded-md px-3 py-2 text-sm bg-surface-subtle border"
            style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
          />
          <button type="button"
            onClick={addCustomAgent}
            disabled={!customName.trim()}
            className="min-h-11 w-full sm:w-auto px-4 py-2 rounded-lg text-sm font-medium bg-indigo-600 text-white hover:bg-indigo-500 disabled:opacity-40 transition-colors whitespace-nowrap"
          >
            + {tr("추가", "Add")}
          </button>
        </div>
        <p className="text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
          {tr(
            "영문 필드를 비워두면 현재 입력값을 그대로 사용합니다. 다국어 대시보드에서 별도 표기가 필요할 때만 채우면 됩니다.",
            "Leave the English fields empty to reuse the current values. Fill them only when you need separate wording in English mode.",
          )}
        </p>
      </div>

      <ChecklistPanel title={tr("Step 3 체크리스트", "Step 3 checklist")} items={step3Checklist} />

      <div className={actionRow}>
        <button type="button" onClick={() => goToStep(2)} className={btnSecondary} style={{ borderColor: "var(--th-border-subtle)" }}>
          {tr("이전", "Back")}
        </button>
        <button type="button" onClick={() => goToStep(4)} disabled={agents.length === 0} className={btnPrimary}>
          {tr("다음", "Next")} ({agents.length}{tr("개 에이전트", " agents")})
        </button>
      </div>
    </div>
  );
}
