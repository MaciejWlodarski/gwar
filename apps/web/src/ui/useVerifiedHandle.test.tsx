// @vitest-environment happy-dom
// Gate regressions (Astra r6 probes, safe expectations): a badge shows only inside the attestation's validity window,
// whatever the clock does and however long verification takes.
import { webcrypto } from "node:crypto";
import { setImmediate as nextTurn } from "node:timers/promises";
import { act, useLayoutEffect } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeAll, beforeEach, expect, it, vi } from "vitest";
import { generateKeyPair, signText, type KeyPairJwk } from "../connect/crypto";
import { loadServiceKey } from "../connect/service-key";
import { handleStatement, verifiedHandle, type HandleMember } from "../net/handle-attestation";
import { uidForPublicKey } from "../net/identity";
import { useVerifiedHandle } from "./useVerifiedHandle";

vi.mock("../connect/service-key", () => ({ loadServiceKey: vi.fn() }));

const NOW = 1_800_000_000_000;
let service: KeyPairJwk, account: KeyPairJwk, uid: string;
beforeAll(async () => {
  vi.stubGlobal("crypto", webcrypto);
  service = await generateKeyPair(); account = await generateKeyPair(); uid = await uidForPublicKey(account.publicKey);
});
async function member(expires = NOW + 60_000, issued = NOW): Promise<HandleMember> {
  const connect = { account_key: account.publicKey, handle: "alice", issued_at: issued, expires_at: expires, signature: "" };
  connect.signature = await signText(service.jwk, new TextDecoder().decode(handleStatement(connect)));
  return { uid, connect };
}

let div: HTMLDivElement, root: Root, commits: (string | null)[];
function Badge({ m, tick = 0 }: { m: HandleMember; tick?: number }) {
  const value = useVerifiedHandle(m);
  useLayoutEffect(() => { commits.push(value); });
  return <span data-tick={tick}>{value ?? "none"}</span>;
}
const show = async (m: HandleMember, tick = 0) => act(async () => { root.render(<Badge m={m} tick={tick} />); });
const settle = async () => act(async () => { for (let i = 0; i < 30; i++) await nextTurn(); });
/** A verified badge on screen. */
const shown = async (expires?: number) => {
  const m = await member(expires);
  await verifiedHandle(m, service.publicKey, NOW);
  await show(m); await settle();
  expect(div.textContent).toBe("alice");
  return m;
};

beforeEach(() => {
  vi.stubGlobal("crypto", webcrypto); vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.useFakeTimers({ toFake: ["Date", "setTimeout", "clearTimeout", "setInterval", "clearInterval"] }); vi.setSystemTime(NOW);
  vi.mocked(loadServiceKey).mockResolvedValue(service.publicKey);
  commits = []; div = document.createElement("div"); document.body.appendChild(div); root = createRoot(div);
});
afterEach(async () => { await act(async () => root.unmount()); div.remove(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("a render after the clock jumps past expiry hides the badge", async () => {
  const m = await shown();
  vi.setSystemTime(NOW + 60_001); await show(m, 1);
  expect(div.textContent).toBe("none");
});
it("a render after the clock jumps back before issuance (beyond the allowed skew) hides the badge", async () => {
  const m = await shown();
  vi.setSystemTime(NOW - 300_001); await show(m, 1);
  expect(div.textContent).toBe("none");
});
it("without any render, the periodic recheck hides a badge the clock jump made invalid", async () => {
  await shown();
  vi.setSystemTime(NOW + 60_001); // the expiry timer still has its full delay to go
  await act(async () => { await vi.advanceTimersByTimeAsync(30_000); });
  expect(div.textContent).toBe("none");
});
it("verification finishing after expiry never commits a badge", async () => {
  const m = await member(NOW + 1);
  const realVerify = crypto.subtle.verify.bind(crypto.subtle);
  let release!: () => void, started = false;
  const held = new Promise<void>((r) => { release = r; });
  vi.spyOn(crypto.subtle, "verify").mockImplementation(async (...args: Parameters<typeof crypto.subtle.verify>) => {
    const valid = await realVerify(...args); started = true; await held; return valid;
  });
  await show(m);
  for (let i = 0; i < 100 && !started; i++) await settle();
  expect(started).toBe(true);
  await act(async () => { await vi.advanceTimersByTimeAsync(2); });
  release(); await settle();
  expect(commits).not.toContain("alice");
});
it("an identical replacement attestation after expiry cannot reuse the earlier badge", async () => {
  const m = await shown();
  commits = []; vi.setSystemTime(NOW + 60_001);
  await show({ ...m, connect: { ...m.connect! } });
  expect(commits).not.toContain("alice");
  expect(div.textContent).toBe("none");
});
it("the expiry timer withdraws the badge", async () => {
  await shown();
  await act(async () => { await vi.advanceTimersByTimeAsync(60_000); });
  expect(div.textContent).toBe("none");
});
it("a changed attestation or uid hides the old result at once, while the new check is pending", async () => {
  const m = await shown();
  vi.mocked(loadServiceKey).mockReturnValue(new Promise(() => {}));
  await show({ ...m, connect: { ...m.connect!, handle: "mallory" } }); expect(div.textContent).toBe("none");
  await show({ ...m, uid: "other" }); expect(div.textContent).toBe("none");
});
it("the expiry timer's clamp withdraws a very long-lived badge early, never late", async () => {
  const m = await shown(NOW + 3_000_000_000);
  act(() => { vi.advanceTimersByTime(2_147_483_647); });
  expect(Date.now()).toBeLessThan(m.connect!.expires_at);
  expect(div.textContent).toBe("none");
});
it("a withdrawn badge leaves no timers or listeners behind (Astra r7 follow-up)", async () => {
  const added = vi.spyOn(window, "addEventListener"), removed = vi.spyOn(window, "removeEventListener");
  await shown();
  expect(vi.getTimerCount()).toBe(2);
  await act(async () => { await vi.advanceTimersByTimeAsync(60_000); });
  expect(div.textContent).toBe("none");
  expect(vi.getTimerCount()).toBe(0);
  const names = (spy: typeof added) => spy.mock.calls.map(([name]) => name).filter((n) => n === "focus" || n === "pageshow").sort();
  expect(names(removed)).toEqual(names(added));
});
for (const [target, event] of [["window", "focus"], ["window", "pageshow"], ["document", "visibilitychange"]] as const) {
  it(`coming back (${event}) rechecks at once, before any timer fires`, async () => {
    await shown();
    vi.setSystemTime(NOW + 60_001);
    act(() => { (target === "window" ? window : document).dispatchEvent(new Event(event)); });
    expect(div.textContent).toBe("none");
    expect(vi.getTimerCount()).toBe(0);
  });
}
it("unmounting a shown badge clears its timers", async () => {
  await shown();
  await act(async () => root.unmount());
  expect(vi.getTimerCount()).toBe(0);
  root = createRoot(div); // for afterEach
});
