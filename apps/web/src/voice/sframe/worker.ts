/**
 * One dedicated worker per encrypted voice run. It starts closed: without a
 * started state and installed keys every frame is dropped, and an encrypted
 * path never passes plaintext through.
 */
import type { FromWorker, ToWorker, TransformOptions } from "./protocol";
import { SframeState } from "./state";
import { pump, type Transformer } from "./pump";

/** The worker's global scope, without the webworker lib (it would retype the app's globals). */
interface WorkerScope { onmessage: ((event: MessageEvent<ToWorker>) => void) | null; onrtctransform: ((event: { transformer: Transformer }) => void) | null; postMessage(message: FromWorker): void }
const self = globalThis as unknown as WorkerScope;
let state: SframeState | null = null;
let sweep: ReturnType<typeof setInterval> | undefined;
let queueDropped = 0;
const post = (message: FromWorker) => self.postMessage(message);

async function cryptoWorks(): Promise<boolean> {
  try {
    const base = await crypto.subtle.importKey("raw", new Uint8Array(32), "HKDF", false, ["deriveBits"]);
    const bits = await crypto.subtle.deriveBits({ name: "HKDF", hash: "SHA-256", salt: new Uint8Array(0), info: new Uint8Array(1) }, base, 128);
    const key = await crypto.subtle.importKey("raw", bits, { name: "AES-GCM" }, false, ["encrypt", "decrypt"]);
    const iv = new Uint8Array(12);
    const sealed = await crypto.subtle.encrypt({ name: "AES-GCM", iv, tagLength: 128 }, key, new Uint8Array([1]));
    return new Uint8Array(await crypto.subtle.decrypt({ name: "AES-GCM", iv, tagLength: 128 }, key, sealed))[0] === 1;
  } catch { return false; }
}

self.onmessage = event => {
  const m = event.data;
  switch (m.type) {
    case "probe":
      void cryptoWorks().then(ok => post({ type: "probe", crypto: ok, transformEvents: "onrtctransform" in self }));
      return;
    case "start":
      if (state) return;
      state = new SframeState({
        binding: m.binding, resumeAfter: m.resume_after === null ? null : BigInt(m.resume_after),
        now: () => performance.now(), wallNow: () => Date.now(), onStatus: status => post({ type: "status", status }),
      });
      sweep = setInterval(() => state?.sweep(), 1_000);
      return;
    case "control":
      if (!state) return;
      void state.control(m.body, m.bytes).then(result => post({ type: "result", id: m.id, result }));
      return;
    case "stats":
      if (state) post({ type: "stats", id: m.id, counters: { ...state.counters, queue_dropped: queueDropped } });
      return;
    case "stop":
      clearInterval(sweep);
      state?.stop(); state = null;
      return;
  }
};

self.onrtctransform = event => {
  const { side } = event.transformer.options as TransformOptions;
  post({ type: "transform", side });
  const process = side === "send" ? (data: Uint8Array<ArrayBuffer>) => state?.encrypt(data) ?? Promise.resolve(null) : (data: Uint8Array<ArrayBuffer>) => state?.decrypt(data) ?? Promise.resolve(null);
  void pump(event.transformer, process, () => { queueDropped++; }).catch(() => {});
};
