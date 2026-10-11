import { useT } from "../i18n";
import type { Phase } from "../state/types";

/** Connection risk stays visible throughout the connected UI, including reconnects. */
export function ConnectionWarning({ phase, insecure }: { phase: Phase; insecure: boolean }) {
  const t = useT();
  if (!insecure || (phase !== "online" && phase !== "reconnecting")) return null;
  return <div role="alert" data-testid="connection-warning" className="shrink-0 bg-red-700 px-4 py-2 text-sm text-white">{t("security.insecureWarning")}</div>;
}
