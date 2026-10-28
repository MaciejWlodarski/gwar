/** A disposable performance hint, local to this device/browser profile; no migration. */
export const SCREEN_UPLINK_KEY = "gwar.screen-uplink.v1";

/** An initial GCC estimate, never a minimum sending rate or a replacement for congestion control. */
export function screenStartBitrate(cap: number): number {
  let remembered: unknown;
  try { remembered = JSON.parse(globalThis.localStorage?.getItem(SCREEN_UPLINK_KEY) ?? "null"); } catch { /* Storage can be refused. */ }
  return Math.min(cap, typeof remembered === "number" && Number.isFinite(remembered) && remembered > 0
    ? Math.max(1_000_000, Math.floor(remembered * 0.8)) : 4_000_000);
}

/** Remember a ten-second stable tail only after twenty seconds on this publish transport. */
export class ScreenUplinkHistory {
  private samples: { at: number; available: number }[] = [];
  private saved: number | undefined;
  private readonly started: number;
  constructor(started: number) { this.started = started; }

  observe(now: number, available: number | undefined): void {
    const last = this.samples.at(-1);
    if (available === undefined || !Number.isFinite(available) || available <= 0 ||
      (last && (now <= last.at || now - last.at > 4_500))) this.samples = [];
    if (available === undefined || !Number.isFinite(available) || available <= 0) return;
    this.samples.push({ at: now, available });
    // Keep the sample at/before the window boundary, including jitter in the two-second poll.
    while (this.samples.length > 1 && this.samples[1]!.at <= now - 10_000) this.samples.shift();
    if (now - this.started < 20_000 || now - this.samples[0]!.at < 10_000) return;
    const values = this.samples.map(s => s.available).sort((a, b) => a - b);
    if (values.at(-1)! > values[0]! * 1.25) return;
    const middle = Math.floor(values.length / 2);
    const median = Math.round((values[middle]! + values[Math.floor((values.length - 1) / 2)]!) / 2);
    if (median === this.saved) return;
    try { globalThis.localStorage?.setItem(SCREEN_UPLINK_KEY, JSON.stringify(median)); this.saved = median; } catch { /* A hint must never block a share. */ }
  }
}

/** Chromium reads the start hint from its remote answer; leave audio, RTX and other fmtp intact. */
export function withScreenStartBitrate(sdp: string, bitrate: number, userAgent = globalThis.navigator?.userAgent ?? ""): string {
  // WebKitGTK's GStreamer VP9 sender can negotiate this extra fmtp but encode zero frames.
  // Native macOS WKWebView uses libwebrtc and accepts it. Linux desktop sharing is unsupported,
  // but keep GTK's browser/test transport usable with its original answer and GCC start.
  if (/Linux/i.test(userAgent) && /AppleWebKit/i.test(userAgent) && !/Chrom(?:e|ium)/i.test(userAgent)) return sdp;
  const newline = sdp.includes("\r\n") ? "\r\n" : "\n";
  const hint = `x-google-start-bitrate=${Math.max(1, Math.floor(bitrate / 1000))}`;
  return sdp.split(/(?=^m=)/m).map(section => {
    if (!/^m=video /m.test(section)) return section;
    const codecs = new Set([...section.matchAll(/^a=rtpmap:(\d+) (?:H264|AV1|VP9)\/90000\r?$/gmi)].map(m => m[1]!));
    const fmtp = new Set([...section.matchAll(/^a=fmtp:(\d+) /gm)].map(m => m[1]!));
    return section.split(/\r?\n/).flatMap(line => {
      const match = /^a=fmtp:(\d+) (.*)$/i.exec(line);
      if (match && codecs.has(match[1]!)) {
        const params = match[2]!.split(";").map(p => p.trim()).filter(p => p && !/^x-google-start-bitrate\s*=/i.test(p));
        return [`a=fmtp:${match[1]} ${[...params, hint].join(";")}`];
      }
      const payload = /^a=rtpmap:(\d+) /.exec(line)?.[1];
      return payload && codecs.has(payload) && !fmtp.has(payload) ? [line, `a=fmtp:${payload} ${hint}`] : [line];
    }).join(newline);
  }).join("");
}
