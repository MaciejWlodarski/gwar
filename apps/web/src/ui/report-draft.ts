import { create } from "zustand";
import { connectApi } from "../connect/api";
import { tNow } from "../i18n";
import { encodeBase64Url } from "../net/base64url";
import { loadConnectRecord } from "../net/identity";
import { describeRequestError } from "../state/controller";
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
 * sent or cancelled.
 */
export const useReportDraft = create<ReportDraft>()(() => EMPTY);

let nextShot = 0;
export const newShotId = () => nextShot++;

/** Drops the draft and its screenshots' preview URLs. */
export function clearReportDraft(): void {
  for (const s of useReportDraft.getState().shots) URL.revokeObjectURL(s.url);
  useReportDraft.setState(EMPTY);
}

const reportShown = () => useUi.getState().section === "bugReport";

/** Sends the draft. Sent, it is dropped and the page left; failed, it stays, the page says why (a toast, if left). */
export async function sendReport(diagnostics: Record<string, unknown> | null): Promise<void> {
  const { text, withDiagnostics, shots, busy } = useReportDraft.getState();
  if (!text.trim() || busy) return;
  useReportDraft.setState({ busy: true, error: null });
  try {
    const images = await Promise.all(shots.map(async (s) => ({ mime: s.blob.type, data: encodeBase64Url(new Uint8Array(await s.blob.arrayBuffer())) })));
    const record = await loadConnectRecord().catch(() => undefined);
    const id = await connectApi.report({ text: text.trim(), diagnostics: withDiagnostics && diagnostics ? diagnostics : { app: { omitted: true } }, images }, record?.token);
    useUi.getState().toast("success", tNow("report.sent", { id }));
    clearReportDraft();
    if (reportShown()) leavePage();
  } catch (err) {
    const error = describeRequestError(err);
    useReportDraft.setState({ busy: false, error });
    if (!reportShown()) useUi.getState().toast("error", tNow("report.failed", { error }));
  }
}
