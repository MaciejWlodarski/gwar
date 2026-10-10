import { describe, expect, it, vi } from "vitest";
import {
  changePassword,
  createAccount,
  normalizeHandle,
  pickUpCertificate,
  prepareAccount,
  recover,
  registerPrepared,
  renewCertificate,
  revokeDevice,
  signIn,
  signOut,
  unlockVault,
} from "./account";
import { ConnectApi, ConnectApiError } from "./api";
import { certifyDevice, generateKeyPair, keyFromSeed } from "./crypto";
import { deviceName } from "./device-name";
import { setup, type FakeService } from "./testing";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

describe("account flows", () => {
  it("normalizes handles like the service", () => {
    expect(normalizeHandle("@Maciej")).toBe("maciej");
    expect(normalizeHandle("a.b_c9")).toBe("a.b_c9");
    for (const bad of ["ab", "has space", ".dot", "dot.", "ąę", "x".repeat(33)]) expect(normalizeHandle(bad)).toBeNull();
  });

  it("creating an account that keeps the identity uses the local key as the account key; a second device gets the same one", async () => {
    const { service, deps } = setup();
    const local = await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"]);
    const localJwk = await crypto.subtle.exportKey("jwk", (local as CryptoKeyPair).privateKey);

    const created = await createAccount({ handle: "Alice", password: "correct horse", keepIdentity: localJwk }, deps("Chrome on macOS"));
    expect(created.record.accountKey).toBe(localJwk.x);
    expect(created.record.certificate.device_key).not.toBe(localJwk.x);
    expect(created.recoveryCode).toMatch(/^([A-Z2-7]{4}-){7}[A-Z2-7]{4}$/);
    expect(service.accounts.get("alice")?.account_key).toBe(localJwk.x);

    const phone = await signIn({ handle: "@ALICE", password: "correct horse" }, deps("Firefox on Linux"));
    expect(phone.accountKey).toBe(created.record.accountKey);
    expect(phone.certificate.device_key).not.toBe(created.record.certificate.device_key);
    expect(phone.certificate.expires_at - phone.certificate.issued_at).toBe(365 * 86_400_000);
    expect(service.devices.map((d) => d.name)).toEqual(["Chrome on macOS", "Firefox on Linux"]);
  }, 30_000);

  it("starting fresh makes a new account key", async () => {
    const { deps } = setup();
    const local = await keyFromSeed(new Uint8Array(32).fill(5));
    const created = await createAccount({ handle: "bob", password: "pw-long-enough" }, deps());
    expect(created.record.accountKey).not.toBe(local.publicKey);
  }, 30_000);

  it("refuses a taken handle and a wrong password", async () => {
    const { deps } = setup();
    await createAccount({ handle: "carol", password: "right password" }, deps());
    await expect(createAccount({ handle: "carol", password: "x-long-enough" }, deps())).rejects.toMatchObject({ kind: "taken" });
    await expect(signIn({ handle: "carol", password: "wrong" }, deps())).rejects.toMatchObject({ kind: "unauthorized" });
    await expect(signIn({ handle: "no such!", password: "x" }, deps())).rejects.toBeInstanceOf(ConnectApiError);
  }, 30_000);

  it("revokes another device with the password, and refuses the wrong one", async () => {
    const { service, deps, stored } = setup();
    const first = (await createAccount({ handle: "dave", password: "right password" }, deps("A"))).record;
    const second = await signIn({ handle: "dave", password: "right password" }, deps("B"));
    await expect(revokeDevice(first, second.certificate.device_key, "wrong", deps())).rejects.toMatchObject({ kind: "unauthorized" });
    const next = await revokeDevice(first, second.certificate.device_key, "right password", deps());
    expect(service.devices.find((d) => d.name === "B")?.revoked_at).not.toBeNull();
    expect(service.devices.find((d) => d.name === "A")?.revoked_at).toBeNull();
    expect(stored()?.token).toBe(next.token);
  }, 30_000);

  it("changes the password and the old one stops working", async () => {
    const { deps } = setup();
    const rec = (await createAccount({ handle: "erin", password: "old password" }, deps())).record;
    await changePassword(rec, { password: "old password", newPassword: "new password" }, deps());
    await expect(signIn({ handle: "erin", password: "old password" }, deps())).rejects.toMatchObject({ kind: "unauthorized" });
    expect((await signIn({ handle: "erin", password: "new password" }, deps())).accountKey).toBe(rec.accountKey);
  }, 60_000);

  it("recovers with the code (any spelling), sets a new password and enrols the device", async () => {
    const { deps } = setup();
    const created = await createAccount({ handle: "frank", password: "forgotten" }, deps());
    const spelled = created.recoveryCode.toLowerCase().replace(/-/g, " ");
    await expect(recover({ handle: "frank", code: "AAAA-AAAA-AAAA-AAAA-AAAA-AAAA-AAAA-AAAA", newPassword: "fresh password" }, deps())).rejects.toMatchObject({ kind: "unauthorized" });
    await expect(recover({ handle: "nobody", code: spelled, newPassword: "fresh password" }, deps())).rejects.toMatchObject({ kind: "unauthorized" });
    const rec = await recover({ handle: "frank", code: spelled, newPassword: "fresh password" }, deps("Phone"));
    expect(rec.accountKey).toBe(created.record.accountKey);
    expect((await signIn({ handle: "frank", password: "fresh password" }, deps())).accountKey).toBe(rec.accountKey);
    await expect(signIn({ handle: "frank", password: "forgotten" }, deps())).rejects.toMatchObject({ kind: "unauthorized" });
  }, 60_000);

  it("renews the certificate of the same device key", async () => {
    const { deps } = setup();
    const rec = (await createAccount({ handle: "gina", password: "right password" }, deps())).record;
    await new Promise((r) => setTimeout(r, 5));
    const next = await renewCertificate(rec, "right password", deps());
    expect(next.certificate.device_key).toBe(rec.certificate.device_key);
    expect(next.certificate.expires_at).toBeGreaterThan(rec.certificate.expires_at);
  }, 30_000);

  it("signing out forgets the record and ends the session", async () => {
    const { service, deps, stored } = setup();
    const rec = (await createAccount({ handle: "hank", password: "right password" }, deps())).record;
    await deps().store.save(rec);
    await signOut(rec, deps());
    expect(stored()).toBeUndefined();
    expect(service.sessions.has(rec.token)).toBe(false);
  }, 30_000);
});

describe("vault key", () => {
  it("is the same on every device and every flow, whatever the password", async () => {
    const { deps, stored } = setup();
    const created = await createAccount({ handle: "ivan", password: "right password" }, deps("A"));
    const key = created.record.vaultKey;
    expect(key).toMatch(/^[A-Za-z0-9_-]{43}$/);
    expect((await signIn({ handle: "ivan", password: "right password" }, deps("B"))).vaultKey).toBe(key);
    const changed = await changePassword(created.record, { password: "right password", newPassword: "other password" }, deps());
    expect(changed.vaultKey).toBe(key);
    expect(stored()?.vaultKey).toBe(key);
    expect((await renewCertificate(changed, "other password", deps())).vaultKey).toBe(key);
    expect((await signIn({ handle: "ivan", password: "other password" }, deps("C"))).vaultKey).toBe(key);
  }, 90_000);

  it("is recomputed by recovery and added to a record from before vaults", async () => {
    const { deps, stored } = setup();
    const created = await createAccount({ handle: "judy", password: "right password" }, deps());
    const recovered = await recover({ handle: "judy", code: created.recoveryCode, newPassword: "fresh password" }, deps("Phone"));
    expect(recovered.vaultKey).toBe(created.record.vaultKey);

    const old = { ...created.record, vaultKey: undefined };
    await deps().store.save(old);
    await expect(unlockVault(old, "wrong", deps())).rejects.toMatchObject({ kind: "unauthorized" });
    expect(stored()?.vaultKey).toBeUndefined();
    const unlocked = await unlockVault(old, "fresh password", deps());
    expect(unlocked.vaultKey).toBe(created.record.vaultKey);
    // The password also renewed the certificates; the device key is the same.
    expect(unlocked.certificate.device_key).toBe(old.certificate.device_key);
    expect(unlocked.certificate.expires_at).toBeGreaterThan(old.certificate.expires_at);
    expect(stored()?.vaultKey).toBe(created.record.vaultKey);
  }, 90_000);
});

/** The server's certificate for a device, by the name it registered under. */
const serverCertificate = (service: FakeService, name: string) => service.devices.find((d) => d.name === name)!.certificate!;
const renewedKeys = (service: FakeService, n: number) => service.renewals[n]!.certificates.map((c) => c.device_key);

describe("renewing every device's certificate", () => {
  it("renews all active devices when the account key is held, this one first, and never revoked ones or the one just certified", async () => {
    const { service, deps } = setup();
    const pw = "right password";
    const a = (await createAccount({ handle: "kate", password: pw }, deps("A"))).record;
    // Creating an account has nothing to renew: its only device was certified a moment ago.
    expect(service.renewals).toHaveLength(0);
    expect(service.requests).toEqual(["POST /register"]);

    await sleep(5);
    const b = await signIn({ handle: "kate", password: pw }, deps("B"));
    expect(renewedKeys(service, 0)).toEqual([a.certificate.device_key]);
    await sleep(5);
    const c = await signIn({ handle: "kate", password: pw }, deps("C"));
    expect(renewedKeys(service, 1).sort()).toEqual([a.certificate.device_key, b.certificate.device_key].sort());
    expect(renewedKeys(service, 1)).not.toContain(c.certificate.device_key);

    // Revoking B: the list is read after, so B is left out. A (the device doing it) goes first.
    const bBefore = serverCertificate(service, "B");
    const aBefore = serverCertificate(service, "A");
    await sleep(5);
    const next = await revokeDevice(a, b.certificate.device_key, pw, deps("A"));
    expect(renewedKeys(service, 2)).toEqual([a.certificate.device_key, c.certificate.device_key]);
    expect(serverCertificate(service, "B")).toEqual(bBefore);
    expect(serverCertificate(service, "A").expires_at).toBeGreaterThan(aBefore.expires_at);
    // This device's record carries its new certificate.
    expect(next.certificate.expires_at).toBe(serverCertificate(service, "A").expires_at);
    expect(next.certificate.signature).toBe(serverCertificate(service, "A").signature);
    expect(next.certificate.device_key).toBe(a.certificate.device_key);
  }, 90_000);

  it("every other flow with the account key renews too: password change, renew, vault unlock, recovery", async () => {
    const { service, deps, stored } = setup();
    const created = await createAccount({ handle: "lena", password: "first password" }, deps("A"));
    await sleep(5);
    const b = await signIn({ handle: "lena", password: "first password" }, deps("B"));
    service.renewals.length = 0;

    await sleep(5);
    const changed = await changePassword(created.record, { password: "first password", newPassword: "second password" }, deps("A"));
    expect(service.renewals).toHaveLength(1);
    expect(renewedKeys(service, 0)[0]).toBe(created.record.certificate.device_key);
    expect(renewedKeys(service, 0)).toHaveLength(2);
    expect(changed.certificate.expires_at).toBeGreaterThan(created.record.certificate.expires_at);
    expect(stored()?.certificate).toEqual(changed.certificate);

    await sleep(5);
    const renewed = await renewCertificate(changed, "second password", deps("A"));
    expect(service.renewals).toHaveLength(2);
    expect(renewedKeys(service, 1)).toContain(b.certificate.device_key);
    expect(renewed.certificate.expires_at).toBeGreaterThan(changed.certificate.expires_at);
    expect(renewed.certificate.expires_at).toBe(serverCertificate(service, "A").expires_at);

    await sleep(5);
    const unlocked = await unlockVault({ ...renewed, vaultKey: undefined }, "second password", deps("A"));
    expect(service.renewals).toHaveLength(3);
    expect(unlocked.certificate.expires_at).toBeGreaterThan(renewed.certificate.expires_at);

    // Recovery enrols a new device (left out, it is fresh) and renews the two others.
    await sleep(5);
    const phone = await recover({ handle: "lena", code: created.recoveryCode, newPassword: "third password" }, deps("Phone"));
    expect(service.renewals).toHaveLength(4);
    expect(renewedKeys(service, 3).sort()).toEqual([created.record.certificate.device_key, b.certificate.device_key].sort());
    expect(renewedKeys(service, 3)).not.toContain(phone.certificate.device_key);
  }, 120_000);

  it("a renewal that fails never fails the flow", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const { service, deps } = setup();
    const pw = "right password";
    const a = (await createAccount({ handle: "mona", password: pw }, deps("A"))).record;
    // The renewal is refused, or the list of devices cannot be read.
    const breaking = (broken: (path: string, method: string) => boolean) => {
      const base = deps("B");
      const fetchFn = (async (url: string | URL | Request, init?: RequestInit) => {
        if (broken(String(url), init?.method ?? "GET")) return new Response(JSON.stringify({ error: "internal", message: "boom" }), { status: 500 });
        return service.fetch(url, init);
      }) as typeof fetch;
      return { ...base, api: new ConnectApi("http://c", fetchFn) };
    };
    const b = await signIn({ handle: "mona", password: pw }, breaking((u) => u.endsWith("/devices/renew")));
    expect(b.certificate.device_key).toBeTruthy();
    expect(service.renewals).toHaveLength(0);
    const c = await signIn({ handle: "mona", password: pw }, breaking((u, m) => u.endsWith("/v1/devices") && m === "GET"));
    expect(c.vaultKey).toBe(a.vaultKey);
    expect(service.renewals).toHaveLength(0);
    const next = await changePassword(a, { password: pw, newPassword: "other password" }, breaking((u) => u.endsWith("/devices/renew")));
    expect(next.certificate).toEqual(a.certificate);
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  }, 120_000);
});

describe("picking up a newer certificate at start", () => {
  it("adopts a newer certificate that the account key signed, and nothing else", async () => {
    const { service, deps, stored } = setup();
    const pw = "right password";
    const a = (await createAccount({ handle: "nina", password: pw }, deps("A"))).record;
    await deps("A").store.save(a);

    // Nothing newer yet.
    expect(await pickUpCertificate(a, deps("A"))).toMatchObject({ outcome: "current", record: a });

    // Another device signs in and renews A.
    await sleep(5);
    await signIn({ handle: "nina", password: pw }, { ...deps("B"), store: { load: async () => undefined, save: async () => undefined, clear: async () => undefined } });
    const newest = serverCertificate(service, "A");
    expect(newest.expires_at).toBeGreaterThan(a.certificate.expires_at);

    const picked = await pickUpCertificate(a, deps("A"));
    expect(picked.outcome).toBe("updated");
    expect(picked.record.certificate).toEqual({ account_key: a.accountKey, device_key: a.certificate.device_key, ...newest });
    expect(stored()?.certificate).toEqual(picked.record.certificate);
    // Having it, a second look finds nothing newer.
    expect((await pickUpCertificate(picked.record, deps("A"))).outcome).toBe("current");
  }, 60_000);

  it("ignores a newer certificate with a bad signature or one signed by another key", async () => {
    const { service, deps, stored } = setup();
    const a = (await createAccount({ handle: "omar", password: "right password" }, deps("A"))).record;
    await deps("A").store.save(a);
    const device = service.devices.find((d) => d.name === "A")!;
    const forged = async (make: () => Promise<{ issued_at: number; expires_at: number; signature: string }>) => {
      device.certificate = await make();
      const result = await pickUpCertificate(a, deps("A"));
      expect(result.outcome).toBe("invalid");
      expect(stored()).toEqual(a);
    };
    const later = a.certificate.expires_at + 10 * 86_400_000;
    await forged(async () => ({ issued_at: a.certificate.issued_at, expires_at: later, signature: a.certificate.signature }));
    const stranger = await generateKeyPair();
    await forged(async () => {
      const { issued_at, expires_at, signature } = await certifyDevice(stranger, a.certificate.device_key);
      return { issued_at, expires_at, signature };
    });
  }, 60_000);

  it("leaves a revoked or unlisted device alone and passes network errors on", async () => {
    const { service, deps } = setup();
    const a = (await createAccount({ handle: "pete", password: "right password" }, deps("A"))).record;
    const device = service.devices.find((d) => d.name === "A")!;
    device.revoked_at = Date.now();
    expect(await pickUpCertificate(a, deps("A"))).toMatchObject({ outcome: "revoked", record: a });
    service.devices.length = 0;
    expect(await pickUpCertificate(a, deps("A"))).toMatchObject({ outcome: "unlisted", record: a });
    const offline = { ...deps("A"), api: new ConnectApi("http://c", (() => Promise.reject(new TypeError("offline"))) as typeof fetch) };
    await expect(pickUpCertificate(a, offline)).rejects.toMatchObject({ kind: "network" });
    const ended = { ...deps("A"), api: new ConnectApi("http://c", (async () => new Response(JSON.stringify({ error: "unauthorized", message: "no" }), { status: 401 })) as typeof fetch) };
    await expect(pickUpCertificate(a, ended)).rejects.toMatchObject({ kind: "unauthorized" });
  }, 60_000);

  it("does not bring a signed-out device back to life", async () => {
    const { service, deps, stored } = setup();
    const pw = "right password";
    const a = (await createAccount({ handle: "quin", password: pw }, deps("A"))).record;
    await sleep(5);
    await signIn({ handle: "quin", password: pw }, { ...deps("B"), store: { load: async () => undefined, save: async () => undefined, clear: async () => undefined } });
    expect(serverCertificate(service, "A").expires_at).toBeGreaterThan(a.certificate.expires_at);
    // The store has no record any more (the person signed out while the request was in flight).
    expect(stored()).toBeUndefined();
    const result = await pickUpCertificate(a, deps("A"));
    expect(result.outcome).toBe("current");
    expect(stored()).toBeUndefined();
  }, 60_000);
});

describe("preparing and registering an account", () => {
  it("sends nothing until registered, and registering gives the same account and code that were prepared", async () => {
    const { service, deps } = setup();
    const { prepared, recoveryCode } = await prepareAccount({ handle: "Rita", password: "right password" }, deps("A"));
    expect(service.requests).toEqual([]);
    expect(service.accounts.size).toBe(0);
    expect(prepared.handle).toBe("rita");
    expect(recoveryCode).toMatch(/^([A-Z2-7]{4}-){7}[A-Z2-7]{4}$/);

    const record = await registerPrepared(prepared, deps("A"));
    expect(service.requests).toEqual(["POST /register"]);
    expect(service.accounts.get("rita")?.account_key).toBe(record.accountKey);
    expect(record.vaultKey).toMatch(/^[A-Za-z0-9_-]{43}$/);
    // The code shown before registering opens the account.
    const back = await recover({ handle: "rita", code: recoveryCode, newPassword: "fresh password" }, deps("Phone"));
    expect(back.accountKey).toBe(record.accountKey);
  }, 60_000);

  it("surfaces a taken handle, then registers the same keys and code under another handle", async () => {
    const { service, deps } = setup();
    await createAccount({ handle: "sven", password: "right password" }, deps());
    const { prepared, recoveryCode } = await prepareAccount({ handle: "sven", password: "other password" }, deps("B"));
    await expect(registerPrepared(prepared, deps("B"))).rejects.toMatchObject({ kind: "taken" });
    expect(service.accounts.size).toBe(1);

    expect(() => prepared.withHandle("no way!")).toThrowError(expect.objectContaining({ kind: "bad_request" }));
    const record = await registerPrepared(prepared.withHandle("@Sven2"), deps("B"));
    expect(service.accounts.get("sven2")?.account_key).toBe(record.accountKey);
    expect(record.handle).toBe("sven2");
    // The code from the first screen still works, and the password is the one typed then.
    expect((await recover({ handle: "sven2", code: recoveryCode, newPassword: "fresh password" }, deps("C"))).accountKey).toBe(record.accountKey);
    expect((await signIn({ handle: "sven2", password: "fresh password" }, deps("D"))).accountKey).toBe(record.accountKey);
  }, 90_000);

  it("surfaces a rate limit and lets the same prepared account try again", async () => {
    const { service, deps } = setup();
    const { prepared } = await prepareAccount({ handle: "tess", password: "right password" }, deps());
    let limited = true;
    const flaky = {
      ...deps(),
      api: new ConnectApi("http://c", (async (url: string | URL | Request, init?: RequestInit) =>
        limited ? new Response(JSON.stringify({ error: "rate_limited", message: "slow down" }), { status: 429 }) : service.fetch(url, init)) as typeof fetch),
    };
    await expect(registerPrepared(prepared, flaky)).rejects.toMatchObject({ kind: "rate_limited" });
    expect(service.accounts.size).toBe(0);
    limited = false;
    expect((await registerPrepared(prepared, flaky)).handle).toBe("tess");
  }, 60_000);

  it("rejects an invalid handle before doing any work", async () => {
    const { service, deps } = setup();
    await expect(prepareAccount({ handle: "x", password: "right password" }, deps())).rejects.toMatchObject({ kind: "bad_request" });
    expect(service.requests).toEqual([]);
  });
});

describe("device names", () => {
  const chromeMac = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";
  const edgeWin = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36 Edg/130.0.0.0";
  const firefoxLinux = "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0";
  const safariIos = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";
  it("names browsers and the desktop app with the OS", () => {
    expect(deviceName(chromeMac, false)).toBe("Chrome on macOS");
    expect(deviceName(edgeWin, false)).toBe("Edge on Windows");
    expect(deviceName(firefoxLinux, false)).toBe("Firefox on Linux");
    expect(deviceName(safariIos, false)).toBe("Safari on iOS");
    expect(deviceName(edgeWin, true)).toBe("Gwar desktop on Windows");
    expect(deviceName(chromeMac, true)).toBe("Gwar desktop on macOS");
  });
});
