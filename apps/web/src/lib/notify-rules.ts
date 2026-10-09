import type { ChatMessage } from "../proto/ChatMessage";
import type { NotificationSettings } from "../state/settings";

export type NotifyReason = "dm" | "mention" | "message";

/**
 * Why (if at all) an incoming message should raise a system notification:
 * never for my own messages or while the window is in front, otherwise per the
 * user's choices. Private messages and mentions come first.
 */
export function notifyReason(
  msg: ChatMessage,
  o: { meUid: string | undefined; focused: boolean; settings: NotificationSettings },
): NotifyReason | null {
  if (o.focused || !o.meUid || msg.author_uid === o.meUid) return null;
  if (typeof msg.target === "object" && "client" in msg.target) return o.settings.privateMessages ? "dm" : o.settings.allMessages ? "message" : null;
  if ((msg.mentions ?? []).includes(o.meUid) && o.settings.mentions) return "mention";
  return o.settings.allMessages ? "message" : null;
}

/** Text for the notification body: the message, or the names of the files it carries. */
export function notificationBody(msg: ChatMessage, max = 140): string {
  const text = msg.text.trim() || (msg.attachments ?? []).map((a) => a.name).join(", ");
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}
