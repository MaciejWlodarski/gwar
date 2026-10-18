import { decodeBase64Url } from "../net/base64url";
import type { HandleMember } from "../net/handle-attestation";
import { useConnectHandle } from "./useConnectHandle";
import { BadgeCheck, BadgeInfo } from "lucide-react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { unreadLabel } from "../lib/chat";

/** Pill with the number of unread messages. */
export function UnreadBadge({ count, className }: { count: number; className?: string }) {
  const t = useT();
  if (count <= 0) return null;
  return (
    <span
      title={t("tree.unread", { count: unreadLabel(count) })}
      className={cn("shrink-0 rounded-full bg-accent px-1.5 text-[11px] leading-4 font-semibold text-accent-fg tabular-nums", className)}
    >
      {unreadLabel(count)}
    </span>
  );
}

/** Pill with the number of unread messages that mention me. */
export function MentionBadge({ count, className }: { count: number; className?: string }) {
  const t = useT();
  if (count <= 0) return null;
  return (
    <span
      title={t("tree.mentions", { count: unreadLabel(count) })}
      className={cn("shrink-0 rounded-full bg-danger px-1.5 text-[11px] leading-4 font-semibold text-white tabular-nums", className)}
    >
      @{unreadLabel(count)}
    </span>
  );
}

/** Marks a server as a TeamSpeak server (as opposed to a native vc/1 one). */
export function TeamSpeakBadge({ className, short }: { className?: string; short?: boolean }) {
  const t = useT();
  return (
    <span
      title={t("badge.teamspeak")}
      className={cn("shrink-0 rounded bg-accent-soft px-1.5 py-px text-[10px] leading-4 font-semibold tracking-wide text-accent uppercase", className)}
    >
      {short ? t("badge.teamspeakShort") : t("badge.teamspeak")}
    </span>
  );
}

/** Only a handle verified by this client enables the badge. Next to a server-chosen `name` that is not the
 *  handle itself, the handle is shown in full, so the check never vouches for a nickname. A handle this client
 *  saw on a different account before gets a neutral marker instead of the check, with the handle always shown. */
export function ConnectBadge({ member, name }: { member?: HandleMember | null; name?: string }) {
  const t = useT();
  const connect = useConnectHandle(member);
  if (!connect) return null;
  const { handle, status } = connect;
  if (status === "changed") {
    const label = t("badge.handleChanged", { handle });
    return (
      <span title={label} aria-label={label} data-testid="connect-badge-changed" className="inline-flex min-w-0 shrink-[3] items-center gap-0.5 align-middle text-muted">
        <BadgeInfo aria-hidden className="size-3.5 shrink-0" />
        <span className="truncate font-mono text-xs">@{handle}</span>
      </span>
    );
  }
  const label = t("badge.connectAccount", { handle });
  const visible = name !== undefined && !sameAsHandle(name, handle);
  return (
    // In a tight row the handle gives way first (the nickname is the name people read); the
    // check mark always stays.
    <span title={label} aria-label={label} data-testid="connect-badge" className="inline-flex min-w-0 shrink-[3] items-center gap-0.5 align-middle text-accent">
      <BadgeCheck aria-hidden className="size-3.5 shrink-0" />
      {visible && <span className="truncate font-mono text-xs">@{handle}</span>}
    </span>
  );
}

function sameAsHandle(name: string, handle: string): boolean {
  const n = name.trim().replace(/^@/, "");
  return n === handle || n.toLowerCase() === handle.toLowerCase();
}

/** "@handle #tag": the verified Connect handle (if any) and the tag derived from the key. */
export function IdentityLine({ member, className }: { member?: HandleMember | null; tag?: string | null; className?: string }) {
  const t = useT();
  const connect = useConnectHandle(member);
  const tag = member?.uid ? localTag(member.uid) : undefined;
  if (!connect && !tag) return null;
  const changed = connect?.status === "changed";
  return (
    <span className={cn("shrink-0 font-mono text-xs text-muted", className)}>
      {connect && (
        <span className={changed ? undefined : "text-accent"} title={changed ? t("badge.handleChanged", { handle: connect.handle }) : undefined}>
          @{connect.handle}
        </span>
      )}
      {connect && tag && " "}
      {tag && <span>#{tag}</span>}
    </span>
  );
}

export function localTag(uid: string): string | undefined {
  try {
    const bytes = uid.startsWith("ts:") ? Uint8Array.from(atob(uid.slice(3)), (c) => c.charCodeAt(0)) : decodeBase64Url(uid);
    let bits = 0, value = 0, out = "";
    for (const b of bytes) { value = (value << 8) | b; bits += 8; while (bits >= 5) { bits -= 5; out += "abcdefghijklmnopqrstuvwxyz234567"[(value >>> bits) & 31]; } }
    return out.slice(0, 10) || undefined;
  } catch { return undefined; }
}
