import { test } from "node:test";
import assert from "node:assert/strict";
import { installMicPrivacyGuard } from "./mic-privacy.mjs";

test("privacy guard rejects hidden requests, overlap, and delayed release on every exit", async () => {
  const original = { window: globalThis.window, document: globalThis.document, MediaDevices: globalThis.MediaDevices, navigator: Object.getOwnPropertyDescriptor(globalThis, "navigator") };
  try {
    globalThis.window = { addEventListener() {} };
    globalThis.document = { addEventListener() {} };
    const track = () => ({ readyState: "live", stop() { this.readyState = "ended"; } });
    let resolve;
    globalThis.MediaDevices = class { getUserMedia() { return new Promise(yes => { resolve = () => { const t = track(); yes({ getTracks: () => [t] }); }; }); } };
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: { mediaDevices: new MediaDevices() } });
    const base = { section: "settings", tab: "audio", mode: "vad", muted: false, deafened: false, visible: true, idle: true };
    let state = { ...base }, changed;
    installMicPrivacyGuard({ read: () => state, subscribe: fn => { changed = fn; } });
    const settle = () => new Promise(yes => setImmediate(yes));
    state.visible = false;
    let first = navigator.mediaDevices.getUserMedia(); await settle(); resolve();
    const hidden = await first; hidden.getTracks()[0].stop(); await settle();
    assert.ok(window.__micPrivacyViolations.includes("Preview requested without authorization"));
    state = { ...base };
    first = navigator.mediaDevices.getUserMedia(); await settle(); const firstResolve = resolve;
    const second = navigator.mediaDevices.getUserMedia(); await settle(); resolve(); (await second).getTracks()[0].stop();
    firstResolve(); const stream = await first; await settle();
    assert.ok(window.__micPrivacyViolations.includes("Overlapping microphone acquisition or live capture"));
    stream.getTracks()[0].stop();
    for (const exit of [{ section: "server" }, { tab: "general" }, { mode: "ptt" }, { muted: true }, { deafened: true }, { visible: false }, { idle: false }]) {
      state = { ...base };
      const acquisition = navigator.mediaDevices.getUserMedia(); await settle(); resolve(); const capture = await acquisition; await settle();
      const before = window.__micPrivacyViolations.length;
      state = { ...base, ...exit }; changed(); await settle();
      assert.equal(window.__micPrivacyViolations.length, before + 1, JSON.stringify(exit));
      capture.getTracks()[0].stop();
    }
  } finally {
    globalThis.window = original.window; globalThis.document = original.document; globalThis.MediaDevices = original.MediaDevices;
    if (original.navigator) Object.defineProperty(globalThis, "navigator", original.navigator); else delete globalThis.navigator;
  }
});

test("a double on the prototype reaches the page, set before or after the guard", async () => {
  const original = { window: globalThis.window, document: globalThis.document, MediaDevices: globalThis.MediaDevices, navigator: Object.getOwnPropertyDescriptor(globalThis, "navigator") };
  const state = { section: "server", tab: "general", mode: "vad", muted: false, deafened: false, visible: true, idle: false };
  const install = () => installMicPrivacyGuard({ read: () => state, subscribe() {} });
  const denied = async () => { throw new DOMException("Permission denied", "NotAllowedError"); };
  try {
    globalThis.window = { addEventListener() {} };
    globalThis.document = { addEventListener() {} };
    globalThis.MediaDevices = class { async getUserMedia() { return { getTracks: () => [] }; } };
    Object.defineProperty(globalThis, "navigator", { configurable: true, value: { mediaDevices: new MediaDevices() } });
    install();
    MediaDevices.prototype.getUserMedia = denied;
    await assert.rejects(navigator.mediaDevices.getUserMedia(), { name: "NotAllowedError" });
    install();
    await assert.rejects(navigator.mediaDevices.getUserMedia(), { name: "NotAllowedError" });
    assert.equal(window.__gum, 1);
  } finally {
    globalThis.window = original.window; globalThis.document = original.document; globalThis.MediaDevices = original.MediaDevices;
    if (original.navigator) Object.defineProperty(globalThis, "navigator", original.navigator); else delete globalThis.navigator;
  }
});
