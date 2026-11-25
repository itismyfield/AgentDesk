import { useEffect, useState, useSyncExternalStore, type ReactNode } from "react";
import {
  authenticationUnavailable, beginAuthentication, getAuthSnapshot,
  requireAuthentication, resolveAuthentication, subscribeAuth,
} from "../api/authState";
import { getDashboardSession } from "../api/dashboardAuth";

export function DashboardAuthGate({ children }: { children: ReactNode }) {
  const auth = useSyncExternalStore(subscribeAuth, getAuthSnapshot);
  const [input, setInput] = useState("");
  useEffect(() => {
    if (auth.phase !== "checking") return;
    const controller = new AbortController();
    void getDashboardSession(controller.signal).then((session) => {
      resolveAuthentication(auth.generation, session.authenticated, session.auth_enabled);
    }).catch(() => {
      if (!controller.signal.aborted) authenticationUnavailable(auth.generation);
    });
    return () => controller.abort();
  }, [auth.generation, auth.phase]);

  if (auth.phase === "authenticated") return <>{children}</>;
  return (
    <main className="flex min-h-dvh items-center justify-center bg-[var(--bg)] p-5 text-[var(--fg)]">
      <form className="w-full max-w-sm rounded-xl border border-[var(--line)] bg-[var(--bg-2)] p-6 text-[var(--fg)] shadow-[var(--shadow-md)]"
        onSubmit={(event) => { event.preventDefault(); beginAuthentication(input); setInput(""); }}>
        <h1 className="mb-3 text-xl font-semibold">AgentDesk 로그인</h1>
        <p className="mb-5 text-sm text-[var(--fg-muted)]" role="status">
          {auth.phase === "checking" ? "서버 인증을 확인하고 있습니다…" : auth.message}
        </p>
        <label className="mb-2 block text-sm" htmlFor="dashboard-token">서버 토큰</label>
        <input id="dashboard-token" type="password" autoComplete="off" spellCheck={false}
          className="mb-4 w-full rounded border border-[var(--line)] bg-[var(--bg)] p-3 text-[var(--fg)]"
          value={input} onChange={(event) => setInput(event.target.value)}
          disabled={auth.phase === "checking"} required={auth.phase === "required"} />
        <button type="submit" disabled={auth.phase === "checking"}
          className="w-full rounded bg-blue-600 px-4 py-3 font-medium text-white disabled:opacity-50">
          {auth.phase === "unavailable" ? "다시 연결" : "로그인"}
        </button>
        <p className="mt-4 text-xs leading-relaxed text-[var(--fg-muted)]">토큰은 현재 탭에서만 사용합니다. 페이지를 새로고침하면 다시 로그인해야 합니다.</p>
      </form>
    </main>
  );
}

export function DashboardLogout() {
  const auth = useSyncExternalStore(subscribeAuth, getAuthSnapshot);
  if (!auth.authEnabled) return null;
  return <button type="button" onClick={() => requireAuthentication("로그아웃했습니다.")}
    className="min-h-11 rounded-[var(--r-sm)] border border-th-border px-3 text-xs text-th-text-secondary sm:min-h-9 pointer-coarse:min-h-11" title="현재 탭의 토큰과 데이터를 지웁니다">로그아웃</button>;
}
