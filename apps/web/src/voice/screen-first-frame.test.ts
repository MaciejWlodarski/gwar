import { describe, expect, it } from "vitest";
import { answerCodecs, encodedAnything, fallbackCodec, firstFrameProgress, leadHinted, restrictAnswer, sourceFrames } from "./screen-first-frame";

const answer = [
  "v=0", "o=- 1 2 IN IP4 0.0.0.0", "m=video 9 UDP/TLS/RTP/SAVPF 96 97 98 99", "a=mid:0",
  "a=rtpmap:96 H264/90000", "a=rtcp-fb:96 nack", "a=fmtp:96 packetization-mode=1;profile-level-id=42e01f",
  "a=rtpmap:97 rtx/90000", "a=fmtp:97 apt=96",
  "a=rtpmap:98 VP9/90000", "a=rtcp-fb:98 nack pli", "a=fmtp:98 profile-id=0",
  "a=rtpmap:99 rtx/90000", "a=fmtp:99 apt=98", "",
].join("\r\n");
const stats = (...entries: Record<string, unknown>[]) => new Map(entries.map((e, i) => [String(e.id ?? i), e])) as unknown as RTCStatsReport;

describe("first-frame guard helpers", () => {
  it("reads the answer's codecs in payload order", () => {
    expect(answerCodecs(answer)).toEqual(["h264", "vp9"]);
    expect(answerCodecs(answer.replace("96 97 98 99", "98 99 96 97"))).toEqual(["vp9", "h264"]);
    expect(answerCodecs("v=0\r\n")).toEqual([]);
  });

  it("tells whether the sent codec carries the start hint", () => {
    expect(leadHinted(answer)).toBe(false);
    expect(leadHinted(answer.replace("profile-level-id=42e01f", "profile-level-id=42e01f;x-google-start-bitrate=4000"))).toBe(true);
    // Only a later codec's hint: the engine sends the first one without it.
    expect(leadHinted(answer.replace("profile-id=0", "profile-id=0;x-google-start-bitrate=4000"))).toBe(false);
    expect(leadHinted("v=0\r\n")).toBe(false);
  });

  it("keeps one codec, its RTX and every other line of the answer", () => {
    expect(restrictAnswer(answer, "vp9")!.split("\r\n")).toEqual([
      "v=0", "o=- 1 2 IN IP4 0.0.0.0", "m=video 9 UDP/TLS/RTP/SAVPF 98 99", "a=mid:0",
      "a=rtpmap:98 VP9/90000", "a=rtcp-fb:98 nack pli", "a=fmtp:98 profile-id=0",
      "a=rtpmap:99 rtx/90000", "a=fmtp:99 apt=98", "",
    ]);
    expect(restrictAnswer(answer, "h264")).toContain("m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n");
    expect(restrictAnswer(answer, "av1")).toBeNull();
    // Audio and other sections stay as they are.
    const bundled = `${answer}m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=rtpmap:111 opus/48000/2\r\n`;
    expect(restrictAnswer(bundled, "vp9")).toContain("m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=rtpmap:111 opus/48000/2\r\n");
  });

  it("falls back along the plan, never to AV1 or a codec that already failed", () => {
    expect(fallbackCodec(["h264", "vp9"], "h264", new Set())).toBe("vp9");
    expect(fallbackCodec(["vp9", "h264"], "vp9", new Set())).toBe("h264");
    expect(fallbackCodec(["av1", "h264", "vp9"], "av1", new Set())).toBe("h264");
    expect(fallbackCodec(["h264", "vp9", "av1"], "h264", new Set(["vp9"]))).toBeNull();
    expect(fallbackCodec(["vp9"], "vp9", new Set())).toBeNull();
  });

  it("counts frames, or packets where an engine reports none, of video only", () => {
    expect(encodedAnything(stats())).toBe(false);
    expect(encodedAnything(stats({ type: "outbound-rtp", kind: "video", framesEncoded: 0, packetsSent: 3 }))).toBe(false);
    expect(encodedAnything(stats({ type: "outbound-rtp", kind: "video", framesEncoded: 1 }))).toBe(true);
    expect(encodedAnything(stats({ type: "outbound-rtp", mediaType: "video", packetsSent: 2 }))).toBe(true);
    expect(encodedAnything(stats({ type: "outbound-rtp", kind: "audio", framesEncoded: 9 }))).toBe(false);
  });

  it("reads the frames the capture handed to the sender, where the engine reports them", () => {
    const linked = stats({ id: "out", type: "outbound-rtp", kind: "video", framesEncoded: 0, mediaSourceId: "src" }, { id: "src", type: "media-source", kind: "video", trackIdentifier: "other", frames: 7 });
    expect(sourceFrames(linked, "screen")).toBe(7);
    // Before any outbound stream, the track's own source.
    expect(sourceFrames(stats({ id: "a", type: "media-source", kind: "audio", trackIdentifier: "mic", frames: 9 }, { id: "v", type: "media-source", kind: "video", trackIdentifier: "screen", frames: 3 }), "screen")).toBe(3);
    expect(sourceFrames(stats({ id: "v", type: "media-source", kind: "video", trackIdentifier: "screen", width: 1920 }), "screen")).toBeUndefined();
    expect(sourceFrames(stats({ type: "outbound-rtp", kind: "video", framesEncoded: 0 }), "screen")).toBeUndefined();
  });

  it("calls a wait idle when the source delivered nothing in it, stalled only when frames went unencoded", () => {
    const source = (frames: number) => ({ id: "v", type: "media-source", kind: "video", trackIdentifier: "screen", frames });
    expect(firstFrameProgress(stats(source(50), { type: "outbound-rtp", kind: "video", framesEncoded: 2 }), "screen", 0).state).toBe("encoded");
    expect(firstFrameProgress(stats(source(0)), "screen", 0)).toEqual({ state: "idle", frames: 0 });
    expect(firstFrameProgress(stats(source(0)), "screen", undefined).state).toBe("idle");
    // Static content: frames before the wait, none during it.
    expect(firstFrameProgress(stats(source(120)), "screen", 120).state).toBe("idle");
    // Frames already delivered with no known start: measure from here.
    expect(firstFrameProgress(stats(source(120)), "screen", undefined)).toEqual({ state: "idle", frames: 120 });
    expect(firstFrameProgress(stats(source(121)), "screen", 120).state).toBe("stalled");
    // No source frame count: judged by encoded frames alone.
    expect(firstFrameProgress(stats(), "screen", undefined)).toEqual({ state: "stalled", frames: undefined });
  });
});
