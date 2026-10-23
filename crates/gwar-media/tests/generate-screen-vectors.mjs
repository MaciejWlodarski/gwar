// Reproduce with: node crates/gwar-media/tests/generate-screen-vectors.mjs
// Independent WebCrypto oracle; both Rust and the existing TS suite verify every byte.
// All keys are public test data, NOT actual MLS exports. Exporter label/gates are tested in gwar-mls.
import { webcrypto, createHash } from "node:crypto";
import { writeFileSync } from "node:fs";
const { subtle } = webcrypto;
const hex = bytes => Buffer.from(bytes).toString("hex");
const concat = (...parts) => Buffer.concat(parts);
function header(kid, ctr) {
  const integer = v => {
    if (v < 8n) return [];
    const s = v.toString(16);
    return [...Buffer.from(s.length % 2 ? `0${s}` : s, "hex")];
  };
  const k = integer(kid), c = integer(ctr);
  return Buffer.from([(k.length ? 0x80 | ((k.length - 1) << 4) : Number(kid) << 4)
    | (c.length ? 8 | (c.length - 1) : Number(ctr)), ...k, ...c]);
}
async function seal(base, kid, ctr, clear, rest, h = header(kid, ctr)) {
  const secret = await subtle.importKey("raw", base, "HKDF", false, ["deriveBits"]);
  async function derive(name, size) {
    const tail = Buffer.alloc(10); tail.writeBigUInt64BE(kid); tail.writeUInt16BE(4, 8);
    const info = concat(Buffer.from(`SFrame 1.0 Secret ${name} `), tail);
    return Buffer.from(await subtle.deriveBits({ name: "HKDF", hash: "SHA-256", salt: Buffer.alloc(0), info }, secret, size * 8));
  }
  const key = await subtle.importKey("raw", await derive("key", 16), "AES-GCM", false, ["encrypt"]);
  const iv = await derive("salt", 12); iv.writeBigUInt64BE(iv.readBigUInt64BE(4) ^ ctr, 4);
  const ct = await subtle.encrypt({ name: "AES-GCM", iv, additionalData: concat(h, clear), tagLength: 128 }, key, rest);
  return concat(clear, h, Buffer.from(ct));
}
const values = [0n, 8n, ...Array.from({ length: 7 }, (_, i) => 1n << BigInt((i + 1) * 8)), (1n << 64n) - 1n];
const frames = [], rejected = [];
for (const codec of ["vp9", "vp8"]) for (const type of ["key", "delta"]) {
  for (const kid of values) for (const ctr of values) {
    const id = `${codec}/${type}/${kid}/${ctr}`;
    const base = createHash("sha256").update(`public screen vector ${id}`).digest();
    // Synthetic encoded-byte fixtures: correct key/delta header bits, opaque coded tails.
    // Decoder/packetizer interoperability is a separate acceptance gate.
    const bytes = codec === "vp9"
      ? (type === "key" ? "824983420027f800efff00010203ff0080" : "860000deadbeef00010203ff0080")
      : (type === "key" ? "1000009d012a8002e00100010203ff0080" : "110000deadbeef00010203ff0080");
    const pt = Buffer.from(bytes, "hex");
    const clear = codec === "vp9" ? 0 : type === "key" ? 10 : 3;
    const wire = await seal(base, kid, ctr, pt.subarray(0, clear), pt.subarray(clear));
    frames.push({ id, codec, type, kid: String(kid), ctr: String(ctr), base_key: hex(base), plaintext: hex(pt), clear, wire: hex(wire) });
  }
}
// Noncanonical headers with valid raw-header authentication tags, plus their canonical forms.
for (const codec of ["vp9", "vp8"]) for (const h of ["8000", "0800", "900008", "090008"]) {
  const kid = h === "900008" ? 8n : 0n, ctr = h === "090008" ? 8n : 0n;
  const base = createHash("sha256").update(`public noncanonical ${codec}/${h}`).digest();
  const pt = Buffer.from("110000aabbcc", "hex"), clear = codec === "vp8" ? 3 : 0;
  const args = [base, kid, ctr, pt.subarray(0, clear), pt.subarray(clear)];
  rejected.push({ id: `${codec}/nonminimal/${h}`, codec, kid: String(kid), ctr: String(ctr), base_key: hex(base), plaintext: hex(pt), clear,
    wire: hex(await seal(...args, Buffer.from(h, "hex"))), canonical_wire: hex(await seal(...args)) });
}
const result = { source: "Independent Node WebCrypto oracle; AES_128_GCM_SHA256_128; public synthetic base keys", frames, rejected };
writeFileSync(new URL("./screen-sframe-vectors.json", import.meta.url), `${JSON.stringify(result, null, 2)}\n`);
console.log(`${frames.length} accepted and ${rejected.length} authenticated noncanonical vectors`);
