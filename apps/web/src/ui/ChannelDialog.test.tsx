// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useSettings } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { Dialogs } from "./Dialogs";
import { TooltipProvider } from "./kit";

const mock = vi.hoisted(() => ({ create: vi.fn(async (_: object) => ({ id: 9 })) }));
vi.mock("../state/controller", () => ({ controller: { messageServerKey: "server-a", createChannel: mock.create }, describeRequestError: String, UploadError: class extends Error {} }));

describe("creating a channel", () => {
  let container: HTMLDivElement, root: Root;
  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    mock.create.mockClear();
    useSettings.setState({ language: "en", channelEncryption: {} });
    useSession.setState({ kind: "vc", channels: {} });
    container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  });
  afterEach(async () => {
    await act(async () => root.unmount()); container.remove(); useUi.getState().closeDialog(); vi.unstubAllGlobals();
  });
  const open = async () => {
    useUi.getState().openDialog({ kind: "channelEdit", mode: "create", parent: null });
    await act(async () => root.render(<TooltipProvider><Dialogs /></TooltipProvider>));
  };
  const switchFor = (label: string) => [...document.querySelectorAll<HTMLButtonElement>('[role="switch"]')].find(s => s.id && document.querySelector(`label[for="${s.id}"]`)?.textContent?.startsWith(label));
  const password = () => document.querySelector('[role="dialog"] input[type="password"]');
  const create = async (name: string) => {
    const input = document.querySelector<HTMLInputElement>('[role="dialog"] input:not([type="password"])')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, name);
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    const button = [...document.querySelectorAll<HTMLButtonElement>('[role="dialog"] button')].find(b => b.textContent === "Create")!;
    await act(async () => button.click());
  };

  it("starts plaintext, drops the password when encrypted, and remembers the choice per server", async () => {
    await open();
    expect(switchFor("End-to-end encrypted")?.getAttribute("aria-checked")).toBe("false");
    expect(switchFor("New members see earlier messages")).toBeUndefined();
    expect(password()).not.toBeNull();
    await act(async () => switchFor("End-to-end encrypted")!.click());
    expect(password()).toBeNull();
    await act(async () => switchFor("New members see earlier messages")!.click());
    await create("secret");
    expect(mock.create).toHaveBeenCalledWith(expect.objectContaining({ name: "secret", password: null, encrypted: true, history: false }));
    expect(useSettings.getState().channelEncryption).toEqual({ "server-a": true });
    await open();
    expect(switchFor("End-to-end encrypted")?.getAttribute("aria-checked")).toBe("true");
  });
  it("sends no encryption options for a plaintext channel", async () => {
    await open();
    await create("open");
    expect(mock.create.mock.calls[0]![0]).not.toHaveProperty("encrypted");
    expect(useSettings.getState().channelEncryption).toEqual({ "server-a": false });
  });
  it("offers no encryption on TeamSpeak servers", async () => {
    useSession.setState({ kind: "teamspeak" });
    await open();
    expect(switchFor("End-to-end encrypted")).toBeUndefined();
    expect(password()).not.toBeNull();
  });
});
