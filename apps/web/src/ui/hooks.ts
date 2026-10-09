import { useEffect } from "react";
import type { Permission } from "../proto/Permission";
import { controller } from "../state/controller";
import { hasGlobalShortcut, isGlobalShortcutEvent } from "../platform/desktop";
import { useSettings } from "../state/settings";
import { useSession, useVoice } from "../state/stores";

export function useMe() {
  return useSession((s) => (s.me ? s.clients[s.me.session] : undefined));
}

export function usePermission(p: Permission): boolean {
  return useSession((s) => s.permissions.includes(p));
}

/** Applies data-theme and follows the OS when theme is "system". */
export function useTheme(): void {
  const theme = useSettings((s) => s.theme);
  const language = useSettings((s) => s.language);
  useEffect(() => {
    const mql = window.matchMedia("(prefers-color-scheme: light)");
    const apply = () => {
      document.documentElement.dataset.theme = theme === "system" ? (mql.matches ? "light" : "dark") : theme;
    };
    apply();
    mql.addEventListener("change", apply);
    return () => mql.removeEventListener("change", apply);
  }, [theme]);
  useEffect(() => {
    document.documentElement.lang = language;
  }, [language]);
}

function isEditable(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null;
  return !!el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.isContentEditable);
}

/** Mute / deafen shortcuts and push-to-talk. */
export function useShortcuts(): void {
  useEffect(() => {
    const down = (e: KeyboardEvent) => {
      // On the desktop a global shortcut bound to the same keys already handled it.
      if ((e.ctrlKey || e.metaKey) && e.shiftKey && !e.altKey) {
        if (e.code === "KeyM" && !isGlobalShortcutEvent("mute", e)) {
          e.preventDefault();
          controller.toggleMute();
          return;
        }
        if (e.code === "KeyD" && !isGlobalShortcutEvent("deafen", e)) {
          e.preventDefault();
          controller.toggleDeafen();
          return;
        }
      }
      const { audio } = useSettings.getState();
      if (audio.inputMode === "ptt" && audio.pttKey && e.code === audio.pttKey && !e.repeat) {
        // Do not steal printable keys while typing a message.
        if (isEditable(e.target) && e.key.length === 1) return;
        controller.setPtt(true);
      }
    };
    const up = (e: KeyboardEvent) => {
      const { audio } = useSettings.getState();
      if (audio.pttKey && e.code === audio.pttKey) controller.setPtt(false);
    };
    // A global push-to-talk shortcut reports its own release, even when unfocused.
    const blur = () => {
      if (!hasGlobalShortcut("ptt")) controller.setPtt(false);
    };
    window.addEventListener("keydown", down);
    window.addEventListener("keyup", up);
    window.addEventListener("blur", blur);
    return () => {
      window.removeEventListener("keydown", down);
      window.removeEventListener("keyup", up);
      window.removeEventListener("blur", blur);
    };
  }, []);
}

export function useVoiceSummary() {
  const muted = useVoice((s) => s.muted);
  const deafened = useVoice((s) => s.deafened);
  return { muted, deafened };
}

/** Human label for a KeyboardEvent.code ("KeyV" -> "V"). */
export function keyLabel(code: string): string {
  if (code.startsWith("Key")) return code.slice(3);
  if (code.startsWith("Digit")) return code.slice(5);
  const map: Record<string, string> = {
    Space: "Space",
    ControlLeft: "Left Ctrl",
    ControlRight: "Right Ctrl",
    ShiftLeft: "Left Shift",
    ShiftRight: "Right Shift",
    AltLeft: "Left Alt",
    AltRight: "Right Alt",
    Backquote: "`",
    CapsLock: "Caps Lock",
  };
  return map[code] ?? code;
}

export const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform);
export const modKey = isMac ? "⌘" : "Ctrl";
