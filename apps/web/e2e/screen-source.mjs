/**
 * Screen sources for the e2e runs (`screen.mjs`, `native-voice.mjs`): frames stamped with a
 * barcode, the reader that gets it back from a decoded video, and the test hook that stands in
 * for the picker with a canvas. The functions run in the page (as init scripts), so each is
 * self-contained.
 */

/**
 * Draws the source's frames: a 20-cell barcode across the top quarter (4 bits of source id, a
 * 12-bit frame counter and a 4-bit check), and a moving block so every frame changes.
 */
export function stampBarcode(canvas, id) {
  const ctx = canvas.getContext("2d");
  let counter = 0;
  const draw = () => {
    const w = canvas.width, h = canvas.height;
    counter = (counter + 1) & 4095;
    ctx.fillStyle = "#777";
    ctx.fillRect(0, 0, w, h);
    ctx.fillStyle = `hsl(${(counter * 7) % 360} 70% 50%)`;
    ctx.fillRect((counter * 9) % w, h / 2, w / 8, h / 4);
    const check = (id ^ counter ^ (counter >> 4) ^ (counter >> 8) ^ 10) & 15;
    const bits = [];
    for (let i = 3; i >= 0; i--) bits.push((id >> i) & 1);
    for (let i = 11; i >= 0; i--) bits.push((counter >> i) & 1);
    for (let i = 3; i >= 0; i--) bits.push((check >> i) & 1);
    bits.forEach((bit, i) => { ctx.fillStyle = bit ? "#000" : "#fff"; ctx.fillRect((i * w) / 20, 0, w / 20 + 1, h / 4); });
  };
  draw();
  setInterval(draw, 50);
}

/** Reads the barcode back from a decoded video: `{id, counter}`, `{invalid}` or null without a frame. */
export function readBarcode(video) {
  const w = video?.videoWidth, h = video?.videoHeight;
  if (!w || !h) return null;
  const canvas = document.createElement("canvas");
  canvas.width = w;
  canvas.height = h;
  const ctx = canvas.getContext("2d", { willReadFrequently: true });
  ctx.drawImage(video, 0, 0, w, h);
  const y = Math.floor(h / 8);
  const bits = [];
  for (let i = 0; i < 20; i++) {
    const x = Math.floor(((i + 0.5) * w) / 20);
    const d = ctx.getImageData(x - 2, y - 2, 5, 5).data;
    let sum = 0;
    for (let k = 0; k < d.length; k += 4) sum += d[k] + d[k + 1] + d[k + 2];
    bits.push(sum / (d.length / 4) / 3 < 128 ? 1 : 0);
  }
  const num = (from, n) => bits.slice(from, from + n).reduce((v, b) => v * 2 + b, 0);
  const id = num(0, 4), counter = num(4, 12), check = num(16, 4);
  return ((id ^ counter ^ (counter >> 4) ^ (counter >> 8) ^ 10) & 15) === check ? { id, counter, width: w, height: h } : { invalid: true };
}

/**
 * The test hook: `getDisplayMedia` answers with a canvas stamped with a source `{id, size}`. Each
 * share takes the next source (the last one repeats), so a share that follows another is told
 * apart by its frames.
 */
export function canvasCapture(sources, stamp) {
  let next = 0;
  navigator.mediaDevices.getDisplayMedia = async () => {
    const { id, size } = sources[Math.min(next++, sources.length - 1)];
    const canvas = document.createElement("canvas");
    [canvas.width, canvas.height] = size;
    canvas.style.cssText = "position:fixed;left:0;bottom:0;width:64px;height:36px;opacity:0.02;pointer-events:none;z-index:-1";
    document.body.appendChild(canvas);
    stamp(canvas, id);
    return canvas.captureStream(20);
  };
}
