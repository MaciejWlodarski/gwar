/**
 * Lip-sync of a watched share with sound (docs/share-audio.md "Lip-sync"). Picture and sound are
 * read where they come out and compared through the sender reports' association, so the two can
 * take different roads. The picture waits for the sound. In the desktop app, where the webview's
 * own sync has no sound to work with, sound also waits when the picture is behind with nothing left
 * to give: the mixer plays it up to 1 % slower, which goes unheard. A browser's own sync does that
 * part there. Voice never waits. A share without sound never comes through here.
 */

/** RTP times of one instant, from `screen.audio.sync`. */
export interface SoundAssociation { known: boolean; audio_rtp: number; video_rtp: number }
/** A sound sample and when it comes out: `share_playback_clock`, or a browser's receiver. */
export interface SoundClock { rtpTime: number; unixMs: number }
/** The last picture shown and when, from the receiver. */
export interface PictureClock { rtpTime: number; unixMs: number }

const AUDIO_KHZ = 48;
const VIDEO_KHZ = 90;
/** The receiver reports a frame when it is handed over; the screen shows it about a frame later. */
const DISPLAY_MS = 16;
/** A picture this old says nothing about now: a still screen sends no frames. */
const STALE_PICTURE_MS = 250;
/** Closer than this is noise: one frame of the picture and one block of the sound. */
const DEADBAND_MS = 10;
/** Half the error each second, in steps a viewer cannot see. */
const GAIN = 0.5;
/** How much a picture wait shrinks at each look that finds the sides together: nobody sees 5 ms. */
const GIVE_BACK_MS = 5;
/** Sound stretches to wait, so it does not for a lead nobody would notice. */
const SOUND_DEADBAND_MS = 25;
/** The mixer plays at most 1 % off, 10 ms a second, and queues so much. */
const SOUND_STEP_MS = 10;
const MAX_SOUND_WAIT_MS = 400;
const MAX_STEP_MS = 40;
/** The first seconds of a watch are its keyframe and its buffers settling, not its lip-sync. */
const SETTLING_LOOKS = 2;
/** The picture's wait, which a Bluetooth speaker can reach. The receiver's own limit is 4 s. */
export const MAX_WAIT_MS = 1_000;

const signedRtp = (value: number) => (value | 0);

/**
 * How far the sound runs behind the picture, in milliseconds; negative when it leads. Each side's
 * position in the share's own time is read at the moment it comes out, so where the two were read
 * does not matter.
 */
export function soundLagMs(association: SoundAssociation, sound: SoundClock, picture: PictureClock): number {
  const pictureAt = signedRtp(picture.rtpTime - association.video_rtp) / VIDEO_KHZ - (picture.unixMs + DISPLAY_MS);
  const soundAt = signedRtp(sound.rtpTime - association.audio_rtp) / AUDIO_KHZ - sound.unixMs;
  return pictureAt - soundAt;
}

/** What each side holds back, in milliseconds. */
export interface Waits { picture: number; sound: number }

const clamp = (value: number, max = MAX_WAIT_MS) => Math.round(Math.min(max, Math.max(0, value)));

/**
 * The next waits. The first look moves the picture the whole way: nobody has seen it settle. Sound
 * only ever moves in steps, because a picture that starts late usually catches up by itself, and
 * sound held for that would then lag. Noise moves nothing, and sound is only made to wait for a
 * clear lead. Whatever one side already waits is given back before the other waits more, and both
 * keep giving back while they agree: a browser's own sync pulls each side up to the other, so a lag
 * alone would never show that a wait is no longer needed.
 */
export function nextWaits(waits: Waits, lag: number, first: boolean, shownPicture: number, soundWaits: boolean): Waits {
  const change = first ? lag : Math.min(MAX_STEP_MS, Math.abs(lag) * GAIN);
  if (lag >= 0 && waits.sound > 0) {
    return { ...waits, sound: clamp(waits.sound - Math.min(SOUND_STEP_MS, Math.max(SOUND_STEP_MS / 2, change)), MAX_SOUND_WAIT_MS) };
  }
  if (!first && Math.abs(lag) < DEADBAND_MS) return waits.picture > 0 ? { ...waits, picture: clamp(waits.picture - GIVE_BACK_MS) } : waits;
  if (lag > 0) return { ...waits, picture: clamp((waits.picture === 0 ? shownPicture : waits.picture) + change) };
  const step = Math.min(MAX_STEP_MS, -lag * GAIN);
  if (waits.picture > 0) return { ...waits, picture: clamp(waits.picture - step) };
  if (!soundWaits || lag > -SOUND_DEADBAND_MS) return waits;
  return { ...waits, sound: clamp(waits.sound + Math.min(SOUND_STEP_MS, step), MAX_SOUND_WAIT_MS) };
}

export interface SyncSlot {
  share: string;
  slot: number;
  /** False while muted or deafened: nothing to line up with, so the picture need not wait. */
  heard: boolean;
  /** Paused shares send nothing: the wait stays for when they resume. */
  paused: boolean;
}

export interface PictureSyncLink {
  association(share: string): Promise<SoundAssociation>;
  sound(slot: number): Promise<SoundClock | null>;
  picture(slot: number): PictureClock | null;
  /** How long the receiver holds the picture now, in milliseconds. */
  shown(slot: number): Promise<number>;
  wait(slot: number, waits: Waits): void;
  /** Whether sound can wait here, or the engine lines it up by itself. */
  soundWaits: boolean;
  now(): number;
}

interface SlotState { share: string; waits: Waits; looks: number }
const NO_WAIT: Waits = { picture: 0, sound: 0 };
const same = (a: Waits, b: Waits) => a.picture === b.picture && a.sound === b.sound;

/** One watch transport's waits, one pair per slot. */
export class PictureSync {
  private readonly states = new Map<number, SlotState>();
  private running = false;
  /** An older server does not know the question; it stops being asked. */
  private unsupported = false;

  constructor(private readonly link: PictureSyncLink) {}

  /** One look at every slot with sound. A slot not listed had its sound end: it stops waiting. */
  async tick(slots: readonly SyncSlot[]): Promise<void> {
    if (this.running) return;
    this.running = true;
    try {
      for (const [slot, state] of [...this.states]) {
        if (slots.find(s => s.slot === slot)?.share !== state.share) this.release(slot);
      }
      if (!this.unsupported) for (const slot of slots) await this.look(slot);
    } finally { this.running = false; }
  }

  /** The slot's sound ended, or its share did: both sides are back to as soon as they can. */
  release(slot: number): void {
    const state = this.states.get(slot);
    this.states.delete(slot);
    if (state && !same(state.waits, NO_WAIT)) this.link.wait(slot, NO_WAIT);
  }

  private async look({ share, slot, heard, paused }: SyncSlot): Promise<void> {
    if (paused) return;
    const state = this.states.get(slot) ?? { share, waits: NO_WAIT, looks: 0 };
    if (!heard) {
      this.set(slot, state, { ...state, waits: NO_WAIT, looks: 0 });
      return;
    }
    const association = await this.link.association(share).catch(() => { this.unsupported = true; return null; });
    if (!association?.known) return;
    const sound = await this.link.sound(slot).catch(() => null);
    const picture = this.link.picture(slot);
    if (!sound || !picture || this.link.now() - picture.unixMs > STALE_PICTURE_MS) return;
    if (state.looks < SETTLING_LOOKS) { this.states.set(slot, { ...state, looks: state.looks + 1 }); return; }
    const lag = soundLagMs(association, sound, picture);
    const shown = await this.link.shown(slot).catch(() => 0);
    this.set(slot, state, { ...state, waits: nextWaits(state.waits, lag, state.looks === SETTLING_LOOKS, shown, this.link.soundWaits), looks: state.looks + 1 });
  }

  private set(slot: number, before: SlotState, after: SlotState): void {
    if (!same(before.waits, after.waits)) this.link.wait(slot, after.waits);
    if (after.looks === 0) this.states.delete(slot); else this.states.set(slot, after);
  }
}
