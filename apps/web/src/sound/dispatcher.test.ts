import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Client } from "../proto/Client";
import type { ChatMessage } from "../proto/ChatMessage";
import { initialState } from "../state/reducer";
import type { SessionState } from "../state/types";
import { defaultSounds, readSounds, renderCue, SOUND_CUES, type SoundCue } from "./catalog";
import { SoundDispatcher } from "./dispatcher";

const client = (id: number, channel: number | null) => ({ id, channel, uid: `u${id}` }) as Client;
const session = (extra: Partial<SessionState> = {}): SessionState => ({ ...initialState, phase: "online", epoch: 1,
  me: { session: 1, uid: "u1" }, clients: { 1: client(1, 2), 2: client(2, 2) }, ...extra });
const message = (extra: Partial<ChatMessage> = {}): ChatMessage => ({ id: 1, author: 2, author_uid: "u2", author_name: "Peer", target: { channel: 2 }, mentions: ["u1"], text: "hi", sent_at: 1, ...extra });
let time: number, deafened: boolean, settings: typeof defaultSounds, play: ReturnType<typeof vi.fn<(cue: SoundCue, volume: number) => void>>, d: SoundDispatcher;
beforeEach(() => {
  time = 0; deafened = false; settings = readSounds(defaultSounds); play = vi.fn();
  d = new SoundDispatcher(() => settings, () => deafened, play, () => time);
});
const heard = () => play.mock.calls.map(([cue]) => cue);
const tick = () => { time += 1000; };

describe("cue preferences and budgets", () => {
  it("plays every enabled event at the selected cue volume", () => {
    settings.volume = 0.25;
    for (const cue of SOUND_CUES) { tick(); d.cue(cue); }
    expect(heard()).toEqual(SOUND_CUES);
    expect(play.mock.calls.every(([, volume]) => volume === 0.25)).toBe(true);
  });
  it("honors the master, zero volume, and every individual off switch, including previews", () => {
    settings.enabled = false; d.cue("mute"); d.cue("mute", true);
    settings.enabled = true; settings.volume = 0; d.cue("mute");
    settings.volume = 0.6;
    for (const cue of SOUND_CUES) { settings.events[cue] = false; tick(); d.cue(cue); d.cue(cue, true); }
    expect(play).not.toHaveBeenCalled();
  });
  it("coalesces activity across cue types, without hiding controls or connection loss", () => {
    for (let i = 0; i < 50; i++) d.cue(i % 2 ? "peerJoin" : "peerLeave");
    d.cue("mention"); d.cue("shareStarted"); d.cue("mute"); d.cue("connectionLost");
    expect(heard()).toEqual(["peerLeave", "mute", "connectionLost"]);
    time = 749; d.cue("peerJoin");
    time = 750; d.cue("peerJoin");
    expect(heard().at(-1)).toBe("peerJoin");
    expect(play).toHaveBeenCalledTimes(4);
  });
  it("keeps only control confirmations audible while deafened", () => {
    deafened = true;
    for (const cue of SOUND_CUES) { tick(); d.cue(cue); }
    expect(heard()).toEqual(["mute", "unmute", "deafen", "undeafen"]);
  });
});

describe("voice lifecycle", () => {
  it("confirms actual connection, reports loss once, and distinguishes recovery from joining", () => {
    d.connected(null); d.lost(); expect(play).not.toHaveBeenCalled();
    d.connected(2); d.connected(2); tick(); d.lost(); d.lost(); tick(); d.connected(2); d.connected(2);
    tick(); d.connected(3); tick(); d.left(); d.left();
    expect(heard()).toEqual(["selfJoin", "connectionLost", "reconnected", "selfJoin", "selfLeave"]);
  });
  it("remembers recovery even when teardown defers the lost sound", () => {
    d.connected(2); tick(); expect(d.lost(true)).toBe(true); expect(d.lost()).toBe(false);
    d.connected(2);
    expect(heard()).toEqual(["selfJoin", "reconnected"]);
    d.reset(); tick(); d.connected(2); expect(heard().at(-1)).toBe("selfJoin");
  });
});

describe("live presence and screen events", () => {
  it("sounds only for joins and leaves in my voice channel, including disconnects", () => {
    const before = session();
    const joined = session({ clients: { ...before.clients, 3: client(3, 2), 4: client(4, 3) } });
    d.session(joined, before); tick();
    d.session(session({ clients: { 1: client(1, 2), 4: client(4, 3) } }), joined);
    expect(heard()).toEqual(["peerJoin", "peerLeave"]);
  });
  it("does not announce welcomes, resync, my channel move, or joins while I am outside voice", () => {
    const before = session(), joined = session({ clients: { ...before.clients, 3: client(3, 2) } });
    d.session({ ...joined, epoch: 2 }, before);
    d.session(joined, { ...before, phase: "reconnecting" });
    d.session({ ...joined, clients: { ...joined.clients, 1: client(1, 3) } }, before);
    d.session(session({ clients: { 1: client(1, null), 3: client(3, 2) } }), session({ clients: { 1: client(1, null) } }));
    expect(play).not.toHaveBeenCalled();
  });
  it("announces a peer's new share once, only in my voice channel", () => {
    const before = session();
    const share = { share: "s", client: 2, channel: 2, preset: "text", surface: "screen" } as const;
    const next = session({ shares: { s: share } });
    d.session(next, before); tick(); d.session(next, next);
    d.session(session({ shares: { mine: { ...share, share: "mine", client: 1 }, other: { ...share, share: "other", channel: 3 } } }), before);
    expect(heard()).toEqual(["shareStarted"]);
    tick(); d.cue("shareStopped"); expect(heard().at(-1)).toBe("shareStopped");
  });
});

describe("verified live messages", () => {
  it("distinguishes mentions and DMs, skips my own and already visible messages", () => {
    d.message(message(), session({ focused: false })); tick();
    d.message(message({ id: 2, target: { client: 1 } }), session({ focused: true, activeThread: "server" })); tick();
    d.message(message({ id: 3, author_uid: "u1" }), session());
    d.message(message({ id: 4, mentions: [] }), session({ focused: false }));
    d.message(message({ id: 5 }), session({ focused: true, activeThread: "channel", viewChannel: 2 }));
    d.message(message({ id: 6, target: { client: 1 } }), session({ focused: true, activeThread: "dm:u2" }));
    expect(heard()).toEqual(["mention", "directMessage"]);
  });
  it("deduplicates live message echoes even after the burst budget expires", () => {
    d.message(message(), session({ focused: false })); tick();
    d.message(message(), session({ focused: false }));
    expect(heard()).toEqual(["mention"]);
  });
});

it("renders short, click-free, quiet original cues and normalizes damaged preferences", () => {
  for (const cue of SOUND_CUES) {
    const pcm = renderCue(cue, 1);
    expect(pcm.length).toBeLessThan(14_400);
    expect(pcm.every(s => Number.isFinite(s) && Math.abs(s) < 0.1)).toBe(true);
    expect(Math.abs(pcm[0]!)).toBeLessThan(0.0001);
    expect(Math.abs(pcm.at(-1)!)).toBeLessThan(0.0001);
    expect(renderCue(cue, 0).every(s => s === 0)).toBe(true);
  }
  expect(readSounds({ enabled: false, volume: NaN, events: { mute: false } })).toMatchObject({ enabled: false, volume: 0.6, events: { mute: false, unmute: true } });
});
