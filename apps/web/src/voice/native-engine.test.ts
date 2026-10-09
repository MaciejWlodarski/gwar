import { describe, expect, it } from "vitest";
import { acceleratorFromEvent, acceleratorParts } from "../platform/desktop";
import { isDesktop } from "../platform";
import { dbToLevel, micErrorKind } from "./native-engine";

describe("dbToLevel", () => {
  it("maps -60 dB to 0 and 0 dB to 1", () => {
    expect(dbToLevel(-100)).toBe(0);
    expect(dbToLevel(-60)).toBe(0);
    expect(dbToLevel(-30)).toBeCloseTo(0.5);
    expect(dbToLevel(0)).toBe(1);
    expect(dbToLevel(6)).toBe(1);
  });
});

describe("micErrorKind", () => {
  it("maps engine issues to voice errors", () => {
    expect(micErrorKind("permission_denied")).toBe("mic_denied");
    expect(micErrorKind("no_device")).toBe("no_mic");
    expect(micErrorKind("failed")).toBe("mic_failed");
  });
});

describe("desktop shortcuts", () => {
  const ev = (code: string, mods: Partial<KeyboardEvent> = {}) => ({ code, ctrlKey: false, altKey: false, shiftKey: false, metaKey: false, ...mods }) as KeyboardEvent;
  it("builds accelerators from key events", () => {
    expect(acceleratorFromEvent(ev("F13"))).toBe("F13");
    expect(acceleratorFromEvent(ev("KeyM", { ctrlKey: true, shiftKey: true }))).toBe("Control+Shift+KeyM");
    expect(acceleratorFromEvent(ev("KeyD", { metaKey: true }))).toBe("Super+KeyD");
  });
  it("ignores bare modifiers", () => {
    expect(acceleratorFromEvent(ev("ShiftLeft", { shiftKey: true }))).toBeNull();
  });
  it("splits accelerators for display", () => {
    expect(acceleratorParts("Control+Shift+KeyM")).toEqual(["Ctrl", "Shift", "KeyM"]);
  });
});

describe("platform", () => {
  it("is not the desktop in tests / browsers", () => {
    expect(isDesktop()).toBe(false);
  });
});
