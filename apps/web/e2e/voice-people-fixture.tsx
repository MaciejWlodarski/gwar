/** Offline approval fixture: production components, generated microphone input, no server connection. */
import { createRoot } from "react-dom/client";
import { controller } from "../src/state/controller";
import { initialState } from "../src/state/reducer";
import { defaultAudio, useSettings } from "../src/state/settings";
import { useSession, useUi, useVoice } from "../src/state/stores";
import { SettingsSection } from "../src/ui/Settings";
import { ChannelTree } from "../src/ui/ChannelTree";
import { ChatArea } from "../src/ui/Chat";
import { VoicePanel } from "../src/ui/VoicePanel";
import { MemberProfileDialog } from "../src/ui/MemberProfile";
import { TooltipProvider } from "../src/ui/kit";
import "../src/index.css";

const query = new URLSearchParams(location.search);
const theme = query.get("theme") === "light" ? "light" : "dark";
const view = query.get("view") ?? "settings";
const uid = "local-maciej", person = "stable-anna";
document.documentElement.dataset.theme = theme;
useSettings.setState({ language: "pl", theme, resumeConnection: null, userVolumes: { [person]: 0.45 }, userMuted: {}, audio: { ...defaultAudio, noiseSuppression: false } });
useUi.setState({ section: view === "settings" ? "settings" : "servers", settingsTab: "audio", pane: "list", overview: false, dialog: view === "person" ? { kind: "memberProfile", uid: person, fallback: "Anna" } : { kind: "none" } });
useSession.setState({ ...initialState, kind: "gwar", phase: "online", rtt: 20,
  owner: { uid } as never, me: { session: 1, uid }, permissions: ["connect", "speak", "screen_share"],
  server: { name: "Gwar", welcome: "", version: "local", default_channel: 1, max_clients: 10, upload_limit: 0, file_access: "link", teamspeak: false, screen_guest_links: false, server_chat: { encrypted: false, history: false } },
  channels: Object.fromEntries(["Rozmowy", "Wieczorne granie", "Muzyka"].map((name, index) => [index + 1, { id: index + 1, parent: null, name, topic: "Porozmawiajmy", position: index, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false }])),
  viewChannel: 2,
  clients: {
    1: { id: 1, uid, nickname: "Maciej", channel: view === "settings" ? null : 1, groups: [], platform: "web", muted: false, deafened: false, away: null, talking: false, voice: true, voice_binding: null },
    2: { id: 2, uid: person, nickname: "Anna", channel: 1, groups: [], platform: "web", muted: false, deafened: false, away: null, talking: false, voice: true, voice_binding: null },
    3: { id: 3, uid: "stable-piotr", nickname: "Piotr", channel: 2, groups: [], platform: "web", muted: false, deafened: false, away: null, talking: false, voice: true, voice_binding: null },
  },
});
useVoice.setState({ state: view === "settings" ? "idle" : "connected", muted: false, deafened: false, reconnecting: false, failure: null, encryption: null, micError: null });
// The real engine records/decodes/replays a generated tone. The OS mic is never opened.
Object.defineProperty(navigator, "permissions", { configurable: true, value: { query: async () => ({ state: "granted" }) } });
const tracks: MediaStreamTrack[] = [];
const replays: Array<{ liveInputs: number; rms: number }> = [];
const sources: WeakRef<AudioBufferSourceNode>[] = [], recorders: WeakRef<MediaRecorder>[] = [];
let currentGain: GainNode | null = null;
const Recorder = MediaRecorder;
window.MediaRecorder = class extends Recorder {
  constructor(stream: MediaStream, options?: MediaRecorderOptions) { super(stream, options); recorders.push(new WeakRef(this)); }
};
Object.assign(window, {
  fixtureTracks: tracks, fixtureReplays: replays,
  fixtureSetGain: (value: number) => { if (currentGain) currentGain.gain.value = value; },
  fixtureSampleReleased: () => sources.every(ref => { const node = ref.deref(); return !node || (node.buffer === null && node.onended === null); }) &&
    recorders.every(ref => { const recorder = ref.deref(); return !recorder || (!recorder.ondataavailable && !recorder.onstop && !recorder.onerror); }),
});
const createBufferSource = AudioContext.prototype.createBufferSource;
AudioContext.prototype.createBufferSource = function () {
  const source = createBufferSource.call(this), start = source.start.bind(source);
  sources.push(new WeakRef(source));
  source.start = (...args) => {
    const pcm = source.buffer?.getChannelData(0) ?? [];
    const rms = Math.sqrt(pcm.reduce((sum, sample) => sum + sample * sample, 0) / Math.max(1, pcm.length));
    replays.push({ liveInputs: tracks.filter(track => track.readyState === "live").length, rms }); start(...args);
  };
  return source;
};
navigator.mediaDevices.getUserMedia = async () => {
  const ctx = new AudioContext();
  const tone = ctx.createOscillator(), gain = ctx.createGain(), destination = ctx.createMediaStreamDestination();
  gain.gain.value = 0.03; currentGain = gain;
  tone.connect(gain).connect(destination); tone.start(); await ctx.resume();
  const track = destination.stream.getAudioTracks()[0]!, stop = track.stop.bind(track);
  tracks.push(track);
  track.stop = () => { if (track.readyState === "ended") return; stop(); tone.stop(); if (currentGain === gain) currentGain = null; void ctx.close(); };
  return destination.stream;
};
controller.voice.listDevices = async () => ({ inputs: [{ id: "fixture-input", label: "Mikrofon zestawu słuchawkowego" }], outputs: [{ id: "fixture-output", label: "Słuchawki" }] });
controller.init();
// Retain the actual local meter in Settings; no microphone or voice transport for other views.
function Surface() {
  const dialog = useUi(s => s.dialog);
  return <TooltipProvider>
    <div className="flex h-dvh flex-col bg-rail text-fg">
      <header className="flex h-12 shrink-0 items-center border-b border-line bg-side px-4 text-sm font-semibold">Gwar</header>
      <div className="relative flex min-h-0 flex-1 gap-2 p-2">
        {view === "settings" ? <SettingsSection card /> : <>
          <aside className={`${view === "chat" ? "hidden md:flex" : "flex"} w-full shrink-0 flex-col rounded-2xl border border-line bg-side md:w-72`}>
            <h1 className="px-4 py-3 text-base font-semibold">Gwar</h1>
            <div className="min-h-0 flex-1 overflow-y-auto"><ChannelTree /></div>
            <VoicePanel />
          </aside>
          <div className={`${view === "chat" ? "flex" : "hidden md:flex"} min-w-0 flex-1`}><ChatArea card /></div>
        </>}
      </div>
    </div>
    {dialog.kind === "memberProfile" && <MemberProfileDialog uid={dialog.uid} fallback={dialog.fallback} />}
  </TooltipProvider>;
}
createRoot(document.getElementById("root")!).render(<Surface />);
