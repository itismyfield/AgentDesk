import { useEffect, useState, useSyncExternalStore, type ReactNode } from "react";
import {
  authenticationUnavailable, beginAuthentication, getAuthSnapshot,
  requireAuthentication, resolveAuthentication, subscribeAuth,
} from "../api/authState";
import { getDashboardSession } from "../api/dashboardAuth";
import { useI18n } from "../i18n";

function localizeAuthMessage(message: string, tr: (ko: string, en: string) => string): string {
  switch (message) {
    case "서버 토큰으로 로그인해 주세요.":
      return tr("서버 토큰으로 로그인해 주세요.", "Sign in with your server token.");
    case "서버 토큰을 확인해 주세요.":
      return tr("서버 토큰을 확인해 주세요.", "Check your server token.");
    case "서버에 연결할 수 없습니다. 연결 상태를 확인한 뒤 다시 시도해 주세요.":
      return tr(
        "서버에 연결할 수 없습니다. 연결 상태를 확인한 뒤 다시 시도해 주세요.",
        "Cannot reach the server. Check the connection and try again.",
      );
    case "로그아웃했습니다.":
      return tr("로그아웃했습니다.", "Signed out.");
    default:
      return message;
  }
}

export function DashboardAuthGate({ children }: { children: ReactNode }) {
  const auth = useSyncExternalStore(subscribeAuth, getAuthSnapshot);
  const [input, setInput] = useState("");
  const i18n = useI18n();
  const isKo = i18n.__fromProvider ? i18n.language === "ko" : true;
  const tr = (ko: string, en: string) => (isKo ? ko : en);
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
        <h1 className="mb-3 text-xl font-semibold">{tr("AgentDesk 로그인", "Sign in to AgentDesk")}</h1>
        <p className="mb-5 text-sm text-[var(--fg-muted)]" role="status">
          {auth.phase === "checking"
            ? tr("서버 인증을 확인하고 있습니다…", "Checking server authentication…")
            : localizeAuthMessage(auth.message, tr)}
        </p>
        <label className="mb-2 block text-sm" htmlFor="dashboard-token">{tr("서버 토큰", "Server token")}</label>
        <input id="dashboard-token" type="password" autoComplete="off" spellCheck={false}
          className="mb-4 w-full rounded border border-[var(--line)] bg-[var(--bg)] p-3 text-[var(--fg)]"
          value={input} onChange={(event) => setInput(event.target.value)}
          disabled={auth.phase === "checking"} required={auth.phase === "required"} />
        <button type="submit" disabled={auth.phase === "checking"}
          className="min-h-11 w-full rounded bg-[var(--accent)] px-4 py-3 font-medium text-[var(--accent-ink)] disabled:opacity-50">
          {auth.phase === "unavailable" ? tr("다시 연결", "Reconnect") : tr("로그인", "Sign in")}
        </button>
        <p className="mt-4 text-xs leading-relaxed text-[var(--fg-muted)]">
          {tr(
            "토큰은 현재 탭에서만 사용합니다. 페이지를 새로고침하면 다시 로그인해야 합니다.",
            "The token stays in this tab only. Reloading the page requires signing in again.",
          )}
        </p>
      </form>
    </main>
  );
}

export function DashboardLogout() {
  const auth = useSyncExternalStore(subscribeAuth, getAuthSnapshot);
  const i18n = useI18n();
  const isKo = i18n.__fromProvider ? i18n.language === "ko" : true;
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  if (!auth.authEnabled) return null;
  return <button type="button" onClick={() => requireAuthentication("로그아웃했습니다.")}
    className="min-h-11 rounded-[var(--r-sm)] border border-th-border px-3 text-xs text-th-text-secondary min-[900px]:min-h-9 max-[899px]:min-h-11 pointer-coarse:min-h-11"
    title={tr("현재 탭의 토큰과 데이터를 지웁니다", "Clear token and data in this tab")}>{tr("로그아웃", "Sign out")}</button>;
}
