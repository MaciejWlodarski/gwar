type Waiter = { fits: () => boolean; grant: () => void };

/** Bounds what automatically loaded images cost together: a few fetches at a time, and a ceiling on the
 *  memory held by decoded images. Whatever does not fit waits, in order, until something is released. */
export class ImageBudget {
  private fetching = 0;
  private memory = 0;
  private readonly fetches: Waiter[] = [];
  private readonly reservations: Waiter[] = [];

  constructor(readonly limits: { fetches: number; memory: number }) {}

  /** A fetch slot; call the returned function to give it back. */
  fetchSlot(signal: AbortSignal): Promise<() => void> {
    return this.enter(this.fetches, () => this.fetching < this.limits.fetches, (n) => { this.fetching += n; }, signal);
  }

  /** Reserves `amount` bytes of image memory until the returned function is called. */
  reserve(amount: number, signal: AbortSignal): Promise<() => void> {
    if (!(amount >= 0 && amount <= this.limits.memory)) return Promise.reject(new Error("Image exceeds the memory budget"));
    return this.enter(this.reservations, () => this.memory + amount <= this.limits.memory, (n) => { this.memory += n * amount; }, signal);
  }

  get usage() { return { fetching: this.fetching, memory: this.memory }; }

  private enter(queue: Waiter[], fits: () => boolean, add: (n: 1 | -1) => void, signal: AbortSignal): Promise<() => void> {
    return new Promise((resolve, reject) => {
      if (signal.aborted) return reject(signal.reason);
      const abort = () => {
        queue.splice(queue.indexOf(waiter), 1);
        reject(signal.reason);
        this.pump();
      };
      const waiter: Waiter = {
        fits,
        grant: () => {
          signal.removeEventListener("abort", abort);
          add(1);
          let held = true;
          resolve(() => { if (held) { held = false; add(-1); this.pump(); } });
        },
      };
      signal.addEventListener("abort", abort, { once: true });
      queue.push(waiter);
      this.pump();
    });
  }

  private pump() {
    for (const queue of [this.fetches, this.reservations]) while (queue[0]?.fits()) queue.shift()!.grant();
  }
}

/** Decoded RGBA pixels plus the encoded bytes kept for the object URL. */
export const imageMemory = (width: number, height: number, bytes: number) => width * height * 4 + bytes;

export const imageBudget = new ImageBudget({ fetches: 3, memory: 256 * 1024 * 1024 });
