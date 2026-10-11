// Gate regressions (Astra r8 probes): listener identity, effect ownership and late completions of the badge hook.
// @vitest-environment happy-dom
// Gate regressions (Astra r6 probes, safe expectations): a badge shows only inside the attestation's validity window,
// whatever the clock does and however long verification takes.
import { webcrypto } from "node:crypto";
import { setImmediate as nextTurn } from "node:timers/promises";
import { act, StrictMode, useLayoutEffect } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeAll, beforeEach, expect, it, vi } from "vitest";
import { generateKeyPair, signText, type KeyPairJwk } from "../connect/crypto";
import { loadServiceKey } from "../connect/service-key";
import { handleStatement, verifiedHandle, type HandleMember } from "../net/handle-attestation";
import { uidForPublicKey } from "../net/identity";
import { useVerifiedHandle } from "./useVerifiedHandle";

vi.mock("../connect/service-key", () => ({ loadServiceKey: vi.fn() }));

const writes = vi.hoisted(() => ({ count: 0 }));
vi.mock("react", async (importOriginal) => {
  const real = await importOriginal<typeof import("react")>();
  return {
    ...real,
    useState: ((initial: unknown) => {
      const [value, setValue] = real.useState(initial);
      const tracked = real.useCallback((next: unknown) => { writes.count++; setValue(next); }, [setValue]);
      return [value, tracked];
    }) as typeof real.useState,
  };
});

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

function listeners() {
  const wa = vi.spyOn(window, "addEventListener"), wr = vi.spyOn(window, "removeEventListener");
  const da = vi.spyOn(document, "addEventListener"), dr = vi.spyOn(document, "removeEventListener");
  const rows = () => [
    ...wa.mock.calls.filter(([n]) => n === "focus" || n === "pageshow").map(([name, fn, options]) => ({ target: window, name, fn, options, removals: wr })),
    ...da.mock.calls.filter(([n]) => n === "visibilitychange").map(([name, fn, options]) => ({ target: document, name, fn, options, removals: dr })),
  ];
  return {
    rows,
    live: () => rows().filter(r => !r.removals.mock.calls.some(([name, fn, options]) => name === r.name && fn === r.fn && options === r.options)),
    invoke: (r: ReturnType<typeof rows>[number]) => {
      const e = new Event(r.name);
      if (typeof r.fn === "function") r.fn.call(r.target, e);
      else r.fn.handleEvent(e);
    },
  };
}

for (const identical of [false, true]) {
  it(`replacement removes exact old identities; queued old callbacks cannot stop newer resources (identical=${identical})`, async () => {
    const ls = listeners();
    const timeouts = vi.spyOn(globalThis, "setTimeout");
    const m = await shown();
    const old = ls.rows(); expect(old).toHaveLength(3);
    expect(new Set(old.map(r => r.fn)).size).toBe(1);
    const withdraw = timeouts.mock.calls.find(([, ms]) => ms === 60_000)![0];
    expect(typeof withdraw).toBe("function");
    const replacement = identical ? { ...m, connect: { ...m.connect! } } : await member(NOW + 120_000);
    await show(replacement); await settle();
    const fresh = ls.rows().filter(r => r.fn !== old[0]!.fn);
    expect(fresh).toHaveLength(3); expect(ls.live()).toEqual(fresh);
    expect(fresh[0]!.fn).not.toBe(old[0]!.fn);
    expect(vi.getTimerCount()).toBe(2);
    const n = writes.count;
    // Force a retained callback through withdraw even though its real listener was removed.
    vi.setSystemTime(NOW + 60_001);
    act(() => { old.forEach(ls.invoke); (withdraw as () => void)(); });
    expect(writes.count).toBe(n); expect(vi.getTimerCount()).toBe(2); expect(ls.live()).toEqual(fresh);
    if (identical) vi.setSystemTime(NOW); // replacement's expiry is identical; restore before testing valid resume
    act(() => { window.dispatchEvent(new Event("focus")); });
    expect(div.textContent).toBe("alice"); expect(vi.getTimerCount()).toBe(2);
    vi.setSystemTime(replacement.connect!.expires_at);
    act(() => { document.dispatchEvent(new Event("visibilitychange")); });
    expect(div.textContent).toBe("none"); expect(vi.getTimerCount()).toBe(0); expect(ls.live()).toHaveLength(0);
  });
}

it("removed resume callbacks cannot write state after unmount", async () => {
  const ls = listeners(); await shown(); const old = ls.rows(); expect(old).toHaveLength(3);
  await act(async () => root.unmount());
  expect(ls.live()).toHaveLength(0); expect(vi.getTimerCount()).toBe(0);
  const n = writes.count; vi.setSystemTime(NOW + 60_001);
  act(() => { old.forEach(ls.invoke); window.dispatchEvent(new Event("focus")); document.dispatchEvent(new Event("visibilitychange")); });
  expect(writes.count).toBe(n); expect(vi.getTimerCount()).toBe(0);
  root = createRoot(div);
});

for (const [target, event] of [["window", "focus"], ["window", "pageshow"], ["document", "visibilitychange"]] as const) {
  for (const direction of ["forward", "backward"] as const) {
    it(`${event} withdraws on a ${direction} jump, removes all listener identities and cannot resurrect`, async () => {
      const ls = listeners(); await shown(); expect(ls.live()).toHaveLength(3);
      const dispatch = () => (target === "window" ? window : document).dispatchEvent(new Event(event));
      act(() => { dispatch(); }); expect(div.textContent).toBe("alice"); expect(ls.live()).toHaveLength(3);
      vi.setSystemTime(direction === "forward" ? NOW + 60_000 : NOW - 300_001);
      act(() => { dispatch(); });
      expect(div.textContent).toBe("none"); expect(vi.getTimerCount()).toBe(0); expect(ls.live()).toHaveLength(0);
      const n = writes.count;
      vi.setSystemTime(NOW); act(() => { dispatch(); });
      expect(div.textContent).toBe("none"); expect(writes.count).toBe(n);
    });
  }
}

for (const pending of ["key", "crypto"] as const) {
  for (const cleanup of ["unmount", "replacement"] as const) {
    it(`late ${pending} after ${cleanup} cannot set state, add listeners or alter new timers`, async () => {
      const ls = listeners(); let release!: () => void;
      let started = false;
      if (pending === "key") {
        vi.mocked(loadServiceKey).mockReturnValueOnce(new Promise(r => { release = () => r(service.publicKey); started = true; }));
      } else {
        const realVerify = crypto.subtle.verify.bind(crypto.subtle);
        const held = new Promise<void>(r => { release = r; });
        vi.spyOn(crypto.subtle, "verify").mockImplementationOnce(async (...args: Parameters<typeof crypto.subtle.verify>) => {
          const valid = await realVerify(...args); started = true; await held; return valid;
        });
      }
      await show(await member());
      for (let i = 0; i < 100 && !started; i++) await settle();
      expect(started).toBe(true); expect(ls.rows()).toHaveLength(0);
      if (cleanup === "unmount") await act(async () => root.unmount());
      else { await show(await member(NOW + 120_000)); await settle(); expect(div.textContent).toBe("alice"); }
      const n = writes.count, rows = ls.rows(), timers = vi.getTimerCount();
      release(); await settle();
      expect(writes.count).toBe(n); expect(ls.rows()).toEqual(rows); expect(vi.getTimerCount()).toBe(timers);
      if (cleanup === "unmount") { expect(timers).toBe(0); root = createRoot(div); }
      else {
        expect(ls.live()).toHaveLength(3); expect(timers).toBe(2);
        vi.setSystemTime(NOW + 60_001); act(() => { window.dispatchEvent(new Event("pageshow")); });
        expect(div.textContent).toBe("alice"); expect(vi.getTimerCount()).toBe(2);
        vi.setSystemTime(NOW + 120_000); act(() => { window.dispatchEvent(new Event("pageshow")); });
        expect(div.textContent).toBe("none"); expect(ls.live()).toHaveLength(0); expect(vi.getTimerCount()).toBe(0);
      }
    });
  }
}

it("StrictMode setup/cleanup replay leaves exactly one live listener set and two timers", async () => {
  const ls = listeners(); const m = await member();
  await act(async () => root.render(<StrictMode><Badge m={m} /></StrictMode>)); await settle();
  expect(div.textContent).toBe("alice"); expect(ls.rows()).toHaveLength(3); expect(ls.live()).toHaveLength(3); expect(vi.getTimerCount()).toBe(2);
  await act(async () => root.unmount()); expect(ls.live()).toHaveLength(0); expect(vi.getTimerCount()).toBe(0);
  root = createRoot(div);
});


