/** Counts only time the person can read a toast. Sticky notices never create a timer. */
export function toastTimer(duration: number, expire: () => void) {
  let remaining = duration;
  let started = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const pause = () => {
    if (timer === undefined) return;
    clearTimeout(timer);
    timer = undefined;
    remaining = Math.max(0, remaining - (Date.now() - started));
  };
  return {
    pause,
    resume() {
      if (timer !== undefined) return;
      started = Date.now();
      timer = setTimeout(expire, remaining);
    },
  };
}
