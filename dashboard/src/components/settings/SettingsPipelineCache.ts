import type { Agent } from "../../types";
import * as api from "../../api";
import type { GitHubRepoOption } from "../../api";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import { readLocalStorageValue, writeLocalStorageValue } from "../../lib/useLocalStorage";

export const PIPELINE_SELECTOR_REFRESH_TIMEOUT_MS = 5_000;
export const PIPELINE_SELECTOR_CACHE_MAX_AGE_MS = 60_000;

export interface PipelineRepoCacheEntry {
  viewerLogin: string;
  repos: GitHubRepoOption[];
  fetchedAt: number;
}

export interface PipelineAgentCacheEntry {
  agents: Agent[];
  fetchedAt: number;
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

export function isPipelineRepoCacheEntry(value: unknown): value is PipelineRepoCacheEntry {
  return isRecord(value)
    && typeof value.viewerLogin === "string"
    && typeof value.fetchedAt === "number"
    && Array.isArray(value.repos);
}

export function isPipelineAgentCacheEntry(value: unknown): value is PipelineAgentCacheEntry {
  return isRecord(value)
    && typeof value.fetchedAt === "number"
    && Array.isArray(value.agents);
}

export function readStoredPipelineRepoCache(): PipelineRepoCacheEntry | null {
  return readLocalStorageValue<PipelineRepoCacheEntry | null>(
    STORAGE_KEYS.settingsPipelineRepoCache,
    null,
    {
      validate: (value): value is PipelineRepoCacheEntry | null =>
        value === null || isPipelineRepoCacheEntry(value),
    },
  );
}

export function writeStoredPipelineRepoCache(cache: PipelineRepoCacheEntry): void {
  writeLocalStorageValue(STORAGE_KEYS.settingsPipelineRepoCache, cache);
}

export function readStoredPipelineAgentCache(): PipelineAgentCacheEntry | null {
  return readLocalStorageValue<PipelineAgentCacheEntry | null>(
    STORAGE_KEYS.settingsPipelineAgentCache,
    null,
    {
      validate: (value): value is PipelineAgentCacheEntry | null =>
        value === null || isPipelineAgentCacheEntry(value),
    },
  );
}

export function writeStoredPipelineAgentCache(cache: PipelineAgentCacheEntry): void {
  writeLocalStorageValue(STORAGE_KEYS.settingsPipelineAgentCache, cache);
}

export function pickMostRecentCache<T extends { fetchedAt: number }>(...entries: Array<T | null>): T | null {
  return entries.reduce<T | null>((latest, entry) => {
    if (!entry) return latest;
    if (!latest || entry.fetchedAt > latest.fetchedAt) {
      return entry;
    }
    return latest;
  }, null);
}

export function isCacheFresh(cache: { fetchedAt: number } | null): boolean {
  if (!cache) return false;
  return Date.now() - cache.fetchedAt < PIPELINE_SELECTOR_CACHE_MAX_AGE_MS;
}

export function getCachedPipelineRepoEntry(): PipelineRepoCacheEntry | null {
  const memoryCache = api.getCachedGitHubRepos();
  return pickMostRecentCache(
    memoryCache
      ? {
          viewerLogin: memoryCache.data.viewer_login,
          repos: memoryCache.data.repos,
          fetchedAt: memoryCache.fetchedAt,
        }
      : null,
    readStoredPipelineRepoCache(),
  );
}

export function getCachedPipelineAgentEntry(): PipelineAgentCacheEntry | null {
  const memoryCache = api.getCachedAgents();
  return pickMostRecentCache(
    memoryCache
      ? {
          agents: memoryCache.data,
          fetchedAt: memoryCache.fetchedAt,
        }
      : null,
    readStoredPipelineAgentCache(),
  );
}
