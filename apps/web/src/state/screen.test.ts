import { describe, expect, it } from "vitest";
import { initialScreen, reduceScreen, slotOf, type ScreenAction, type ScreenState } from "./screen";

const stream = {} as MediaStream;
const run = (state: ScreenState, ...actions: ScreenAction[]) => actions.reduce(reduceScreen, state);
const connected = (r = 7) => run(initialScreen, { type: "transport", run: r, phase: "connecting" }, { type: "transport", run: r, phase: "connected" });

describe("own share", () => {
  it("goes picker → preset menu → starting → live, and ends", () => {
    let s = run(initialScreen, { type: "pick", attempt: 1 });
    expect(s.own?.phase).toBe("picking");
    s = run(s, { type: "picked", attempt: 1, stream, surface: "window" });
    expect(s.own).toMatchObject({ phase: "choosing", spec: { mode: "sharp", height: 1080, fps: 30 }, surface: "window" });
    s = run(s, { type: "choose", spec: { mode: "smooth", height: 1080, fps: 60 } });
    expect(s.own?.spec).toEqual({ mode: "smooth", height: 1080, fps: 60 });
    s = run(s, { type: "starting", attempt: 1 }, { type: "started", attempt: 1, share: "A" });
    expect(s.own).toMatchObject({ phase: "starting", share: "A" });
    // Someone else's share going live is not mine.
    expect(run(s, { type: "live", share: "B" }).own?.phase).toBe("starting");
    s = run(s, { type: "live", share: "A" }, { type: "viewers", share: "A", members: 3, guests: 2 }, { type: "viewers", share: "B", members: 9, guests: 9 });
    expect(s.own).toMatchObject({ phase: "live", viewers: 3, guests: 2 });
    // The checkbox belongs to the preset menu: once started it stays as confirmed.
    expect(run(s, { type: "guestLink", on: false }).own?.guestLink).toBe(true);
    expect(run(s, { type: "ownEnded", attempt: 1 }).own).toBeNull();
  });

  it("auto picks Smooth for screens and unknown surfaces, Text for windows and tabs", () => {
    const picked = (surface: "screen" | "window" | "tab" | "unknown") => run(initialScreen, { type: "pick", attempt: 1 }, { type: "picked", attempt: 1, stream, surface }).own?.spec.mode;
    expect([picked("screen"), picked("unknown"), picked("window"), picked("tab")]).toEqual(["smooth", "smooth", "sharp", "sharp"]);
  });

  it("ignores late results of an earlier attempt", () => {
    const s = run(initialScreen, { type: "pick", attempt: 1 }, { type: "ownEnded", attempt: 1 }, { type: "pick", attempt: 2 });
    expect(run(s, { type: "picked", attempt: 1, stream, surface: "tab" }).own?.phase).toBe("picking");
    expect(run(s, { type: "ownEnded", attempt: 1 }).own?.attempt).toBe(2);
    // One share at a time: a second pick while one runs changes nothing.
    expect(run(s, { type: "pick", attempt: 3 }).own?.attempt).toBe(2);
    // Starting needs the preset menu first.
    expect(run(s, { type: "starting", attempt: 2 }).own?.phase).toBe("picking");
  });
});

describe("watching", () => {
  it("watches on the stage once its own request is answered", () => {
    let s = run(connected(), { type: "watch", share: "A", id: 1 });
    expect(s.stage).toBe("A");
    expect(s.watches).toEqual([{ share: "A", id: 1, slot: null, status: "requesting" }]);
    expect(slotOf(s, "A")).toBeNull();
    // The server's slot event comes first; only the answer makes it a watch.
    s = run(s, { type: "slot", run: 7, slot: 2, share: "A" });
    expect(slotOf(s, "A")).toBeNull();
    s = run(s, { type: "granted", run: 7, id: 1, slot: 2 });
    expect(slotOf(s, "A")).toBe(2);
    expect(s.slots).toEqual([null, null, "A", null]);
  });

  it("holds a granted watch's pause until it resumes, and forgets it with the grant", () => {
    let s = run(connected(), { type: "watch", share: "A", id: 1 });
    // Not granted yet: an earlier grant's pause cannot reach it.
    expect(run(s, { type: "paused", share: "A", reason: "bandwidth" }).watches[0]?.paused).toBeUndefined();
    s = run(s, { type: "granted", run: 7, id: 1, slot: 0 }, { type: "paused", share: "A", reason: "bandwidth" });
    expect(s.watches[0]).toEqual({ share: "A", id: 1, slot: 0, status: "watching", paused: "bandwidth" });
    expect(run(s, { type: "paused", share: "A", reason: "budget" }).watches[0]?.paused).toBe("budget");
    expect(run(s, { type: "resumed", share: "A" }).watches[0]).toEqual({ share: "A", id: 1, slot: 0, status: "watching" });
    // A new transport run asks for new slots: no pause carries over.
    expect(run(s, { type: "transport", run: 8, phase: "connecting" }).watches[0]).toEqual({ share: "A", id: 1, slot: null, status: "requesting" });
  });

  it("ignores answers of other runs", () => {
    const s = run(connected(), { type: "watch", share: "A", id: 1 }, { type: "granted", run: 8, id: 1, slot: 1 });
    expect(slotOf(s, "A")).toBeNull();
    expect(run(s, { type: "stream", run: 6, slot: 0, stream }).streams[0]).toBeNull();
    expect(run(s, { type: "stream", run: 7, slot: 0, stream }).streams[0]).toBe(stream);
  });

  it("ends a watch when the server frees its slot, and moves the stage on", () => {
    let s = run(connected(), { type: "watch", share: "A", id: 1 }, { type: "granted", run: 7, id: 1, slot: 0 },
      { type: "watch", share: "B", id: 2 }, { type: "granted", run: 7, id: 2, slot: 1 });
    expect(s.stage).toBe("B");
    s = run(s, { type: "stage", share: "A" });
    expect(s.stage).toBe("A");
    s = run(s, { type: "slot", run: 7, slot: 0, share: null });
    expect(s.watches.map(w => w.share)).toEqual(["B"]);
    expect(s.stage).toBe("B");
    s = run(s, { type: "unwatch", share: "B" });
    expect(s.stage).toBeNull();
    expect(s.slots).toEqual([null, null, null, null]);
  });

  it("watch → stop → watch: the first watch's grant and release never touch the second", () => {
    let s = run(connected(), { type: "watch", share: "A", id: 1 }, { type: "unwatch", share: "A" }, { type: "watch", share: "A", id: 2 });
    // The first request's assignment, its answer and its release all arrive after the second watch began.
    s = run(s, { type: "slot", run: 7, slot: 0, share: "A" }, { type: "granted", run: 7, id: 1, slot: 0 }, { type: "slot", run: 7, slot: 0, share: null });
    expect(s.watches).toEqual([{ share: "A", id: 2, slot: null, status: "requesting" }]);
    s = run(s, { type: "slot", run: 7, slot: 3, share: "A" }, { type: "granted", run: 7, id: 2, slot: 3 });
    expect(slotOf(s, "A")).toBe(3);
  });

  it("puts a share already watched back on the stage instead of watching it twice", () => {
    const s = run(connected(), { type: "watch", share: "A", id: 1 }, { type: "watch", share: "B", id: 2 }, { type: "watch", share: "A", id: 3 });
    expect(s.watches.map(w => w.id)).toEqual([1, 2]);
    expect(s.stage).toBe("A");
    // Only watched shares go on the stage.
    expect(run(s, { type: "stage", share: "C" }).stage).toBe("A");
  });

  it("a new run waits for new answers; a failed transport ends every watch", () => {
    let s = run(connected(), { type: "watch", share: "A", id: 1 }, { type: "granted", run: 7, id: 1, slot: 0 }, { type: "stream", run: 7, slot: 0, stream });
    const renewed = run(s, { type: "transport", run: 9, phase: "connecting" });
    expect(renewed.watches).toEqual([{ share: "A", id: 1, slot: null, status: "requesting" }]);
    expect(renewed.streams[0]).toBeNull();
    // An older run's report changes nothing.
    expect(run(renewed, { type: "transport", run: 7, phase: "failed" }).watches).toHaveLength(1);
    s = run(s, { type: "transport", run: 7, phase: "failed" });
    expect(s).toMatchObject({ watches: [], stage: null, transport: { run: 7, phase: "failed" } });
  });

  it("watching is independent of my own share, which start → stop → start leaves alone", () => {
    let s = run(connected(), { type: "watch", share: "A", id: 1 }, { type: "granted", run: 7, id: 1, slot: 0 },
      { type: "watch", share: "B", id: 2 }, { type: "granted", run: 7, id: 2, slot: 1 });
    const watching = s.watches;
    s = run(s, { type: "pick", attempt: 1 }, { type: "picked", attempt: 1, stream, surface: "tab" }, { type: "starting", attempt: 1 },
      { type: "started", attempt: 1, share: "M1" }, { type: "live", share: "M1" }, { type: "ownEnded", attempt: 1 },
      { type: "pick", attempt: 2 }, { type: "picked", attempt: 2, stream, surface: "tab" }, { type: "starting", attempt: 2 },
      { type: "started", attempt: 2, share: "M2" }, { type: "live", share: "M2" });
    expect(s.watches).toBe(watching);
    expect(s.transport).toEqual({ run: 7, phase: "connected" });
    expect(s.own).toMatchObject({ share: "M2", phase: "live" });
  });

  it("resets with the session but keeps the chat preference", () => {
    const s = run(connected(), { type: "watch", share: "A", id: 1 }, { type: "chat", hidden: true }, { type: "pick", attempt: 1 }, { type: "reset" });
    expect(s).toEqual({ ...initialScreen, chatHidden: true });
  });
});
