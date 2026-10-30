// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { controller } from "../state/controller";
import { initialState } from "../state/reducer";
import { initialScreen, useScreen } from "../state/screen";
import { useSettings } from "../state/settings";
import { useSession, useUi, useVoice } from "../state/stores";
import { connectionQuality, EPISODE_MERGE_MS, QUALITY_IMPROVE_MS, resetCongestionEpisodes, useUplink } from "../state/uplink";
import { ConnectionDetails, NOTICE_MS } from "./ConnectionQuality";
import { SharingBar } from "./Screen";
import { Toaster } from "./Toaster";
import { VoicePanel, VoicePill } from "./VoicePanel";
import { TooltipProvider } from "./kit";

vi.mock("./useVerifiedHandle", () => ({ useVerifiedHandle: () => null }));

const healthy = { congested: false, rttMs: 20, baselineMs: 20, availableOutgoingBitrate: null };
const bloated = { congested: true, rttMs: 480, baselineMs: 20, availableOutgoingBitrate: 900_000, lossPct: 0.5 };
let root: Root, container: HTMLDivElement;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  resetCongestionEpisodes();
  useSettings.setState({ language: "pl" });
  useVoice.setState({ state: "connected", reconnecting: false, failure: null, encryption: null, micError: null });
  useUi.setState({ section: "home", toasts: [] });
  useSession.setState({ ...initialState, kind: "gwar", phase: "online", rtt: 20, me: { session: 1, uid: "me" },
    clients: { 1: { id: 1, uid: "me", nickname: "Maciej", channel: 1, groups: [], platform: "web", muted: false, deafened: false, away: null, talking: false, voice: true, voice_binding: null } },
    channels: { 1: { id: 1, parent: null, name: "Rozmowy", topic: "", position: 0, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false } }, permissions: ["speak"],
  });
  useScreen.setState({ ...initialScreen, own: { phase: "live", attempt: 1, share: "S", stream: null, surface: "screen", spec: { height: 1080, fps: 30, mode: "sharp" }, viewers: 2, guests: 0, guestLink: false, preview: false } });
  vi.spyOn(controller.screen, "ownStats").mockResolvedValue({ height: 720, width: 1280, fps: 30, bitrate: 1e6, limitation: "bandwidth", codec: "H264", encoder: null, hardware: null });
  container = document.createElement("div"); document.body.append(container); root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount()); container.remove();
  vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals();
});
const render = (node: React.ReactNode) => act(async () => root.render(<TooltipProvider>{node}</TooltipProvider>));
const set = (state: Partial<ReturnType<typeof useUplink.getState>>) => act(async () => useUplink.setState(state));
const indicator = () => container.querySelector<HTMLButtonElement>('[data-testid="connection-quality"]');
const notice = () => container.querySelector('[data-testid="congestion-notice"]');

describe("connectionQuality", () => {
  it("colours from what is measured now, never from the detector's floor", () => {
    expect(connectionQuality(null, null, null)).toBeNull();
    expect(connectionQuality(40, null, null)?.quality).toBe("good");
    // A far server alone is not flagged: a conversation stays natural up to ~150 ms one way.
    expect(connectionQuality(120, null, null)?.quality).toBe("good");
    expect(connectionQuality(150, null, null)?.quality).toBe("fair");
    expect(connectionQuality(250, null, null)?.quality).toBe("fair");
    expect(connectionQuality(300, null, null)?.quality).toBe("poor");
    // Media RTT is fresher than the server ping; the worse transport counts.
    expect(connectionQuality(20, healthy, { ...healthy, rttMs: 330 })).toMatchObject({ quality: "poor", rttMs: 330 });
    // A floor the detector adopted (no verdict) still shows a bloated RTT as poor.
    expect(connectionQuality(480, { ...bloated, congested: false, baselineMs: 470 }, null)?.quality).toBe("poor");
    // A detected queue is never good, even below the RTT thresholds.
    expect(connectionQuality(20, { ...bloated, rttMs: 95, baselineMs: 15 }, null)).toMatchObject({ quality: "fair", congestion: { congested: true } });
    expect(connectionQuality(20, { ...healthy, lossPct: 4 }, null)?.quality).toBe("fair");
    expect(connectionQuality(20, { ...healthy, lossPct: 12 }, null)).toMatchObject({ quality: "poor", lossPct: 12 });
  });
});

it("shows the panel indicator always, coloured by quality, and the voice status in the same colour", async () => {
  useScreen.setState({ ...initialScreen });
  await render(<VoicePanel />);
  expect(indicator()?.textContent).toBe("20 ms");
  expect(indicator()?.dataset.quality).toBe("good");
  expect(indicator()?.getAttribute("aria-label")).toBe("Dobre połączenie · 20 ms. Pokaż szczegóły");
  await set({ voice: bloated });
  expect(indicator()?.textContent).toBe("480 ms");
  expect(indicator()?.className).toContain("text-danger");
  expect(container.querySelector(".rounded-full.bg-current")?.className).toContain("text-danger");
  expect(indicator()?.getAttribute("aria-label")).toBe("Wysyłanie nie nadąża · 480 ms. Pokaż szczegóły");
});

it("adds the indicator to the pill only while the connection is not good", async () => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
  useScreen.setState({ ...initialScreen });
  useUi.setState({ section: "home" });
  await render(<VoicePill />);
  expect(indicator()).toBeNull();
  await set({ voice: bloated });
  expect(indicator()?.dataset.quality).toBe("poor");
  await set({ voice: healthy });
  // A better colour waits until it has held, so a threshold does not make the pill flicker.
  expect(indicator()?.dataset.quality).toBe("poor");
  await set({ voice: bloated });
  await set({ voice: healthy });
  await act(async () => vi.advanceTimersByTime(QUALITY_IMPROVE_MS - 1));
  expect(indicator()?.dataset.quality).toBe("poor");
  await act(async () => vi.advanceTimersByTime(1));
  expect(indicator()).toBeNull();
});

it("steps down to the worst recent quality within 4 s, however often the measurement changes", async () => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
  useScreen.setState({ ...initialScreen });
  useUi.setState({ section: "home" });
  await render(<><VoicePill /><VoicePanel /></>);
  await set({ voice: { ...healthy, rttMs: 400 } });
  const quality = () => [...container.querySelectorAll<HTMLElement>('[data-testid="connection-quality"]')].map(e => e.dataset.quality);
  expect(quality()).toEqual(["poor", "poor"]);
  // A round trip at the good/fair edge: every change must not restart the hold.
  for (let i = 0; i < 4; i++) {
    await set({ voice: { ...healthy, rttMs: i % 2 ? 160 : 140 } });
    await act(async () => vi.advanceTimersByTime(1000));
  }
  // One colour for every view: the pill and the panel agree.
  expect(quality()).toEqual(["fair", "fair"]);
});

it("turns good 4 s after the connection settles, even through a moment of fair", async () => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
  useScreen.setState({ ...initialScreen });
  await render(<VoicePanel />);
  await set({ voice: { ...healthy, rttMs: 400 } });
  await set({ voice: { ...healthy, rttMs: 200 } });
  await act(async () => vi.advanceTimersByTime(100));
  await set({ voice: healthy });
  await act(async () => vi.advanceTimersByTime(QUALITY_IMPROVE_MS - 101));
  expect(indicator()?.dataset.quality).toBe("poor");
  await act(async () => vi.advanceTimersByTime(1));
  // Better than poor for 4 s, and better than fair only for 3.9 s.
  expect(indicator()?.dataset.quality).toBe("fair");
  await act(async () => vi.advanceTimersByTime(100));
  expect(indicator()?.dataset.quality).toBe("good");
  // Worse while an improvement is pending shows at once and cancels it.
  await set({ voice: { ...healthy, rttMs: 400 } });
  await set({ voice: healthy });
  await act(async () => vi.advanceTimersByTime(2000));
  await set({ voice: { ...healthy, rttMs: 400 } });
  await act(async () => vi.advanceTimersByTime(QUALITY_IMPROVE_MS));
  expect(indicator()?.dataset.quality).toBe("poor");
});

it("drops the held colour with the measurement, and shows the next one as it is", async () => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
  useScreen.setState({ ...initialScreen });
  await render(<VoicePanel />);
  await set({ voice: bloated });
  expect(indicator()?.dataset.quality).toBe("poor");
  await act(async () => { useSession.setState({ rtt: null }); useUplink.setState({ voice: null }); });
  expect(indicator()).toBeNull();
  await act(async () => useSession.setState({ rtt: 20 }));
  expect(indicator()?.dataset.quality).toBe("good");
});

it("replaces the generic sender reason in the sharing bar while congested, then restores it", async () => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
  await render(<SharingBar />);
  expect(container.textContent).toContain("Ograniczone przez łącze");
  expect(indicator()).toBeNull();
  await set({ screen: bloated });
  expect(indicator()?.textContent).toBe("Wysyłanie nie nadąża");
  expect(indicator()?.getAttribute("aria-live")).toBe("off");
  expect(container.textContent).not.toContain("Ograniczone przez łącze");
  await set({ screen: healthy });
  await act(async () => vi.advanceTimersByTime(QUALITY_IMPROVE_MS));
  expect(container.textContent).toContain("Ograniczone przez łącze");
  expect(indicator()).toBeNull();
});

it("shows voice congestion in the sharing bar even before encoder stats arrive", async () => {
  vi.mocked(controller.screen.ownStats).mockResolvedValue(null);
  useUplink.setState({ voice: bloated });
  await render(<SharingBar />);
  expect(indicator()?.textContent).toBe("Wysyłanie nie nadąża");
});

it.each([
  { state: "failed" as const },
  { state: "starting" as const },
  { state: "idle" as const },
  { reconnecting: true },
  { failure: "Permanent refusal" },
])("drops voice feedback and its episode immediately on %j, ignoring late stats", async transition => {
  useScreen.setState({ ...initialScreen });
  await render(<><VoicePanel /><VoicePill /><Toaster /></>);
  await set({ voice: bloated });
  const episode = useUplink.getState().notice!.episode;
  expect(indicator()).not.toBeNull();
  await act(async () => useVoice.setState(transition));
  expect(useUplink.getState().voice).toBeNull();
  expect(notice()).toBeNull();
  expect(indicator()).toBeNull();
  await set({ voice: bloated }); // A late callback from the retired call.
  expect(useUplink.getState().voice).toBeNull();
  await act(async () => useVoice.setState({ state: "connected", reconnecting: false, failure: null }));
  expect(notice()).toBeNull();
  await set({ voice: healthy });
  expect(notice()).toBeNull();
  await set({ voice: bloated });
  expect(useUplink.getState().notice!.episode).toBeGreaterThan(episode);
});

it("keeps screen congestion on the share while the server reconnects, without voice claims", async () => {
  await render(<><VoicePanel /><SharingBar /><Toaster /><ConnectionDetails /></>);
  await set({ voice: bloated, screen: bloated });
  await act(async () => useSession.setState({ phase: "reconnecting" }));
  expect(useUplink.getState().voice).toBeNull();
  expect(notice()).toBeNull();
  expect(container.querySelectorAll('[data-testid="connection-quality"]')).toHaveLength(1);
  expect(indicator()?.textContent).toBe("Wysyłanie nie nadąża");
  await act(async () => indicator()!.click());
  const details = document.querySelector('[data-testid="connection-details"]')!;
  expect(details.textContent).toContain("wysyłaniem udostępnianego ekranu");
  expect(details.textContent).not.toContain("słyszą");
  expect(details.textContent).not.toContain("głos");
  await act(async () => useSession.setState({ phase: "online" }));
  expect(notice()).toBeNull(); // The previous share verdict cannot re-pop a voice notice.
});

it("shows one quiet retry during recovery and reserves the red explanation row for permanent refusals", async () => {
  useScreen.setState({ ...initialScreen });
  await act(async () => {
    useVoice.setState({ state: "failed", reconnecting: true, failure: null });
    useUi.getState().toast("error", "Utracono połączenie głosowe. Spróbujemy ponownie.", { key: "voice-closed", sticky: true });
  });
  await render(<><VoicePanel /><Toaster /></>);
  const status = () => container.querySelector('[data-testid="voice-status"]')!;
  const retries = () => [...container.querySelectorAll<HTMLButtonElement>("button")].filter(b => b.textContent === "Ponów");
  expect(status().textContent).toContain("Łączenie ponownie…");
  expect(status().querySelector(".text-warn")).not.toBeNull();
  expect(status().querySelector(".text-danger")).toBeNull();
  expect(container.textContent).not.toContain("Dlaczego?");
  expect(retries()).toHaveLength(1);
  expect(retries()[0]!.className).toContain("text-muted");
  const toast = container.querySelector('[data-testid="toast"]')!;
  expect(toast.querySelector("details")).toBeNull();
  expect(toast.textContent).not.toContain("Ponów");
  const retry = vi.spyOn(controller, "retryVoice").mockImplementation(() => {});
  await act(async () => retries()[0]!.click());
  expect(retry).toHaveBeenCalledOnce();
  await act(async () => {
    useVoice.setState({ state: "failed", reconnecting: false, failure: "Permanent refusal", encryption: "failed" });
    useUi.getState().dismissToast("voice-closed");
  });
  expect(status().textContent).toContain("Głos niedostępny");
  expect(status().querySelector(".text-danger")).not.toBeNull();
  expect(container.textContent).toContain("Dlaczego?");
  expect(retries()).toHaveLength(1);
});

it("opens plain-language details from the indicator, with the numbers and router advice folded away", async () => {
  useScreen.setState({ ...initialScreen });
  useUplink.setState({ voice: bloated });
  await render(<><VoicePanel /><ConnectionDetails /></>);
  await act(async () => indicator()!.click());
  const details = document.querySelector('[data-testid="connection-details"]')!;
  expect(details.textContent).toContain("Twój internet nie nadąża z wysyłaniem, więc inni słyszą Cię z opóźnieniem ok. 0,5 s.");
  expect(details.textContent).toContain("Zatrzymaj wysyłanie plików, kopię zapasową");
  const technical = details.querySelector("details")!;
  expect(technical.open).toBe(false);
  expect(technical.querySelector("summary")?.textContent).toBe("Szczegóły techniczne");
  expect(technical.textContent).toContain("480 ms");
  expect(technical.textContent).toContain("Zwykle20 ms");
  expect(technical.textContent).toContain("0,5 %");
  expect(technical.textContent).toContain("0,9 Mb/s");
  expect(technical.textContent).toContain("SQM lub QoS");
  // Without a queue there is no upload explanation, and no router advice.
  await set({ voice: { ...healthy, rttMs: 260, baselineMs: 30 } });
  expect(details.textContent).toContain("Połączenie z serwerem jest wolne lub niestabilne");
  expect(details.textContent).not.toContain("SQM");
});

describe("the congestion notice", () => {
  beforeEach(() => { vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] }); });

  it("appears once per episode in the toasts' live region, then collapses to the indicator", async () => {
    await render(<Toaster />);
    expect(container.firstElementChild?.getAttribute("aria-live")).toBe("polite");
    await set({ voice: bloated });
    expect(notice()?.getAttribute("role")).toBe("status");
    expect(notice()?.textContent).toContain("Twój internet nie nadąża z wysyłaniem — inni słyszą Cię z opóźnieniem ok. 0,5 s.");
    // Later RTT updates neither change nor repeat it.
    await set({ voice: { ...bloated, rttMs: 900 } });
    expect(notice()?.textContent).toContain("ok. 0,5 s");
    await act(async () => vi.advanceTimersByTime(NOTICE_MS));
    expect(notice()).toBeNull();
    // A verdict that flickers off and on is the same episode.
    await set({ voice: healthy });
    await act(async () => vi.advanceTimersByTime(EPISODE_MERGE_MS - 1000));
    await set({ voice: bloated });
    expect(notice()).toBeNull();
    await set({ voice: healthy });
    await act(async () => vi.advanceTimersByTime(EPISODE_MERGE_MS));
    await set({ voice: { ...bloated, rttMs: 1020 } });
    expect(notice()?.textContent).toContain("ok. 1,0 s");
  });

  it("can be dismissed or held open, and leaves when the queue drains", async () => {
    await render(<><Toaster /><ConnectionDetails /></>);
    await set({ voice: bloated });
    await act(async () => notice()!.dispatchEvent(new Event("focusin", { bubbles: true })));
    await act(async () => vi.advanceTimersByTime(NOTICE_MS * 2));
    expect(notice()).not.toBeNull();
    await act(async () => notice()!.querySelector<HTMLButtonElement>('button[aria-label="Zamknij"]')!.click());
    expect(notice()).toBeNull();
    await set({ voice: null });
    await act(async () => vi.advanceTimersByTime(EPISODE_MERGE_MS));
    await set({ voice: bloated });
    await act(async () => [...notice()!.querySelectorAll("button")].find(b => b.textContent === "Szczegóły")!.click());
    expect(notice()).toBeNull();
    expect(useUplink.getState().details).toBe(true);
    await set({ details: false, voice: null });
    await act(async () => vi.advanceTimersByTime(EPISODE_MERGE_MS));
    await set({ voice: bloated });
    expect(notice()).not.toBeNull();
    await set({ voice: healthy });
    expect(notice()).toBeNull();
  });

  it("still collapses in the next episode after one was closed while hovered", async () => {
    await render(<Toaster />);
    await set({ voice: bloated });
    // A touch or hover, then a close: the removed notice never gets its pointerleave.
    await act(async () => notice()!.dispatchEvent(new Event("pointerover", { bubbles: true })));
    await act(async () => notice()!.dispatchEvent(new Event("focusin", { bubbles: true })));
    await act(async () => notice()!.querySelector<HTMLButtonElement>('button[aria-label="Zamknij"]')!.click());
    expect(notice()).toBeNull();
    await set({ voice: null });
    await act(async () => vi.advanceTimersByTime(EPISODE_MERGE_MS));
    await set({ voice: bloated });
    expect(notice()).not.toBeNull();
    await act(async () => vi.advanceTimersByTime(NOTICE_MS));
    expect(notice()).toBeNull();
  });

  it("goes to the top on phones, clear of the call controls, in its own live region", async () => {
    vi.stubGlobal("matchMedia", (query: string) => ({ matches: true, media: query, addEventListener() {}, removeEventListener() {} }));
    await render(<Toaster />);
    await set({ voice: bloated });
    const region = notice()!.parentElement!;
    expect(region.getAttribute("aria-live")).toBe("polite");
    expect(region.className).toContain("top-3");
    expect(container.querySelectorAll('[data-testid="congestion-notice"]')).toHaveLength(1);
  });
});
