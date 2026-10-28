// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { controller } from "../state/controller";
import { initialState } from "../state/reducer";
import { initialScreen, useScreen } from "../state/screen";
import { useSettings } from "../state/settings";
import { useSession, useUi, useVoice } from "../state/stores";
import { useUplink } from "../state/uplink";
import { SharingBar } from "./Screen";
import { VoicePanel, VoicePill } from "./VoicePanel";
import { TooltipProvider } from "./kit";

vi.mock("./useVerifiedHandle", () => ({ useVerifiedHandle: () => null }));

const bloated = { congested: true, rttMs: 480, baselineMs: 20, availableOutgoingBitrate: null };
let root: Root, container: HTMLDivElement;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useSettings.setState({ language: "pl" });
  useUplink.setState({ voice: null, screen: null });
  useVoice.setState({ state: "connected", encryption: null, micError: null });
  useUi.setState({ section: "home" });
  useSession.setState({ ...initialState, kind: "gwar", phase: "online", rtt: 20, me: { session: 1, uid: "me" },
    clients: { 1: { id: 1, uid: "me", nickname: "Maciej", channel: 1, groups: [], platform: "web", muted: false, deafened: false, away: null, talking: false, voice: true, voice_binding: null } },
    channels: { 1: { id: 1, parent: null, name: "Rozmowy", topic: "", position: 0, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false } }, permissions: ["speak"],
  });
  useScreen.setState({ ...initialScreen, own: { phase: "live", attempt: 1, share: "S", stream: null, surface: "screen", spec: { height: 1080, fps: 30, mode: "sharp" }, viewers: 2, guests: 0, guestLink: false, preview: false } });
  vi.spyOn(controller.screen, "ownStats").mockResolvedValue({ height: 720, width: 1280, fps: 30, bitrate: 1e6, limitation: "bandwidth", codec: "H264", encoder: null, hardware: null });
  container = document.createElement("div"); document.body.append(container); root = createRoot(container);
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });
const render = (node: React.ReactNode) => act(async () => root.render(<TooltipProvider>{node}</TooltipProvider>));

it("replaces the generic sender reason with congestion, then restores it", async () => {
  await render(<SharingBar />);
  expect(container.textContent).toContain("Ograniczone przez łącze");
  await act(async () => useUplink.setState({ screen: bloated }));
  expect(container.textContent).toContain("Łącze wysyłania zapchane · 480 ms");
  expect(container.textContent).not.toContain("Ograniczone przez łącze");
  const details = container.querySelector<HTMLDetailsElement>("details")!;
  await act(async () => container.querySelector<HTMLElement>("summary")!.click());
  expect(details.open).toBe(true);
  expect(details.textContent).toContain("SQM/QoS");
  await act(async () => details.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })));
  expect(details.open).toBe(false);
  await act(async () => useUplink.setState({ screen: null }));
  expect(container.textContent).toContain("Ograniczone przez łącze");
});

it("shows the same warning for voice-only users in the panel and pill", async () => {
  useScreen.setState({ ...initialScreen });
  await render(<VoicePanel />);
  expect(container.textContent).toContain("20 ms");
  await act(async () => useUplink.setState({ voice: bloated }));
  expect(container.querySelector('[data-testid="upload-warning"]')?.className).toContain("text-warn");
  expect(container.textContent).toContain("Łącze wysyłania zapchane · 480 ms");
  expect(container.textContent).not.toContain("20 ms");
  await render(<VoicePill />);
  expect(container.textContent).toContain("Łącze wysyłania zapchane · 480 ms");
  await act(async () => useUplink.setState({ voice: null }));
  expect(container.querySelector('[data-testid="upload-warning"]')).toBeNull();
});

it("shows voice congestion in the sharing bar even before encoder stats arrive", async () => {
  vi.mocked(controller.screen.ownStats).mockResolvedValue(null);
  useUplink.setState({ voice: bloated });
  await render(<SharingBar />);
  expect(container.textContent).toContain("Łącze wysyłania zapchane · 480 ms");
});
