import { useMemo } from "react";
import { countKey, useLanguage, useT } from "../i18n";
import type { SealedKey } from "../state/sealed";
import { useSession, useUi } from "../state/stores";
import { useMemberName, useRoster } from "./hooks";
import { Avatar, Dialog } from "./kit";

/** The devices holding an encrypted conversation's keys, grouped by member. */
export function ReadersDialog({ thread }: { thread: SealedKey }) {
  const t = useT();
  const lang = useLanguage();
  const close = useUi((s) => s.closeDialog);
  const roster = useRoster(thread);
  // While securing, the list is the last group this device knew; changes may be on their way.
  const securing = useSession(s => s.threads[thread]?.securing !== false);
  const byUid = useMemo(() => {
    const counts = new Map<string, number>();
    for (const m of roster?.members ?? []) counts.set(m.uid, (counts.get(m.uid) ?? 0) + 1);
    return [...counts];
  }, [roster]);
  return (
    <Dialog open onOpenChange={(o) => !o && close()} title={t("channel.readers")} width="max-w-sm">
      <p className="mb-3 text-sm text-muted">
        {t("chat.messagesEncrypted")}. {roster && !securing ? t(thread === "server" ? "chat.serverReadersHint" : "channel.readersHint") : t(thread === "server" ? "chat.serverReadersPending" : "channel.readersPending")}
      </p>
      <ul className="flex max-h-80 flex-col gap-1 overflow-y-auto">
        {byUid.map(([uid, devices]) => (
          <Reader key={uid} uid={uid} devices={t(countKey(lang, "channel.devices", devices), { count: devices })} />
        ))}
      </ul>
    </Dialog>
  );
}

function Reader({ uid, devices }: { uid: string; devices: string }) {
  const name = useMemberName(uid, uid.slice(0, 8));
  return (
    <li className="flex items-center gap-3 rounded-md px-1 py-1.5">
      <Avatar name={name} seed={uid} size={28} />
      <span className="min-w-0 flex-1 truncate text-sm">{name}</span>
      <span className="shrink-0 text-xs text-subtle">{devices}</span>
    </li>
  );
}
