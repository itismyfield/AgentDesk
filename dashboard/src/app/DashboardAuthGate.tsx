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
    <main className="flex min-h-dvh items-center justify-center bg-gray-950 p-5 text-gray-100">
      <form className="w-full max-w-sm rounded-xl border border-gray-700 bg-gray-900 p-6 shadow-xl"
        onSubmit={(event) => { event.preventDefault(); beginAuthentication(input); setInput(""); }}>
        <h1 className="mb-3 text-xl font-semibold">AgentDesk 로그인</h1>
        <p className="mb-5 text-sm text-gray-300" role="status">
          {auth.phase === "checking" ? "서버 인증을 확인하고 있습니다…" : auth.message}
        </p>
        <label className="mb-2 block text-sm" htmlFor="dashboard-token">서버 토큰</label>
        <input id="dashboard-token" type="password" autoComplete="off" spellCheck={false}
          className="mb-4 w-full rounded border border-gray-600 bg-gray-950 p-3"
          value={input} onChange={(event) => setInput(event.target.value)}
          disabled={auth.phase === "checking"} required={auth.phase === "required"} />
        <button type="submit" disabled={auth.phase === "checking"}
          className="w-full rounded bg-blue-600 px-4 py-3 font-medium disabled:opacity-50">
          {auth.phase === "unavailable" ? "다시 연결" : "로그인"}
        </button>
        <p className="mt-4 text-xs leading-relaxed text-gray-400">토큰은 현재 탭에서만 사용합니다. 페이지를 새로고침하면 다시 로그인해야 합니다.</p>
      </form>
    </main>
  );
}

export function DashboardLogout() {
  const auth = useSyncExternalStore(subscribeAuth, getAuthSnapshot);
  if (!auth.authEnabled) return null;
  return <button type="button" onClick={() => requireAuthentication("로그아웃했습니다.")}
    className="rounded border border-gray-600 px-2 py-1 text-xs" title="현재 탭의 토큰과 데이터를 지웁니다">로그아웃</button>;
}
