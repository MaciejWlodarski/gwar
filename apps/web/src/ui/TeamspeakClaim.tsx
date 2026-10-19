import { useState } from "react";
import { useT } from "../i18n";
import type { TsEntryInfo } from "../connect/teamspeak";
import { shortUid } from "../connect/ts-list";
import { useSession, useUi } from "../state/stores";
import { Button, Dialog } from "./kit";

/** Offers to bring the roles of these TeamSpeak identities here (docs/teamspeak-import.md, "Claim"). */
export function ClaimDialog({ identities, resolve }: { identities: TsEntryInfo[]; resolve: (accepted: boolean) => void }) {
  const t = useT();
  // The same codes the desktop's own question shows, to check that it asks about this account and server.
  const account = useSession((s) => s.me?.uid ?? "").slice(0, 8);
  const server = useSession((s) => s.serverKey ?? "").slice(0, 8);
  const closeDialog = useUi((s) => s.closeDialog);
  const [answered, setAnswered] = useState(false);
  const answer = (accepted: boolean) => {
    if (answered) return;
    setAnswered(true);
    resolve(accepted);
    closeDialog();
  };
  return (
    <Dialog
      open
      onOpenChange={(o) => !o && answer(false)}
      title={t("tsClaim.title")}
      description={t("tsClaim.body")}
      footer={
        <>
          <Button onClick={() => answer(false)}>{t("tsClaim.later")}</Button>
          <Button variant="primary" onClick={() => answer(true)}>
            {t("tsClaim.accept")}
          </Button>
        </>
      }
    >
      <ul aria-label={t("tsClaim.linked")} className="flex flex-col gap-1">
        {identities.map((i) => (
          <li key={i.uid} className="rounded-lg border border-line px-3 py-2">
            <span className="block truncate text-sm font-medium">{i.name}</span>
            <span className="block truncate font-mono text-xs text-muted">{shortUid(i.uid)}</span>
          </li>
        ))}
      </ul>
      <p className="text-xs text-muted">{t("tsClaim.codes", { account, server })}</p>
    </Dialog>
  );
}
