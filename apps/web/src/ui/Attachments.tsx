import { boundedImageBlob, verifiedAttachmentBlob } from "../net/attachment";
import { imageBudget, imageMemory, type ImageBudget } from "../net/image-budget";
import * as DialogPrimitive from "@radix-ui/react-dialog";
import { Download, File as FileIcon, FileArchive, FileAudio, FileText, FileVideo, Loader2, X } from "lucide-react";
import { useEffect, useRef, useState, type ReactNode, type RefCallback } from "react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { fileKind, fitBox, formatBytes, isInlineImage, type FileKind } from "../lib/files";
import { isDesktop } from "../platform";
import { fetchFileBytes, saveBlob } from "../platform/download";
import type { Attachment } from "../proto/Attachment";
import { useSession, useUi } from "../state/stores";
import { Button, IconButton, Tooltip } from "./kit";

const MAX_W = 400;
const MAX_H = 300;

const KIND_ICON: Record<FileKind, ReactNode> = {
  image: <FileIcon className="size-5" />,
  audio: <FileAudio className="size-5" />,
  video: <FileVideo className="size-5" />,
  archive: <FileArchive className="size-5" />,
  text: <FileText className="size-5" />,
  other: <FileIcon className="size-5" />,
};

function AttachmentState({ failed, onLoad, untrusted = false }: { failed: boolean; onLoad?: () => void; untrusted?: boolean }) {
  const t = useT();
  return <span className="text-xs text-subtle"><span role={failed ? "alert" : "status"}>{untrusted ? t("file.unverified") : t(failed ? "file.notVerified" : "file.verifying")}</span>
    {onLoad && <Tooltip label={t("file.unverifiedRisk")}><Button size="sm" variant="ghost" className="ml-2 underline" onClick={onLoad}>{t("file.loadUnverified")}</Button></Tooltip>}</span>;
}

/** Images more than this far outside the chat's visible area are not loaded, or are released. */
const NEAR = "1500px 0px";

function scrollParent(node: Element): Element | null {
  for (let p = node.parentElement; p; p = p.parentElement) if (/auto|scroll/.test(getComputedStyle(p).overflowY)) return p;
  return null;
}

/** Whether the element is near the visible part of its scroll container. */
function useNearView<T extends Element>(): [RefCallback<T>, boolean] {
  const [node, setNode] = useState<T | null>(null);
  const [near, setNear] = useState(false);
  const observable = typeof IntersectionObserver !== "undefined";
  useEffect(() => {
    if (!node || !observable) return;
    const observer = new IntersectionObserver((entries) => setNear(entries.some((e) => e.isIntersecting)), { root: scrollParent(node), rootMargin: NEAR });
    observer.observe(node);
    return () => observer.disconnect();
  }, [node, observable]);
  return [setNode, near || !observable];
}

type UrlOptions = { autoLoad?: boolean; active?: boolean; budget?: ImageBudget };
function useAttachmentUrl(att: Attachment, verified: boolean, { autoLoad = true, active = true, budget }: UrlOptions = {}) {
  const origin = useSession((s) => s.httpOrigin), limit = useSession((s) => s.server?.upload_limit ?? 0);
  const [explicit, setExplicit] = useState<string>();
  const [requested, setRequested] = useState<string>();
  const key = JSON.stringify([origin, att, limit, verified]);
  const [loaded, setLoaded] = useState<{ key: string; url: string; failed: boolean }>();
  const load = () => setExplicit(key);
  const open = () => setRequested(key);
  const allowed = verified || explicit === key;
  useEffect(() => {
    if (!active || !allowed || (!autoLoad && requested !== key && explicit !== key)) return;
    const abort = new AbortController(), signal = abort.signal;
    let url: string | undefined, release: (() => void) | undefined;
    const opts = { origin, size: att.size, uploadLimit: limit, signal };
    // With a budget, the fetch waits for a slot and decoding waits until the decoded image fits in memory;
    // the memory stays reserved until the image is released.
    void (async () => {
      const slot = await budget?.fetchSlot(signal);
      try {
        const blob = explicit === key ? new Blob([await fetchFileBytes(att.url, opts)], { type: att.mime }) : await verifiedAttachmentBlob(att, att.url, undefined, opts);
        const image = await boundedImageBlob(blob, budget && (async (width, height) => {
          release = await budget.reserve(imageMemory(width, height, blob.size), signal);
          signal.throwIfAborted();
        }));
        signal.throwIfAborted();
        url = URL.createObjectURL(image); setLoaded({ key, url, failed: false });
      } finally { slot?.(); }
    })().catch(() => { release?.(); if (!signal.aborted) setLoaded({ key, url: "", failed: true }); });
    return () => {
      abort.abort(); release?.();
      if (url) { URL.revokeObjectURL(url); const gone = url; setLoaded((l) => l?.url === gone ? undefined : l); }
    };
  }, [key, allowed, explicit, origin, limit, att, autoLoad, requested, active, budget]);
  return { open, requested: requested === key || explicit === key, url: loaded?.key === key ? loaded.url : "", failed: loaded?.key === key && loaded.failed, load, allowed, unverified: explicit === key };
}
function useSave(att: Attachment, verified: boolean) {
  const t = useT();
  const [busy, setBusy] = useState(false), [failed, setFailed] = useState(false);
  const abort = useRef<AbortController | null>(null);
  useEffect(() => () => abort.current?.abort(), []);
  /** Unverified bytes are saved only when the caller's control says so; a retry verifies again. */
  const run = async (unverified: boolean) => {
    abort.current?.abort(); const request = new AbortController(); abort.current = request; setBusy(true);
    try {
      const s = useSession.getState(), opts = { origin: s.httpOrigin, size: att.size, uploadLimit: s.server?.upload_limit ?? 0, signal: request.signal };
      const blob = verified && !unverified ? await verifiedAttachmentBlob(att, att.url, undefined, opts)
        : new Blob([await fetchFileBytes(att.url, opts)], { type: att.mime });
      request.signal.throwIfAborted();
      const ok = await saveBlob(blob, att.name);
      if (ok && isDesktop() && !request.signal.aborted) useUi.getState().toast("success", t("file.saved", { name: att.name }));
    } catch {
      if (!request.signal.aborted) { setFailed(true); useUi.getState().toast("error", t("file.notVerified")); }
    } finally { if (!request.signal.aborted) setBusy(false); }
  };
  return { busy, run, failed };
}

function DownloadButton({ att, verified }: { att: Attachment; verified: boolean }) {
  const t = useT();
  const { busy, run, failed } = useSave(att, verified);
  // After a failed check the same button is relabeled as an unverified download.
  return (
    <IconButton label={failed || !verified ? t("file.downloadUnverified") : t("file.download")} size="sm" onClick={() => void run(failed || !verified)} disabled={busy}>
      {busy ? <Loader2 className="size-4 animate-spin" /> : failed ? <X className="size-4 text-danger" /> : <Download className="size-4" />}
    </IconButton>
  );
}

function ImageAttachment({ att, verified }: { att: Attachment; verified: boolean }) {
  const t = useT();
  const [failed, setFailed] = useState(false);
  const [ref, near] = useNearView<HTMLDivElement>();
  const { url, failed: hashFailed, load, allowed, unverified } = useAttachmentUrl(att, verified, { active: near, budget: imageBudget });
  const box = att.width && att.height ? fitBox(att.width, att.height, MAX_W, MAX_H) : null;
  return <div ref={ref} className="max-w-full">{body()}</div>;
  function body() {
  if (hashFailed) return <AttachmentState failed untrusted={!allowed} onLoad={load} />;
  // Until it is loaded (or after it was released far off screen) the image keeps its box, so the chat does not shift.
  if (!url) return allowed && box ? <div style={{ width: box.width, height: box.height }} className="flex max-w-full items-center justify-center rounded-lg bg-side"><AttachmentState failed={false} /></div>
    : <AttachmentState failed={false} untrusted={!allowed} onLoad={!allowed ? load : undefined} />;
  if (failed) return <FileCard att={att} verified={verified} />;
  return (
    <>{unverified && <span className="text-xs text-warn">{t("file.loadedUnverified")}</span>}<button
      type="button"
      disabled={!url}
      aria-label={t("file.open", { name: att.name })}
      onClick={() => useUi.getState().openDialog({ kind: "lightbox", attachment: att, verified: verified && !unverified })}
      // The box is sized up front from the server's dimensions, so loading does not shift the chat.
      style={box ? { width: box.width, height: box.height } : { maxWidth: MAX_W, maxHeight: MAX_H }}
      className="block max-w-full cursor-zoom-in overflow-hidden rounded-lg bg-side shadow-[0_0_0_1px_var(--line)]"
    >
      <img
        src={url}
        alt={att.name}
        loading="lazy"
        decoding="async"
        onError={() => setFailed(true)}
        width={box?.width}
        height={box?.height}
        className={cn("size-full object-contain", !box && "max-h-[300px] max-w-[400px]")}
      />
    </button></>
  );
  }
}

function VideoAttachment({ att, verified }: { att: Attachment; verified: boolean }) {
  const t = useT();
  const { url, failed, load, allowed, unverified, open, requested } = useAttachmentUrl(att, verified, { autoLoad: false });
  if (verified && !requested) return <Button size="sm" onClick={open}>{t("file.open", { name: att.name })}</Button>;
  if (failed || !url) return <AttachmentState failed={failed} untrusted={!allowed} onLoad={failed || !allowed ? load : undefined} />;
  const box = att.width && att.height ? fitBox(att.width, att.height, MAX_W, MAX_H) : null;
  return (
    <div className="flex max-w-full flex-col overflow-hidden rounded-lg border border-line bg-side" style={{ width: box?.width ?? MAX_W }}>
      {unverified && <span className="text-xs text-warn">{t("file.loadedUnverified")}</span>}
      <video
        controls
        preload="metadata"
        playsInline
        src={url}
        style={box ? { height: box.height } : { maxHeight: MAX_H }}
        className="w-full bg-black object-contain"
      />
      <div className="flex items-center gap-2 px-2.5 py-1.5">
        <span className="min-w-0 flex-1 truncate text-xs text-muted" title={att.name}>
          {att.name} · {formatBytes(att.size)}
        </span>
        <DownloadButton att={att} verified={verified} />
      </div>
    </div>
  );
}

function AudioAttachment({ att, verified }: { att: Attachment; verified: boolean }) {
  const t = useT();
  const { url, failed, load, allowed, unverified, open, requested } = useAttachmentUrl(att, verified, { autoLoad: false });
  if (verified && !requested) return <Button size="sm" onClick={open}>{t("file.open", { name: att.name })}</Button>;
  if (failed || !url) return <AttachmentState failed={failed} untrusted={!allowed} onLoad={failed || !allowed ? load : undefined} />;
  return (
    <div className="flex w-full max-w-[400px] flex-col gap-1.5 rounded-lg border border-line bg-side p-2.5">
      <div className="flex items-center gap-2">
        <span className="flex size-8 shrink-0 items-center justify-center rounded-md bg-accent-soft text-accent">
          <FileAudio className="size-4" />
        </span>
        <span className="min-w-0 flex-1">
          <span className="block truncate text-sm font-medium" title={att.name}>
            {att.name}
          </span>
          <span className="block text-xs text-subtle">{formatBytes(att.size)}</span>
        </span>
        <DownloadButton att={att} verified={verified} />
      </div>
      {unverified && <span className="text-xs text-warn">{t("file.loadedUnverified")}</span>}
      <audio controls preload="metadata" src={url} className="h-9 w-full" />
    </div>
  );
}

function FileCard({ att, verified }: { att: Attachment; verified: boolean }) {
  const t = useT();
  const { busy, run, failed } = useSave(att, verified);
  return (
    <div data-attachment="file" className="flex w-full max-w-[400px] items-center gap-3 rounded-lg border border-line bg-side px-3 py-2.5">
      <span className="flex size-9 shrink-0 items-center justify-center rounded-md bg-accent-soft text-accent">{KIND_ICON[fileKind(att.mime, att.name)]}</span>
      <span className="min-w-0 flex-1">
        <span className="block truncate text-sm font-medium" title={att.name}>
          {att.name}
        </span>
        <span role={failed ? "alert" : undefined} className="block text-xs text-subtle">{failed ? t("file.notVerified") : formatBytes(att.size)}</span>
      </span>
      <IconButton label={failed || !verified ? t("file.downloadUnverifiedNamed", { name: att.name }) : t("file.downloadNamed", { name: att.name })} onClick={() => void run(failed || !verified)} disabled={busy}>
        {busy ? <Loader2 className="size-4 animate-spin" /> : <Download className="size-4" />}
      </IconButton>
    </div>
  );
}

/** The files of one message: images and video inline, audio with a player, everything else as a card. */
export function AttachmentList({ attachments, verified = false }: { attachments: Attachment[]; verified?: boolean | null }) {
  if (verified === null) return <AttachmentState failed={false} />;
  return (
    <div className="mt-1 flex flex-col items-start gap-1.5">
      {attachments.map((a) =>
        isInlineImage(a.mime) ? (
          <ImageAttachment key={a.id} att={a} verified={verified} />
        ) : a.mime.startsWith("video/") ? (
          <VideoAttachment key={a.id} att={a} verified={verified} />
        ) : a.mime.startsWith("audio/") ? (
          <AudioAttachment key={a.id} att={a} verified={verified} />
        ) : (
          <FileCard key={a.id} att={a} verified={verified} />
        ),
      )}
    </div>
  );
}

export function Lightbox({ attachment, verified }: { attachment: Attachment; verified: boolean }) {
  const { url, load, failed, allowed, unverified } = useAttachmentUrl(attachment, verified);
  const name = attachment.name;
  const t = useT();
  const close = useUi((s) => s.closeDialog);
  const { busy, run, failed: saveFailed } = useSave(attachment, verified);
  return (
    <DialogPrimitive.Root open onOpenChange={(o) => !o && close()}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Overlay className="anim-fade fixed inset-0 z-50 bg-black/80" />
        <DialogPrimitive.Content
          aria-describedby={undefined}
          tabIndex={-1}
          // Focus the picture's frame, not a button, so Escape closes the lightbox at once instead of first dismissing a tooltip.
          onOpenAutoFocus={(e) => {
            e.preventDefault();
            (e.currentTarget as HTMLElement).focus();
          }}
          className="fixed inset-0 z-50 flex flex-col items-center justify-center gap-3 p-4 outline-none"
          onClick={(e) => e.target === e.currentTarget && close()}
        >
          <DialogPrimitive.Title className="sr-only">{name}</DialogPrimitive.Title>
          {!url && <AttachmentState failed={failed} untrusted={!allowed} onLoad={failed || !allowed ? load : undefined} />}
          {unverified && <span className="text-xs text-warn">{t("file.loadedUnverified")}</span>}
          <img src={url || undefined} alt={name} className="max-h-[calc(100dvh-8rem)] max-w-full rounded-lg object-contain shadow-pop" />
          <div className="flex items-center gap-2 rounded-lg border border-line-strong bg-raised px-3 py-1.5 text-sm">
            <span className="max-w-[50vw] truncate text-muted">{name}</span>
            {saveFailed && verified && <span role="alert" className="text-xs text-danger">{t("file.notVerified")}</span>}
            {/* Saving retries the check; unverified bytes need the separate, labeled action. */}
            <IconButton label={verified ? t("file.saveImage") : t("file.downloadUnverified")} size="sm" onClick={() => void run(!verified)} disabled={busy}>
              <Download className="size-4" />
            </IconButton>
            {saveFailed && verified && (
              <Tooltip label={t("file.unverifiedRisk")}>
                <Button size="sm" variant="ghost" className="underline" onClick={() => void run(true)} disabled={busy}>{t("file.downloadUnverified")}</Button>
              </Tooltip>
            )}
            <DialogPrimitive.Close asChild>
              <IconButton label={t("common.close")} size="sm">
                <X className="size-4" />
              </IconButton>
            </DialogPrimitive.Close>
          </div>
        </DialogPrimitive.Content>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}
