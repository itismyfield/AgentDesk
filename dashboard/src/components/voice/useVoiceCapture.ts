import { useCallback, useEffect, useRef, useState } from "react";

// Levels are RMS of the time-domain signal (0..1).
const SPEECH_START_LEVEL = 0.04;
const SPEECH_START_LEVEL_WHILE_BUSY = 0.1;
const SPEECH_START_MS = 150;
const SILENCE_END_MS = 1_200;
const MIN_UTTERANCE_MS = 500;
const TICK_MS = 50;

function pickMimeType(): string {
  const candidates = ["audio/webm;codecs=opus", "audio/webm", "audio/mp4", "audio/ogg;codecs=opus"];
  return candidates.find((type) => typeof MediaRecorder !== "undefined" && MediaRecorder.isTypeSupported(type)) ?? "";
}

interface VoiceCaptureOptions {
  /** Start and stop recording from the microphone level instead of taps. */
  handsFree: boolean;
  /** The app is thinking or speaking; only louder speech starts a recording. */
  busy: boolean;
  onSpeechStart: () => void;
  onUtterance: (audio: Blob) => void;
}

export function useVoiceCapture({ handsFree, busy, onSpeechStart, onUtterance }: VoiceCaptureOptions) {
  const [active, setActive] = useState(false);
  const [recording, setRecording] = useState(false);
  const [level, setLevel] = useState(0);
  const streamRef = useRef<MediaStream | null>(null);
  const contextRef = useRef<AudioContext | null>(null);
  const recorderRef = useRef<MediaRecorder | null>(null);
  const timerRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const optionsRef = useRef({ handsFree, busy, onSpeechStart, onUtterance });
  useEffect(() => {
    optionsRef.current = { handsFree, busy, onSpeechStart, onUtterance };
  }, [handsFree, busy, onSpeechStart, onUtterance]);

  const startRecording = useCallback(() => {
    const stream = streamRef.current;
    if (!stream || recorderRef.current) return;
    const mimeType = pickMimeType();
    const recorder = new MediaRecorder(stream, mimeType ? { mimeType } : undefined);
    const chunks: Blob[] = [];
    const startedAt = Date.now();
    recorder.ondataavailable = (event) => {
      if (event.data.size > 0) chunks.push(event.data);
    };
    recorder.onstop = () => {
      recorderRef.current = null;
      setRecording(false);
      if (Date.now() - startedAt >= MIN_UTTERANCE_MS && chunks.length > 0) {
        optionsRef.current.onUtterance(new Blob(chunks, { type: recorder.mimeType || mimeType }));
      }
    };
    recorderRef.current = recorder;
    recorder.start();
    setRecording(true);
    optionsRef.current.onSpeechStart();
  }, []);

  const stopRecording = useCallback(() => {
    if (recorderRef.current?.state === "recording") recorderRef.current.stop();
  }, []);

  const stop = useCallback(() => {
    stopRecording();
    if (timerRef.current) clearInterval(timerRef.current);
    timerRef.current = null;
    streamRef.current?.getTracks().forEach((track) => track.stop());
    streamRef.current = null;
    void contextRef.current?.close();
    contextRef.current = null;
    setActive(false);
    setLevel(0);
  }, [stopRecording]);

  const start = useCallback(async () => {
    if (streamRef.current) return;
    const stream = await navigator.mediaDevices.getUserMedia({
      audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true },
    });
    const context = new AudioContext();
    const analyser = context.createAnalyser();
    analyser.fftSize = 1024;
    context.createMediaStreamSource(stream).connect(analyser);
    streamRef.current = stream;
    contextRef.current = context;
    setActive(true);

    const samples = new Float32Array(analyser.fftSize);
    let loudSince: number | null = null;
    let quietSince: number | null = null;
    timerRef.current = setInterval(() => {
      analyser.getFloatTimeDomainData(samples);
      const rms = Math.sqrt(samples.reduce((sum, value) => sum + value * value, 0) / samples.length);
      setLevel(rms);
      const { handsFree: auto, busy: isBusy } = optionsRef.current;
      if (!auto) return;
      const now = Date.now();
      if (!recorderRef.current) {
        const threshold = isBusy ? SPEECH_START_LEVEL_WHILE_BUSY : SPEECH_START_LEVEL;
        loudSince = rms >= threshold ? (loudSince ?? now) : null;
        if (loudSince !== null && now - loudSince >= SPEECH_START_MS) {
          loudSince = null;
          quietSince = null;
          startRecording();
        }
        return;
      }
      quietSince = rms < SPEECH_START_LEVEL ? (quietSince ?? now) : null;
      if (quietSince !== null && now - quietSince >= SILENCE_END_MS) {
        quietSince = null;
        stopRecording();
      }
    }, TICK_MS);
  }, [startRecording, stopRecording]);

  const toggleRecording = useCallback(() => {
    if (recorderRef.current) stopRecording();
    else startRecording();
  }, [startRecording, stopRecording]);

  useEffect(() => stop, [stop]);

  return { active, recording, level, start, stop, toggleRecording };
}
