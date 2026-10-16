/** RFC 9605 §4.3 SFrame header: a config byte, then the KID and CTR as
 * minimal big-endian integers. Values are unsigned 64-bit BigInts; never
 * use JS bitwise operators on Numbers for them. */
const U64 = (1n << 64n) - 1n;

export interface Header { kid: bigint; ctr: bigint; length: number }

function minimal(value: bigint): number[] {
  const out: number[] = [];
  for (let v = value; v > 0n; v >>= 8n) out.unshift(Number(v & 0xffn));
  return out;
}

export function encodeHeader(kid: bigint, ctr: bigint): Uint8Array {
  if (kid < 0n || kid > U64 || ctr < 0n || ctr > U64) throw new RangeError("SFrame KID/CTR outside u64");
  const k = kid < 8n ? [] : minimal(kid), c = ctr < 8n ? [] : minimal(ctr);
  const config = (kid < 8n ? Number(kid) << 4 : 0x80 | ((k.length - 1) << 4)) | (ctr < 8n ? Number(ctr) : 0x08 | (c.length - 1));
  return new Uint8Array([config, ...k, ...c]);
}

/** Decodes a header at the start of `data`; null for a truncated or
 * non-minimal encoding (an extended value below 8 or with a leading zero). */
export function decodeHeader(data: Uint8Array): Header | null {
  if (data.length < 1) return null;
  const config = data[0]!;
  let offset = 1;
  const field = (extended: boolean, bits: number): bigint | null => {
    if (!extended) return BigInt(bits);
    const length = bits + 1;
    if (offset + length > data.length || (length > 1 && data[offset] === 0)) return null;
    let value = 0n;
    for (let i = 0; i < length; i++) value = (value << 8n) | BigInt(data[offset + i]!);
    offset += length;
    return value < 8n ? null : value;
  };
  const kid = field((config & 0x80) !== 0, (config >> 4) & 7);
  if (kid === null) return null;
  const ctr = field((config & 0x08) !== 0, config & 7);
  if (ctr === null) return null;
  return { kid, ctr, length: offset };
}
