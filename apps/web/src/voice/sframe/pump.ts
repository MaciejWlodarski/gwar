/** An encoded frame from `RTCRtpScriptTransformer`; only its payload changes. */
export interface EncodedFrame { data: ArrayBuffer }
export interface Transformer { readonly readable: ReadableStream; readonly writable: WritableStream; readonly options: unknown }

/** At most one frame in flight and one waiting per transform: a newer frame
 * replaces the waiting one instead of queueing, so encryption adds no buffering.
 * Output stays in order; a null result drops the frame. */
export async function pump(transformer: Transformer, process: (data: Uint8Array<ArrayBuffer>) => Promise<Uint8Array<ArrayBuffer> | null>, dropped: () => void): Promise<void> {
  const reader = transformer.readable.getReader() as ReadableStreamDefaultReader<EncodedFrame>;
  const writer = transformer.writable.getWriter();
  let waiting: EncodedFrame | null = null, busy = false;
  const run = async (first: EncodedFrame) => {
    busy = true;
    try {
      for (let frame: EncodedFrame | null = first; frame; frame = waiting, waiting = null) {
        const out = await process(new Uint8Array(frame.data));
        if (out) { frame.data = out.buffer; await writer.write(frame); }
      }
    } finally { busy = false; }
  };
  for (;;) {
    const { value, done } = await reader.read();
    if (done) return;
    if (!busy) void run(value).catch(() => {});
    else { if (waiting) dropped(); waiting = value; }
  }
}
