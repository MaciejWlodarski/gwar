import { describe, expect, it } from "vitest";
import { answerCodecs, encodedAnything, fallbackCodec, restrictAnswer } from "./screen-first-frame";

const answer = [
  "v=0", "o=- 1 2 IN IP4 0.0.0.0", "m=video 9 UDP/TLS/RTP/SAVPF 96 97 98 99", "a=mid:0",
  "a=rtpmap:96 H264/90000", "a=rtcp-fb:96 nack", "a=fmtp:96 packetization-mode=1;profile-level-id=42e01f",
  "a=rtpmap:97 rtx/90000", "a=fmtp:97 apt=96",
  "a=rtpmap:98 VP9/90000", "a=rtcp-fb:98 nack pli", "a=fmtp:98 profile-id=0",
  "a=rtpmap:99 rtx/90000", "a=fmtp:99 apt=98", "",
].join("\r\n");
const stats = (...entries: Record<string, unknown>[]) => new Map(entries.map((e, i) => [String(i), e])) as unknown as RTCStatsReport;

describe("first-frame guard helpers", () => {
  it("reads the answer's codecs in payload order", () => {
    expect(answerCodecs(answer)).toEqual(["h264", "vp9"]);
    expect(answerCodecs(answer.replace("96 97 98 99", "98 99 96 97"))).toEqual(["vp9", "h264"]);
    expect(answerCodecs("v=0\r\n")).toEqual([]);
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
});
