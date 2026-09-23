import { z } from "zod";
import { request } from "./httpClient";

const sessionSchema = z.object({
  ok: z.boolean(),
  authenticated: z.boolean(),
  auth_enabled: z.boolean(),
  csrf_token: z.string(),
});
// Admission remains server-owned; reject obviously truncated opaque tickets here.
const MIN_SOCKET_TICKET_LENGTH = 32;
const ticketSchema = z.object({ ticket: z.string().min(MIN_SOCKET_TICKET_LENGTH), expires_in: z.number().positive() });

export function getDashboardSession(signal?: AbortSignal) {
  return request("/api/auth/session", { signal, cache: "no-store", maxRetries: 0, suppressErrorToast: true }, sessionSchema);
}
export function getDashboardSocketTicket(signal: AbortSignal) {
  return request("/api/auth/ws-ticket", {
    method: "POST", body: "{}", signal, maxRetries: 0, suppressErrorToast: true,
  }, ticketSchema);
}
