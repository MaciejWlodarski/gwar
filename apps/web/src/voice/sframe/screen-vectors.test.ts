import { describe, expect, it } from "vitest";
import vectors from "../../../../../crates/gwar-media/tests/screen-sframe-vectors.json";
import { decodeHeader } from "./header";
import { deriveKey, open, seal, type SuiteKey } from "./suite";

// This revision has no screen worker. Exercise the documented screen split using the
// unchanged, deployed WebCrypto primitives; do not pretend this tests a video transport.
const hex = (s: string) => new Uint8Array(Buffer.from(s, "hex"));
const toHex = (b: Uint8Array) => Buffer.from(b).toString("hex");
const clearLength = (codec: string, data: Uint8Array) => codec === "vp9" ? 0 : (data[0]! & 1) === 0 ? 10 : 3;
const MAX_FRAME = 2 * 1024 * 1024;
async function openFrame(codec: string, key: SuiteKey, wire: Uint8Array<ArrayBuffer>) {
  const clear = clearLength(codec, wire);
  if (wire.length < clear || wire.length > MAX_FRAME + 33) return null;
  const body = wire.slice(clear), header = decodeHeader(body);
  if (!header || body.length < header.length + 16 || wire.length - header.length - 16 > MAX_FRAME) return null;
  const rest = await open(key, body.subarray(0, header.length), header.ctr, wire.subarray(0, clear), body.slice(header.length));
  return rest && new Uint8Array([...wire.subarray(0, clear), ...rest]);
}

describe("VP9/VP8 shared Rust/WebCrypto screen wire contract", () => {
  it("seals and opens identical bytes at every KID/CTR width on key and delta inputs", async () => {
    expect(vectors.frames).toHaveLength(400);
    for (const v of vectors.frames) {
      const kid = BigInt(v.kid), ctr = BigInt(v.ctr), pt = hex(v.plaintext);
      const enc = await deriveKey(kid, hex(v.base_key), "encrypt");
      const clear = clearLength(v.codec, pt);
      expect(clear).toBe(v.clear);
      const body = await seal(enc, kid, ctr, pt.subarray(0, clear), pt.slice(clear));
      expect(toHex(new Uint8Array([...pt.subarray(0, clear), ...body])), v.id).toBe(v.wire);
      const dec = await deriveKey(kid, hex(v.base_key), "decrypt");
      expect(toHex((await openFrame(v.codec, dec, hex(v.wire)))!), v.id).toBe(v.plaintext);
    }
  });

  it("rejects authenticated nonminimal headers and accepts their explicit canonical forms", async () => {
    expect(vectors.rejected).toHaveLength(8);
    for (const v of vectors.rejected) {
      const dec = await deriveKey(BigInt(v.kid), hex(v.base_key), "decrypt");
      const wire = hex(v.wire), body = wire.slice(v.clear);
      expect(decodeHeader(body), v.id).toBeNull();
      expect(await openFrame(v.codec, dec, wire), v.id).toBeNull();
      expect(toHex((await openFrame(v.codec, dec, hex(v.canonical_wire)))!), v.id).toBe(v.plaintext);
      // Prove this really is authenticated noncanonical data, not simply a bad tag.
      const hlen = 1 + ((body[0]! & 0x80) ? ((body[0]! >> 4) & 7) + 1 : 0)
        + ((body[0]! & 8) ? (body[0]! & 7) + 1 : 0);
      const pt = await open(dec, body.subarray(0, hlen), BigInt(v.ctr), wire.subarray(0, v.clear), body.slice(hlen));
      expect(toHex(new Uint8Array([...wire.subarray(0, v.clear), ...pt!])), v.id).toBe(v.plaintext);
    }
  });

  it("authenticates every bit, including VP8's frame-type bit and entire clear prefix", async () => {
    for (const v of vectors.frames.filter(v => v.kid === v.ctr && ["0", "18446744073709551615"].includes(v.kid))) {
      const dec = await deriveKey(BigInt(v.kid), hex(v.base_key), "decrypt"), wire = hex(v.wire);
      for (let i = 0; i < wire.length; i++) for (let bit = 0; bit < 8; bit++) {
        const bad = wire.slice(); bad[i]! ^= 1 << bit;
        expect(await openFrame(v.codec, dec, bad), `${v.id} byte ${i} bit ${bit}`).toBeNull();
      }
      for (let length = 0; length < wire.length; length++) {
        expect(await openFrame(v.codec, dec, wire.slice(0, length)), `${v.id} length ${length}`).toBeNull();
      }
      const wrongKey = await deriveKey(BigInt(v.kid), new Uint8Array(32).fill(255), "decrypt");
      const wrongEpoch = await deriveKey(BigInt(v.kid) ^ (1n << 32n), hex(v.base_key), "decrypt");
      expect(await openFrame(v.codec, wrongKey, wire)).toBeNull();
      expect(await openFrame(v.codec, wrongEpoch, wire)).toBeNull();
    }
  });
});
