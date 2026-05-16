import { useEffect } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import * as api from "../../api";
import type { GitHubIssue, KanbanRepoSource } from "../../api";

const kanbanRepoSourcesQueryKey = ["kanban", "repo-sources"] as const;
const kanbanAvailableReposQueryKey = ["kanban", "available-repos"] as const;
const kanbanRepoIssuesQueryKey = (repo: string) =>
  ["kanban", "repo-issues", repo] as const;

interface UseKanbanRepoDataParams {
  repoInput: string;
  selectedRepo: string;
  setActionError: (message: string | null) => void;
  setClosingIssueNumber: (issueNumber: number | null) => void;
  setRepoBusy: (busy: boolean) => void;
  setRepoInput: (repo: string) => void;
  setSelectedRepo: (repo: string) => void;
  tr: (ko: string, en: string) => string;
}

export function useKanbanRepoData({
  repoInput,
  selectedRepo,
  setActionError,
  setClosingIssueNumber,
  setRepoBusy,
  setRepoInput,
  setSelectedRepo,
  tr,
}: UseKanbanRepoDataParams) {
  const queryClient = useQueryClient();
  const repoSourcesQuery = useQuery({
    queryKey: kanbanRepoSourcesQueryKey,
    queryFn: () => api.getKanbanRepoSources(),
    staleTime: 60_000,
  });
  const availableReposQuery = useQuery({
    queryKey: kanbanAvailableReposQueryKey,
    queryFn: () => api.getGitHubRepos().then((result) => result.repos),
    staleTime: 5 * 60_000,
  });
  const repoIssuesQuery = useQuery({
    queryKey: selectedRepo
      ? kanbanRepoIssuesQueryKey(selectedRepo)
      : ["kanban", "repo-issues", "none"],
    queryFn: () => api.getGitHubIssues(selectedRepo, "open", 100),
    enabled: Boolean(selectedRepo),
    staleTime: 30_000,
  });

  const repoSources = repoSourcesQuery.data ?? [];
  const availableRepos = availableReposQuery.data ?? [];
  const issues = repoIssuesQuery.data?.issues ?? [];
  const loadingIssues = repoIssuesQuery.isFetching;
  const initialLoading = repoSourcesQuery.isLoading || availableReposQuery.isLoading;

  useEffect(() => {
    if (!selectedRepo && repoSources[0]?.repo) {
      setSelectedRepo(repoSources[0].repo);
      return;
    }
    if (selectedRepo && !repoSources.some((source) => source.repo === selectedRepo)) {
      setSelectedRepo(repoSources[0]?.repo ?? "");
    }
  }, [repoSources, selectedRepo, setSelectedRepo]);

  useEffect(() => {
    if (repoIssuesQuery.error) {
      setActionError(repoIssuesQuery.error instanceof Error
        ? repoIssuesQuery.error.message
        : "Failed to load GitHub issues.");
      return;
    }
    if (repoIssuesQuery.data?.error) {
      setActionError(repoIssuesQuery.data.error);
    }
  }, [repoIssuesQuery.data?.error, repoIssuesQuery.error, setActionError]);

  const handleAddRepo = async () => {
    const repo = repoInput.trim();
    if (!repo) return;
    setRepoBusy(true);
    setActionError(null);
    try {
      const created = await api.addKanbanRepoSource(repo);
      queryClient.setQueryData<KanbanRepoSource[]>(kanbanRepoSourcesQueryKey, (prev = []) =>
        prev.some((source) => source.id === created.id) ? prev : [...prev, created],
      );
      setSelectedRepo(created.repo);
      setRepoInput("");
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("repo 추가에 실패했습니다.", "Failed to add repo."));
    } finally {
      setRepoBusy(false);
    }
  };

  const handleRemoveRepo = async (source: KanbanRepoSource) => {
    const confirmed = window.confirm(tr(
      `이 backlog source를 제거할까요? 저장된 카드 자체는 남습니다.\n${source.repo}`,
      `Remove this backlog source? Existing cards stay intact.\n${source.repo}`,
    ));
    if (!confirmed) return;
    setRepoBusy(true);
    setActionError(null);
    try {
      await api.deleteKanbanRepoSource(source.id);
      queryClient.setQueryData<KanbanRepoSource[]>(kanbanRepoSourcesQueryKey, (prev = []) =>
        prev.filter((item) => item.id !== source.id),
      );
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("repo 제거에 실패했습니다.", "Failed to remove repo."));
    } finally {
      setRepoBusy(false);
    }
  };

  const updateRepoDefaultAgent = (source: KanbanRepoSource, defaultAgentId: string | null) => {
    void api.updateKanbanRepoSource(source.id, { default_agent_id: defaultAgentId });
    queryClient.setQueryData<KanbanRepoSource[]>(kanbanRepoSourcesQueryKey, (prev = []) =>
      prev.map((item) => (
        item.id === source.id ? { ...item, default_agent_id: defaultAgentId } : item
      )),
    );
  };

  const handleCloseIssue = async (issue: GitHubIssue) => {
    if (!selectedRepo) return;
    setClosingIssueNumber(issue.number);
    setActionError(null);
    try {
      await api.closeGitHubIssue(selectedRepo, issue.number);
      queryClient.setQueryData<Awaited<ReturnType<typeof api.getGitHubIssues>>>(
        kanbanRepoIssuesQueryKey(selectedRepo),
        (prev) => prev ? {
          ...prev,
          issues: prev.issues.filter((item) => item.number !== issue.number),
        } : prev,
      );
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("이슈 닫기에 실패했습니다.", "Failed to close issue."));
    } finally {
      setClosingIssueNumber(null);
    }
  };

  return {
    availableRepos,
    handleAddRepo,
    handleCloseIssue,
    handleRemoveRepo,
    initialLoading,
    issues,
    loadingIssues,
    repoSources,
    updateRepoDefaultAgent,
  };
}
