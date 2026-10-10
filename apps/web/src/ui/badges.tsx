import { ShieldCheck } from "lucide-react";
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

/** Only the server's verified account handle enables this badge. */
export function ConnectBadge({ handle }: { handle?: string | null }) {
  const t = useT();
  if (!handle) return null;
  const label = t("badge.connectAccount", { handle });
  return <span title={label} aria-label={label} className="inline-flex shrink-0 align-middle text-accent"><ShieldCheck className="size-3.5" /></span>;
}

/** "@handle #tag": the verified Connect handle (if any) and the tag derived from the key. */
export function IdentityLine({ connect, tag, className }: { connect?: string | null; tag?: string | null; className?: string }) {
  if (!connect && !tag) return null;
  return (
    <span className={cn("shrink-0 font-mono text-xs text-muted", className)}>
      {connect && <span className="text-accent">@{connect}</span>}
      {connect && tag && " "}
      {tag && <span>#{tag}</span>}
    </span>
  );
}
