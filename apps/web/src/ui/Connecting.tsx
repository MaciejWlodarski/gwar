import { Loader2 } from "lucide-react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { controller } from "../state/controller";
import { useSettings } from "../state/settings";
import { useConnectUi, useSession } from "../state/stores";
import { Button, panelCard } from "./kit";

/** The server shell stays visible while the single startup connection is attempted. */
export function Connecting({ card = false }: { card?: boolean }) {
  const t = useT();
  const saved = useSettings((s) => s.resumeConnection);
  const address = useSession((s) => s.address);
  const retrying = useConnectUi((s) => s.startupRetry);
  return (
    <main className={cn("flex min-w-0 flex-1 flex-col items-center justify-center gap-4 bg-surface px-6 text-center", card && panelCard)}>
      <Loader2 aria-hidden className="size-6 animate-spin text-muted" />
      <div className="max-w-full" role="status">
        <h1 className="text-lg font-semibold">{t("connect.connecting")}</h1>
        <p className="mt-1 break-words text-sm text-muted">{saved?.name || address}</p>
        {retrying && <p className="mt-3 max-w-sm text-sm text-muted">{t("connect.retrying")}</p>}
      </div>
      <Button variant="secondary" onClick={() => void controller.disconnect()}>{t("common.cancel")}</Button>
    </main>
  );
}
