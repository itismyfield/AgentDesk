import type { GitHubIssue } from "../../api";
import type { UiLanguage } from "../../types";
import MarkdownContent from "../common/MarkdownContent";
import { SurfaceActionButton, SurfaceCard, SurfaceEmptyState, SurfaceNotice } from "../common/SurfacePrimitives";
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
    <div className="fixed inset-0 z-50 flex items-end justify-center sm:items-center p-0 sm:p-4" style={{ backgroundColor: "var(--th-modal-overlay)" }} onClick={onClose}>
      <div
        onClick={(e) => e.stopPropagation()}
        className="w-full max-w-3xl max-h-[88svh] overflow-y-auto rounded-t-3xl border p-5 sm:max-h-[90vh] sm:rounded-3xl sm:p-6 space-y-4"
        style={{
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
          borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
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
          <SurfaceActionButton onClick={onClose} tone="neutral" className="shrink-0">
            {tr("닫기", "Close")}
          </SurfaceActionButton>
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
          <SurfaceCard className="p-3" style={{ background: "color-mix(in srgb, var(--th-bg-surface) 82%, var(--th-card-bg) 18%)" }}>
            <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("생성", "Created")}</div>
            <div style={{ color: "var(--th-text-primary)" }}>{formatIso(issue.createdAt, locale)}</div>
          </SurfaceCard>
          <SurfaceCard className="p-3" style={{ background: "color-mix(in srgb, var(--th-bg-surface) 82%, var(--th-card-bg) 18%)" }}>
            <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("업데이트", "Updated")}</div>
            <div style={{ color: "var(--th-text-primary)" }}>{formatIso(issue.updatedAt, locale)}</div>
          </SurfaceCard>
        </div>

        {(() => {
          const parsed = parseIssueSections(issue.body);
          if (!parsed) {
            // Fallback: non-PMD format
            return issue.body ? (
              <SurfaceCard className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                <MarkdownContent content={issue.body} />
              </SurfaceCard>
            ) : (
              <SurfaceEmptyState className="px-3 py-4 text-xs text-center">
                {tr("이슈 본문이 없습니다.", "No issue body.")}
              </SurfaceEmptyState>
            );
          }
          // Structured view for PMD-format issues
          return (
            <div className="space-y-3">
              {parsed.background && (
                <SurfaceCard>
                  <div className="text-xs font-semibold uppercase tracking-widest mb-2" style={{ color: "var(--th-text-muted)" }}>
                    {tr("배경", "Background")}
                  </div>
                  <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                    <MarkdownContent content={parsed.background} />
                  </div>
                </SurfaceCard>
              )}
              {parsed.content && (
                <SurfaceCard>
                  <div className="text-xs font-semibold uppercase tracking-widest mb-2" style={{ color: "var(--th-text-muted)" }}>
                    {tr("내용", "Content")}
                  </div>
                  <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                    <MarkdownContent content={parsed.content} />
                  </div>
                </SurfaceCard>
              )}
              {parsed.dodItems.length > 0 && (
                <SurfaceNotice tone="success" className="block space-y-2 p-4" leading={<span className="mt-1 text-sm">✓</span>}>
                  <div className="text-xs font-semibold uppercase tracking-widest" style={{ color: "var(--th-accent-primary)" }}>
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
                </SurfaceNotice>
              )}
              {parsed.dependencies && (
                <SurfaceNotice tone="info" className="block p-3" leading={<span className="mt-1 text-sm">↗</span>}>
                  <div className="text-xs font-semibold uppercase tracking-widest mb-1" style={{ color: "#93c5fd" }}>
                    {tr("의존성", "Dependencies")}
                  </div>
                  <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                    <MarkdownContent content={parsed.dependencies} />
                  </div>
                </SurfaceNotice>
              )}
              {parsed.risks && (
                <SurfaceNotice tone="danger" className="block p-3" leading={<span className="mt-1 text-sm">!</span>}>
                  <div className="text-xs font-semibold uppercase tracking-widest mb-1" style={{ color: "#fca5a5" }}>
                    {tr("리스크", "Risks")}
                  </div>
                  <div className="text-sm" style={{ color: "#fecaca" }}>
                    <MarkdownContent content={parsed.risks} />
                  </div>
                </SurfaceNotice>
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
            <SurfaceActionButton
              onClick={() => {
                onClose();
                void onCloseIssue(issue);
              }}
              disabled={closingIssueNumber === issue.number}
              tone="neutral"
            >
              {closingIssueNumber === issue.number ? tr("닫는 중", "Closing") : tr("이슈 닫기", "Close issue")}
            </SurfaceActionButton>
            <SurfaceActionButton
              onClick={() => {
                onClose();
                onAssign(issue);
              }}
            >
              {tr("할당", "Assign")}
            </SurfaceActionButton>
          </div>
        </div>
      </div>
    </div>
  );
}
