import { describe, expect, it } from "vitest";
import vectors from "../../../../../crates/gwar-client/tests/sframe-vectors.json";
import interop from "../../../../../crates/gwar-client/tests/sframe-interop.json";
import { decodeHeader, encodeHeader } from "./header";
import { Replay } from "./replay";
import { voiceKid } from "./state";
import { deriveKey, nonce, open, seal, SUITE } from "./suite";

const hex = (s: string) => new Uint8Array(s.match(/../g)!.map(b => parseInt(b, 16)));
const toHex = (b: Uint8Array) => [...b].map(x => x.toString(16).padStart(2, "0")).join("");

describe("SFrame header (RFC 9605 C.1)", () => {
  it("encodes and decodes every vector", () => {
    expect(vectors.headers.length).toBe(289);
    for (const v of vectors.headers) {
      expect(toHex(encodeHeader(BigInt(v.kid), BigInt(v.ctr))), `${v.kid}/${v.ctr}`).toBe(v.header);
      const bytes = hex(v.header);
      expect(decodeHeader(new Uint8Array([...bytes, 0xaa, 0xbb]))).toEqual({ kid: BigInt(v.kid), ctr: BigInt(v.ctr), length: bytes.length });
    }
  });
  it("covers the shortest and widest encodings", () => {
    expect(encodeHeader(0n, 0n)).toEqual(new Uint8Array([0]));
    const max = (1n << 64n) - 1n;
    expect(encodeHeader(max, max).length).toBe(17);
    expect(() => encodeHeader(max + 1n, 0n)).toThrow();
    expect(() => encodeHeader(0n, -1n)).toThrow();
  });
  it("rejects truncated and non-minimal headers", () => {
    for (const bytes of [[], [0x80], [0x08], [0x81, 0x01], [0x88, 0x07], [0x08, 0x07], [0x09, 0x00, 0xff], [0x90, 0x00, 0x08], [0x8f, 0x08]]) {
      expect(decodeHeader(new Uint8Array(bytes)), JSON.stringify(bytes)).toBeNull();
    }
  });
});

describe("AES_128_GCM_SHA256_128 (RFC 9605 C.3, non-empty metadata)", () => {
  const v = vectors.encryption[0]!;
  it("matches the vector, including the header + metadata AAD order", async () => {
    expect(v.cipher_suite).toBe(SUITE);
    const kid = BigInt(v.kid), ctr = BigInt(v.ctr);
    const enc = await deriveKey(kid, hex(v.base_key), "encrypt");
    expect(toHex(enc.salt)).toBe(v.sframe_salt);
    expect(toHex(nonce(enc.salt, ctr))).toBe(v.nonce);
    const ct = await seal(enc, kid, ctr, hex(v.metadata), hex(v.pt));
    expect(toHex(ct)).toBe(v.ct);
    const dec = await deriveKey(kid, hex(v.base_key), "decrypt");
    const header = decodeHeader(ct)!;
    expect(toHex((await open(dec, ct.subarray(0, header.length), header.ctr, hex(v.metadata), ct.slice(header.length)))!)).toBe(v.pt);
    // Swapping metadata and header in the AAD (the sframe-rs #124 bug) fails.
    expect(await open(dec, hex(v.metadata), header.ctr, ct.subarray(0, header.length), ct.slice(header.length))).toBeNull();
  });
  it("authenticates every header, ciphertext and tag byte", async () => {
    const kid = BigInt(v.kid), ctr = BigInt(v.ctr);
    const dec = await deriveKey(kid, hex(v.base_key), "decrypt");
    const ct = hex(v.ct);
    for (let i = 0; i < ct.length; i++) {
      const bad = ct.slice(); bad[i]! ^= 1;
      const header = decodeHeader(bad);
      const opened = header && await open(dec, bad.subarray(0, header.length), header.ctr, hex(v.metadata), bad.slice(header.length));
      expect(opened ?? null, `byte ${i}`).toBeNull();
    }
    expect(await open(dec, ct.subarray(0, 5), ctr, new Uint8Array(), ct.slice(5))).toBeNull(); // metadata missing
  });
  it("keeps keys one-directional", async () => {
    const enc = await deriveKey(1n, hex(v.base_key), "encrypt"), dec = await deriveKey(1n, hex(v.base_key), "decrypt");
    const ct = await seal(enc, 1n, 9n, new Uint8Array(), new Uint8Array([1, 2, 3]));
    await expect(seal(dec, 1n, 10n, new Uint8Array(), new Uint8Array([1]))).rejects.toThrow();
    expect(await open(enc, ct.subarray(0, 2), 9n, new Uint8Array(), ct.slice(2))).toBeNull();
    expect(await open(dec, ct.subarray(0, 2), 9n, new Uint8Array(), ct.slice(2))).toEqual(new Uint8Array([1, 2, 3]));
  });
});

describe("Native interop (crates/gwar-client/tests/sframe-interop.json)", () => {
  it("seals the same bytes as the Rust engine and opens its frames", async () => {
    expect(interop.frames.length).toBe(4);
    for (const f of interop.frames) {
      const kid = voiceKid(BigInt(f.epoch), f.leaf), ctr = BigInt(f.ctr);
      const enc = await deriveKey(kid, hex(f.base_key), "encrypt");
      expect(toHex(await seal(enc, kid, ctr, new Uint8Array(), hex(f.pt))), f.epoch).toBe(f.frame);
      const dec = await deriveKey(kid, hex(f.base_key), "decrypt");
      const frame = hex(f.frame), header = decodeHeader(frame)!;
      expect(header).toMatchObject({ kid, ctr });
      expect(toHex((await open(dec, frame.subarray(0, header.length), header.ctr, new Uint8Array(), frame.slice(header.length)))!)).toBe(f.pt);
    }
  });
});

describe("Replay window", () => {
  it("admits each counter once within 128 of the highest", () => {
    const r = new Replay();
    expect(r.admit(1000n)).toBe(true);
    expect(r.admit(1000n)).toBe(false);
    expect(r.fresh(873n)).toBe(true); // 127 behind
    expect(r.fresh(872n)).toBe(false); // 128 behind
    expect(r.admit(873n)).toBe(true);
    expect(r.admit(873n)).toBe(false);
    expect(r.admit(1001n)).toBe(true);
    expect(r.fresh(873n)).toBe(false); // already set, now 128 behind
    expect(r.admit(1200n)).toBe(true); // jump past the window clears it
    expect(r.fresh(1001n)).toBe(false);
    expect(r.fresh(1073n)).toBe(true);
    expect(r.admit((1n << 64n) - 1n)).toBe(true);
    expect(r.fresh((1n << 64n) - 2n)).toBe(true);
  });
  it("fresh never mutates", () => {
    const r = new Replay();
    r.admit(5n);
    for (let i = 0; i < 3; i++) expect(r.fresh(4n)).toBe(true);
    expect(r.admit(4n)).toBe(true);
  });
});
