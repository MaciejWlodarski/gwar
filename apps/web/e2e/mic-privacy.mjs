/** Inspect real authorization and capture ownership; independent of visible headings. */
export function installMicPrivacyGuard(injected) {
  window.__gum = 0;
  window.__unexpectedPreview = 0;
  window.__micStreams = [];
  window.__micPrivacyViolations = [];
  let pending = 0;
  let pendingPreviews = 0;
  let focused = true;
  const previews = new Set();
  const violation = message => {
    window.__unexpectedPreview++;
    window.__micPrivacyViolations.push(message);
  };
  const live = stream => stream.getTracks().some(track => track.readyState === "live");
  let read;
  const eligible = state => state.section === "settings" && state.tab === "audio" && state.mode === "vad" && !state.muted && !state.deafened && state.visible && focused && state.idle;
  const released = () => {
    const state = read();
    if (!eligible(state) && [...previews].some(live)) violation("Preview capture survived loss of authorization");
  };
  const changed = () => {
    // Drain the engine's nested awaits when permission returns after cancellation.
    // This stays within the event: a timer or later navigation cannot hide capture.
    const drain = remaining => queueMicrotask(() => remaining ? drain(remaining - 1) : released());
    drain(8);
  };
  const ready = injected ? Promise.resolve(injected) : (async () => {
    while (!window.__gwarE2eImport) await new Promise(resolve => setTimeout(resolve, 0));
    const [{ useUi, useVoice }, { useSettings }] = await Promise.all([
      window.__gwarE2eImport("/src/state/stores.ts"), window.__gwarE2eImport("/src/state/settings.ts"),
    ]);
    return {
      read: () => {
        const ui = useUi.getState(), voice = useVoice.getState();
        return { section: ui.section, tab: ui.settingsTab, mode: useSettings.getState().audio.inputMode, muted: voice.muted, deafened: voice.deafened, idle: voice.state === "idle", visible: document.visibilityState !== "hidden" };
      },
      subscribe: fn => { useUi.subscribe(fn); useVoice.subscribe(fn); useSettings.subscribe(fn); },
    };
  })();
  const initialized = ready.then(state => {
    read = state.read;
    state.subscribe(changed);
    document.addEventListener("visibilitychange", changed);
    window.addEventListener("focus", () => { focused = true; });
    window.addEventListener("blur", () => {
      focused = false;
      queueMicrotask(() => { if ([...previews].some(live)) violation("Preview capture survived window blur"); });
    });
  });
  const media = navigator.mediaDevices;
  if (!media?.getUserMedia) return;
  const native = media.getUserMedia.bind(media);
  media.getUserMedia = async (...args) => {
    window.__gum++;
    const priorPending = pending++;
    await initialized;
    const state = read();
    const preview = state.idle;
    const previewInFlight = pendingPreviews > 0;
    if (preview) pendingPreviews++;
    if (preview && !eligible(state)) violation("Preview requested without authorization");
    if ((preview || previewInFlight || [...previews].some(live)) && (priorPending || window.__micStreams.some(live))) violation("Overlapping microphone acquisition or live capture");
    try {
      const stream = await native(...args);
      window.__micStreams.push(stream);
      if (preview) { previews.add(stream); changed(); }
      return stream;
    } finally { pending--; if (preview) pendingPreviews--; }
  };
}
