import { describe, expect, it } from "vitest";
import native from "./native-offer.fixture.sdp?raw";
import { advertiseInputGate, microphoneSsrc } from "./sdp";

/** A browser-shaped offer: only the sending section announces an SSRC. */
const browser = [
  "v=0", "o=- 1 2 IN IP4 127.0.0.1", "s=-", "t=0 0", "a=group:BUNDLE 0 1 2",
  "m=audio 9 UDP/TLS/RTP/SAVPF 111", "a=mid:0", "a=sendonly", "a=ssrc:42 cname:x", "a=ssrc:42 msid:s t",
  "m=audio 9 UDP/TLS/RTP/SAVPF 111", "a=mid:1", "a=recvonly",
  "m=audio 9 UDP/TLS/RTP/SAVPF 111", "a=mid:2", "a=recvonly", "",
].join("\r\n");

describe("microphoneSsrc", () => {
  it("adds the optional client gate as a session attribute without changing microphone identity", () => {
    for (const offer of [native, browser]) {
      const gated = advertiseInputGate(offer);
      expect(gated).toContain("a=x-gwar-input-gate:1\r\nm=audio");
      expect(microphoneSsrc(gated)).toBe(microphoneSsrc(offer));
    }
  });
  it("reads the sending section of a real native (str0m) offer, whose receive sections carry SSRCs too", () => {
    expect([...native.matchAll(/^a=ssrc:(\d+) /gm)].map(m => m[1]).filter((v, i, a) => a.indexOf(v) === i).length).toBe(9);
    expect(microphoneSsrc(native)).toBe(2537854144);
  });
  it("reads a browser offer", () => {
    expect(microphoneSsrc(browser)).toBe(42);
    expect(microphoneSsrc(browser.replaceAll("\r\n", "\n"))).toBe(42);
  });
  it("refuses an offer without exactly one sending section and one source", () => {
    expect(microphoneSsrc(browser.replace("a=sendonly", "a=recvonly"))).toBeNull();
    expect(microphoneSsrc(browser.replace("a=mid:1\r\na=recvonly", "a=mid:1\r\na=sendrecv\r\na=ssrc:7 cname:y"))).toBeNull();
    expect(microphoneSsrc(browser.replace("a=ssrc:42 msid:s t", "a=ssrc:43 msid:s t"))).toBeNull();
    expect(microphoneSsrc(browser.replaceAll("a=ssrc:42", "a=ssrc:0"))).toBeNull();
    expect(microphoneSsrc(browser.replace(/a=ssrc:42[^\r]*\r\n/g, ""))).toBeNull();
  });
});
