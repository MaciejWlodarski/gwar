// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Attachment } from "../proto/Attachment";
import { verifiedAttachmentBlob } from "../net/attachment";
import { imageBudget } from "../net/image-budget";
import { useSession } from "../state/stores";
import { AttachmentList } from "./Attachments";
import { TooltipProvider } from "./kit";

vi.mock("../net/attachment", () => ({
  verifiedAttachmentBlob: vi.fn(async () => new Blob(["x"], { type: "image/png" })),
  boundedImageBlob: async (blob: Blob, beforeDecode?: (w: number, h: number) => Promise<void>) => { await beforeDecode?.(100, 100); return blob; },
}));
// Room for exactly two decoded 100×100 images, one fetch at a time.
vi.mock("../net/image-budget", async (actual) => {
  const m = await actual<typeof import("../net/image-budget")>();
  return { ...m, imageBudget: new m.ImageBudget({ fetches: 1, memory: 2 * m.imageMemory(100, 100, 1) }) };
});

const observed = new Map<Element, (entries: { isIntersecting: boolean }[]) => void>();
class FakeObserver {
  constructor(private readonly callback: (entries: { isIntersecting: boolean }[]) => void) {}
  observe(node: Element) { observed.set(node, this.callback); }
  disconnect() { for (const [node, cb] of observed) if (cb === this.callback) observed.delete(node); }
}

describe("automatic image loading", () => {
  let container: HTMLDivElement, root: Root, urls = 0;
  const images: Attachment[] = [0, 1, 2, 3].map((i) => ({ id: `f${i}`, name: `img${i}`, mime: "image/png", url: `/files/f${i}/img${i}`, size: 1, width: 100, height: 100, sha256: "h" }));
  const settle = () => act(async () => { for (let i = 0; i < 10; i++) await new Promise((r) => setTimeout(r, 0)); });
  const near = async (index: number, value: boolean) => {
    const node = [...observed.keys()][index]!;
    await act(async () => observed.get(node)!([{ isIntersecting: value }]));
    await settle();
  };
  const shown = () => [...container.querySelectorAll("img")].map((img) => img.getAttribute("alt"));
  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true); vi.stubGlobal("IntersectionObserver", FakeObserver);
    vi.spyOn(URL, "createObjectURL").mockImplementation(() => `blob:${++urls}`); vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    useSession.setState({ httpOrigin: "https://voice.example" });
    container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  });
  afterEach(async () => { await act(async () => root.unmount()); container.remove(); observed.clear(); vi.restoreAllMocks(); vi.clearAllMocks(); vi.unstubAllGlobals(); });

  it("loads only images near the view, within the memory budget, and releases those that leave", async () => {
    await act(async () => root.render(<TooltipProvider><AttachmentList attachments={images} verified /></TooltipProvider>));
    await settle();
    expect(verifiedAttachmentBlob).not.toHaveBeenCalled();
    // Off-screen images keep their size, so the chat does not shift.
    expect(container.querySelector<HTMLElement>('[style*="width: 100px"]')).not.toBeNull();

    for (const i of [0, 1, 2, 3]) await near(i, true);
    expect(shown()).toEqual(["img0", "img1"]);
    expect(imageBudget.usage.memory).toBe(imageBudget.limits.memory);

    await near(0, false);
    expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:1");
    expect(shown()).toEqual(["img1", "img2"]);

    await act(async () => root.render(<></>));
    await settle();
    expect(imageBudget.usage).toEqual({ fetching: 0, memory: 0 });
  });
});
