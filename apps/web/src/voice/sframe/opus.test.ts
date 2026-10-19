import { describe, expect, it } from "vitest";
import { packetSamples } from "./opus";

const packet = (bytes: number[], pad = 0) => new Uint8Array([...bytes, ...new Array<number>(pad).fill(0)]);

describe("packetSamples (the gwar-proto RFC 6716 cases)", () => {
  it("accepts every code and length boundary", () => {
    for (const [bytes, samples] of [
      [[0xf0], 480], [[0xf0, 1], 480], [[0xf1, 1, 2], 960], [[0xf1], 960], [[0xf2, 1, 1, 2, 3], 960], [[0xf2, 0], 960],
      [[0xf3, 3, 1, 2, 3], 1440], [[0xf3, 0x83, 1, 2, 1, 2, 3, 4], 1440], [[0xf3, 0xc2, 1, 1, 10, 20, 0], 960], [[0x83, 48], 5760],
    ] as [number[], number][]) expect(packetSamples(packet(bytes)), JSON.stringify(bytes)).toBe(samples);
    expect(packetSamples(packet([0xf2, 255, 255], 1275))).toBe(960); // first frame = 1275
    expect(packetSamples(packet([0xf3, 0x41, 255, 1, 10], 255))).toBe(480);
    expect(packetSamples(packet([0xf0], 1275))).toBe(480);
    expect(packetSamples(packet([0xf0], 1276))).toBeNull();
  });
  it("rejects malformed CBR, VBR, padding and durations", () => {
    for (const bytes of [
      [], [0xf2], [0xf1, 1], [0xf2, 2, 1], [0xf2, 252], [0xf3], [0xf3, 0], [0xf3, 3, 1, 2], [0xf3, 0x82], [0xf3, 0x83, 1],
      [0xf3, 0x82, 5, 1], [0xf3, 0x41], [0xf3, 0x41, 255], [0xf3, 0x41, 2, 0], [0xf3, 0xc2, 1, 1, 0], [0xfb, 7], [0x83, 49],
    ]) expect(packetSamples(packet(bytes)), JSON.stringify(bytes)).toBeNull();
    for (const bytes of [[0xf1], [0xf3, 2], [0xf2, 0]]) expect(packetSamples(packet(bytes, 2552))).toBeNull();
  });
});
