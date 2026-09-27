import { z } from "zod";
import { SLOW_MUTATION_TIMEOUT_MS, request } from "./httpClient";

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

const post = (body: unknown) => ({
  method: "POST",
  body: JSON.stringify(body),
  timeoutMs: SLOW_MUTATION_TIMEOUT_MS,
  maxRetries: 0,
});

export const transcribeVoice = (audioBase64: string, mime: string) =>
  request("/api/voice/transcribe", post({ audio_base64: audioBase64, mime }), z.object({ text: z.string() }));

export const speakVoice = (text: string) =>
  request("/api/voice/speak", post({ text }), z.object({ audio_base64: z.string(), mime: z.string() }));

export const sayToConductor = (text: string) =>
  request("/api/voice/conductor/say", { ...post({ text }), timeoutMs: 120_000 }, jobSchema);

export const getConductorJob = (id: string) =>
  request(`/api/voice/conductor/jobs/${encodeURIComponent(id)}`, { cache: "no-store", maxRetries: 0 }, jobSchema);
