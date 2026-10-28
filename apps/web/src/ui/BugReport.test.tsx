// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { connectApi } from "../connect/api";
import { initialState } from "../state/reducer";
import { useSettings } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { BugReportSection } from "./BugReport";
import { TooltipProvider } from "./kit";
import { clearReportDraft, useReportDraft } from "./report-draft";

vi.mock("../state/controller", () => ({
  controller: { voice: { capabilities: {}, listDevices: async () => ({ inputs: [], outputs: [] }) }, reportDiagnostics: async () => ({ app: { version: "test" }, logs: [] }) },
  describeVoiceError: String,
  describeRequestError: (e: unknown) => (e instanceof Error ? e.message : String(e)),
  identityFailure: () => null,
  bookmarkParams: () => ({}),
}));
vi.mock("../net/identity", async (original) => ({ ...(await original<typeof import("../net/identity")>()), loadConnectRecord: async () => undefined }));

let root: Root, container: HTMLDivElement;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useSettings.setState({ language: "en" });
  useUi.setState({ section: "bugReport", back: "servers", toasts: [], dialog: { kind: "none" } });
  useSession.setState({ ...initialState, phase: "idle" });
  clearReportDraft();
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount()); container.remove();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

/** The page as App shows it: only while its section is open. */
function Page() {
  const section = useUi((s) => s.section);
  return section === "bugReport" ? <BugReportSection /> : null;
}

const render = () => act(async () => root.render(<TooltipProvider><Page /></TooltipProvider>));
const textarea = () => container.querySelector("textarea");
const button = (label: string) => [...container.querySelectorAll("button")].find((b) => b.textContent === label)!;
const type = (text: string) =>
  act(async () => {
    Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!.call(textarea(), text);
    textarea()!.dispatchEvent(new Event("input", { bubbles: true }));
  });
const go = (section: "home" | "bugReport") => act(async () => useUi.getState().setSection(section));
const shot = { id: 99, blob: new Blob(["x"], { type: "image/png" }), url: "blob:shot" };

describe("the bug report draft", () => {
  it("stays while the page is left and comes back, screenshots and the diagnostics choice too", async () => {
    const revoke = vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    await render();
    await type("it broke");
    await act(async () => useReportDraft.setState({ shots: [shot], withDiagnostics: false }));
    // Another section (a tab, the system's back, a notification): the page unmounts.
    await go("home");
    expect(textarea()).toBeNull();
    await go("bugReport");
    expect(textarea()?.value).toBe("it broke");
    expect(container.querySelector("img")?.getAttribute("src")).toBe("blob:shot");
    expect(container.querySelector('[role="switch"]')?.getAttribute("aria-checked")).toBe("false");
    expect(revoke).not.toHaveBeenCalled();
  });

  it("is dropped on Cancel, its screenshots released", async () => {
    const revoke = vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    await render();
    await type("never mind");
    await act(async () => useReportDraft.setState({ shots: [shot] }));
    await act(async () => button("Cancel").click());
    expect(useUi.getState().section).toBe("servers");
    expect(revoke).toHaveBeenCalledWith("blob:shot");
    await go("bugReport");
    expect(textarea()?.value).toBe("");
    expect(container.querySelector("img")).toBeNull();
  });

  it("is dropped once sent, and the page left", async () => {
    const report = vi.spyOn(connectApi, "report").mockResolvedValue(7);
    await render();
    await type("it broke");
    await act(async () => button("Send report").click());
    expect(report).toHaveBeenCalledWith(expect.objectContaining({ text: "it broke" }), undefined);
    expect(useUi.getState().section).toBe("servers");
    expect(useUi.getState().toasts.map((x) => [x.kind, x.text])).toEqual([["success", "Thank you! Report #7 was sent."]]);
    expect(useReportDraft.getState().text).toBe("");
  });

  it("a send that fails after the page was left says so in a toast, and the draft waits on the page", async () => {
    let fail!: (e: Error) => void;
    vi.spyOn(connectApi, "report").mockReturnValue(new Promise((_, reject) => (fail = reject)));
    await render();
    await type("it broke");
    await act(async () => button("Send report").click());
    await go("home");
    await act(async () => fail(new Error("Connect is unreachable")));
    expect(useUi.getState().toasts.map((x) => [x.kind, x.text])).toEqual([["error", "The bug report was not sent: Connect is unreachable"]]);
    await go("bugReport");
    expect(textarea()?.value).toBe("it broke");
    expect(container.querySelector('[role="alert"]')?.textContent).toBe("Connect is unreachable");
    expect(button("Send report").disabled).toBe(false);
  });

  it("a send that fails on the page says so on the page, without a toast", async () => {
    vi.spyOn(connectApi, "report").mockRejectedValue(new Error("Connect is unreachable"));
    await render();
    await type("it broke");
    await act(async () => button("Send report").click());
    expect(container.querySelector('[role="alert"]')?.textContent).toBe("Connect is unreachable");
    expect(useUi.getState().toasts).toEqual([]);
  });
});
