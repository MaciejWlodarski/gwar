// @vitest-environment happy-dom
import { Blob } from "node:buffer";
import { webcrypto } from "node:crypto";
import { URL } from "node:url";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Attachment } from "../proto/Attachment";
import { sha256 } from "../net/message-signature";
import { fetchFileBytes, saveBlob } from "../platform/download";
import { useSession } from "../state/stores";
import { AttachmentList, Lightbox } from "./Attachments";
import { TooltipProvider } from "./kit";
vi.mock("../platform/download", () => ({ MAX_FILE_BYTES: 30 * 1024 * 1024, fetchFileBytes: vi.fn(), saveBlob: vi.fn(async () => true) }));

describe("attachment trust", () => {
  let container: HTMLDivElement, root: Root;
  const bytes = new TextEncoder().encode("hello");
  const media = async (mime: string, hash?: string): Promise<Attachment> => ({ id: "f1", name: "media", mime, url: "/files/f1/media", size: bytes.length, sha256: hash ?? await sha256(bytes) });
  const show = async (att: Attachment, verified: boolean | null) => {
    await act(async () => { root.render(<TooltipProvider><AttachmentList attachments={[att]} verified={verified} /></TooltipProvider>); });
  };
  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true); vi.stubGlobal("crypto", webcrypto); vi.stubGlobal("Blob", Blob); vi.stubGlobal("URL", URL);
    vi.mocked(fetchFileBytes).mockResolvedValue(bytes); vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:checked"); vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    useSession.setState({ httpOrigin: "https://voice.example" });
    container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  });
  afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.restoreAllMocks(); vi.clearAllMocks(); vi.unstubAllGlobals(); });
  for (const [mime, tag] of [["video/mp4", "video"], ["audio/ogg", "audio"]] as const) {
    it(`holds unsigned ${mime} until explicit loading`, async () => {
      await show(await media(mime, ""), false);
      expect(container.querySelector(tag)).toBeNull(); expect(fetchFileBytes).not.toHaveBeenCalled();
      const load = [...container.querySelectorAll("button")].find((b) => b.textContent === "Load unverified attachment")!;
      await act(async () => load.click());
      await vi.waitFor(() => expect(container.querySelector(tag)?.getAttribute("src")).toBe("blob:checked"));
      expect(fetchFileBytes).toHaveBeenCalledWith("/files/f1/media", expect.objectContaining({ origin: "https://voice.example", size: 5 }));
    });
    it(`hashes signed ${mime} before creating an object URL`, async () => {
      await show(await media(mime), true);
      expect(fetchFileBytes).not.toHaveBeenCalled();
      await act(async () => container.querySelector<HTMLButtonElement>('button')!.click());
      await vi.waitFor(() => expect(container.querySelector(tag)?.getAttribute("src")).toBe("blob:checked"));
      expect(await (vi.mocked(URL.createObjectURL).mock.calls[0]![0] as Blob).text()).toBe("hello");
    });
    it(`does not display substituted ${mime} until explicitly requested`, async () => {
      await show(await media(mime, "wrong"), true);
      expect(fetchFileBytes).not.toHaveBeenCalled();
      await act(async () => container.querySelector<HTMLButtonElement>('button')!.click());
      // Hashing takes a while on slow machines: wait for the outcome, then check nothing was shown on the way.
      await vi.waitFor(() => expect(container.textContent).toContain("Attachment verification failed"));
      expect(container.querySelector(tag)).toBeNull(); expect(URL.createObjectURL).not.toHaveBeenCalled();
      expect(container.textContent).toContain("Load unverified attachment");
    });
    it(`does not fetch pending ${mime}`, async () => { await show(await media(mime), null); expect(fetchFileBytes).not.toHaveBeenCalled(); expect(container.querySelector(tag)).toBeNull(); });
  }
  it("a lightbox retry verifies again; substituted bytes are saved only through the labeled action", async () => {
    const att = await media("image/png");
    const substituted = new TextEncoder().encode("evil!");
    vi.mocked(fetchFileBytes).mockResolvedValueOnce(bytes).mockResolvedValue(substituted);
    vi.stubGlobal("createImageBitmap", undefined);
    await act(async () => { root.render(<TooltipProvider><Lightbox attachment={att} verified /></TooltipProvider>); });
    const save = () => document.querySelector<HTMLButtonElement>('button[aria-label="Save image"]')!;
    const unverified = () => [...document.querySelectorAll("button")].find((b) => b.textContent === "Download unverified attachment");
    // The preview consumes the matching bytes; every save must fetch and check the substitution again.
    await vi.waitFor(() => expect(fetchFileBytes).toHaveBeenCalledOnce());
    for (let attempt = 0; attempt < 2; attempt++) {
      await act(async () => save().click());
      // A timer tick can finish before hashing does, leaving the retry or unverified action disabled.
      await vi.waitFor(() => {
        expect(save().disabled).toBe(false);
        expect(unverified()?.disabled).toBe(false);
      });
      expect(fetchFileBytes).toHaveBeenCalledTimes(attempt + 2);
      expect(saveBlob).not.toHaveBeenCalled();
      expect(save().parentElement?.querySelector('[role="alert"]')?.textContent).toBe("Attachment verification failed");
    }
    await act(async () => unverified()!.click());
    await vi.waitFor(() => expect(saveBlob).toHaveBeenCalledOnce());
    expect(await (vi.mocked(saveBlob).mock.calls[0]![0] as unknown as Blob).text()).toBe("evil!");
  });
  it("does not fetch unsigned images or create an image element", async () => { await show(await media("image/png", ""), false); expect(container.querySelector("img")).toBeNull(); expect(fetchFileBytes).not.toHaveBeenCalled(); });
  it("downloads only matching signed bytes, then permits explicitly labeled unverified saving after failure", async () => {
    await show(await media("text/plain", "wrong"), true);
    expect(fetchFileBytes).not.toHaveBeenCalled();
    await act(async () => container.querySelector<HTMLButtonElement>('button[aria-label="Download media"]')!.click());
    await vi.waitFor(() => expect(container.querySelector('button[aria-label="Download unverified attachment: media"]')).not.toBeNull());
    expect(saveBlob).not.toHaveBeenCalled();
    await act(async () => container.querySelector<HTMLButtonElement>('button[aria-label="Download unverified attachment: media"]')!.click());
    await vi.waitFor(() => expect(saveBlob).toHaveBeenCalledOnce());
    expect(await vi.mocked(saveBlob).mock.calls[0]![0].text()).toBe("hello");
  });
  it("saves unsigned files only after a labeled user click", async () => {
    await show(await media("application/octet-stream", ""), false); expect(fetchFileBytes).not.toHaveBeenCalled();
    await act(async () => container.querySelector<HTMLButtonElement>('button[aria-label="Download unverified attachment: media"]')!.click());
    await vi.waitFor(() => expect(saveBlob).toHaveBeenCalledOnce());
  });
});
