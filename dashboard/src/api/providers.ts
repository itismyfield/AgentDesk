import { useEffect, useMemo, useState } from "react";
import { z } from "zod";

import { getProviderLabel } from "../app/providerTheme";
import {
  CLI_PROVIDERS,
  LEGACY_ONLY_PROVIDERS,
} from "../components/agent-manager/constants";
import { request } from "./httpClient";

const providerCatalogEntrySchema = z.object({
  id: z.string().trim().min(1),
  display_name: z.string().trim().min(1),
  channel_suffix: z.string().nullable(),
  binary_name: z.string().trim().min(1),
  execution_surface: z.string().trim().min(1),
  supports_resume: z.boolean(),
  supports_structured_output: z.boolean(),
  supports_tool_stream: z.boolean(),
  supports_restricted_tool_policy: z.boolean(),
  supports_tui_hosting: z.boolean(),
  system_prompt_transport: z.string().trim().min(1),
  context_window_tokens: z.number().int().positive().nullable(),
});

const providerCatalogResponseSchema = z.looseObject({
  catalog: z.array(providerCatalogEntrySchema),
});

export type ProviderCatalogEntry = z.infer<typeof providerCatalogEntrySchema>;

const LEGACY_ONLY = new Set<string>(LEGACY_ONLY_PROVIDERS);

export async function getProviderCatalog(): Promise<ProviderCatalogEntry[]> {
  const body = await request(
    "/api/provider-cli",
    undefined,
    providerCatalogResponseSchema,
  );
  return body.catalog;
}

export function selectableCatalogIds(
  entries: ProviderCatalogEntry[],
  currentId?: string | null,
): string[] {
  const ids = entries
    .map((entry) => entry.id)
    .filter((id) => !LEGACY_ONLY.has(id));
  if (currentId && !ids.includes(currentId)) {
    return [currentId, ...ids];
  }
  return ids;
}

export function meetingCatalogIds(entries: ProviderCatalogEntry[]): string[] {
  return entries
    .filter(
      (entry) =>
        entry.supports_restricted_tool_policy && !LEGACY_ONLY.has(entry.id),
    )
    .map((entry) => entry.id);
}

export function catalogLabel(
  entries: ProviderCatalogEntry[],
  id: string,
  fallback?: string,
): string {
  const fromCatalog = entries
    .find((entry) => entry.id === id)
    ?.display_name.trim();
  if (fromCatalog) return fromCatalog;
  if (fallback) return fallback;
  return getProviderLabel(id);
}

export function useProviderCatalog(currentId?: string | null) {
  const [entries, setEntries] = useState<ProviderCatalogEntry[] | null>(null);
  const [error, setError] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void getProviderCatalog()
      .then((list) => {
        if (cancelled) return;
        setEntries(list);
        setError(false);
      })
      .catch(() => {
        if (cancelled) return;
        setEntries(null);
        setError(true);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const selectableIds = useMemo(() => {
    if (entries && entries.length > 0) {
      return selectableCatalogIds(entries, currentId);
    }
    const fallback = CLI_PROVIDERS.filter((id) => !LEGACY_ONLY.has(id));
    if (currentId && !fallback.some((id) => id === currentId)) {
      return [currentId, ...fallback];
    }
    return fallback;
  }, [entries, currentId]);

  const meetingIds = useMemo(() => {
    if (entries && entries.length > 0) {
      return meetingCatalogIds(entries);
    }
    return CLI_PROVIDERS.filter((id) => !LEGACY_ONLY.has(id));
  }, [entries]);

  return {
    entries: entries ?? [],
    selectableIds,
    meetingIds,
    loading: entries === null && !error,
    error,
    catalogReady: Boolean(entries && entries.length > 0),
  };
}
