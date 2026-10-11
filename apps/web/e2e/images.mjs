/**
 * Runs image fixtures through the real decoders of Chromium and WebKit: whatever the
 * header parser accepts must never decode larger than the header it read, and inputs
 * that a real decoder was seen sizing differently must be refused before decoding.
 */
import assert from "node:assert/strict";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium, webkit } from "playwright";
import { createServer } from "vite";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

/** A GIF whose frames are [left, top, width, height]; every pixel is colour 0. */
function gif(screen, frames) {
  const b = [...Buffer.from("GIF89a")];
  const u16 = (n) => b.push(n & 255, n >> 8);
  u16(screen[0]); u16(screen[1]); b.push(0x80, 0, 0, 0, 0, 0, 255, 255, 255);
  for (const [left, top, w, h] of frames) {
    b.push(0x21, 0xf9, 4, 0, 10, 0, 0, 0);
    b.push(0x2c); u16(left); u16(top); u16(w); u16(h); b.push(0);
    // LZW with 3-bit codes: a clear code before every pixel keeps the code size fixed.
    const codes = [];
    for (let i = 0; i < w * h; i++) codes.push(4, 0);
    codes.push(5);
    const data = []; let acc = 0, bits = 0;
    for (const c of codes) { acc |= c << bits; bits += 3; while (bits >= 8) { data.push(acc & 255); acc >>= 8; bits -= 8; } }
    if (bits) data.push(acc & 255);
    b.push(2);
    for (let i = 0; i < data.length; i += 255) { const chunk = data.slice(i, i + 255); b.push(chunk.length, ...chunk); }
    b.push(0);
  }
  b.push(0x3b);
  return Buffer.from(b);
}

const sofAt = (jpeg) => { for (let i = 2; i < jpeg.length - 1; i++) if (jpeg[i] === 0xff && jpeg[i + 1] === 0xc0) return i; throw new Error("no SOF0"); };
const sofSegment = (jpeg) => { const i = sofAt(jpeg); return jpeg.subarray(i, i + 2 + jpeg.readUInt16BE(i + 2)); };
const riff = (chunks) => {
  const body = Buffer.concat([Buffer.from("WEBP"), ...chunks]);
  const header = Buffer.alloc(8); header.write("RIFF", 0); header.writeUInt32LE(body.length, 4);
  return Buffer.concat([header, body]);
};
/** The lossy `VP8 ` chunk of a WebP (canvas encoders may wrap it in VP8X). */
function vp8Chunk(webp) {
  for (let at = 12; at + 8 <= webp.length;) {
    const size = webp.readUInt32LE(at + 4), end = at + 8 + size + (size & 1);
    if (webp.toString("latin1", at, at + 4) === "VP8 ") return webp.subarray(at, end);
    at = end;
  }
  throw new Error("no VP8 chunk");
}
/** A VP8 chunk in a VP8X container claiming a `width`x`height` canvas. */
function vp8x(chunk, width, height) {
  const x = Buffer.alloc(18); x.write("VP8X", 0); x.writeUInt32LE(10, 4);
  x.writeUIntLE(width - 1, 12, 3); x.writeUIntLE(height - 1, 15, 3);
  return riff([x, chunk]);
}
/** A JPEG with an Exif orientation (6 and 8 turn it a quarter, swapping its sides). */
function oriented(jpeg, orientation) {
  const tiff = Buffer.from([0x4d, 0x4d, 0, 42, 0, 0, 0, 8, 0, 1, 0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, orientation, 0, 0, 0, 0, 0, 0]);
  const app1 = Buffer.concat([Buffer.from([0xff, 0xe1, 0, 2 + 6 + tiff.length]), Buffer.from("Exif\0\0", "latin1"), tiff]);
  return Buffer.concat([jpeg.subarray(0, 2), app1, jpeg.subarray(2)]);
}

const server = await createServer({ root, configFile: path.join(root, "vite.config.ts"), logLevel: "error", server: { port: 0, host: "127.0.0.1" } });
await server.listen();
const base = server.resolvedUrls.local[0];
const browsers = [];
let passed = 0;
try {
  const encoder = await chromium.launch();
  browsers.push(encoder);
  const encoderPage = await encoder.newPage();
  const encode = async (w, h, type) => Buffer.from(await encoderPage.evaluate(async ([w, h, type]) => {
    const canvas = new OffscreenCanvas(w, h); const context = canvas.getContext("2d");
    context.fillStyle = "#3a6"; context.fillRect(0, 0, w, h);
    const blob = await canvas.convertToBlob({ type, quality: 0.9 });
    return btoa(String.fromCharCode(...new Uint8Array(await blob.arrayBuffer())));
  }, [w, h, type]), "base64");
  const png = await encode(300, 200, "image/png");
  const jpeg = await encode(300, 200, "image/jpeg"), smallJpeg = await encode(16, 16, "image/jpeg");
  const webp = await encode(300, 200, "image/webp");
  const eoi = smallJpeg.length - 2, smallSof = sofSegment(smallJpeg), smallSofEnd = sofAt(smallJpeg) + smallSof.length;
  const vp8 = vp8Chunk(webp);
  // `cwebp -lossless -z 9` of a transparent 1x1 PNG: 28 bytes.
  const tinyLossless = Buffer.from("UklGRhQAAABXRUJQVlA4TAgAAAAvAAAAEIiICA==", "base64");

  // [name, bytes, mime, expected]: "ok" decodes through boundedImageBlob, "refused" never reaches a decoder,
  // "bounded" may decode or not but never beyond its header.
  const fixtures = [
    ["canvas PNG", png, "image/png", "ok"],
    ["canvas JPEG", jpeg, "image/jpeg", "ok"],
    ["canvas WebP", webp, "image/webp", "ok"],
    ["plain VP8 WebP", riff([vp8]), "image/webp", "ok"],
    ["VP8X WebP with its own canvas", vp8x(vp8, 300, 200), "image/webp", "ok"],
    // Accepted by the header parser; WebKit on Linux declines to decode it, which is its call.
    ["28-byte lossless WebP", tinyLossless, "image/webp", "bounded"],
    ["JPEG turned by Exif (6)", oriented(jpeg, 6), "image/jpeg", "ok"],
    ["JPEG turned by Exif (8)", oriented(jpeg, 8), "image/jpeg", "ok"],
    ["animated GIF, frames inside", gif([30, 20], [[0, 0, 30, 20], [5, 5, 10, 10]]), "image/gif", "ok"],
    ["GIF frame larger than its screen", gif([10, 10], [[0, 0, 300, 200]]), "image/gif", "refused"],
    ["GIF frame offset right past its screen", gif([10, 10], [[5, 0, 10, 10]]), "image/gif", "refused"],
    ["GIF frame offset down past its screen", gif([10, 10], [[0, 5, 10, 10]]), "image/gif", "refused"],
    ["GIF second frame past its screen", gif([30, 20], [[0, 0, 30, 20], [20, 0, 30, 20]]), "image/gif", "refused"],
    ["JPEG small SOF before the real one", Buffer.concat([jpeg.subarray(0, 2), Buffer.from([0xff, 0xc0, 0, 17, 8, 0, 16, 0, 16, 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]), jpeg.subarray(2)]), "image/jpeg", "refused"],
    ["JPEG large SOF after the real one", Buffer.concat([smallJpeg.subarray(0, smallSofEnd), sofSegment(jpeg), smallJpeg.subarray(smallSofEnd)]), "image/jpeg", "refused"],
    ["JPEG large SOF after the scan", Buffer.concat([smallJpeg.subarray(0, eoi), sofSegment(jpeg), smallJpeg.subarray(eoi)]), "image/jpeg", "bounded"],
    ["JPEG with a large JPEG appended", Buffer.concat([smallJpeg, jpeg]), "image/jpeg", "bounded"],
    ["WebP VP8X canvas smaller than its VP8", vp8x(vp8, 10, 10), "image/webp", "bounded"],
  ];

  for (const [engineName, engine] of [["chromium", chromium], ["webkit", webkit]]) {
    const browser = await engine.launch();
    browsers.push(browser);
    const page = await browser.newPage();
    await page.goto(base);
    for (const [name, bytes, mime, expected] of fixtures) {
      const result = await page.evaluate(async ([b64, mime]) => {
        const { boundedImageBlob, imageDimensions } = await import("/src/net/attachment.ts");
        const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
        const blob = new Blob([bytes], { type: mime });
        let header;
        try { header = imageDimensions(bytes); } catch (error) { return { refused: String(error) }; }
        let decoded = null;
        try { const bitmap = await createImageBitmap(blob); decoded = [bitmap.width, bitmap.height]; bitmap.close(); } catch { /* the decoder may refuse it too */ }
        let bounded;
        try {
          const out = await boundedImageBlob(blob);
          const bitmap = await createImageBitmap(out); bounded = [bitmap.width, bitmap.height]; bitmap.close();
        } catch (error) { bounded = String(error); }
        return { header, decoded, bounded };
      }, [bytes.toString("base64"), mime]);
      const label = `${engineName}: ${name}`;
      // What a decoder allocates is its pixel count; Exif orientation may swap the sides.
      const within = ([dw, dh], [w, h]) => dw * dh <= w * h && Math.max(dw, dh) <= Math.max(w, h);
      if (expected === "refused") assert.ok(result.refused, `${label} reached a decoder: ${JSON.stringify(result)}`);
      else {
        assert.ok(!result.refused, `${label} was refused: ${result.refused}`);
        if (result.decoded) assert.ok(within(result.decoded, result.header), `${label} decodes ${result.decoded} from a ${result.header} header`);
        if (expected === "ok") {
          assert.ok(result.decoded, `${label} did not decode: ${JSON.stringify(result)}`);
          assert.deepEqual(result.bounded, result.decoded, `${label}: ${JSON.stringify(result)}`);
        } else if (Array.isArray(result.bounded)) assert.ok(within(result.bounded, result.header), label);
      }
      passed++;
      console.log(`PASS ${label} (${expected})`);
    }
  }
  console.log(`${passed} image checks passed`);
} finally {
  await Promise.all(browsers.map((browser) => browser.close()));
  await server.close();
}
