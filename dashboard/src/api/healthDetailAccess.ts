import { onCredentialChange } from "./authState";
import { ApiRequestError } from "./httpClient";

// Detail health is loopback-only. A 403 is remembered so tailnet tabs stop
// asking, and forgotten when the tab's credential changes.
let healthDetailForbidden = false;
onCredentialChange(() => { healthDetailForbidden = false; });

export function healthDetailBlocked(): boolean {
  return healthDetailForbidden;
}

/** Remember a 403 and report whether the caller should fall back to the public summary. */
export function noteHealthDetailError(error: unknown): "forbidden" | "failed" {
  const forbidden = error instanceof ApiRequestError && error.status === 403;
  if (forbidden) healthDetailForbidden = true;
  return forbidden ? "forbidden" : "failed";
}
