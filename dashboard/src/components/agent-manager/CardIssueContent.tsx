import type { ReactNode } from "react";
import MarkdownContent from "../common/MarkdownContent";
import type { KanbanCard } from "../../types";
import { Badge } from "../../kit/free/badge-clean";
import { Checkbox } from "../../kit/free/checkbox-clean";
import { DetailHeading } from "./CardDetailParts";
import {
  parseIssueSections,
  type EditorState,
} from "./kanban-utils";

// ---------------------------------------------------------------------------
// Props
// ---------------------------------------------------------------------------

export interface CardIssueContentProps {
  card: KanbanCard;
  editor: EditorState;
  setEditor: React.Dispatch<React.SetStateAction<EditorState>>;
  tr: (ko: string, en: string) => string;
}

const SECTION_TONE = {
  plain: "bg-surface-subtle shadow-[inset_0_0_0_0.5px_var(--th-border-subtle)]",
  info: "bg-[color-mix(in_oklch,var(--info)_8%,transparent)] shadow-[inset_0_0_0_0.5px_color-mix(in_oklch,var(--info)_36%,transparent)]",
  danger: "bg-[color-mix(in_oklch,var(--err)_8%,transparent)] shadow-[inset_0_0_0_0.5px_color-mix(in_oklch,var(--err)_36%,transparent)]",
} as const;

function IssueSection({ title, tone = "plain", aside, children }: { title: ReactNode; tone?: keyof typeof SECTION_TONE; aside?: ReactNode; children: ReactNode }) {
  return (
    <section className="flex min-w-0 flex-col gap-2">
      <DetailHeading aside={aside}>{title}</DetailHeading>
      <div className={`min-w-0 rounded-xl px-4 py-3 text-sm text-th-text-primary [overflow-wrap:anywhere] ${SECTION_TONE[tone]}`}>{children}</div>
    </section>
  );
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export default function CardIssueContent({
  card,
  editor,
  setEditor,
  tr,
}: CardIssueContentProps) {
  const parsed = parseIssueSections(editor.description);

  if (!parsed) {
    // Fallback: non-PMD format -> show as markdown
    return editor.description ? (
      <IssueSection title={tr("설명", "Description")}>
        <MarkdownContent content={editor.description} />
      </IssueSection>
    ) : (
      <section className="flex min-w-0 flex-col gap-2">
        <DetailHeading>{tr("설명", "Description")}</DetailHeading>
        <p className="m-0 rounded-xl border border-dashed border-th-border-subtle px-3 py-4 text-center text-xs text-th-text-muted">
          {tr("설명이 없습니다.", "No description.")}
        </p>
      </section>
    );
  }

  const isGitHubLinked = Boolean(card.github_issue_number);
  const doneCount = editor.review_checklist.filter((item) => item.done).length;

  // Structured view for PMD-format issues
  return (
    <div className="flex min-w-0 flex-col gap-4">
      {parsed.background && (
        <IssueSection title={tr("배경", "Background")}>
          <MarkdownContent content={parsed.background} />
        </IssueSection>
      )}

      {parsed.content && (
        <IssueSection title={tr("내용", "Content")}>
          <MarkdownContent content={parsed.content} />
        </IssueSection>
      )}

      {editor.review_checklist.length > 0 && (
        <section className="flex min-w-0 flex-col gap-2" data-testid="card-dod">
          <DetailHeading aside={<Badge tone={doneCount === editor.review_checklist.length ? "success" : "neutral"}>{`${doneCount}/${editor.review_checklist.length}`}</Badge>}>
            DoD (Definition of Done)
            {isGitHubLinked && (
              <span className="ml-2 font-normal text-th-text-muted">
                {tr("(GitHub 정본)", "(synced from GitHub)")}
              </span>
            )}
          </DetailHeading>
          <ul className="m-0 flex list-none flex-col gap-1.5 p-0">
            {editor.review_checklist.map((item) => (
              <li key={item.id} className="min-w-0">
                <Checkbox
                  variant="card"
                  label={<span className={`block py-0.5 font-normal [overflow-wrap:anywhere] ${item.done ? "text-th-text-secondary line-through" : "text-th-text-primary"}`}>{item.label}</span>}
                  checked={item.done}
                  disabled={isGitHubLinked}
                  onChange={isGitHubLinked ? undefined : (event) => setEditor((prev) => ({
                    ...prev,
                    review_checklist: prev.review_checklist.map((current) =>
                      current.id === item.id ? { ...current, done: event.target.checked } : current,
                    ),
                  }))}
                />
              </li>
            ))}
          </ul>
        </section>
      )}

      {parsed.dependencies && (
        <IssueSection title={tr("의존성", "Dependencies")} tone="info">
          <MarkdownContent content={parsed.dependencies} />
        </IssueSection>
      )}

      {parsed.risks && (
        <IssueSection title={tr("리스크", "Risks")} tone="danger">
          <MarkdownContent content={parsed.risks} />
        </IssueSection>
      )}
    </div>
  );
}
