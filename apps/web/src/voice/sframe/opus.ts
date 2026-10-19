/**
 * Complete Opus packet framing validation without decoding (RFC 6716 §3.2),
 * the same rules as `gwar-proto::opus::packet_samples`.
 */

/** Samples per channel at 48 kHz, only after validating every frame boundary.
 * Zero-byte frames are legal DTX/loss frames; an empty packet is not. */
export function packetSamples(packet: Uint8Array): number | null {
  if (packet.length === 0) return null;
  const toc = packet[0]!;
  let start = 1, end = packet.length;
  const config = toc >> 3;
  const frame = config <= 11 ? [480, 960, 1920, 2880][config & 3]! : config <= 15 ? [480, 960][config & 1]! : [120, 240, 480, 960][config & 3]!;
  let frames: number, vbr: boolean;
  switch (toc & 3) {
    case 0: frames = 1; vbr = false; break;
    case 1: frames = 2; vbr = false; break;
    case 2: frames = 2; vbr = true; break;
    default: {
      if (start >= end) return null;
      const control = packet[start++]!;
      if (control & 0x40) {
        let padding = 0;
        for (;;) {
          if (start >= end) return null;
          const byte = packet[start++]!;
          // 255 contributes 254 bytes and requires another length byte.
          padding += byte === 255 ? 254 : byte;
          if (byte !== 255) break;
        }
        if (padding > end - start) return null;
        end -= padding;
      }
      frames = control & 0x3f; vbr = (control & 0x80) !== 0;
    }
  }
  const samples = frame * frames;
  if (frames === 0 || samples > 5760) return null;
  if (vbr) {
    let used = 0;
    for (let i = 1; i < frames; i++) {
      if (start >= end) return null;
      const first = packet[start++]!;
      let length = first;
      if (first >= 252) {
        if (start >= end) return null;
        length = first + 4 * packet[start++]!;
      }
      if (length > 1275) return null;
      used += length;
    }
    // Length bytes all precede frame data; the last frame is implicit.
    const body = end - start;
    if (body < used || body - used > 1275) return null;
  } else {
    const body = end - start;
    if (body % frames !== 0 || body / frames > 1275) return null;
  }
  return samples;
}
