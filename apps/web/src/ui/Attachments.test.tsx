import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import type { Attachment } from "../proto/Attachment";
import { AttachmentList } from "./Attachments";
import { TooltipProvider } from "./kit";

const att = (extra: Partial<Attachment>): Attachment => ({ id: "f1", name: "a.bin", mime: "application/octet-stream", size: 2048, url: "/files/f1/a.bin", ...extra });
const html = (list: Attachment[]) =>
  renderToStaticMarkup(
    <TooltipProvider>
      <AttachmentList attachments={list} />
    </TooltipProvider>,
  );

describe("AttachmentList", () => {
  it("sizes images up front without fetching until mounted", () => {
    const out = html([att({ name: "cat.png", mime: "image/png", url: "/files/f1/cat.png", width: 800, height: 600 })]);
    expect(out).not.toContain('src="/files/');
    expect(out).toContain("width:400px;height:300px");
  });

  it("plays video and audio in place, with a download button", () => {
    const video = html([att({ name: "clip.mp4", mime: "video/mp4", url: "/files/f2/clip.mp4", width: 1280, height: 720 })]);
    expect(video).toContain("<video");
    expect(video).toContain('preload="metadata"');
    expect(video).not.toContain('src="/files/');
    expect(video).toContain("Download");
    const audio = html([att({ name: "song.mp3", mime: "audio/mpeg", url: "/files/f3/song.mp3" })]);
    expect(audio).toContain("<audio");
    expect(audio).toContain("song.mp3");
  });

  it("shows everything else as a file card, and SVG never inline", () => {
    const out = html([att({ name: "report.pdf", mime: "application/pdf" }), att({ id: "f9", name: "x.svg", mime: "image/svg+xml" })]);
    expect(out.match(/data-attachment="file"/g)).toHaveLength(2);
    expect(out).not.toContain("<img");
    expect(out).toContain("2 KB");
  });
});
