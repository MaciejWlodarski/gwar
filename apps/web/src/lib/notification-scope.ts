import { serverKey, type ServerRef } from "./server-key";
import type { ChatTarget } from "../proto/ChatTarget";
import type { NotificationMode, NotificationOverrides, NotificationSettings } from "../state/settings";
import type { SessionState } from "../state/types";

/** Stable endpoint key, including protocol and custom path; independent of pin discovery. */
export const notificationServerKey = (server: ServerRef & { endpoint?: string | null }): string => serverKey({ ...server, address: server.kind === "teamspeak" ? server.address : server.endpoint ?? server.address }, undefined, null);

export function notificationMode(overrides: NotificationOverrides, server: string, channel: number | null): NotificationMode | undefined {
  return (channel === null ? undefined : overrides.channels[server]?.[channel]) ?? overrides.servers[server];
}

export function scopedNotifications(settings: NotificationSettings, overrides: NotificationOverrides, server: string, target: ChatTarget): NotificationSettings {
  const channel = typeof target === "object" && "channel" in target ? target.channel : null;
  const mode = notificationMode(overrides, server, channel);
  if (!mode) return settings;
  return { mentions: mode !== "none", privateMessages: mode !== "none", allMessages: mode === "all" };
}

/** Attention counts use existing unread and verified-mention counters, never decrypt messages. */
export function notificationBadge(s: SessionState, settings: NotificationSettings, overrides: NotificationOverrides, server: string): number {
  return Object.entries(s.threads).reduce((count, [key, thread]) => {
    const target: ChatTarget = key.startsWith("ch:") ? { channel: Number(key.slice(3)) } : key.startsWith("dm:") ? { client: 0 } : "server";
    const prefs = scopedNotifications(settings, overrides, server, target);
    const dm = key.startsWith("dm:");
    return count + (prefs.allMessages || (dm && prefs.privateMessages) ? thread.unread : prefs.mentions ? thread.mentions : 0);
  }, 0);
}
