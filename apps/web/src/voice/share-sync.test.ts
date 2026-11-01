import { describe, expect, it, vi } from "vitest";
import { RequestError } from "../net/connection";
import { MAX_WAIT_MS, nextWaits, PictureSync, SOUND_AIM_MS, soundLagMs, type PictureSyncLink, type SyncSlot, type Waits } from "./share-sync";

const association = { known: true, audio_rtp: 48_000, video_rtp: 90_000 };
/** A picture and a sound of the same instant of the share, `lag` ms apart at the output. */
const pair = (lag: number, ms = 1_000, at = 5_000) => ({
  sound: { rtpTime: association.audio_rtp + ms * 48, unixMs: at + lag },
  picture: { rtpTime: association.video_rtp + ms * 90, unixMs: at - 16 },
});

describe("soundLagMs", () => {
  it("is zero when both come out together, whatever instant was read", () => {
    const { sound, picture } = pair(0);
    expect(soundLagMs(association, sound, picture)).toBeCloseTo(0);
    expect(soundLagMs(association, { rtpTime: sound.rtpTime + 4_800, unixMs: sound.unixMs + 100 }, picture)).toBeCloseTo(0);
  });

  it("says the sound lags, or leads", () => {
    const late = pair(80), early = pair(-30);
    expect(soundLagMs(association, late.sound, late.picture)).toBeCloseTo(80);
    expect(soundLagMs(association, early.sound, early.picture)).toBeCloseTo(-30);
  });

  it("survives RTP time wrapping on both clocks", () => {
    const wrapped = { known: true, audio_rtp: 4_294_967_000, video_rtp: 4_294_967_000 };
    const sound = { rtpTime: (wrapped.audio_rtp + 48_000) % 2 ** 32, unixMs: 6_000 };
    const picture = { rtpTime: (wrapped.video_rtp + 90_000) % 2 ** 32, unixMs: 5_984 };
    expect(soundLagMs(wrapped, sound, picture)).toBeCloseTo(0);
  });
});

const zero = { picture: 0, sound: 0 };

describe("nextWaits", () => {
  it("moves the picture the whole way the first time, on top of what it already waits", () => {
    expect(nextWaits(zero, 80, true, 30, true)).toEqual({ picture: 110, sound: 0 });
  });

  it("then moves in steps nobody sees, and leaves noise alone", () => {
    const waits = { picture: 100, sound: 0 };
    expect(nextWaits(waits, 20, false, 30, true).picture).toBe(110);
    expect(nextWaits(waits, 500, false, 30, true).picture).toBe(140);
    expect(nextWaits(waits, -500, false, 30, true).picture).toBe(60);
    expect(nextWaits(zero, 9, false, 30, true)).toBe(zero);
  });

  it("gives a picture wait back while the sides agree", () => {
    expect(nextWaits({ picture: 100, sound: 0 }, 9, false, 30, true).picture).toBe(95);
  });

  it("stays within what a receiver can do", () => {
    expect(nextWaits({ picture: 10, sound: 0 }, -500, false, 30, true).picture).toBe(0);
    expect(nextWaits({ picture: MAX_WAIT_MS, sound: 0 }, 500, false, 30, true).picture).toBe(MAX_WAIT_MS);
    expect(nextWaits({ picture: 0, sound: 400 }, -500, false, 30, true).sound).toBe(400);
  });

  it("holds sound only for a clear lead, and only in small steps", () => {
    expect(nextWaits(zero, -20, false, 30, true)).toEqual(zero);
    expect(nextWaits(zero, -300, false, 30, true)).toEqual({ picture: 0, sound: 10 });
    expect(nextWaits({ picture: 0, sound: 200 }, -100, false, 30, true).sound).toBe(210);
  });

  it("leaves sound alone where the engine lines it up by itself", () => {
    expect(nextWaits(zero, -300, false, 30, false)).toEqual(zero);
    expect(nextWaits(zero, -300, true, 30, false)).toEqual(zero);
  });

  it("does not jump sound for a picture that starts late", () => {
    expect(nextWaits(zero, -300, true, 30, true).sound).toBe(10);
  });

  it("gives held sound back even when nothing shows it is too much", () => {
    expect(nextWaits({ picture: 0, sound: 200 }, 0, false, 30, true).sound).toBe(195);
    expect(nextWaits({ picture: 0, sound: 200 }, -4, false, 30, true).sound).toBe(200);
  });

  it("moves the picture the whole way even while sound waits: sound can only come down slowly", () => {
    expect(nextWaits({ picture: 0, sound: 150 }, 260, true, 30, true)).toEqual({ picture: 290, sound: 150 });
  });

  it("gives back what one side waits before the other waits more", () => {
    expect(nextWaits({ picture: 0, sound: 200 }, 300, false, 30, true)).toEqual({ picture: 0, sound: 190 });
    expect(nextWaits({ picture: 150, sound: 0 }, -60, false, 30, true)).toEqual({ picture: 120, sound: 0 });
    expect(nextWaits({ picture: 10, sound: 0 }, -60, false, 30, true)).toEqual({ picture: 0, sound: 0 });
  });
});

/** `lag` is measured from where sync aims, as every wait is. */
function fixture(lag = 80) {
  const waits: Array<[number, Waits]> = [];
  const { sound, picture } = pair(lag + SOUND_AIM_MS);
  const link = {
    association: vi.fn(async () => association),
    sound: vi.fn(async () => sound),
    picture: vi.fn(() => picture),
    shown: vi.fn(async () => 30),
    wait: (slot, value) => { waits.push([slot, value]); },
    soundWaits: true,
    now: () => picture.unixMs + 20,
  } satisfies PictureSyncLink;
  return { waits, link, sync: new PictureSync(link) };
}
const slot: SyncSlot = { share: "A", slot: 1, heard: true, paused: false };
const at = (picture: number) => [1, { picture, sound: 0 }];
/** Past the seconds a watch settles in. */
const settle = async (sync: PictureSync, slots: SyncSlot[] = [slot]) => { await sync.tick(slots); await sync.tick(slots); };

describe("PictureSync", () => {
  it("moves the whole way once after a break has settled, then gently again", async () => {
    const { waits, sync, link } = fixture(30);
    await settle(sync);
    await sync.tick([slot]);
    const { sound, picture } = pair(300 + SOUND_AIM_MS);
    link.sound.mockResolvedValue(sound);
    link.picture.mockReturnValue(picture);
    link.now = () => picture.unixMs + 20;
    await sync.tick([slot]);
    expect(waits.at(-1)).toEqual(at(100));
    sync.linkBack(1);
    await sync.tick([slot]);
    expect(waits.at(-1)).toEqual(at(100));
    await sync.tick([slot]);
    expect(waits.at(-1)).toEqual(at(400));
    await sync.tick([slot]);
    expect(waits.at(-1)).toEqual(at(440));
  });

  it("keeps its one move after a break for the gap that opens when the sound lands", async () => {
    const { waits, sync, link } = fixture(20);
    await settle(sync);
    await sync.tick([slot]);
    sync.linkBack(1);
    await sync.tick([slot]);
    await sync.tick([slot]);
    const { sound, picture } = pair(250 + SOUND_AIM_MS);
    link.sound.mockResolvedValue(sound);
    link.picture.mockReturnValue(picture);
    link.now = () => picture.unixMs + 20;
    const before = waits.at(-1)?.[1] as Waits;
    await sync.tick([slot]);
    expect(waits.at(-1)).toEqual(at(before.picture + 250));
  });

  it("drops a stale wait at once after a break, when the picture is the late one", async () => {
    const { waits, sync, link } = fixture(200);
    await settle(sync);
    await sync.tick([slot]);
    expect(waits.at(-1)).toEqual(at(230));
    const { sound, picture } = pair(-150 + SOUND_AIM_MS);
    link.sound.mockResolvedValue(sound);
    link.picture.mockReturnValue(picture);
    link.now = () => picture.unixMs + 20;
    sync.linkBack(1);
    await sync.tick([slot]);
    await sync.tick([slot]);
    expect(waits.at(-1)).toEqual(at(80));
  });

  it("aims at the middle of what nobody notices, not at the edge sound must not lead past", async () => {
    expect(SOUND_AIM_MS).toBe(40);
    const { sync, waits, link } = fixture(0);
    const { sound, picture } = pair(40);
    link.sound.mockResolvedValue(sound);
    link.picture.mockReturnValue(picture);
    await settle(sync);
    await sync.tick([slot]);
    expect(waits).toEqual([]);
  });

  it("makes a late joiner's picture wait at once, then corrects gently", async () => {
    const { sync, waits } = fixture(80);
    await settle(sync);
    expect(waits).toEqual([]);
    await sync.tick([slot]);
    expect(waits).toEqual([at(110)]);
    await sync.tick([slot]);
    expect(waits.at(-1)).toEqual(at(150));
  });

  it("a muted or deafened viewer does not pay for sound nobody hears", async () => {
    const { sync, waits } = fixture();
    await settle(sync);
    await sync.tick([slot]);
    await sync.tick([{ ...slot, heard: false }]);
    expect(waits.at(-1)).toEqual(at(0));
    await settle(sync);
    await sync.tick([{ ...slot, heard: true }]);
    expect(waits.at(-1)).toEqual(at(110));
  });

  it("releases a slot whose sound ended, and starts over for its next share", async () => {
    const { sync, waits } = fixture();
    await settle(sync);
    await sync.tick([slot]);
    await sync.tick([]);
    expect(waits.at(-1)).toEqual(at(0));
    await settle(sync);
    await sync.tick([slot]);
    const next = { ...slot, share: "B" };
    await sync.tick([next]);
    expect(waits.at(-1)).toEqual(at(0));
    await settle(sync, [next]);
    expect(waits.at(-1)).toEqual(at(110));
  });

  it("keeps the wait through a pause", async () => {
    const { sync, waits, link } = fixture();
    await settle(sync);
    await sync.tick([slot]);
    await sync.tick([{ ...slot, paused: true }]);
    expect(waits).toEqual([at(110)]);
    expect(link.association).toHaveBeenCalledTimes(3);
  });

  it("does nothing on a still picture or before sound has come", async () => {
    const stale = fixture();
    stale.link.now = () => 99_999;
    for (let i = 0; i < 4; i++) await stale.sync.tick([slot]);
    expect(stale.waits).toEqual([]);
    const silent = fixture();
    silent.link.sound.mockResolvedValue(null as never);
    for (let i = 0; i < 4; i++) await silent.sync.tick([slot]);
    expect(silent.waits).toEqual([]);
    const unknown = fixture();
    unknown.link.association.mockResolvedValue({ known: false, audio_rtp: 0, video_rtp: 0 });
    for (let i = 0; i < 4; i++) await unknown.sync.tick([slot]);
    expect(unknown.waits).toEqual([]);
  });

  it("stops asking an older server", async () => {
    const { sync, link, waits } = fixture();
    link.association.mockRejectedValue(new RequestError("bad_request", "unknown request"));
    await sync.tick([slot]);
    await sync.tick([slot]);
    expect(link.association).toHaveBeenCalledTimes(1);
    expect(waits).toEqual([]);
  });

  it("a question that got no answer is asked again, and the sides line up as before", async () => {
    const { sync, link, waits } = fixture();
    link.association.mockRejectedValueOnce(new Error("request timed out"));
    await sync.tick([slot]);
    await settle(sync);
    await sync.tick([slot]);
    expect(link.association).toHaveBeenCalledTimes(4);
    expect(waits.at(-1)).toEqual(at(110));
  });

  it("a wait is released on mute after a question that got no answer", async () => {
    const { sync, link, waits } = fixture();
    await settle(sync);
    await sync.tick([slot]);
    expect(waits.at(-1)).toEqual(at(110));
    link.association.mockRejectedValueOnce(new Error("request timed out"));
    await sync.tick([slot]);
    await sync.tick([{ ...slot, heard: false }]);
    expect(waits.at(-1)).toEqual(at(0));
  });

  it("a wait is released on mute even after the server said it does not know the question", async () => {
    const { sync, link, waits } = fixture();
    await settle(sync);
    await sync.tick([slot]);
    link.association.mockRejectedValueOnce(new RequestError("bad_request", "unknown request"));
    await sync.tick([slot]);
    await sync.tick([{ ...slot, heard: false }]);
    expect(waits.at(-1)).toEqual(at(0));
  });

  it("a slot that never waited is told nothing when it ends", async () => {
    const { sync, waits } = fixture(0);
    sync.release(1);
    await sync.tick([]);
    expect(waits).toEqual([]);
  });
});
