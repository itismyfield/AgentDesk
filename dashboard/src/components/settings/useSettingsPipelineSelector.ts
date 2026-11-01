import { useCallback, useEffect, useState } from "react";
import type { GitHubRepoOption } from "../../api";
import * as api from "../../api";
import type { Agent } from "../../types";
import {
  getCachedPipelineAgentEntry,
  getCachedPipelineRepoEntry,
  isCacheFresh,
  PIPELINE_SELECTOR_REFRESH_TIMEOUT_MS,
  selectDefaultPipelineRepo,
  writeStoredPipelineAgentCache,
  writeStoredPipelineRepoCache,
  type PipelineAgentCacheEntry,
  type PipelineRepoCacheEntry,
  type SettingsNotificationType,
} from "./SettingsModel";

type UseSettingsPipelineSelectorArgs = {
  enabled: boolean;
  notify: (ko: string, en: string, type?: SettingsNotificationType) => void;
  tr: (ko: string, en: string) => string;
};

export function useSettingsPipelineSelector({
  enabled,
  notify,
  tr,
}: UseSettingsPipelineSelectorArgs) {
  const [pipelineRepos, setPipelineRepos] = useState<GitHubRepoOption[]>([]);
  const [pipelineAgents, setPipelineAgents] = useState<Agent[]>([]);
  const [selectedPipelineRepo, setSelectedPipelineRepo] = useState("");
  const [selectedPipelineAgentId, setSelectedPipelineAgentId] = useState<string | null>(null);
  const [pipelineSelectorLoading, setPipelineSelectorLoading] = useState(false);
  const [pipelineSelectorError, setPipelineSelectorError] = useState<string | null>(null);

  const applyPipelineRepoCache = useCallback((cache: PipelineRepoCacheEntry) => {
    setPipelineRepos(cache.repos);
    setSelectedPipelineRepo((current) => {
      if (current && cache.repos.some((repo) => repo.nameWithOwner === current)) {
        return current;
      }
      return selectDefaultPipelineRepo(cache.repos, cache.viewerLogin);
    });
  }, []);

  const applyPipelineAgentCache = useCallback((cache: PipelineAgentCacheEntry) => {
    setPipelineAgents(cache.agents);
    setSelectedPipelineAgentId((current) => (
      current && cache.agents.some((agent) => agent.id === current) ? current : null
    ));
  }, []);

  useEffect(() => {
    if (!enabled) {
      return;
    }
    let stale = false;
    const cachedRepoEntry = getCachedPipelineRepoEntry();
    const cachedAgentEntry = getCachedPipelineAgentEntry();
    const hasCachedRepos = (cachedRepoEntry?.repos.length ?? 0) > 0;
    const shouldRefreshRepos = !isCacheFresh(cachedRepoEntry);
    const shouldRefreshAgents = !isCacheFresh(cachedAgentEntry);

    if (cachedRepoEntry) {
      applyPipelineRepoCache(cachedRepoEntry);
      setPipelineSelectorError(null);
    }
    if (cachedAgentEntry) {
      applyPipelineAgentCache(cachedAgentEntry);
    }

    if (!shouldRefreshRepos && !shouldRefreshAgents) {
      return;
    }

    setPipelineSelectorLoading(true);
    if (!hasCachedRepos) {
      setPipelineSelectorError(null);
    }

    const repoPromise = shouldRefreshRepos
      ? api.getGitHubRepos({
          timeoutMs: PIPELINE_SELECTOR_REFRESH_TIMEOUT_MS,
          maxRetries: 0,
        })
      : Promise.resolve(null);
    const agentPromise = shouldRefreshAgents
      ? api.getAgents({
          timeoutMs: PIPELINE_SELECTOR_REFRESH_TIMEOUT_MS,
          maxRetries: 0,
        })
      : Promise.resolve(null);

    void Promise.allSettled([repoPromise, agentPromise])
      .then(([repoResult, agentResult]) => {
        if (stale) return;

        if (repoResult.status === "fulfilled" && repoResult.value) {
          const nextRepoCache: PipelineRepoCacheEntry = {
            viewerLogin: repoResult.value.viewer_login,
            repos: repoResult.value.repos,
            fetchedAt: Date.now(),
          };
          applyPipelineRepoCache(nextRepoCache);
          writeStoredPipelineRepoCache(nextRepoCache);
          setPipelineSelectorError(null);
        } else if (!hasCachedRepos) {
          setPipelineSelectorError(
            tr(
              "편집기에서 쓸 저장소 목록을 불러오지 못했어요. 잠시 뒤 다시 열어 주세요.",
              "Could not load repositories for the editor. Try opening it again shortly.",
            ),
          );
          notify(
            "편집기에서 쓸 저장소 목록을 불러오지 못했어요.",
            "Could not load repositories for the editor.",
            "error",
          );
        }

        if (agentResult.status === "fulfilled" && agentResult.value) {
          const nextAgentCache: PipelineAgentCacheEntry = {
            agents: agentResult.value,
            fetchedAt: Date.now(),
          };
          applyPipelineAgentCache(nextAgentCache);
          writeStoredPipelineAgentCache(nextAgentCache);
        }
      })
      .finally(() => {
        if (!stale) {
          setPipelineSelectorLoading(false);
        }
      });
    return () => {
      stale = true;
      setPipelineSelectorLoading(false);
    };
  }, [
    enabled,
    applyPipelineAgentCache,
    applyPipelineRepoCache,
    notify,
    tr,
  ]);

  return {
    pipelineAgents,
    pipelineRepos,
    pipelineSelectorError,
    pipelineSelectorLoading,
    selectedPipelineAgentId,
    selectedPipelineRepo,
    setSelectedPipelineAgentId,
    setSelectedPipelineRepo,
  };
}
