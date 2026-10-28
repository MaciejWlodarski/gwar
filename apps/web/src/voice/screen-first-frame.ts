/**
 * The publisher's first-frame guard (docs/screen-share.md, "First encoded frame"): a transport
 * can connect while the engine never creates an encoder, so `outbound-rtp.framesEncoded` stays
 * zero and nothing reaches the SFU. These helpers judge the stats and rewrite the server's
 * answer for the guard's in-place renegotiation.
 */

export type SendCodec = "av1" | "h264" | "vp9";

/**
 * How long a connected publish transport with a live, unmuted track may go without encoding a
 * frame. Measured first frames come within a second of connecting, CPU-bound encoders included.
 */
export const FIRST_ENCODE_TIMEOUT_MS = 5_000;

export function codecFamily(name: string | undefined): SendCodec | null {
  const codec = name?.toLowerCase().replace(/^video\//, "");
  return codec === "av1" || codec === "h264" || codec === "vp9" ? codec : null;
}

/** The video codecs of an answer, in payload order: the engine sends the first one. */
export function answerCodecs(sdp: string): SendCodec[] {
  const section = sdp.split(/(?=^m=)/m).find(s => /^m=video /.test(s));
  if (!section) return [];
  const names = new Map([...section.matchAll(/^a=rtpmap:(\d+) ([^/\s]+)\//gm)].map(m => [m[1]!, m[2]!]));
  const order = /^m=video \S+ \S+((?: \d+)*)/.exec(section)?.[1]?.trim().split(" ") ?? [];
  return [...new Set(order.flatMap(pt => codecFamily(names.get(pt)) ?? []))];
}

/** The answer with only `codec`'s payload types and their RTX in its video section; null without one. */
export function restrictAnswer(sdp: string, codec: SendCodec): string | null {
  const eol = sdp.includes("\r\n") ? "\r\n" : "\n";
  let found = false;
  const sections = sdp.split(/(?=^m=)/m).map(section => {
    if (!/^m=video /.test(section)) return section;
    const names = new Map([...section.matchAll(/^a=rtpmap:(\d+) ([^/\s]+)\//gm)].map(m => [m[1]!, m[2]!.toLowerCase()]));
    const keep = new Set([...names].filter(([, name]) => codecFamily(name) === codec).map(([pt]) => pt));
    if (keep.size === 0) return section;
    found = true;
    for (const m of section.matchAll(/^a=fmtp:(\d+) (?:.*;)?\s*apt=(\d+)/gm)) if (names.get(m[1]!) === "rtx" && keep.has(m[2]!)) keep.add(m[1]!);
    return section.split(/\r?\n/).flatMap(line => {
      const media = /^(m=video \S+ \S+)((?: \d+)*)$/.exec(line);
      if (media) return [`${media[1]} ${media[2]!.trim().split(" ").filter(pt => keep.has(pt)).join(" ")}`];
      const pt = /^a=(?:rtpmap|fmtp|rtcp-fb):(\d+) /.exec(line)?.[1];
      return pt && !keep.has(pt) ? [] : [line];
    }).join(eol);
  });
  return found ? sections.join("") : null;
}

/**
 * The plan's next codec after `current`: H.264, then VP9 (or back to H.264 from VP9). AV1 is
 * hardware-only and never a fallback. Null when the answer has no other working codec.
 */
export function fallbackCodec(codecs: readonly SendCodec[], current: SendCodec | null, broken: ReadonlySet<SendCodec>): SendCodec | null {
  return (["h264", "vp9"] as const).find(c => c !== current && !broken.has(c) && codecs.includes(c)) ?? null;
}

/**
 * Whether the sender has encoded anything. Chromium lists an outbound stream only once it sends,
 * so no stream is no frame; an engine without a frame count proves progress by sent packets.
 */
export function encodedAnything(stats: RTCStatsReport): boolean {
  let encoded = false;
  stats.forEach(s => {
    if (s.type !== "outbound-rtp" || (s.kind ?? s.mediaType) !== "video") return;
    const frames = s.framesEncoded as number | undefined, packets = s.packetsSent as number | undefined;
    if (typeof frames === "number" ? frames > 0 : (packets ?? 0) > 0) encoded = true;
  });
  return encoded;
}
