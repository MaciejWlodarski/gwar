/** Offline UI approval fixture: actual components and detector, no media/server connection. */
import { createRoot } from "react-dom/client";
import { controller } from "../src/state/controller";
import { initialState } from "../src/state/reducer";
import { initialScreen, useScreen } from "../src/state/screen";
import { useSettings } from "../src/state/settings";
import { useSession, useUi, useVoice } from "../src/state/stores";
import { useUplink } from "../src/state/uplink";
import { RtcUplinkObserver } from "../src/voice/uplink-congestion";
import { SharingBar } from "../src/ui/Screen";
import { VoicePanel, VoicePill } from "../src/ui/VoicePanel";
import { TooltipProvider } from "../src/ui/kit";
import "../src/index.css";

const query = new URLSearchParams(location.search);
const congested = query.get("state") === "congested";
const mode = query.get("view") ?? "screen";
const observer = new RtcUplinkObserver();
let status;
for (let sec = 0; sec <= 6; sec++) {
  status = observer.observe(sec * 1000, new Map([
    ["transport", { id: "transport", type: "transport", selectedCandidatePairId: "pair" }],
    ["pair", { id: "pair", type: "candidate-pair", responsesReceived: sec + 1, timestamp: sec * 1000,
      currentRoundTripTime: congested && sec > 0 ? .480 : .020, availableOutgoingBitrate: congested ? 900_000 : 8_000_000 }],
  ]) as unknown as RTCStatsReport);
}
useSettings.setState({ language: "pl", theme: "dark" });
useUi.setState({ section: mode === "pill" ? "home" : "servers" });
useVoice.setState({ state: "connected", encryption: null, micError: null });
useUplink.setState({ voice: mode === "screen" ? null : status, screen: mode === "screen" ? status : null });
useSession.setState({ ...initialState, kind: "gwar", phase: "online", rtt: congested ? 480 : 20,
  owner: { uid: "local-fixture" } as never,
  me: { session: 1, uid: "local-fixture" }, permissions: ["speak", "screen_share"],
  server: { name: "Gwar", welcome: "", version: "local", default_channel: 1, max_clients: 10, upload_limit: 0, file_access: "link", teamspeak: false, screen_guest_links: false, server_chat: { encrypted: false, history: false } },
  clients: { 1: { id: 1, uid: "local-fixture", nickname: "Maciej", channel: 1, groups: [], platform: "web", muted: false, deafened: false, away: null, talking: false, voice: true, voice_binding: null } },
  channels: { 1: { id: 1, parent: null, name: "Rozmowy", topic: "", position: 0, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false } },
});
useScreen.setState({ ...initialScreen, own: mode === "screen" ? { phase: "live", attempt: 1, share: "local", stream: null, surface: "screen", spec: { height: 1080, fps: 30, mode: "sharp" }, viewers: 2, guests: 0, guestLink: false, preview: false } : null });
controller.screen.ownStats = async () => ({ width: 1920, height: 1080, fps: 30, bitrate: congested ? 900_000 : 4_000_000, limitation: congested ? "bandwidth" : "none", codec: "H264", encoder: "VideoToolbox", hardware: true });
createRoot(document.getElementById("root")!).render(
  <TooltipProvider>
    <main className="min-h-screen bg-surface text-fg">
      <header className="bg-side px-4 py-3 text-sm font-semibold">Gwar <span className="font-normal text-muted">/ Rozmowy</span></header>
      {mode === "screen" ? <SharingBar /> : mode === "voice" ? <div className="max-w-xs py-3"><VoicePanel /></div> : <div className="relative h-56"><VoicePill /></div>}
    </main>
  </TooltipProvider>,
);
