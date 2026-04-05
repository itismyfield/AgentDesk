import type { GitHubRepoOption, KanbanRepoSource } from "../../api";
import * as api from "../../api";
import type { Agent, Department, UiLanguage } from "../../types";
import { localeName } from "../../i18n";

interface KanbanSettingsProps {
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  repoSources: KanbanRepoSource[];
  selectedRepo: string;
  availableRepos: GitHubRepoOption[];
  agents: Agent[];
  departments: Department[];
  showClosed: boolean;
  agentFilter: string;
  deptFilter: string;
  cardTypeFilter: "all" | "issue" | "review";
  search: string;
  repoInput: string;
  repoBusy: boolean;
  setSelectedRepo: (repo: string) => void;
  setShowClosed: (v: boolean) => void;
  setAgentFilter: (v: string) => void;
  setDeptFilter: (v: string) => void;
  setCardTypeFilter: (v: "all" | "issue" | "review") => void;
  setSearch: (v: string) => void;
  setRepoInput: (v: string) => void;
  onAddRepo: () => void;
  onRemoveRepo: (source: KanbanRepoSource) => void;
  onUpdateRepoSource: (sourceId: string, patch: { default_agent_id: string | null }) => void;
  getAgentLabel: (agentId: string | null | undefined) => string;
}

export default function KanbanSettings({
  tr,
  locale,
  repoSources,
  selectedRepo,
  availableRepos,
  agents,
  departments,
  showClosed,
  agentFilter,
  deptFilter,
  cardTypeFilter,
  search,
  repoInput,
  repoBusy,
  setSelectedRepo,
  setShowClosed,
  setAgentFilter,
  setDeptFilter,
  setCardTypeFilter,
  setSearch,
  setRepoInput,
  onAddRepo,
  onRemoveRepo,
  onUpdateRepoSource,
  getAgentLabel,
}: KanbanSettingsProps) {
  return (
    <div className="space-y-3 min-w-0 overflow-hidden">
      <div className="flex flex-wrap gap-2">
        {repoSources.length === 0 && (
          <span className="px-3 py-2 rounded-xl text-sm border border-dashed" style={{ borderColor: "rgba(148,163,184,0.28)", color: "var(--th-text-muted)" }}>
            {tr("먼저 backlog repo를 추가하세요.", "Add a backlog repo first.")}
          </span>
        )}
        {repoSources.map((source) => (
          <div
            key={source.id}
            className={`inline-flex items-center gap-2 rounded-xl px-3 py-2 border text-sm ${selectedRepo === source.repo ? "bg-blue-500/20" : "bg-surface-light"}`}
            style={{ borderColor: selectedRepo === source.repo ? "rgba(96,165,250,0.45)" : "rgba(148,163,184,0.22)" }}
          >
            <button
              onClick={() => setSelectedRepo(source.repo)}
              className="text-left truncate"
              style={{ color: selectedRepo === source.repo ? "#dbeafe" : "var(--th-text-primary)" }}
            >
              {source.repo}
            </button>
            <button
              onClick={() => void onRemoveRepo(source)}
              disabled={repoBusy}
              className="text-xs"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr("삭제", "Remove")}
            </button>
          </div>
        ))}
      </div>

      <div className="grid gap-2 sm:grid-cols-[minmax(0,1fr)_auto]">
        <input
          list="kanban-repo-options"
          value={repoInput}
          onChange={(event) => setRepoInput(event.target.value)}
          placeholder={tr("owner/repo 입력 또는 선택", "Type or pick owner/repo")}
          className="min-w-0 rounded-xl px-3 py-2 text-sm bg-surface-subtle border"
          style={{ borderColor: "rgba(148,163,184,0.28)", color: "var(--th-text-primary)" }}
        />
        <datalist id="kanban-repo-options">
          {availableRepos.map((repo) => (
            <option key={repo.nameWithOwner} value={repo.nameWithOwner} />
          ))}
        </datalist>
        <button
          onClick={() => void onAddRepo()}
          disabled={repoBusy || !repoInput.trim()}
          className="rounded-xl px-4 py-2 text-sm font-medium text-white disabled:opacity-50 w-full sm:w-auto"
          style={{ backgroundColor: "#2563eb" }}
        >
          {repoBusy ? tr("처리 중", "Working") : tr("Repo 추가", "Add repo")}
        </button>
      </div>

      <div className="flex flex-col gap-2 w-full">
        <label className="flex items-center gap-2 rounded-xl px-3 py-2 text-sm border bg-surface-subtle" style={{ borderColor: "rgba(148,163,184,0.28)", color: "var(--th-text-secondary)" }}>
          <input
            type="checkbox"
            checked={showClosed}
            onChange={(event) => setShowClosed(event.target.checked)}
          />
          {tr("닫힌 컬럼 표시", "Show closed columns")}
        </label>
        {selectedRepo && (() => {
          const currentSource = repoSources.find((s) => s.repo === selectedRepo);
          if (!currentSource) return null;
          return (
            <label className="flex items-center gap-2 rounded-xl px-3 py-2 text-sm border bg-surface-subtle" style={{ borderColor: "rgba(148,163,184,0.28)", color: "var(--th-text-secondary)" }}>
              <span className="shrink-0">{tr("기본 담당자", "Default agent")}</span>
              <select
                value={currentSource.default_agent_id ?? ""}
                onChange={(event) => {
                  const value = event.target.value || null;
                  onUpdateRepoSource(currentSource.id, { default_agent_id: value });
                }}
                className="min-w-0 flex-1 rounded-lg px-2 py-1 text-xs bg-surface-light border"
                style={{ borderColor: "rgba(148,163,184,0.2)", color: "var(--th-text-primary)" }}
              >
                <option value="">{tr("없음", "None")}</option>
                {agents.map((agent) => (
                  <option key={agent.id} value={agent.id}>{getAgentLabel(agent.id)}</option>
                ))}
              </select>
            </label>
          );
        })()}
      </div>

      <div className="grid gap-2 md:grid-cols-3">
        <input
          value={search}
          onChange={(event) => setSearch(event.target.value)}
          placeholder={tr("제목 / 설명 / 담당자 검색", "Search title / description / assignee")}
          className="rounded-xl px-3 py-2 text-sm bg-surface-subtle border"
          style={{ borderColor: "rgba(148,163,184,0.28)", color: "var(--th-text-primary)" }}
        />
        <select
          value={agentFilter}
          onChange={(event) => setAgentFilter(event.target.value)}
          className="rounded-xl px-3 py-2 text-sm bg-surface-subtle border"
          style={{ borderColor: "rgba(148,163,184,0.28)", color: "var(--th-text-primary)" }}
        >
          <option value="all">{tr("전체 에이전트", "All agents")}</option>
          {agents.map((agent) => (
            <option key={agent.id} value={agent.id}>{getAgentLabel(agent.id)}</option>
          ))}
        </select>
        <select
          value={deptFilter}
          onChange={(event) => setDeptFilter(event.target.value)}
          className="rounded-xl px-3 py-2 text-sm bg-surface-subtle border"
          style={{ borderColor: "rgba(148,163,184,0.28)", color: "var(--th-text-primary)" }}
        >
          <option value="all">{tr("전체 부서", "All departments")}</option>
          {departments.map((department) => (
            <option key={department.id} value={department.id}>{localeName(locale, department)}</option>
          ))}
        </select>
        <select
          value={cardTypeFilter}
          onChange={(event) => setCardTypeFilter(event.target.value as "all" | "issue" | "review")}
          className="rounded-xl px-3 py-2 text-sm bg-surface-subtle border"
          style={{ borderColor: "rgba(148,163,184,0.28)", color: "var(--th-text-primary)" }}
        >
          <option value="all">{tr("전체 카드", "All cards")}</option>
          <option value="issue">{tr("이슈만", "Issues only")}</option>
          <option value="review">{tr("리뷰만", "Reviews only")}</option>
        </select>
      </div>
    </div>
  );
}
