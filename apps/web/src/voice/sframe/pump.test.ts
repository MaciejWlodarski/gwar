import { describe, expect, it } from "vitest";
import { pump, type EncodedFrame } from "./pump";

describe("pump", () => {
  it("keeps one frame in flight and one waiting, dropping older waiting frames", async () => {
    let push!: (f: EncodedFrame) => void, end!: () => void;
    const readable = new ReadableStream<EncodedFrame>({ start(c) { push = f => c.enqueue(f); end = () => c.close(); } });
    const written: number[] = [];
    const writable = new WritableStream<EncodedFrame>({ write(f) { written.push(new Uint8Array(f.data)[0]!); } });
    const releases: (() => void)[] = [];
    let dropped = 0;
    const done = pump({ readable, writable, options: {} }, async data => { await new Promise<void>(r => releases.push(r)); return data[0] === 3 ? null : new Uint8Array([data[0]! + 100]); }, () => { dropped++; });
    const frame = (n: number): EncodedFrame => ({ data: new Uint8Array([n]).buffer });
    for (const n of [1, 2, 3, 4]) push(frame(n));
    await new Promise(r => setTimeout(r, 0));
    // 1 in flight; 2 replaced by 3, 3 replaced by 4.
    expect(dropped).toBe(2);
    releases.shift()!();
    await new Promise(r => setTimeout(r, 0));
    releases.shift()!();
    await new Promise(r => setTimeout(r, 0));
    push(frame(5)); push(frame(3));
    await new Promise(r => setTimeout(r, 0));
    releases.shift()!();
    await new Promise(r => setTimeout(r, 0));
    releases.shift()!();
    end();
    await done;
    await new Promise(r => setTimeout(r, 0));
    expect(written).toEqual([101, 104, 105]); // frame 3 was dropped by processing
  });
});
