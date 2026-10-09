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
