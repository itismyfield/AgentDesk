import { z } from "zod";
import { request } from "./httpClient";

const dispatchSchema = z.object({
  agent_id: z.string(),
  agent_name: z.string(),
  prompt: z.string(),
  turn_id: z.string().nullable(),
  status: z.enum(["running", "done", "failed"]),
  result: z.string().nullable(),
  error: z.string().nullable(),
});

const jobSchema = z.object({
  id: z.string(),
  request: z.string(),
  reply: z.string(),
  created_at: z.string(),
  dispatches: z.array(dispatchSchema),
  summary: z.string().nullable(),
  finished_at: z.string().nullable(),
});

export type VoiceConductorJob = z.infer<typeof jobSchema>;
export type VoiceConductorDispatch = z.infer<typeof dispatchSchema>;

// Browser limits sit above the server's own budgets so the server, not the
// browser, decides when a slow provider has failed.
// STT: up to two 120 s provider calls (one retry on an empty transcript).
const TRANSCRIBE_TIMEOUT_MS = 250_000;
// TTS: one 120 s provider call.
const SPEAK_TIMEOUT_MS = 130_000;
// Conductor: one 90 s planner call plus the agent turn starts.
const SAY_TIMEOUT_MS = 150_000;

const post = (body: unknown, timeoutMs: number) => ({
  method: "POST",
  body: JSON.stringify(body),
  timeoutMs,
  maxRetries: 0,
});

export const transcribeVoice = (audioBase64: string, mime: string) =>
  request(
    "/api/voice/transcribe",
    post({ audio_base64: audioBase64, mime }, TRANSCRIBE_TIMEOUT_MS),
    z.object({ text: z.string() }),
  );

export const speakVoice = (text: string) =>
  request("/api/voice/speak", post({ text }, SPEAK_TIMEOUT_MS), z.object({ audio_base64: z.string(), mime: z.string() }));

export const sayToConductor = (text: string) =>
  request("/api/voice/conductor/say", post({ text }, SAY_TIMEOUT_MS), jobSchema);

export const getConductorJob = (id: string) =>
  request(`/api/voice/conductor/jobs/${encodeURIComponent(id)}`, { cache: "no-store", maxRetries: 0 }, jobSchema);
