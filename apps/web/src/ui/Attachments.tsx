import * as DialogPrimitive from "@radix-ui/react-dialog";
import { Download, File as FileIcon, FileArchive, FileAudio, FileText, FileVideo, Loader2, X } from "lucide-react";
import { useState, type ReactNode } from "react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { fileKind, fitBox, formatBytes, isInlineImage, type FileKind } from "../lib/files";
import { isDesktop } from "../platform";
import { saveFile } from "../platform/download";
import { absoluteUrl } from "../net/address";
import type { Attachment } from "../proto/Attachment";
import { controller } from "../state/controller";
import { useSession, useUi } from "../state/stores";
import { IconButton } from "./kit";

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

function useAttachmentUrl(att: Pick<Attachment, "url">): string {
  const origin = useSession((s) => s.httpOrigin);
  return absoluteUrl(origin, att.url);
}

function useSave(att: Pick<Attachment, "url" | "name">) {
  const t = useT();
  const [busy, setBusy] = useState(false);
  const run = async () => {
    setBusy(true);
    try {
      const ok = await saveFile(controller.attachmentUrl(att.url), att.name);
      if (ok && isDesktop()) useUi.getState().toast("success", t("file.saved", { name: att.name }));
    } catch (e) {
      useUi.getState().toast("error", t("file.saveFailed", { reason: e instanceof Error ? e.message : String(e) }));
    }
    setBusy(false);
  };
  return { busy, run };
}

function DownloadButton({ att }: { att: Attachment }) {
  const t = useT();
  const { busy, run } = useSave(att);
  return (
    <IconButton label={t("file.download")} size="sm" onClick={() => void run()} disabled={busy}>
      {busy ? <Loader2 className="size-4 animate-spin" /> : <Download className="size-4" />}
    </IconButton>
  );
}

function ImageAttachment({ att }: { att: Attachment }) {
  const t = useT();
  const [failed, setFailed] = useState(false);
  const url = useAttachmentUrl(att);
  const box = att.width && att.height ? fitBox(att.width, att.height, MAX_W, MAX_H) : null;
  if (failed) return <FileCard att={att} />;
  return (
    <button
      type="button"
      aria-label={t("file.open", { name: att.name })}
      onClick={() => useUi.getState().openDialog({ kind: "lightbox", url, name: att.name })}
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
    </button>
  );
}

function VideoAttachment({ att }: { att: Attachment }) {
  const url = useAttachmentUrl(att);
  const box = att.width && att.height ? fitBox(att.width, att.height, MAX_W, MAX_H) : null;
  return (
    <div className="flex max-w-full flex-col overflow-hidden rounded-lg border border-line bg-side" style={{ width: box?.width ?? MAX_W }}>
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
        <DownloadButton att={att} />
      </div>
    </div>
  );
}

function AudioAttachment({ att }: { att: Attachment }) {
  const url = useAttachmentUrl(att);
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
        <DownloadButton att={att} />
      </div>
      <audio controls preload="metadata" src={url} className="h-9 w-full" />
    </div>
  );
}

function FileCard({ att }: { att: Attachment }) {
  const t = useT();
  const { busy, run } = useSave(att);
  return (
    <div data-attachment="file" className="flex w-full max-w-[400px] items-center gap-3 rounded-lg border border-line bg-side px-3 py-2.5">
      <span className="flex size-9 shrink-0 items-center justify-center rounded-md bg-accent-soft text-accent">{KIND_ICON[fileKind(att.mime, att.name)]}</span>
      <span className="min-w-0 flex-1">
        <span className="block truncate text-sm font-medium" title={att.name}>
          {att.name}
        </span>
        <span className="block text-xs text-subtle">{formatBytes(att.size)}</span>
      </span>
      <IconButton label={t("file.downloadNamed", { name: att.name })} onClick={() => void run()} disabled={busy}>
        {busy ? <Loader2 className="size-4 animate-spin" /> : <Download className="size-4" />}
      </IconButton>
    </div>
  );
}

/** The files of one message: images and video inline, audio with a player, everything else as a card. */
export function AttachmentList({ attachments }: { attachments: Attachment[] }) {
  return (
    <div className="mt-1 flex flex-col items-start gap-1.5">
      {attachments.map((a) =>
        isInlineImage(a.mime) ? (
          <ImageAttachment key={a.id} att={a} />
        ) : a.mime.startsWith("video/") ? (
          <VideoAttachment key={a.id} att={a} />
        ) : a.mime.startsWith("audio/") ? (
          <AudioAttachment key={a.id} att={a} />
        ) : (
          <FileCard key={a.id} att={a} />
        ),
      )}
    </div>
  );
}

export function Lightbox({ url, name }: { url: string; name: string }) {
  const t = useT();
  const close = useUi((s) => s.closeDialog);
  const { busy, run } = useSave({ url: url, name });
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
          <img src={url} alt={name} className="max-h-[calc(100dvh-8rem)] max-w-full rounded-lg object-contain shadow-pop" />
          <div className="flex items-center gap-2 rounded-lg border border-line-strong bg-raised px-3 py-1.5 text-sm">
            <span className="max-w-[50vw] truncate text-muted">{name}</span>
            <IconButton label={t("file.saveImage")} size="sm" onClick={() => void run()} disabled={busy}>
              <Download className="size-4" />
            </IconButton>
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
