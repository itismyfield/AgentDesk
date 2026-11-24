import { useId, useMemo, useRef, useState } from "react";
import { AlertTriangle, ExternalLink, ShieldAlert } from "lucide-react";
import { useInRouterContext, useNavigate } from "react-router";
import { Button } from "../../kit/free/button-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { TreeView, type TreeNode } from "../../kit/free/tree-view-clean";
import { InlineDiff } from "../../kit/pro/inline-diff";
import { Cap } from "../common/KitPrimitives";

export interface PromptHeadingNode {
  id: string; level: 1 | 2; title: string; line: number; offset: number; children: PromptHeadingNode[];
}

/** Extracts `#` and `##` Markdown headings outside fenced code blocks for `TreeView` section jumping. */
export function parsePromptHeadings(markdown: string): { tree: TreeNode[]; byId: Map<string, { line: number; offset: number }> } {
  const roots: PromptHeadingNode[] = [];
  const byId = new Map<string, { line: number; offset: number }>();
  let inFence = false, offset = 0, lastH1: PromptHeadingNode | null = null;
  markdown.split("\n").forEach((raw, idx) => {
    const trimmed = raw.trimStart();
    if (/^(`{3,}|~{3,})/.test(trimmed)) inFence = !inFence;
    else if (!inFence) {
      const m = /^(#{1,2})\s+(.+?)\s*#*$/.exec(trimmed);
      if (m) {
        const level = m[1].length as 1 | 2, line = idx + 1, id = `sec-${line}`;
        const node: PromptHeadingNode = { id, level, title: m[2].trim(), line, offset, children: [] };
        byId.set(id, { line, offset });
        if (level === 1) { roots.push(node); lastH1 = node; }
        else if (lastH1) lastH1.children.push(node);
        else roots.push(node);
      }
    }
    offset += raw.length + 1;
  });
  const toTree = (nodes: PromptHeadingNode[]): TreeNode[] => nodes.map((n) => ({
    id: n.id, label: `${n.level === 1 ? "#" : "##"} ${n.title}`, meta: `L${n.line}`,
    children: n.children.length ? toTree(n.children) : undefined,
  }));
  return { tree: toTree(roots), byId };
}

/** Detects `***` secret-masking placeholders while ignoring Markdown horizontal rules (`***` line) and bold-italic spans (`***text***`). */
export function hasMaskedPromptValue(text: string): boolean {
  if (!text.includes("***")) return false;
  return text.split("\n").some((line) => {
    if (/^\s*\*{3,}\s*$/.test(line)) return false;
    const withoutBoldItalic = line.replace(/\*{3}[^\s*](?:[^*]*?[^\s*])?\*{3}/g, "");
    return /\*{3,}/.test(withoutBoldItalic);
  });
}

type EditorTab = "edit" | "diff";

interface AgentPromptEditorProps {
  value: string;
  initialValue?: string;
  onChange: (value: string) => void;
  minHeight?: number;
  label: string;
  agentId?: string;
  promptPath?: string | null;
  /** Return `false` to abort navigation (e.g. when user cancels a dirty-form confirmation). */
  onNavigateAudit?: () => boolean | void;
  tr?: (ko: string, en: string) => string;
}

function AuditJumpButton({ agentId, onNavigateAudit, tr }: {
  agentId: string; onNavigateAudit?: () => boolean | void; tr: (ko: string, en: string) => string;
}) {
  const inRouter = useInRouterContext(), navigate = useNavigate();
  const target = `/audit?agent=${encodeURIComponent(agentId)}`;
  const go = () => {
    if (onNavigateAudit?.() === false) return;
    if (inRouter) navigate(target);
    else window.location.assign(target);
  };
  return (
    <Button variant="secondary" size="sm" iconRight={<ExternalLink size={13} aria-hidden />} onClick={go} data-testid="prompt-open-audit">
      {tr("실제 조합 프롬프트 감사 열기", "Open combined prompt audit")}
    </Button>
  );
}

export default function AgentPromptEditor({
  value, initialValue, onChange, minHeight = 300, label, agentId, promptPath, onNavigateAudit, tr = (ko) => ko,
}: AgentPromptEditorProps) {
  const baseline = initialValue ?? value;
  const [mode, setMode] = useState<EditorTab>("edit");
  const [selectedHeading, setSelectedHeading] = useState<string | null>(null);
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);
  const panelId = useId();
  const { tree, byId } = useMemo(() => parsePromptHeadings(value), [value]);
  const hasDiff = value !== baseline;
  const masked = hasMaskedPromptValue(value);

  const handleSelectHeading = (id: string) => {
    setSelectedHeading(id);
    const target = byId.get(id);
    if (!target) return;
    if (mode !== "edit") setMode("edit");
    requestAnimationFrame(() => {
      const el = textareaRef.current;
      if (!el) return;
      el.focus();
      el.setSelectionRange(target.offset, target.offset);
      el.scrollTop = Math.max(0, (target.line - 2) * 20);
    });
  };

  return (
    <div className="flex min-w-0 flex-col gap-2.5" data-testid="agent-prompt-editor">
      <div role="note" data-testid="prompt-canon-banner" className="flex min-w-0 flex-col gap-2 rounded-(--r-md) border border-[color-mix(in_oklch,var(--warn)_45%,var(--line))] bg-[color-mix(in_oklch,var(--warn)_8%,var(--bg-2))] px-3 py-2.5 text-xs leading-5">
        <div className="flex min-w-0 items-start gap-2">
          <AlertTriangle size={15} aria-hidden className="mt-0.5 shrink-0 text-(--warn)" />
          <div className="min-w-0 flex-1 text-(--fg-dim)">
            <strong className="font-semibold text-(--fg)">{tr("정본(source-of-truth) 보호 주의", "Source-of-truth protection notice")}</strong>
            {" — "}
            {tr("공용 지침·기억 정책 정본(~/ObsidianVault/... 및 agents/_shared.prompt.md)은 정본 파일에서만 고쳐요. 저장 전 변경점 검증 탭에서 수정 범위를 확인해 주세요.", "Shared rules and memory policies (~/ObsidianVault/... and agents/_shared.prompt.md) belong in their canonical files. Review changes in the Diff tab before saving.")}
            {promptPath ? <span className="mt-0.5 block font-mono text-[11px] text-(--fg-muted)">{promptPath}</span> : null}
          </div>
        </div>
        {agentId ? <div className="flex flex-wrap items-center justify-end gap-2"><AuditJumpButton agentId={agentId} onNavigateAudit={onNavigateAudit} tr={tr} /></div> : null}
      </div>

      {masked ? (
        <div role="alert" data-testid="prompt-mask-warning" className="flex min-w-0 items-start gap-2 rounded-(--r-md) border border-[color-mix(in_oklch,var(--err)_50%,var(--line))] bg-[color-mix(in_oklch,var(--err)_10%,var(--bg-2))] px-3 py-2 text-xs leading-5 text-(--fg)">
          <ShieldAlert size={15} aria-hidden className="mt-0.5 shrink-0 text-(--err)" />
          <span>{tr("본문에 마스킹 문자열(***)이 포함되어 있어요. 감사 화면 사본을 붙여넣었다면 실제 값이 ***로 덮어써지지 않는지 꼭 확인하세요.", "Prompt contains masked placeholders (***). Verify you are not overwriting real values with *** from an audit copy.")}</span>
        </div>
      ) : null}

      <div className="flex min-w-0 flex-wrap items-center justify-between gap-2">
        <div className="w-full sm:w-64">
          <SegmentedControl<EditorTab>
            kind="tabs" variant="tray" size="sm" fullWidth aria-label={tr("프롬프트 편집 모드", "Prompt editor mode")} value={mode} onChange={setMode}
            options={[
              { value: "edit", label: tr("편집", "Edit"), controls: panelId },
              { value: "diff", label: hasDiff ? tr("변경점 검증 ●", "Verify diff ●") : tr("변경점 검증", "Verify diff"), controls: panelId },
            ]}
          />
        </div>
        <Cap className="tabular-nums">{tr(`${value.length.toLocaleString()}자`, `${value.length.toLocaleString()} chars`)}</Cap>
      </div>

      {tree.length > 0 ? (
        <div data-testid="prompt-heading-toc" className="rounded-(--r-md) border border-(--line-soft) bg-(--bg-2) px-2.5 py-2">
          <Cap className="mb-1 block px-1.5">{tr("섹션 목차 (누르면 이동)", "Section outline (jump)")}</Cap>
          <TreeView aria-label={tr("프롬프트 마크다운 섹션 목차", "Prompt Markdown sections")} items={tree} defaultExpanded={tree.map((i) => i.id)} selected={selectedHeading} onSelect={handleSelectHeading} />
        </div>
      ) : null}

      <div id={panelId} role="tabpanel" className="min-w-0">
        {mode === "edit" ? (
          <textarea ref={textareaRef} aria-label={label} value={value} onChange={(e) => onChange(e.target.value)} style={{ minHeight }} className="w-full resize-y rounded-(--r-md) border border-(--line) bg-(--bg-1) px-3 py-2.5 font-mono text-base leading-5 text-(--fg) outline-none focus-visible:outline-2 focus-visible:outline-offset-[-1px] focus-visible:outline-(--accent) sm:text-xs" />
        ) : (
          <div data-testid="prompt-inline-diff" className="min-w-0">
            <InlineDiff original={baseline} suggested={value} defaultView="split" preserveLines showTools label={tr("저장 전 변경점 검증 (원문 대비 수정본)", "Pre-save diff verification (original vs draft)")} onDone={onChange} />
          </div>
        )}
      </div>
    </div>
  );
}
