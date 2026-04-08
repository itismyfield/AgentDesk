import MarkdownContent from "../common/MarkdownContent";
import type { KanbanCard } from "../../types";
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
    return (
      <div className="space-y-1">
        <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("설명", "Description")}</span>
        {editor.description ? (
          <div
            className="rounded-2xl border p-4 bg-surface-subtle text-sm"
            style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-primary)" }}
          >
            <MarkdownContent content={editor.description} />
          </div>
        ) : (
          <div className="rounded-xl border border-dashed px-3 py-4 text-xs text-center" style={{ borderColor: "rgba(148,163,184,0.24)", color: "var(--th-text-muted)" }}>
            {tr("설명이 없습니다.", "No description.")}
          </div>
        )}
      </div>
    );
  }

  const isGitHubLinked = Boolean(card.github_issue_number);

  // Structured view for PMD-format issues
  return (
    <div className="space-y-3">
      {/* Background */}
      {parsed.background && (
        <div className="rounded-2xl border p-4 bg-surface-subtle" style={{ borderColor: "var(--th-border-subtle)" }}>
          <div className="text-xs font-semibold uppercase tracking-widest mb-2" style={{ color: "var(--th-text-muted)" }}>
            {tr("배경", "Background")}
          </div>
          <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
            <MarkdownContent content={parsed.background} />
          </div>
        </div>
      )}

      {/* Content */}
      {parsed.content && (
        <div className="rounded-2xl border p-4 bg-surface-subtle" style={{ borderColor: "var(--th-border-subtle)" }}>
          <div className="text-xs font-semibold uppercase tracking-widest mb-2" style={{ color: "var(--th-text-muted)" }}>
            {tr("내용", "Content")}
          </div>
          <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
            <MarkdownContent content={parsed.content} />
          </div>
        </div>
      )}

      {/* DoD Checklist */}
      {editor.review_checklist.length > 0 && (
        <div className="rounded-2xl border p-4 bg-surface-subtle space-y-3" style={{ borderColor: "rgba(20,184,166,0.3)" }}>
          <div className="flex items-center justify-between gap-3">
            <div className="text-xs font-semibold uppercase tracking-widest" style={{ color: "#2dd4bf" }}>
              DoD (Definition of Done)
              {isGitHubLinked && (
                <span className="ml-2 text-xs font-normal normal-case tracking-normal" style={{ color: "var(--th-text-muted)" }}>
                  {tr("(GitHub 정본)", "(synced from GitHub)")}
                </span>
              )}
            </div>
            <span className="text-xs px-2 py-1 rounded-full bg-surface-medium" style={{ color: "var(--th-text-secondary)" }}>
              {editor.review_checklist.filter((item) => item.done).length}/{editor.review_checklist.length}
            </span>
          </div>
          <div className="space-y-2">
            {editor.review_checklist.map((item) => (
              <label
                key={item.id}
                className="flex items-center gap-3 rounded-xl px-3 py-2"
                style={{ backgroundColor: "rgba(255,255,255,0.04)", opacity: isGitHubLinked ? 0.85 : 1 }}
              >
                <input
                  type="checkbox"
                  checked={item.done}
                  disabled={isGitHubLinked}
                  onChange={isGitHubLinked ? undefined : (event) => setEditor((prev) => ({
                    ...prev,
                    review_checklist: prev.review_checklist.map((current) =>
                      current.id === item.id ? { ...current, done: event.target.checked } : current,
                    ),
                  }))}
                />
                <span
                  className="min-w-0 flex-1 text-sm"
                  style={{
                    color: item.done ? "var(--th-text-secondary)" : "var(--th-text-primary)",
                    textDecoration: item.done ? "line-through" : "none",
                  }}
                >
                  {item.label}
                </span>
              </label>
            ))}
          </div>
        </div>
      )}

      {/* Dependencies */}
      {parsed.dependencies && (
        <div className="rounded-2xl border p-3 bg-surface-subtle" style={{ borderColor: "rgba(96,165,250,0.25)" }}>
          <div className="text-xs font-semibold uppercase tracking-widest mb-1" style={{ color: "#93c5fd" }}>
            {tr("의존성", "Dependencies")}
          </div>
          <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
            <MarkdownContent content={parsed.dependencies} />
          </div>
        </div>
      )}

      {/* Risks */}
      {parsed.risks && (
        <div className="rounded-2xl border p-3" style={{ borderColor: "rgba(239,68,68,0.25)", backgroundColor: "rgba(127,29,29,0.12)" }}>
          <div className="text-xs font-semibold uppercase tracking-widest mb-1" style={{ color: "#fca5a5" }}>
            {tr("리스크", "Risks")}
          </div>
          <div className="text-sm" style={{ color: "#fecaca" }}>
            <MarkdownContent content={parsed.risks} />
          </div>
        </div>
      )}
    </div>
  );
}
