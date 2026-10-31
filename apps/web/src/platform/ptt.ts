/** Only bound PTT state crosses IPC. Native observation owns capture on macOS/Windows. */
import { create } from "zustand";
import { useSettings } from "../state/settings";
import { useVoice } from "../state/stores";
import { isDesktop, type TauriBridge, type Unlisten } from ".";

export type PttStatus = "off" | "ready" | "permission" | "recovering" | "unsupported_key" | "local_only" | "legacy";
export const useGlobalPtt = create<{ status: PttStatus; native: boolean; capturing: boolean }>(() => ({ status: "off", native: isDesktop(), capturing: false }));

/** The local window must never bypass a missing global-input permission or a failed hook. */
export function nativeOwnsPtt(): boolean { return useGlobalPtt.getState().capturing || (isDesktop() && useGlobalPtt.getState().native); }
export function mousePttCode(button: number): string | null { return button === 1 ? "Mouse3" : button === 3 || button === 4 ? `Mouse${button + 1}` : null; }

export async function initGlobalPtt(api: TauriBridge): Promise<Unlisten> {
  let disposed = false, revision = 0, statusEvents = 0;
  let last: string | null | undefined;
  let pending = Promise.resolve();
  const report = (status: PttStatus) => {
    useGlobalPtt.setState({ status });
    if (status !== "ready" && status !== "legacy") useVoice.getState().set({ pttActive: false });
  };
  const unstatus = await api.listen<PttStatus>("ptt://status", status => { if (!disposed) { ++statusEvents; report(status); } });
  const unstate = await api.listen<boolean>("ptt://state", down => {
    // Reflect in the UI only: sending this back to voice_set_ptt could renew a stale press.
    if (!disposed) useVoice.getState().set({ pttActive: down && useGlobalPtt.getState().status === "ready" });
  });
  const sync = () => {
    const { audio } = useSettings.getState();
    const key = audio.inputMode === "ptt" && !useGlobalPtt.getState().capturing ? audio.pttKey : null;
    if (last === key) return;
    last = key;
    const current = ++revision;
    useVoice.getState().set({ pttActive: false });
    useGlobalPtt.setState({ status: key ? "recovering" : "off", native: true });
    pending = pending.then(async () => {
      if (disposed || current !== revision) return;
      try {
        const beforeEvents = statusEvents;
        let status = await api.invoke<PttStatus>("set_global_ptt", { key });
        if (disposed || current !== revision) return;
        const native = status !== "local_only";
        if (!native && key) {
          // Linux keeps its existing global keyboard shortcut where supported.
          try { await api.invoke("set_global_shortcut", { action: "ptt", accelerator: key }); status = "legacy"; } catch { /* Window-local fallback. */ }
        }
        if (!disposed && current === revision) {
          useGlobalPtt.setState({ native });
          if (beforeEvents === statusEvents) report(status);
        }
      } catch {
        if (!disposed && current === revision) useGlobalPtt.setState({ status: "recovering", native: true });
      }
    });
  };
  sync();
  const unsubscribe = useSettings.subscribe(sync);
  const uncapture = useGlobalPtt.subscribe((s, p) => { if (s.capturing !== p.capturing) sync(); });
  const health = setInterval(() => {
    const current = revision, beforeEvents = statusEvents;
    if (!last || !useGlobalPtt.getState().native) return;
    void api.invoke<PttStatus>("global_ptt_status").then(status => {
      if (!disposed && current === revision && beforeEvents === statusEvents) report(status);
    }).catch(() => { if (!disposed && current === revision) report("recovering"); });
  }, 1000);
  return () => {
    clearInterval(health);
    disposed = true; ++revision;
    unsubscribe(); uncapture(); unstatus(); unstate();
    useVoice.getState().set({ pttActive: false });
    void pending.then(() => api.invoke("set_global_ptt", { key: null })).catch(() => {});
  };
}
