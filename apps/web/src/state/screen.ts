/**
 * Screen sharing state of this session (docs/screen-share.md): my own share, the watch
 * transport, the shares I watch and which of them is on the stage. Pure transitions, applied
 * by the store; the screen controller (`screen-controller.ts`) feeds them from the server's
 * events and the media layer (`voice/screen.ts`). Which shares run where is session state
 * (`SessionState.shares`), not this.
 */
import { create } from "zustand";
import type { ScreenSurface } from "../proto/ScreenSurface";
import { SCREEN_SLOTS } from "../voice/screen";
import { resolveQuality, defaultQuality, sameSpec, type ScreenSpec } from "../voice/screen-quality";

/**
 * My share: `picking` while the browser's picker is open, `choosing` in the quality menu,
 * `starting` from `screen.start` until the server announces it, then `live`.
 */
export type OwnPhase = "picking" | "choosing" | "starting" | "live";

export interface OwnShare {
  phase: OwnPhase;
  /** One per attempt: late results of an earlier attempt change nothing. */
  attempt: number;
  share: string | null;
  stream: MediaStream | null;
  surface: ScreenSurface;
  spec: ScreenSpec;
  /** Members watching, from `screen.viewers`. */
  viewers: number;
  /** TeamSpeak guests watching through the watch link, from `screen.viewers`. */
  guests: number;
  /** The quality menu's "Post a watch link to TeamSpeak", on by default. */
  guestLink: boolean;
  /** The sharer's own thumbnail, off by default. */
  preview: boolean;
}

/**
 * `requesting` until the server answers this watch's own `screen.watch`. `id` tells a watch
 * from an earlier one of the same share whose requests and releases may still be in flight.
 */
export interface Watch {
  share: string;
  id: number;
  slot: number | null;
  status: "requesting" | "watching";
  /** New-assignment keyframe evidence and readiness survive video remounts. */
  video?: { slot: number; generation: number; at: number; baseline: number | null; ready: boolean };
}

export type TransportPhase = "none" | "connecting" | "connected" | "failed";

export interface ScreenState {
  own: OwnShare | null;
  /** The session's watch transport and its run (`screen.watch.offer`). */
  transport: { phase: TransportPhase; run: number };
  /** Shares I watch, in watch order (their bandwidth priority after the stage). */
  watches: Watch[];
  /** The share each slot of the current run carries (`screen.slot`). */
  slots: Array<string | null>;
  /** Each slot's received video, once its track arrived. */
  streams: Array<MediaStream | null>;
  /** The share on the stage; the stage shows while there is one. */
  stage: string | null;
  /** The chat beside the stage is collapsed. */
  chatHidden: boolean;
  /** Shares whose TeamSpeak watch link works, as `screen.link` said (to their sharer and moderators). */
  links: Record<string, true>;
}

export type ScreenAction =
  | { type: "pick"; attempt: number }
  | { type: "picked"; attempt: number; stream: MediaStream; surface: ScreenSurface }
  | { type: "choose"; spec: ScreenSpec }
  | { type: "surface"; attempt: number; surface: ScreenSurface; spec: ScreenSpec }
  | { type: "starting"; attempt: number }
  | { type: "started"; attempt: number; share: string }
  | { type: "live"; share: string }
  | { type: "viewers"; share: string; members: number; guests: number }
  | { type: "guestLink"; on: boolean }
  | { type: "link"; share: string; active: boolean }
  | { type: "preview"; on: boolean }
  | { type: "ownEnded"; attempt: number }
  | { type: "transport"; run: number; phase: TransportPhase }
  | { type: "stream"; run: number; slot: number; stream: MediaStream }
  | { type: "watch"; share: string; id: number }
  | { type: "granted"; run: number; id: number; slot: number }
  | { type: "watchKeyframe"; run: number; id: number; slot: number; share: string; generation: number; at: number }
  | { type: "watchBaseline"; run: number; id: number; slot: number; share: string; generation: number; framesDecoded: number }
  | { type: "watchFrame"; run: number; id: number; slot: number; share: string; generation: number; framesDecoded?: number; presented?: boolean; fallback?: boolean }
  | { type: "slot"; run: number; slot: number; share: string | null }
  | { type: "unwatch"; share: string }

  | { type: "stage"; share: string }
  | { type: "chat"; hidden: boolean }
  | { type: "reset" };

const emptySlots = <T>(): Array<T | null> => Array.from({ length: SCREEN_SLOTS }, () => null);

export const initialScreen: ScreenState = {
  own: null,
  transport: { phase: "none", run: 0 },
  watches: [],
  slots: emptySlots(),
  streams: emptySlots(),
  stage: null,
  chatHidden: false,
  links: {},
};

function withoutWatch(state: ScreenState, share: string): ScreenState {
  const watches = state.watches.filter(w => w.share !== share);
  if (watches.length === state.watches.length) return state;
  const stage = state.stage === share ? (watches[0]?.share ?? null) : state.stage;
  return { ...state, watches, stage, slots: state.slots.map(s => (s === share ? null : s)) };
}

function setOwn(state: ScreenState, attempt: number, phases: OwnPhase[], patch: Partial<OwnShare>): ScreenState {
  const own = state.own;
  if (!own || own.attempt !== attempt || !phases.includes(own.phase)) return state;
  return { ...state, own: { ...own, ...patch } };
}

export function reduceScreen(state: ScreenState, action: ScreenAction): ScreenState {
  switch (action.type) {
    case "pick":
      if (state.own) return state;
      return { ...state, own: { phase: "picking", attempt: action.attempt, share: null, stream: null, surface: "unknown", spec: resolveQuality(defaultQuality, "unknown"), viewers: 0, guests: 0, guestLink: true, preview: false } };
    case "picked":
      return setOwn(state, action.attempt, ["picking"], { phase: "choosing", stream: action.stream, surface: action.surface, spec: resolveQuality(defaultQuality, action.surface) });
    case "choose": {
      const own = state.own;
      if (!own || own.phase === "picking") return state;
      if (sameSpec(own.spec, action.spec)) return state;
      return { ...state, own: { ...own, spec: { ...action.spec } } };
    }
    case "starting":
      return setOwn(state, action.attempt, ["choosing"], { phase: "starting" });
    case "surface":
      return setOwn(state, action.attempt, ["starting", "live"], { surface: action.surface, spec: { ...action.spec } });
    case "started":
      return setOwn(state, action.attempt, ["starting"], { share: action.share });
    case "live":
      return state.own?.share === action.share ? setOwn(state, state.own.attempt, ["starting", "live"], { phase: "live" }) : state;
    case "viewers":
      return state.own?.share === action.share ? { ...state, own: { ...state.own, viewers: action.members, guests: action.guests } } : state;
    case "guestLink":
      return state.own?.phase === "choosing" ? { ...state, own: { ...state.own, guestLink: action.on } } : state;
    case "link": {
      const { [action.share]: _, ...rest } = state.links;
      return { ...state, links: action.active ? { ...rest, [action.share]: true } : rest };
    }
    case "preview":
      return state.own ? { ...state, own: { ...state.own, preview: action.on } } : state;
    case "ownEnded":
      return state.own?.attempt === action.attempt ? { ...state, own: null } : state;
    case "transport": {
      const { run, phase } = action;
      if (run < state.transport.run) return state;
      if (run > state.transport.run) {
        // A new run: nothing of the old one's slots carries over; watches wait for new slots.
        return { ...state, transport: { run, phase }, slots: emptySlots(), streams: emptySlots(), watches: state.watches.map(({ video: _, ...w }) => ({ ...w, slot: null, status: "requesting" as const })) };
      }
      if (phase === "failed") {
        // The server ends every watch of a transport that is gone.
        return { ...state, transport: { run, phase }, watches: [], stage: null, slots: emptySlots(), streams: emptySlots() };
      }
      return { ...state, transport: { run, phase } };
    }
    case "stream": {
      if (action.run !== state.transport.run || action.slot < 0 || action.slot >= SCREEN_SLOTS) return state;
      const streams = state.streams.slice();
      streams[action.slot] = action.stream;
      return { ...state, streams };
    }
    case "watch": {
      if (state.watches.some(w => w.share === action.share)) return { ...state, stage: action.share };
      return { ...state, watches: [...state.watches, { share: action.share, id: action.id, slot: null, status: "requesting" }], stage: action.share };
    }
    case "granted": {
      // Only the answer to this watch's own request: an earlier watch's assignment never counts.
      const { run, id, slot } = action;
      if (run !== state.transport.run || slot < 0 || slot >= SCREEN_SLOTS) return state;
      const watch = state.watches.find(w => w.id === id && w.status === "requesting");
      if (!watch) return state;
      const slots = state.slots.slice();
      slots[slot] = watch.share;
      return { ...state, slots, watches: state.watches.map(w => {
        if (w !== watch) return w;
        const { video, ...rest } = w;
        return { ...rest, slot, status: "watching", ...(video?.slot === slot ? { video } : {}) };
      }) };
    }
    case "watchKeyframe": {
      if (action.run !== state.transport.run || state.slots[action.slot] !== action.share) return state;
      const watch = state.watches.find(w => w.id === action.id && w.share === action.share && (w.slot === null || w.slot === action.slot));
      if (!watch || watch.video?.ready || (watch.video?.slot === action.slot && watch.video.generation === action.generation)) return state;
      const video = { slot: action.slot, generation: action.generation, at: action.at, baseline: null, ready: false };
      return { ...state, watches: state.watches.map(w => w === watch ? { ...w, video } : w) };
    }
    case "watchBaseline":
    case "watchFrame": {
      if (action.run !== state.transport.run || state.slots[action.slot] !== action.share) return state;
      const watch = state.watches.find(w => w.id === action.id && w.share === action.share);
      const video = watch?.video;
      if (!watch || !video || video.ready || video.slot !== action.slot || video.generation !== action.generation) return state;
      let next = video;
      if (action.type === "watchBaseline") {
        if (video.baseline === null) next = { ...video, baseline: action.framesDecoded };
      } else if (watch.status === "watching" && watch.slot === action.slot &&
        (action.presented || action.fallback || (video.baseline !== null && action.framesDecoded !== undefined && action.framesDecoded > video.baseline))) {
        next = { ...video, ready: true };
      }
      return next !== video ? { ...state, watches: state.watches.map(w => w === watch ? { ...w, video: next } : w) } : state;
    }
    case "slot": {
      const { run, slot, share } = action;
      if (run !== state.transport.run || slot < 0 || slot >= SCREEN_SLOTS) return state;
      const slots = state.slots.slice();
      slots[slot] = share;
      // A pending rewatch may have seen the preceding grant's keyframe.
      // Every server assignment/release retires that evidence before its reply.
      const watches = state.watches.map(w => {
        if (w.status !== "requesting" || w.video?.slot !== slot) return w;
        const { video: _, ...rest } = w;
        return rest;
      });
      const next: ScreenState = { ...state, slots, watches };
      if (share !== null) return next;
      // The server freed the slot: the watch it granted ended (unwatched, stopped, no access).
      // A watch asked again since then is still requesting, and waits for its own answer.
      const ended = next.watches.find(w => w.slot === slot && w.status === "watching");
      return ended ? withoutWatch(next, ended.share) : next;
    }
    case "unwatch":
      return withoutWatch(state, action.share);
    case "stage":
      return state.watches.some(w => w.share === action.share) ? { ...state, stage: action.share } : state;
    case "chat":
      return { ...state, chatHidden: action.hidden };
    case "reset":
      return { ...initialScreen, chatHidden: state.chatHidden };

  }
}

/** The slot carrying a watched share, once the server named it. */
export function slotOf(state: Pick<ScreenState, "watches">, share: string): number | null {
  const watch = state.watches.find(w => w.share === share);
  return watch?.status === "watching" ? watch.slot : null;
}

interface ScreenStore extends ScreenState {
  dispatch(action: ScreenAction): void;
}

export const useScreen = create<ScreenStore>()(set => ({
  ...initialScreen,
  dispatch: action => set(s => reduceScreen(s, action)),
}));
