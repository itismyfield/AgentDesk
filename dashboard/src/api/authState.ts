// Credentials live only in this tab's memory. Reloading requires authentication
// again; neither browser storage nor WebSocket URLs contain the server token.
type AuthPhase = "checking" | "authenticated" | "required" | "unavailable";
type AuthSnapshot = {
  phase: AuthPhase;
  generation: number;
  authEnabled: boolean;
  message: string;
};
let snapshot: AuthSnapshot = { phase: "checking", generation: 0, authEnabled: false, message: "" };
let token: string | null = null;
let scope = new AbortController();
const listeners = new Set<() => void>();
const credentialListeners = new Set<() => void>();

export const getAuthSnapshot = () => snapshot;
export function subscribeAuth(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}
export function onCredentialChange(listener: () => void): () => void {
  credentialListeners.add(listener);
  return () => { credentialListeners.delete(listener); };
}
export function credentialScope() {
  return { token, signal: scope.signal, generation: snapshot.generation, authEnabled: snapshot.authEnabled };
}
function publish(next: AuthSnapshot) {
  snapshot = next;
  listeners.forEach((listener) => listener());
}
function replaceCredential(next: string | null, phase: AuthPhase, message: string) {
  scope.abort();
  scope = new AbortController();
  token = next;
  snapshot = { ...snapshot, generation: snapshot.generation + 1, phase, message };
  credentialListeners.forEach((listener) => listener());
  listeners.forEach((listener) => listener());
}
export function beginAuthentication(next: string | null) {
  replaceCredential(next?.trim() || null, "checking", "");
}
export function requireAuthentication(message = "서버 토큰으로 로그인해 주세요.") {
  replaceCredential(null, "required", message);
}
export function resolveAuthentication(generation: number, authenticated: boolean, authEnabled: boolean) {
  if (snapshot.generation !== generation) return;
  if (!authenticated) {
    snapshot = { ...snapshot, authEnabled };
    requireAuthentication(token ? "서버 토큰을 확인해 주세요." : "서버 토큰으로 로그인해 주세요.");
    return;
  }
  // A trusted loopback probe can authenticate without token entry. Discard
  // old persisted responses on that first authenticated boot as well.
  if (authEnabled && snapshot.generation === 0) credentialListeners.forEach((listener) => listener());
  publish({ ...snapshot, phase: "authenticated", authEnabled, message: "" });
}
export function authenticationUnavailable(generation: number) {
  if (snapshot.generation !== generation || snapshot.phase !== "checking") return;
  publish({ ...snapshot, phase: "unavailable", message: "서버에 연결할 수 없습니다. 연결 상태를 확인한 뒤 다시 시도해 주세요." });
}
