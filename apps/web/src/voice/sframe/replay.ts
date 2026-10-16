/** A 128-counter replay window per (group, full epoch, leaf). Bit 0 is the
 * highest authenticated counter. */
const MASK = (1n << 128n) - 1n;
export const REPLAY_WINDOW = 128n;

export class Replay {
  private highest: bigint | null = null;
  private bitmap = 0n;
  /** Read-only: whether `ctr` could still be admitted. */
  fresh(ctr: bigint): boolean {
    if (this.highest === null || ctr > this.highest) return true;
    const behind = this.highest - ctr;
    return behind < REPLAY_WINDOW && ((this.bitmap >> behind) & 1n) === 0n;
  }
  /** Marks an authenticated counter; false (and no change) for a replay. */
  admit(ctr: bigint): boolean {
    if (!this.fresh(ctr)) return false;
    if (this.highest === null) { this.highest = ctr; this.bitmap = 1n; return true; }
    if (ctr > this.highest) {
      const shift = ctr - this.highest;
      this.bitmap = shift >= REPLAY_WINDOW ? 1n : ((this.bitmap << shift) | 1n) & MASK;
      this.highest = ctr;
    } else this.bitmap |= 1n << (this.highest - ctr);
    return true;
  }
}
