import { create } from "zustand";
import { connectApi } from "../connect/api";
import { tNow } from "../i18n";
import { encodeBase64Url } from "../net/base64url";
import { loadConnectRecord } from "../net/identity";
import { describeRequestError } from "../state/controller";
import { useAccountIdentity } from "../state/identity";
import { useUi } from "../state/stores";
import { leavePage } from "./hooks";

interface Shot { id: number; blob: Blob; url: string }

interface ReportDraft {
  text: string;
  withDiagnostics: boolean;
  shots: Shot[];
  busy: boolean;
  error: string | null;
}

const EMPTY: ReportDraft = { text: "", withDiagnostics: true, shots: [], busy: false, error: null };

/**
 * The report being written, outside the page: leaving it (another section, the system's back, a notification) keeps
 * the draft, and a send still going reports how it went. In memory only, as screenshots may be large. Dropped once
 * sent or cancelled, and when another identity takes over (signing in or out, an import): it is not theirs to read or
 * send.
 */
export const useReportDraft = create<ReportDraft>()(() => EMPTY);

let nextShot = 0;
export const newShotId = () => nextShot++;

/** Which draft this is: a send's result belongs to the draft it was sent from, not to one written after it. */
let draft = 0;

/** Drops the draft and its screenshots' preview URLs. */
export function clearReportDraft(): void {
  draft++;
  for (const s of useReportDraft.getState().shots) URL.revokeObjectURL(s.url);
  useReportDraft.setState(EMPTY);
}

useAccountIdentity.subscribe((s, prev) => {
  if (s.replaced !== prev.replaced) clearReportDraft();
});

const reportShown = () => useUi.getState().section === "bugReport";

/**
 * Sends the draft. Sent, it is dropped and the page left; failed, it stays, the page says why (a toast, if left).
 * A draft dropped meanwhile only gets the toast.
 */
export async function sendReport(diagnostics: Record<string, unknown> | null): Promise<void> {
  const { text, withDiagnostics, shots, busy } = useReportDraft.getState();
  if (!text.trim() || busy) return;
  const sent = draft;
  useReportDraft.setState({ busy: true, error: null });
  try {
    const images = await Promise.all(shots.map(async (s) => ({ mime: s.blob.type, data: encodeBase64Url(new Uint8Array(await s.blob.arrayBuffer())) })));
    const record = await loadConnectRecord().catch(() => undefined);
    const id = await connectApi.report({ text: text.trim(), diagnostics: withDiagnostics && diagnostics ? diagnostics : { app: { omitted: true } }, images }, record?.token);
    useUi.getState().toast("success", tNow("report.sent", { id }));
    if (sent !== draft) return;
    clearReportDraft();
    if (reportShown()) leavePage();
  } catch (err) {
    const error = describeRequestError(err);
    const current = sent === draft;
    if (current) useReportDraft.setState({ busy: false, error });
    if (!current || !reportShown()) useUi.getState().toast("error", tNow("report.failed", { error }));
  }
}
