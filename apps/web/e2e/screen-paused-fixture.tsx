/** Offline UI approval fixture: the actual stage with a share paused for my connection; no media or server connection. */
import { createRoot } from "react-dom/client";
import { controller } from "../src/state/controller";
import { initialState } from "../src/state/reducer";
import { initialScreen, useScreen } from "../src/state/screen";
import { useSettings } from "../src/state/settings";
import { useSession, useUi } from "../src/state/stores";
import { Stage } from "../src/ui/Screen";
import { TooltipProvider } from "../src/ui/kit";
import "../src/index.css";

const query = new URLSearchParams(location.search);
// What the server would make of a Try again now: held for 12 s more, or carried out at once.
const wait = query.get("retry") === "deferred" ? 12_000 : 0;
const client = (id: number, nickname: string) => ({ id, uid: `local-${id}`, nickname, channel: 1, groups: [], platform: "web", muted: false, deafened: false, away: null, talking: false, voice: true, voice_binding: null });
useSettings.setState({ language: "pl", theme: "dark" });
useUi.setState({ section: "servers" });
useSession.setState({ ...initialState, kind: "gwar", phase: "online", rtt: 20,
  me: { session: 1, uid: "local-1" }, permissions: ["speak", "share_screen"],
  server: { name: "Gwar", welcome: "", version: "local", default_channel: 1, max_clients: 10, upload_limit: 0, file_access: "link", teamspeak: false, screen_guest_links: false, server_chat: { encrypted: false, history: false } },
  clients: { 1: client(1, "Maciej"), 2: client(2, "Ola"), 3: client(3, "Kuba") } as never,
  channels: { 1: { id: 1, parent: null, name: "Rozmowy", topic: "", position: 0, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false } },
  shares: {
    B: { share: "B", client: 2, channel: 1, preset: "text", surface: "screen" },
    A: { share: "A", client: 3, channel: 1, preset: "text", surface: "screen" },
  } as never,
});
useScreen.setState({ ...initialScreen, transport: { phase: "connected", run: 1 }, stage: "B", slots: ["B", "A", null, null],
  watches: [
    { share: "B", id: 1, slot: 0, status: "watching", paused: "bandwidth" },
    { share: "A", id: 2, slot: 1, status: "watching", paused: "bandwidth" },
  ],
});
controller.screen.retry = () => wait;
controller.screen.stats = async () => null;
createRoot(document.getElementById("root")!).render(
  <TooltipProvider>
    <main className="flex h-screen flex-col bg-surface text-fg">
      <header className="bg-side px-4 py-3 text-sm font-semibold">Gwar <span className="font-normal text-muted">/ Rozmowy</span></header>
      <div className="flex min-h-0 flex-1"><Stage /></div>
    </main>
  </TooltipProvider>,
);
