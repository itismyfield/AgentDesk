import { Mic, MicOff, Square } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  getConductorJob,
  sayToConductor,
  speakVoice,
  transcribeVoice,
  type VoiceConductorJob,
} from "../../api/voice";
import { useSettings } from "../../contexts/SettingsContext";
import { useI18n } from "../../i18n";
import { useVoiceCapture } from "./useVoiceCapture";

const JOB_POLL_MS = 3_000;

type Phase = "idle" | "listening" | "recording" | "thinking" | "speaking";

interface Entry {
  id: string;
  role: "user" | "assistant";
  text: string;
}

function blobToBase64(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result).split(",")[1] ?? "");
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(blob);
  });
}

function useSpeechQueue(onSpeakingChange: (speaking: boolean) => void) {
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const queueRef = useRef<string[]>([]);
  const playingRef = useRef(false);
  // Bumped by stop() so audio still being synthesized is dropped, not played.
  const generationRef = useRef(0);

  const playNext = useCallback(async () => {
    if (playingRef.current) return;
    const text = queueRef.current.shift();
    if (!text) {
      onSpeakingChange(false);
      return;
    }
    playingRef.current = true;
    onSpeakingChange(true);
    const generation = generationRef.current;
    try {
      const { audio_base64, mime } = await speakVoice(text);
      if (generation !== generationRef.current) return;
      const audio = audioRef.current ?? new Audio();
      audioRef.current = audio;
      audio.src = `data:${mime};base64,${audio_base64}`;
      await new Promise<void>((resolve) => {
        audio.onended = () => resolve();
        audio.onerror = () => resolve();
        audio.onpause = () => resolve();
        void audio.play().catch(() => resolve());
      });
    } catch {
      // A failed utterance is shown on screen; keep the queue moving.
    } finally {
      playingRef.current = false;
      void playNext();
    }
  }, [onSpeakingChange]);

  const speak = useCallback((text: string) => {
    if (!text.trim()) return;
    queueRef.current.push(text);
    void playNext();
  }, [playNext]);

  const stop = useCallback(() => {
    generationRef.current += 1;
    queueRef.current = [];
    audioRef.current?.pause();
  }, []);

  useEffect(() => stop, [stop]);

  // Mobile browsers only allow playback on an element first played from a tap.
  const unlock = useCallback(() => {
    const audio = audioRef.current ?? new Audio();
    audioRef.current = audio;
    audio.src = "data:audio/mpeg;base64,";
    void audio.play().catch(() => undefined);
  }, []);

  return useMemo(() => ({ speak, stop, unlock }), [speak, stop, unlock]);
}

export default function VoicePage() {
  const { settings } = useSettings();
  const { t } = useI18n(settings.language);
  const [handsFree, setHandsFree] = useState(true);
  const [phase, setPhase] = useState<Phase>("idle");
  const [entries, setEntries] = useState<Entry[]>([]);
  const [jobs, setJobs] = useState<VoiceConductorJob[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [speaking, setSpeaking] = useState(false);
  const spokenSummaries = useRef(new Set<string>());

  const addEntry = useCallback((role: Entry["role"], text: string) => {
    setEntries((prev) => [...prev, { id: `${Date.now()}-${prev.length}`, role, text }]);
  }, []);
  const upsertJob = useCallback((job: VoiceConductorJob) => {
    setJobs((prev) => [job, ...prev.filter((existing) => existing.id !== job.id)]);
  }, []);

  const speech = useSpeechQueue(setSpeaking);

  const handleUtterance = useCallback(async (audio: Blob) => {
    setPhase("thinking");
    setError(null);
    try {
      const { text } = await transcribeVoice(await blobToBase64(audio), audio.type);
      if (!text.trim()) return;
      addEntry("user", text);
      const job = await sayToConductor(text);
      upsertJob(job);
      addEntry("assistant", job.reply);
      speech.speak(job.reply);
      // Jobs with nothing to wait for (every start failed) come back finished.
      if (job.summary) {
        spokenSummaries.current.add(job.id);
        addEntry("assistant", job.summary);
        speech.speak(job.summary);
      }
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setPhase("idle");
    }
  }, [addEntry, speech, upsertJob]);

  const capture = useVoiceCapture({
    handsFree,
    busy: phase === "thinking" || speaking,
    onSpeechStart: speech.stop,
    onUtterance: (audio) => void handleUtterance(audio),
  });

  const runningJobIds = jobs.filter((job) => !job.finished_at).map((job) => job.id).join(",");
  useEffect(() => {
    if (!runningJobIds) return;
    const timer = setInterval(() => {
      for (const id of runningJobIds.split(",")) {
        void getConductorJob(id).then((job) => {
          upsertJob(job);
          if (job.summary && !spokenSummaries.current.has(job.id)) {
            spokenSummaries.current.add(job.id);
            addEntry("assistant", job.summary);
            speech.speak(job.summary);
          }
        }).catch(() => undefined);
      }
    }, JOB_POLL_MS);
    return () => clearInterval(timer);
  }, [addEntry, runningJobIds, speech, upsertJob]);

  useEffect(() => {
    if (!capture.active || !handsFree || !("wakeLock" in navigator)) return;
    let lock: WakeLockSentinel | null = null;
    void navigator.wakeLock.request("screen").then((sentinel) => { lock = sentinel; }).catch(() => undefined);
    return () => { void lock?.release(); };
  }, [capture.active, handsFree]);

  const onMicPress = async () => {
    speech.unlock();
    if (!capture.active) {
      try {
        await capture.start();
      } catch (caught) {
        setError(caught instanceof Error ? caught.message : String(caught));
        return;
      }
      if (!handsFree) capture.toggleRecording();
      return;
    }
    if (handsFree) capture.stop();
    else capture.toggleRecording();
  };

  const status: Phase = phase === "thinking" ? "thinking"
    : speaking ? "speaking"
    : capture.recording ? "recording"
    : capture.active ? "listening"
    : "idle";
  const statusLabel = {
    idle: t({ ko: "마이크를 눌러 시작하세요", en: "Tap the mic to start" }),
    listening: handsFree ? t({ ko: "듣고 있어요", en: "Listening" }) : t({ ko: "녹음하려면 마이크를 누르세요", en: "Tap the mic to record" }),
    recording: t({ ko: "말씀하세요", en: "Go ahead" }),
    thinking: t({ ko: "처리 중", en: "Working" }),
    speaking: t({ ko: "읽어주는 중", en: "Speaking" }),
  }[status];
  const ringScale = 1 + Math.min(capture.level * 6, 0.5);

  return (
    <div
      data-testid="voice-page"
      className="flex h-full min-h-0 flex-col overflow-hidden bg-th-bg-primary px-4 py-5 sm:px-8 sm:py-7"

    >
      <div className="mx-auto flex min-h-0 w-full max-w-5xl flex-1 flex-col gap-5">
        <header className="shrink-0 border-b border-th-border-subtle pb-5">
          <h1 className="text-2xl font-semibold tracking-tight text-th-text-heading">{t({ ko: "음성", en: "Voice" })}</h1>
          <p className="mt-2 text-xs leading-5 text-th-text-muted">{t({ ko: "말로 작업을 맡기고 에이전트의 응답을 확인합니다.", en: "Delegate work by voice and follow the agents' responses." })}</p>
        </header>
        <section aria-label={t({ko:"대화와 작업",en:"Conversation and jobs"})} className="flex min-h-0 flex-1 flex-col-reverse gap-3 overflow-y-auto rounded-lg border border-th-border-subtle bg-th-bg-surface p-4 sm:p-5">
          {entries.length === 0 && jobs.length === 0 ? (
            <div data-testid="voice-empty" className="flex flex-1 flex-col items-center justify-center gap-2 py-6 text-center">
              <Mic size={24} className="mb-1 text-th-text-muted" aria-hidden="true" />
              <h2 className="text-sm font-medium text-th-text-primary">{t({ko:"무엇을 도와드릴까요?",en:"What would you like to work on?"})}</h2>
              <p className="max-w-sm text-xs leading-5 text-th-text-muted">{t({ko:"아래 마이크를 켜고 할 일을 말하세요. 대화와 작업 진행이 여기에 표시됩니다.",en:"Turn on the microphone below and describe a task. Your conversation and job progress will appear here."})}</p>
            </div>
          ) : null}
          {jobs.slice(0, 3).map((job) => (
            <div
              key={job.id}
              className="min-w-0 shrink-0 rounded-md border p-3 text-sm"
              style={{ borderColor: "var(--th-border)", background: "var(--th-card-bg)" }}
            >
              <div className="mb-2 leading-6 [overflow-wrap:anywhere]" style={{ color: "var(--th-text-muted)" }}>{job.request}</div>
              <div className="flex flex-wrap gap-2">
                {job.dispatches.map((dispatch) => (
                  <span
                    key={dispatch.agent_id}
                    className="max-w-full rounded-md px-2 py-1 text-xs [overflow-wrap:anywhere]"
                    style={{
                      color: "var(--th-text-primary)",
                      background: dispatch.status === "done" ? "var(--th-badge-emerald-bg)"
                        : dispatch.status === "failed" ? "var(--th-badge-amber-bg)"
                        : "var(--th-badge-sky-bg)",
                    }}
                  >
                    {dispatch.agent_name} · {dispatch.status === "done" ? t({ ko: "완료", en: "done" })
                      : dispatch.status === "failed" ? t({ ko: "실패", en: "failed" })
                      : t({ ko: "진행 중", en: "running" })}
                  </span>
                ))}
              </div>
            </div>
          ))}
          {[...entries].reverse().slice(0, 12).map((entry) => (
            <p
              key={entry.id}
              className={`max-w-[92%] shrink-0 rounded-md px-3 py-2 text-sm leading-6 [overflow-wrap:anywhere] ${entry.role === "user" ? "self-end" : "self-start"}`}
              style={{
                color: "var(--th-text-primary)",
                background: entry.role === "user" ? "var(--th-accent-primary-soft)" : "var(--th-bg-surface)",
              }}
            >
              <span className="mb-1 block text-xs font-medium text-th-text-muted">{entry.role === "user" ? t({ko:"나",en:"You"}) : "AgentDesk"}</span>
              {entry.text}
            </p>
          ))}
        </section>

        {error && (
          <p role="alert" className="max-h-32 shrink-0 overflow-y-auto rounded-md border border-th-border-subtle px-4 py-3 text-sm leading-6 [overflow-wrap:anywhere]" style={{ color: "var(--th-accent-danger)" }}>{error}</p>
        )}

        <div className="flex shrink-0 flex-col items-center gap-3 pb-1">
          <p className="text-sm" style={{ color: "var(--th-text-muted)" }} aria-live="polite">{statusLabel}</p>
          <div className="flex flex-wrap items-center justify-center gap-4">
            <button
              type="button"
              onClick={() => setHandsFree((prev) => !prev)}
              className="min-h-11 rounded-md border px-3 py-2 text-xs"
              style={{ borderColor: "var(--th-border)", color: "var(--th-text-secondary)" }}
              aria-pressed={handsFree}
            >
              {handsFree ? t({ ko: "핸즈프리", en: "Hands-free" }) : t({ ko: "눌러서 말하기", en: "Tap to talk" })}
            </button>
            <button
              type="button"
              data-testid="voice-mic-button"
              onClick={() => void onMicPress()}
              aria-label={!capture.active ? t({ ko: "마이크 켜기", en: "Start microphone" }) : handsFree ? t({ ko: "마이크 끄기", en: "Stop microphone" }) : capture.recording ? t({ko:"녹음 마치기",en:"Finish recording"}) : t({ko:"녹음 시작",en:"Start recording"})}
              className="flex h-20 w-20 shrink-0 items-center justify-center rounded-full border border-th-border-subtle transition-transform sm:h-24 sm:w-24"
              style={{
                transform: `scale(${capture.recording ? ringScale : 1})`,
                background: capture.active ? "var(--th-accent-primary)" : "var(--th-bg-surface)",
                color: capture.active ? "white" : "var(--th-text-primary)",
              }}
            >
              {capture.active ? <Mic size={40} aria-hidden /> : <MicOff size={40} aria-hidden />}
            </button>
            <button
              type="button"
              onClick={speech.stop}
              disabled={!speaking}
              className="flex h-11 w-11 items-center justify-center rounded-md border disabled:opacity-40"
              style={{ borderColor: "var(--th-border)", color: "var(--th-text-secondary)" }}
              aria-label={t({ ko: "읽기 멈추기", en: "Stop speaking" })}
            >
              <Square size={16} aria-hidden />
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
