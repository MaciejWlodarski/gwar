import { describe, expect, it } from "vitest";
import { checkUpload, fileKind, fitBox, formatBytes, isInlineImage } from "./files";

describe("files", () => {
  it("formats sizes", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(1023)).toBe("1023 B");
    expect(formatBytes(1536)).toBe("1.5 KB");
    expect(formatBytes(25 * 1024 * 1024)).toBe("25 MB");
  });
  it("knows which images show inline", () => {
    expect(isInlineImage("image/png")).toBe(true);
    expect(isInlineImage("image/svg+xml")).toBe(false);
    expect(isInlineImage("text/plain")).toBe(false);
  });
  it("classifies files for the icon", () => {
    expect(fileKind("application/zip", "a.zip")).toBe("archive");
    expect(fileKind("text/plain", "a.txt")).toBe("text");
    expect(fileKind("audio/mpeg", "a.mp3")).toBe("audio");
    expect(fileKind("video/mp4", "a.mp4")).toBe("video");
    expect(fileKind("application/octet-stream", "a.bin")).toBe("other");
  });
  it("checks the server's upload limit", () => {
    expect(checkUpload(10, 0)).toEqual({ ok: false, reason: "disabled" });
    expect(checkUpload(0, 100)).toEqual({ ok: false, reason: "empty" });
    expect(checkUpload(101, 100)).toEqual({ ok: false, reason: "too_large" });
    expect(checkUpload(100, 100)).toEqual({ ok: true });
  });
  it("fits images into 400x300 without scaling up", () => {
    expect(fitBox(800, 600)).toEqual({ width: 400, height: 300 });
    expect(fitBox(1000, 1000)).toEqual({ width: 300, height: 300 });
    expect(fitBox(100, 50)).toEqual({ width: 100, height: 50 });
    expect(fitBox(2000, 100)).toEqual({ width: 400, height: 20 });
  });
});
