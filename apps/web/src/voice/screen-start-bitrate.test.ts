import { describe, expect, it } from "vitest";
import { screenStartBitrate, withScreenStartBitrate } from "./screen-start-bitrate";

describe("screen start bitrate", () => {
  it("starts at 4 Mb/s, bounded by the resolved spec cap", () => {
    expect(screenStartBitrate(12_000_000)).toBe(4_000_000);
    expect(screenStartBitrate(750_000)).toBe(750_000);
  });
});

describe("screen answer start hint", () => {
  it("recognizes GTK's platform even when its user agent identifies as macOS Safari", () => {
    const answer = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=rtpmap:96 VP9/90000\r\na=fmtp:96 profile-id=0\r\n";
    const userAgent = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 Version/18.4 Safari/605.1.15";
    expect(withScreenStartBitrate(answer, 4_000_000, userAgent, "Linux x86_64")).toBe(answer);
    expect(withScreenStartBitrate(answer, 4_000_000, userAgent, "MacIntel")).toContain("x-google-start-bitrate=4000");
  });
  it.each([
    ["Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 Version/60.5 Safari/605.1.15", false, "Linux x86_64"],
    ["Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/153.0.0.0 Safari/537.36", true, "Linux x86_64"],
    ["Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15", true, "MacIntel"],
  ])("keeps WebKitGTK's answer compatible while enabling Chromium and native WKWebView: %s", (userAgent, enabled, platform) => {
    const answer = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=rtpmap:96 VP9/90000\r\na=fmtp:96 profile-id=0\r\n";
    const result = withScreenStartBitrate(answer, 4_000_000, userAgent, platform);
    if (enabled) expect(result).toContain("profile-id=0;x-google-start-bitrate=4000");
    else expect(result).toBe(answer);
  });
  it.each(["\r\n", "\n"])("updates every H.264, AV1 and VP9 payload, retaining the rest of the answer (%j)", newline => {
    const lines = ["v=0", "m=audio 9 UDP/TLS/RTP/SAVPF 96", "a=rtpmap:96 opus/48000/2", "a=fmtp:96 minptime=10", "m=video 9 UDP/TLS/RTP/SAVPF 96 97 98 99 100", "a=rtpmap:96 H264/90000", "a=fmtp:96 packetization-mode=1;profile-level-id=42e01f;x-google-start-bitrate=300", "a=rtpmap:97 rtx/90000", "a=fmtp:97 apt=96", "a=rtpmap:98 AV1/90000", "a=fmtp:98 level-idx=5", "a=rtpmap:99 VP9/90000", "a=rtpmap:100 H264/90000", "a=fmtp:100 packetization-mode=1;profile-level-id=640c1f", "a=ice-ufrag:fixture", ""];
    const result = withScreenStartBitrate(lines.join(newline), 4_000_000);
    expect(result.split("x-google-start-bitrate=4000")).toHaveLength(5);
    expect(result).toContain(`a=fmtp:96 packetization-mode=1;profile-level-id=42e01f;x-google-start-bitrate=4000${newline}`);
    expect(result).toContain(`a=rtpmap:99 VP9/90000${newline}a=fmtp:99 x-google-start-bitrate=4000${newline}`);
    expect(result).toContain(`a=fmtp:97 apt=96${newline}`);
    expect(result).toContain(`a=fmtp:96 minptime=10${newline}`);
    expect(result).toContain(`a=ice-ufrag:fixture${newline}`);
    expect(withScreenStartBitrate(result, 4_000_000)).toBe(result);
    expect(result).not.toContain("x-google-min-bitrate");
  });
});
