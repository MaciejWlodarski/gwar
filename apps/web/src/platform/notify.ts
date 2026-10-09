/**
 * Cross-platform system notifications. Desktop goes through the Tauri
 * notification plugin, the browser through the `Notification` API (only once
 * the user allowed it). `@tauri-apps/plugin-notification` is loaded lazily.
 *
 * Clicks: the browser reports them. The desktop plugin does not (the OS just
 * activates the app), so there `onClick` fires when the window gets focus
 * shortly after the notification was shown while the app was in the background.
 */
import { isDesktop } from ".";

export type NotificationsPermission = "granted" | "denied" | "default";

export interface NotifyOptions {
  title: string;
  body: string;
  /** Notifications with the same tag replace each other (browser only). */
  tag?: string;
  /** Called when the user clicks the notification (see the note above for the desktop). */
  onClick?: () => void;
}

/** How long after showing a notification a window focus still counts as a click on the desktop. */
export const DESKTOP_CLICK_WINDOW_MS = 15_000;

const desktopPlugin = () => import("@tauri-apps/plugin-notification");

function browserSupported(): boolean {
  return typeof Notification !== "undefined";
}

export async function notificationsPermission(): Promise<NotificationsPermission> {
  if (isDesktop()) {
    if (await (await desktopPlugin()).isPermissionGranted()) return "granted";
    return browserSupported() && Notification.permission === "denied" ? "denied" : "default";
  }
  return browserSupported() ? Notification.permission : "denied";
}

/** Asks the user (browsers need a user gesture for this). True if notifications are allowed afterwards. */
export async function requestNotifications(): Promise<boolean> {
  if (isDesktop()) return (await (await desktopPlugin()).requestPermission()) === "granted";
  if (!browserSupported()) return false;
  if (Notification.permission === "default") return (await Notification.requestPermission()) === "granted";
  return Notification.permission === "granted";
}

/** Shows a notification if permitted; silently does nothing otherwise. */
export function notify({ title, body, tag, onClick }: NotifyOptions): void {
  if (isDesktop()) {
    void desktopNotify(title, body, onClick);
  } else if (browserSupported() && Notification.permission === "granted") {
    const n = new Notification(title, { body, tag });
    n.onclick = () => {
      window.focus();
      n.close();
      onClick?.();
    };
  }
}

async function desktopNotify(title: string, body: string, onClick?: () => void): Promise<void> {
  try {
    const plugin = await desktopPlugin();
    if (!(await plugin.isPermissionGranted())) return;
    const inBackground = typeof document !== "undefined" && !document.hasFocus();
    plugin.sendNotification({ title, body });
    if (inBackground && onClick) armClick(onClick);
  } catch (e) {
    console.warn("[notify] desktop notification failed:", e);
  }
}

function armClick(onClick: () => void): void {
  const done = () => {
    clearTimeout(timer);
    window.removeEventListener("focus", onFocus);
  };
  const onFocus = () => {
    done();
    onClick();
  };
  const timer = setTimeout(done, DESKTOP_CLICK_WINDOW_MS);
  window.addEventListener("focus", onFocus);
}
