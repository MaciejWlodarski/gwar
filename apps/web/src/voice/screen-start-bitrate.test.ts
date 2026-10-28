import { afterEach, describe, expect, it, vi } from "vitest";
import { SCREEN_UPLINK_KEY, screenStartBitrate, ScreenUplinkHistory, withScreenStartBitrate } from "./screen-start-bitrate";

afterEach(() => vi.unstubAllGlobals());

function storage(value: string | null = null) {
  const stored = { getItem: vi.fn(() => value), setItem: vi.fn((_key: string, next: string) => { value = next; }) };
  vi.stubGlobal("localStorage", stored);
  return stored;
}

describe("screen start bitrate", () => {
  it("defaults to 4 Mb/s, bounded by the resolved spec cap", () => {
    storage();
    expect(screenStartBitrate(12_000_000)).toBe(4_000_000);
    expect(screenStartBitrate(750_000)).toBe(750_000);
  });
  it.each([[10_000_000, 8_000_000], [500_000, 1_000_000], [30_000_000, 12_000_000]])("starts at 80 %% of remembered %s, clamped to the cap and 1 Mb/s", (remembered, expected) => {
    storage(JSON.stringify(remembered));
    expect(screenStartBitrate(12_000_000)).toBe(expected);
    expect(screenStartBitrate(750_000)).toBe(750_000);
  });
  it.each(["bad JSON", "null", "0", "-1", "1e309", '"8000000"', "{}"])("ignores corrupt/unusable history: %s", value => {
    storage(value);
    expect(screenStartBitrate(12_000_000)).toBe(4_000_000);
  });
  it("works when localStorage is unavailable or refused", () => {
    vi.stubGlobal("localStorage", undefined);
    expect(screenStartBitrate(12_000_000)).toBe(4_000_000);
    vi.stubGlobal("localStorage", { getItem: () => { throw new Error("refused"); }, setItem: () => { throw new Error("full"); } });
    expect(screenStartBitrate(12_000_000)).toBe(4_000_000);
    const history = new ScreenUplinkHistory(0);
    expect(() => { for (let at = 0; at <= 30_000; at += 2000) history.observe(at, 10_000_000); }).not.toThrow();
  });
});

describe("remembered uplink", () => {
  it("stores the median of the stable last ten seconds after twenty seconds, and uses it next time", () => {
    const local = storage(), history = new ScreenUplinkHistory(0);
    for (let at = 0; at < 20_000; at += 2000) history.observe(at, at < 10_000 ? 3_000_000 : 10_000_000);
    expect(local.setItem).not.toHaveBeenCalled();
    history.observe(20_000, 11_000_000);
    expect(local.setItem).toHaveBeenCalledWith(SCREEN_UPLINK_KEY, "10000000");
    expect(screenStartBitrate(12_000_000)).toBe(8_000_000);
    history.observe(22_000, 10_000_000);
    expect(local.setItem).toHaveBeenCalledTimes(1);
  });
  it("does not train on a ramp or an unstable ten-second tail", () => {
    const local = storage(), history = new ScreenUplinkHistory(0);
    for (let at = 0; at <= 40_000; at += 2000) history.observe(at, at % 4000 ? 4_000_000 : 8_000_000);
    expect(local.setItem).not.toHaveBeenCalled();
  });
  it.each([undefined, NaN, 0, -1, Infinity])("needs a full new window after a missing/probe sample: %s", invalid => {
    const local = storage(), history = new ScreenUplinkHistory(0);
    for (let at = 0; at <= 18_000; at += 2000) history.observe(at, 10_000_000);
    history.observe(20_000, invalid);
    for (let at = 22_000; at <= 30_000; at += 2000) history.observe(at, 10_000_000);
    expect(local.setItem).not.toHaveBeenCalled();
    history.observe(32_000, 10_000_000);
    expect(local.setItem).toHaveBeenCalledOnce();
  });
  it("restarts after a sampling gap or a replacement transport", () => {
    const local = storage(), history = new ScreenUplinkHistory(0);
    for (let at = 0; at <= 18_000; at += 2000) history.observe(at, 10_000_000);
    for (let at = 24_000; at <= 32_000; at += 2000) history.observe(at, 10_000_000);
    expect(local.setItem).not.toHaveBeenCalled();
    history.observe(34_000, 10_000_000);
    expect(local.setItem).toHaveBeenCalledOnce();
    local.setItem.mockClear();
    const replacement = new ScreenUplinkHistory(34_000);
    for (let at = 34_000; at <= 52_000; at += 2000) replacement.observe(at, 5_000_000);
    expect(local.setItem).not.toHaveBeenCalled();
    replacement.observe(54_000, 5_000_000);
    expect(local.setItem).toHaveBeenCalledWith(SCREEN_UPLINK_KEY, "5000000");
  });
});

describe("screen answer start hint", () => {
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
