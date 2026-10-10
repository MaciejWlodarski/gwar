/**
 * Desktop-only wiring between the shared UI state and the Tauri shell:
 * global shortcuts (push-to-talk, mute, deafen), the system tray and the
 * close-to-tray option. Everything here is a no-op in the browser.
 */
import { tNow } from "../i18n";
import { controller } from "../state/controller";
import { useSettings } from "../state/settings";
import { useSession, useUi, useVoice } from "../state/stores";
import { isDesktop, tauri, type TauriBridge, type Unlisten } from ".";

type Action = "ptt" | "mute" | "deafen";

/** The global shortcuts that are really registered (the window handlers step aside for those). */
const registered: Record<Action, string | null> = { ptt: null, mute: null, deafen: null };

export function hasGlobalShortcut(action: Action): boolean {
  return registered[action] !== null;
}

/** True if this key press is the registered global shortcut for `action` (so the OS already handled it). */
export function isGlobalShortcutEvent(action: Action, e: KeyboardEvent): boolean {
  return registered[action] !== null && registered[action] === acceleratorFromEvent(e);
}

/** KeyboardEvent -> Tauri accelerator (`Control+Shift+KeyM`), or null for bare modifier presses. */
export function acceleratorFromEvent(e: KeyboardEvent): string | null {
  if (/^(Control|Shift|Alt|Meta)(Left|Right)$/.test(e.code)) return null;
  const parts = [e.ctrlKey && "Control", e.altKey && "Alt", e.shiftKey && "Shift", e.metaKey && "Super"].filter(Boolean);
  return [...parts, e.code].join("+");
}

/** "Control+Shift+KeyM" -> ["Ctrl", "Shift", "KeyM"] for display. */
export function acceleratorParts(accelerator: string): string[] {
  return accelerator.split("+").map((p) => (p === "Control" ? "Ctrl" : p === "Super" ? "Cmd/Win" : p));
}

/**
 * Starts the desktop wiring. Returns a cleanup function. Call once from the
 * root component.
 */
export function initDesktop(): (() => void) | undefined {
  if (!isDesktop()) return undefined;
  let disposed = false;
  const cleanups: Unlisten[] = [];
  const cleanup = (fn: Unlisten) => (disposed ? fn() : cleanups.push(fn));

  const register = async (api: TauriBridge, action: Action, accelerator: string | null) => {
    try {
      await api.invoke("set_global_shortcut", { action, accelerator });
      registered[action] = accelerator;
    } catch (e) {
      registered[action] = null;
      console.warn(`[desktop] shortcut ${action} ${accelerator}:`, e);
      if (accelerator) useUi.getState().toast("error", tNow("audio.shortcutFailed", { key: accelerator }));
    }
  };

  void (async () => {
    const api = await tauri();
    if (disposed) return;
    cleanup(await api.listen<{ down: boolean }>("shortcut://ptt", (p) => controller.setPtt(p.down)));
    const act = (action: string) => {
      if (action === "mute") controller.toggleMute();
      else if (action === "deafen") controller.toggleDeafen();
    };
    cleanup(await api.listen<{ action: string }>("shortcut://action", (p) => act(p.action)));
    cleanup(await api.listen<{ action: string }>("tray://action", (p) => act(p.action)));

    // Settings -> global shortcuts. Only changed values are re-registered.
    const last: Record<Action, string | null | undefined> = { ptt: undefined, mute: undefined, deafen: undefined };
    let lastClose: boolean | undefined;
    const sync = () => {
      const { audio, desktop } = useSettings.getState();
      const wanted: Record<Action, string | null> = {
        ptt: audio.inputMode === "ptt" ? audio.pttKey : null,
        mute: desktop.muteShortcut,
        deafen: desktop.deafenShortcut,
      };
      for (const action of ["ptt", "mute", "deafen"] as const) {
        if (last[action] === wanted[action]) continue;
        last[action] = wanted[action];
        void register(api, action, wanted[action]);
      }
      if (lastClose !== desktop.closeToTray) {
        lastClose = desktop.closeToTray;
        void api.invoke("set_close_to_tray", { enabled: desktop.closeToTray });
      }
    };
    sync();
    cleanup(useSettings.subscribe(sync));

    // Keep the tray menu in step with the UI.
    const pushTray = () => {
      const { muted, deafened } = useVoice.getState();
      void api.invoke("tray_set_state", { muted, deafened });
    };
    pushTray();
    cleanup(useVoice.subscribe((s, prev) => (s.muted !== prev.muted || s.deafened !== prev.deafened ? pushTray() : undefined)));

    await debugAutoconnect(api);
  })();

  return () => {
    disposed = true;
    for (const fn of cleanups.splice(0)) fn();
  };
}

/**
 * Debug builds only: `VC_AUTOCONNECT="address|kind"` connects on startup
 * and reports progress to the Rust log, for automated verification.
 */
async function debugAutoconnect(api: TauriBridge): Promise<void> {
  const spec = await api.invoke<string | null>("debug_autoconnect").catch(() => null);
  if (!spec) return;
  const [address = "", kindName = "vc"] = spec.split("|");
  const kind = kindName === "teamspeak" ? "teamspeak" : "vc";
  const log = (message: string) => void api.invoke("debug_log", { message }).catch(() => {});
  // Handles for scripted checks (VC_DEBUG_EVAL_FILE); only exist in debug runs with autoconnect.
  Object.assign(window, { __vc: { controller, useSettings, useSession, useVoice, useUi, api, log } });
  log(`autoconnect ${address} as ${(await controller.getIdentity()).nickname} (${kind})`);
  useSession.subscribe((s, prev) => {
    if (s.phase !== prev.phase) log(`session phase: ${s.phase}`);
  });
  useVoice.subscribe((s, prev) => {
    if (s.state !== prev.state) log(`voice state: ${s.state}`);
    if (s.micError !== prev.micError) log(`mic error: ${s.micError ? `${s.micError.kind} ${s.micError.message}` : "cleared"}`);
  });
  const ok = await controller.connectInteractive({ kind, address }, { remember: false });
  log(`connect ${ok ? "ok" : "failed"}`);
}
