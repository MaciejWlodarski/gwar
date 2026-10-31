import { useEffect, useRef, useState } from "react";
import type { VoiceEngine } from "../voice/engine";
import { useSettings } from "../state/settings";
import { useUi, useVoice } from "../state/stores";

/** Page-scoped, local-only capture. Store subscriptions retire it before a call can open capture. */
export function useMicPreview(engine: VoiceEngine) {
  const [level, setLevel] = useState(0);
  const [needsPermission, setNeedsPermission] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [playback, setPlayback] = useState<"idle" | "recording" | "playing">("idle");
  const request = useRef<() => void>(() => {});
  const listen = useRef<() => void>(() => {});
  const stop = useRef<() => void>(() => {});

  useEffect(() => {
    let visible = document.visibilityState !== "hidden";
    let focused = document.hasFocus();
    let generation = 0;
    let key = "";
    let active: AbortController | null = null;
    const retire = () => { active?.abort(); active = null; setLevel(0); setPlayback("idle"); };
    const refresh = (explicit = false, sample = false) => {
      const audio = useSettings.getState().audio;
      const voice = useVoice.getState();
      const ui = useUi.getState();
      const available = visible && focused && ui.section === "settings" && ui.settingsTab === "audio" && !voice.muted && !voice.deafened && voice.state === "idle";
      const eligible = available && (sample || audio.inputMode === "vad");
      const next = JSON.stringify([available, audio.inputMode, audio.inputDeviceId, audio.outputDeviceId, audio.masterVolume, audio.noiseSuppression, audio.noiseStrength, audio.echoCancellation, audio.autoGainControl]);
      if (!explicit && key === next) return;
      key = next;
      const current = ++generation;
      retire();
      setNeedsPermission(false);
      if (!eligible) return;
      setError(null);
      if (sample) setPlayback("recording");
      void (async () => {
        const permission = explicit ? "granted" : await engine.micPermission().catch(() => "unknown");
        if (generation !== current) return;
        if (permission !== "granted") { setNeedsPermission(true); return; }
        const capture = new AbortController();
        active = capture;
        try {
          const stop = await engine.startMicTest(value => {
            if (generation === current && !capture.signal.aborted) setLevel(value);
          }, capture.signal, sample ? {
            phase: value => {
              if (generation !== current || capture.signal.aborted) return;
              if (value === "idle") {
                // Engines report completion only after the sample run has fully retired.
                // Recheck permission and every privacy gate before restoring the silent meter.
                key = "";
                refresh();
              } else setPlayback(value);
            },
            error: failure => {
              if (generation !== current || capture.signal.aborted) return;
              retire();
              setError(failure);
            },
          } : undefined);
          if (capture.signal.aborted || generation !== current) stop();
          else capture.signal.addEventListener("abort", stop, { once: true });
        } catch (failure) {
          if (generation !== current || capture.signal.aborted) return;
          active = null;
          capture.abort();
          setError(failure);
          setPlayback("idle");
          setNeedsPermission(true);
        }
      })();
    };
    const visibility = () => { visible = document.visibilityState !== "hidden"; refresh(); };
    const blur = () => { focused = false; refresh(); };
    const focus = () => { focused = true; visible = document.visibilityState !== "hidden"; refresh(); };
    request.current = () => { if (!active || active.signal.aborted) refresh(true); };
    listen.current = () => refresh(true, true);
    stop.current = () => { key = ""; refresh(); };
    const unlisten = [useSettings.subscribe(() => refresh()), useVoice.subscribe(() => refresh()), useUi.subscribe(() => refresh())];
    document.addEventListener("visibilitychange", visibility);
    window.addEventListener("blur", blur);
    window.addEventListener("focus", focus);
    refresh();
    return () => {
      ++generation;
      retire();
      request.current = () => {};
      listen.current = () => {};
      stop.current = () => {};
      unlisten.forEach(fn => fn());
      document.removeEventListener("visibilitychange", visibility);
      window.removeEventListener("blur", blur);
      window.removeEventListener("focus", focus);
    };
  }, [engine]);

  return { level, needsPermission, error, playback, request: () => request.current(), listen: () => listen.current(), stop: () => stop.current() };
}
