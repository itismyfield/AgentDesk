const BASE = "";
const REQUEST_TIMEOUT_MS = 15_000;
export const TOKEN_ANALYTICS_TIMEOUT_MS = 60_000;
// #2050 P3 finding 15 — mutations hitting GitHub/Discord can exceed 15s
// under rate-limit pressure; a longer ceiling prevents client-side timeouts
// for requests the server is still happily processing.
export const SLOW_MUTATION_TIMEOUT_MS = 60_000;
const MAX_RETRIES = 2;
const INITIAL_BACKOFF_MS = 500;

// ── GET deduplication ──
const inflightGets = new Map<string, Promise<unknown>>();
export interface CachedGetEntry<T = unknown> {
  data: T;
  fetchedAt: number;
}
// #2050 P3 finding 12 — bound response cache (insertion-order LRU) + TTL on
// read so a long-lived SPA tab stops leaking memory linearly with unique
// query strings.
const CACHED_GET_MAX_ENTRIES = 200;
const CACHED_GET_TTL_MS = 60_000;
const cachedGets = new Map<string, CachedGetEntry>();

// #2050 P3 finding 17 — strip `fresh=1` from cache key so forceRefresh
// updates the same slot non-fresh callers look up.
function cacheKeyForUrl(url: string): string {
  if (!url.includes("fresh=1")) return url;
  return url
    .replace(/([?&])fresh=1(&|$)/, (_match, prefix, suffix) =>
      suffix === "&" ? prefix : prefix === "?" ? "" : "",
    )
    .replace(/\?$/, "");
}

function storeCachedGet(url: string, payload: unknown): void {
  const key = cacheKeyForUrl(url);
  if (cachedGets.size >= CACHED_GET_MAX_ENTRIES) {
    const oldest = cachedGets.keys().next().value;
    if (typeof oldest === "string") cachedGets.delete(oldest);
  }
  cachedGets.set(key, { data: payload, fetchedAt: Date.now() });
}

export function clearCachedGet(url: string): void {
  cachedGets.delete(cacheKeyForUrl(url));
}

// ── Global error listener for toast integration ──
type ApiErrorListener = (url: string, error: Error) => void;
let apiErrorListener: ApiErrorListener | null = null;
export function onApiError(listener: ApiErrorListener | null): void {
  apiErrorListener = listener;
}

// #2050 P2 finding 9 — typed API error carrying status + server `code` so
// the previously-dead isApiRequestError guard has real fields to branch on.
export class ApiRequestError extends Error {
  readonly code: string;
  readonly status: number;
  readonly serverCode?: string;
  constructor(
    message: string,
    options: { status: number; code?: string; serverCode?: string },
  ) {
    super(message);
    this.name = "ApiRequestError";
    this.status = options.status;
    this.code = options.code ?? `HTTP_${options.status}`;
    this.serverCode = options.serverCode;
  }
}

export interface RequestOptions extends RequestInit {
  timeoutMs?: number;
  maxRetries?: number;
}

function composeRequestSignal(
  timeoutSignal: AbortSignal,
  externalSignal?: AbortSignal,
): { signal: AbortSignal; cleanup: () => void } {
  if (!externalSignal) {
    return {
      signal: timeoutSignal,
      cleanup: () => {},
    };
  }

  const controller = new AbortController();

  const abortFromSource = () => {
    if (controller.signal.aborted) return;
    controller.abort(externalSignal.reason ?? timeoutSignal.reason);
  };

  if (timeoutSignal.aborted || externalSignal.aborted) {
    abortFromSource();
  }

  timeoutSignal.addEventListener("abort", abortFromSource);
  externalSignal.addEventListener("abort", abortFromSource);

  return {
    signal: controller.signal,
    cleanup: () => {
      timeoutSignal.removeEventListener("abort", abortFromSource);
      externalSignal.removeEventListener("abort", abortFromSource);
    },
  };
}

function isRetryable(status: number): boolean {
  return status === 408 || status === 429 || status >= 500;
}

function isAbortError(error: Error): boolean {
  return error.name === "AbortError" || /aborted/i.test(error.message);
}

export function readCachedGet<T>(url: string): CachedGetEntry<T> | null {
  // #2050 P3 finding 12/17 — normalize fresh=1 + expire stale entries on read.
  const key = cacheKeyForUrl(url);
  const cached = cachedGets.get(key);
  if (!cached) return null;
  if (Date.now() - cached.fetchedAt > CACHED_GET_TTL_MS) {
    cachedGets.delete(key);
    return null;
  }
  return cached as CachedGetEntry<T>;
}

export interface CachedApiSnapshot<T> {
  data: T;
  fetchedAt: number;
}

export function readCachedSnapshot<T>(url: string): CachedApiSnapshot<T> | null {
  const cached = readCachedGet<T>(url);
  if (!cached) return null;
  return {
    data: cached.data,
    fetchedAt: cached.fetchedAt,
  };
}

export async function request<T>(url: string, opts?: RequestOptions): Promise<T> {
  const method = opts?.method?.toUpperCase() ?? "GET";
  const isGet = method === "GET";
  const shouldDedupe = isGet && !opts?.signal;
  const timeoutMs = opts?.timeoutMs ?? REQUEST_TIMEOUT_MS;
  const maxRetries = opts?.maxRetries ?? MAX_RETRIES;

  if (shouldDedupe) {
    const existing = inflightGets.get(url);
    if (existing) return existing as Promise<T>;
  }

  const execute = async (): Promise<T> => {
    let lastError: Error | null = null;
    for (let attempt = 0; attempt <= maxRetries; attempt++) {
      if (attempt > 0) {
        const delay = INITIAL_BACKOFF_MS * 2 ** (attempt - 1);
        await new Promise((resolve) => setTimeout(resolve, delay));
      }
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), timeoutMs);
      const externalSignal = opts?.signal ?? undefined;
      const { signal, cleanup } = composeRequestSignal(controller.signal, externalSignal);
      try {
        const { timeoutMs: _timeoutMs, signal: _signal, ...fetchOpts } = opts ?? {};
        const res = await fetch(`${BASE}${url}`, {
          credentials: "include",
          ...fetchOpts,
          signal,
          headers: {
            "Content-Type": "application/json",
            ...fetchOpts.headers,
          },
        });
        clearTimeout(timer);
        cleanup();
        if (!res.ok) {
          const err = await res.json().catch(() => ({ error: "unknown" }));
          // #2050 P2 finding 9 — throw typed ApiRequestError with status +
          // server-supplied code so isApiRequestError has a real field.
          const serverCode =
            typeof err.code === "string"
              ? err.code
              : typeof err.error_code === "string"
                ? err.error_code
                : undefined;
          const error = new ApiRequestError(
            err.error || `HTTP ${res.status}`,
            { status: res.status, serverCode },
          );
          if (isGet && isRetryable(res.status) && attempt < maxRetries) {
            lastError = error;
            continue;
          }
          throw error;
        }
        const payload = await res.json();
        if (isGet) {
          // #2050 P3 finding 12/17 — bounded cache + fresh=1 normalization.
          storeCachedGet(url, payload);
        }
        return payload;
      } catch (error) {
        clearTimeout(timer);
        cleanup();
        const resolvedError =
          error instanceof Error ? error : new Error(String(error));
        if (resolvedError.name === "AbortError") {
          if (externalSignal?.aborted) throw resolvedError;
          lastError = new Error(`Request timeout: ${url}`);
          if (isGet && attempt < maxRetries) continue;
        } else if (
          isGet &&
          attempt < maxRetries &&
          !resolvedError.message.startsWith("HTTP ")
        ) {
          lastError = resolvedError;
          continue;
        }
        throw lastError ?? resolvedError;
      }
    }
    throw lastError ?? new Error(`Request failed: ${url}`);
  };

  // #2050 P2 finding 11 — wire apiErrorListener into the deduplicated promise
  // so concurrent callers sharing the same in-flight GET observe consistent
  // error reporting. Previously only the *first* caller's outer .catch
  // triggered the toast; B/C/etc got the rejection silently.
  const decorated = execute()
    .catch((error) => {
      const resolvedError =
        error instanceof Error ? error : new Error(String(error));
      if (!isAbortError(resolvedError)) {
        apiErrorListener?.(url, resolvedError);
      }
      throw resolvedError;
    })
    .finally(() => {
      if (shouldDedupe) inflightGets.delete(url);
    });

  if (shouldDedupe) inflightGets.set(url, decorated);

  return decorated;
}
