import type { GitHubIssue } from "../../api";
import type { UiLanguage } from "../../types";
import MarkdownContent from "../common/MarkdownContent";
import { formatIso, parseIssueSections } from "./kanban-utils";

interface BacklogIssueDetailProps {
  issue: GitHubIssue;
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  closingIssueNumber: number | null;
  onClose: () => void;
  onCloseIssue: (issue: GitHubIssue) => void;
  onAssign: (issue: GitHubIssue) => void;
}

export default function BacklogIssueDetail({
  issue,
  tr,
  locale,
  closingIssueNumber,
  onClose,
  onCloseIssue,
  onAssign,
}: BacklogIssueDetailProps) {
  return (
    <div className="fixed inset-0 z-50 backdrop-blur-sm flex items-end justify-center sm:items-center p-0 sm:p-4" style={{ backgroundColor: "var(--th-modal-overlay)" }} onClick={onClose}>
      <div
        onClick={(e) => e.stopPropagation()}
        className="w-full max-w-3xl max-h-[88svh] overflow-y-auto rounded-t-3xl border p-5 sm:max-h-[90vh] sm:rounded-3xl sm:p-6 space-y-4"
        style={{
          backgroundColor: "var(--th-bg-surface)",
          borderColor: "rgba(148,163,184,0.24)",
          paddingBottom: "max(6rem, calc(6rem + env(safe-area-inset-bottom)))",
        }}
        role="dialog" aria-modal="true" aria-label="Backlog issue details"
      >
        <div className="flex items-start justify-between gap-3">
          <div>
            <div className="flex flex-wrap items-center gap-2">
              <span className="px-2 py-0.5 rounded-full text-xs bg-surface-medium" style={{ color: "var(--th-text-secondary)" }}>
                #{issue.number}
              </span>
              <span className="px-2 py-0.5 rounded-full text-xs" style={{ backgroundColor: "#64748b33", color: "#64748b" }}>
                {tr("백로그", "Backlog")}
              </span>
              {issue.labels.map((label) => (
                <span
                  key={label.name}
                  className="px-2 py-0.5 rounded-full text-xs"
                  style={{ backgroundColor: `#${label.color}22`, color: `#${label.color}` }}
                >
                  {label.name}
                </span>
              ))}
            </div>
            <h3 className="mt-2 text-xl font-semibold" style={{ color: "var(--th-text-heading)" }}>
              {issue.title}
            </h3>
          </div>
          <button
            onClick={onClose}
            className="rounded-xl px-3 py-2 text-sm bg-surface-medium shrink-0"
            style={{ color: "var(--th-text-secondary)" }}
          >
            {tr("닫기", "Close")}
          </button>
        </div>

        {issue.assignees.length > 0 && (
          <div className="flex items-center gap-2 text-sm" style={{ color: "var(--th-text-secondary)" }}>
            <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("담당자", "Assignees")}:</span>
            {issue.assignees.map((a) => (
              <span key={a.login} className="px-2 py-0.5 rounded-full text-xs bg-surface-medium">{a.login}</span>
            ))}
          </div>
        )}

        <div className="grid gap-3 md:grid-cols-2 text-sm">
          <div className="rounded-2xl border p-3 bg-surface-subtle" style={{ borderColor: "var(--th-border-subtle)" }}>
            <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("생성", "Created")}</div>
            <div style={{ color: "var(--th-text-primary)" }}>{formatIso(issue.createdAt, locale)}</div>
          </div>
          <div className="rounded-2xl border p-3 bg-surface-subtle" style={{ borderColor: "var(--th-border-subtle)" }}>
            <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("업데이트", "Updated")}</div>
            <div style={{ color: "var(--th-text-primary)" }}>{formatIso(issue.updatedAt, locale)}</div>
          </div>
        </div>

        {(() => {
          const parsed = parseIssueSections(issue.body);
          if (!parsed) {
            // Fallback: non-PMD format
            return issue.body ? (
              <div
                className="rounded-2xl border p-4 bg-surface-subtle text-sm"
                style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-primary)" }}
              >
                <MarkdownContent content={issue.body} />
              </div>
            ) : (
              <div className="rounded-xl border border-dashed px-3 py-4 text-xs text-center" style={{ borderColor: "rgba(148,163,184,0.24)", color: "var(--th-text-muted)" }}>
                {tr("이슈 본문이 없습니다.", "No issue body.")}
              </div>
            );
          }
          // Structured view for PMD-format issues
          return (
            <div className="space-y-3">
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
              {parsed.dodItems.length > 0 && (
                <div className="rounded-2xl border p-4 bg-surface-subtle space-y-2" style={{ borderColor: "rgba(20,184,166,0.3)" }}>
                  <div className="text-xs font-semibold uppercase tracking-widest" style={{ color: "#2dd4bf" }}>
                    DoD (Definition of Done)
                  </div>
                  <div className="space-y-1.5">
                    {parsed.dodItems.map((item, idx) => (
                      <div key={idx} className="flex items-center gap-2 text-sm" style={{ color: "var(--th-text-primary)" }}>
                        <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>☐</span>
                        {item}
                      </div>
                    ))}
                  </div>
                </div>
              )}
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
        })()}

        <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
          <a
            href={issue.url}
            target="_blank"
            rel="noreferrer"
            className="rounded-xl px-4 py-2 text-sm text-center hover:underline"
            style={{ color: "#93c5fd" }}
          >
            {tr("GitHub에서 보기", "View on GitHub")}
          </a>
          <div className="flex flex-col-reverse gap-2 sm:flex-row">
            <button
              onClick={() => {
                onClose();
                void onCloseIssue(issue);
              }}
              disabled={closingIssueNumber === issue.number}
              className="rounded-xl px-4 py-2 text-sm border disabled:opacity-50"
              style={{ borderColor: "rgba(148,163,184,0.28)", color: "var(--th-text-muted)" }}
            >
              {closingIssueNumber === issue.number ? tr("닫는 중", "Closing") : tr("이슈 닫기", "Close issue")}
            </button>
            <button
              onClick={() => {
                onClose();
                onAssign(issue);
              }}
              className="rounded-xl px-4 py-2 text-sm font-medium text-white"
              style={{ backgroundColor: "#2563eb" }}
            >
              {tr("할당", "Assign")}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
