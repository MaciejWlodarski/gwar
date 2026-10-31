/** Offline responsive fixture of the real share components, with generated media only. */
import { createRoot } from "react-dom/client";
import { initialState } from "../src/state/reducer";
import { initialScreen, useScreen } from "../src/state/screen";
import { useSettings } from "../src/state/settings";
import { useSession, useUi } from "../src/state/stores";
import { controller } from "../src/state/controller";
import { SharePresetDialog, SharingBar, Stage } from "../src/ui/Screen";
import { TooltipProvider } from "../src/ui/kit";
import "../src/index.css";
const query = new URLSearchParams(location.search);
const client = (id: number, nickname: string) => ({ id, uid: `fixture-${id}`, nickname, channel: 1, groups: [], platform: "web", muted: false, deafened: false, away: null, talking: false, voice: true, voice_binding: null });
useSettings.setState({ language: query.get("lang") === "en" ? "en" : "pl", theme: "dark", shareAudio: {} });
useUi.setState({ section: "servers" });
useSession.setState({ ...initialState, kind: "gwar", phase: "online", rtt: query.has("slow") ? 900 : 20,
  me: { session: 1, uid: "fixture-1" }, permissions: ["speak", "share_screen"],
  clients: { 1: client(1, "Maciej"), 2: client(2, "Ola") } as never,
  channels: { 1: { id: 1, parent: null, name: "Rozmowy", topic: "", position: 0, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false } },
  shares: { A: { share: "A", client: 2, channel: 1, preset: "smooth", surface: "tab" } },
});
const canvas = document.createElement("canvas"); canvas.width = 1280; canvas.height = 720;
const ctx = canvas.getContext("2d")!; let tick = 0;
const draw = () => {
  ctx.fillStyle = "#181818"; ctx.fillRect(0, 0, 1280, 720);
  ctx.fillStyle = "#ddd"; ctx.font = "48px sans-serif"; ctx.fillText("Gwar / generated screen source", 70, 100);
  ctx.fillStyle = "#579c76"; ctx.fillRect(70, 220, 80 + (++tick % 800), 60);
}; draw(); setInterval(draw, 100);
const stream = canvas.captureStream(10);
const sound = new AudioContext(); const destination = sound.createMediaStreamDestination(); const tone = sound.createOscillator(); tone.frequency.value = 997; tone.connect(destination); tone.start(); stream.addTrack(destination.stream.getAudioTracks()[0]!);
useScreen.setState({ ...initialScreen, transport: { phase: "connected", run: 1 }, stage: "A", slots: ["A", null, null, null], streams: [stream, null, null, null],
  own: { phase: query.has("choose") ? "choosing" : "live", attempt: 1, share: "M", stream, surface: "tab", spec: { mode: "smooth", height: 1080, fps: 30 }, audio: !query.has("silent"), viewers: query.has("zero") || query.has("linked") ? 0 : 2, guests: query.has("linked") ? 2 : 0, guestLink: query.has("linked"), preview: false },
  links: query.has("linked") ? { M: true } : {},
  watches: [{ share: "A", id: 1, slot: 0, status: "watching", audio: true }],
});
controller.screen.ownStats = async () => null; controller.screen.stats = async () => null;
createRoot(document.getElementById("root")!).render(<TooltipProvider><main className="flex h-screen flex-col bg-surface text-fg"><header className="bg-side px-4 py-3 text-sm font-semibold"><h1>Gwar / Rozmowy</h1></header><SharingBar /><div className="flex min-h-0 flex-1"><Stage /></div><SharePresetDialog /></main></TooltipProvider>);
