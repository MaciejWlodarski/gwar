import { describe, expect, it } from "vitest";
import {
  changePassword,
  changeRecoveryCode,
  newRecoveryCode,
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
    expect(phone.certificate.expires_at - phone.certificate.issued_at).toBe(90 * 86_400_000);
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

it("a recovery code change whose reply was lost goes on once the new code works", async () => {
  const { service, deps, stored } = setup();
  const created = await createAccount({ handle: "lost", password: "right password" }, deps());
  await deps().store.save(created.record);
  const code = newRecoveryCode();
  // Connect takes the change, but the reply never arrives.
  const lossy = { ...deps(), api: new ConnectApi("https://c", (async (url: string, init?: RequestInit) => {
    const response = await service.fetch(url, init);
    if (String(url).endsWith("/account/recovery")) throw new TypeError("network");
    return response;
  }) as typeof fetch) };
  const next = await changeRecoveryCode(created.record, "right password", code, lossy);
  expect(next.archiveRecovery).toBeUndefined();
  expect(service.archive.get("lost")?.recovery.map(k => k.statement.split("\n")[2])).toEqual(["1"]);
  expect(stored()?.archiveRecovery).toBeUndefined();
  // A change that never reached Connect fails, and leaves nothing to publish.
  const dropped = { ...deps(), api: new ConnectApi("https://c", (async (url: string, init?: RequestInit) => {
    if (String(url).endsWith("/account/recovery")) throw new TypeError("network");
    return service.fetch(url, init);
  }) as typeof fetch) };
  await expect(changeRecoveryCode(next, "right password", newRecoveryCode(), dropped)).rejects.toMatchObject({ kind: "network" });
  expect(stored()?.archiveRecovery).toBeUndefined();
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

  it("is recomputed by recovery", async () => {
    const { deps } = setup();
    const created = await createAccount({ handle: "judy", password: "right password" }, deps());
    const recovered = await recover({ handle: "judy", code: created.recoveryCode, newPassword: "fresh password" }, deps("Phone"));
    expect(recovered.vaultKey).toBe(created.record.vaultKey);
  }, 90_000);
});

/** The server's certificate for a device, by the name it registered under. */
const serverCertificate = (service: FakeService, name: string) => service.devices.find((d) => d.name === name)!.certificate!;
describe("renewing only the local device", () => {
  it("never certifies an attacker key inserted in the inventory in any unlocking flow", async () => {
    const { service, deps } = setup();
    const created = await createAccount({ handle: "kate", password: "right password" }, deps("A"));
    const attacker = await generateKeyPair();
    service.devices.push({ handle: "kate", device_key: attacker.publicKey, name: "attacker", revoked_at: null, certificate: null });
    const other = await signIn({ handle: "kate", password: "right password" }, deps("B"));
    let own = await renewCertificate(created.record, "right password", deps("A"));
    own = await revokeDevice(own, other.certificate.device_key, "right password", deps("A"));
    await changePassword(own, { password: "right password", newPassword: "other password" }, deps("A"));
    await recover({ handle: "kate", code: created.recoveryCode, newPassword: "fresh password" }, deps("C"));
    expect(service.devices.find((d) => d.device_key === attacker.publicKey)?.certificate).toBeNull();
    expect(service.requests).not.toContain("GET /devices");
    expect(service.requests).not.toContain("POST /devices/renew");
  }, 120_000);

  it("derives the renewal public key from the locally held private key", async () => {
    const { deps } = setup();
    const rec = (await createAccount({ handle: "lena", password: "right password" }, deps())).record;
    const attacker = await generateKeyPair();
    await expect(renewCertificate({ ...rec, certificate: { ...rec.certificate, device_key: attacker.publicKey } }, "right password", deps())).rejects.toThrow("local device key");
  }, 60_000);

  it("rejects a malicious prelogin profile before sending auth_key", async () => {
    const { service, deps } = setup();
    await createAccount({ handle: "mona", password: "right password" }, deps());
    service.accounts.get("mona")!.kdf = { salt: "AAAAAAAAAAAAAAAAAAAAAA", m: 19456, t: 1, p: 1 };
    service.requests.length = 0;
    await expect(signIn({ handle: "mona", password: "right password" }, deps())).rejects.toThrow("Unsupported password key derivation profile");
    expect(service.requests).toEqual(["POST /prelogin"]);
  }, 30_000);
});

describe("picking up a newer certificate at start", () => {
  it("adopts a newer certificate that the account key signed, and nothing else", async () => {
    const { service, deps, stored } = setup();
    const pw = "right password";
    const a = (await createAccount({ handle: "nina", password: pw }, deps("A"))).record;
    await deps("A").store.save(a);

    // Nothing newer yet.
    expect(await pickUpCertificate(a, deps("A"))).toMatchObject({ outcome: "current", record: a });

    // A renewal reached Connect before its local save.
    await sleep(5);
    await renewCertificate(a, pw, { ...deps("A"), store: { load: async () => undefined, save: async () => undefined, clear: async () => undefined } });
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
    const offline = { ...deps("A"), api: new ConnectApi("https://c", (() => Promise.reject(new TypeError("offline"))) as typeof fetch) };
    await expect(pickUpCertificate(a, offline)).rejects.toMatchObject({ kind: "network" });
    const ended = { ...deps("A"), api: new ConnectApi("https://c", (async () => new Response(JSON.stringify({ error: "unauthorized", message: "no" }), { status: 401 })) as typeof fetch) };
    await expect(pickUpCertificate(a, ended)).rejects.toMatchObject({ kind: "unauthorized" });
  }, 60_000);

  it("does not bring a signed-out device back to life", async () => {
    const { service, deps, stored } = setup();
    const pw = "right password";
    const a = (await createAccount({ handle: "quin", password: pw }, deps("A"))).record;
    await sleep(5);
    await renewCertificate(a, pw, { ...deps("A"), store: { load: async () => undefined, save: async () => undefined, clear: async () => undefined } });
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
    expect(service.requests).toEqual(["POST /register", "GET /service-key", "POST /challenges", "POST /session"]);
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
      api: new ConnectApi("https://c", (async (url: string | URL | Request, init?: RequestInit) =>
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

describe("device-bound session paths", () => {
  it("password change, revocation and renewal keep tokens bound to this device and revocation ends them", async () => {
    const { service, deps } = setup();
    const created = await createAccount({ handle: "bound", password: "first password" }, deps("A"));
    const a = created.record;
    const other = await signIn({ handle: "bound", password: "first password" }, deps("B"));
    const unlocked = await renewCertificate(a, "first password", deps("A"));
    const revoked = await revokeDevice(unlocked, other.certificate.device_key, "first password", deps("A"));
    const changed = await changePassword(revoked, { password: "first password", newPassword: "second password" }, deps("A"));
    for (const record of [a, unlocked, revoked, changed]) {
      expect(service.enrollmentTokens.has(record.token)).toBe(false);
      expect(service.sessionDevices.get(record.token)).toBe(a.certificate.device_key);
    }
    const fresh = await signIn({ handle: "bound", password: "second password" }, deps("C"));
    await revokeDevice(fresh, a.certificate.device_key, "second password", deps("C"));
    for (const record of [a, unlocked, revoked, changed]) await expect(deps().api.getVault(record.token)).rejects.toMatchObject({ kind: "unauthorized" });
  }, 90_000);

  it("refuses a substituted or expired challenge before producing the device proof", async () => {
    for (const change of [{ service_key: "foreign" }, { expires_at: 1 }, { challenge: "not a nonce" }, { credential_version: -1 }]) {
      const { service, deps } = setup();
      const broken = { ...deps(), api: new ConnectApi("https://c", (async (url, init) => {
        const response = await service.fetch(url, init);
        if (!String(url).endsWith("/challenges")) return response;
        return new Response(JSON.stringify({ ...await response.json(), ...change }));
      }) as typeof fetch) };
      await expect(createAccount({ handle: "challenge", password: "right password" }, broken)).rejects.toThrow("Invalid Connect challenge");
      expect(service.requests).not.toContain("POST /session");
    }
  }, 60_000);
});

describe("signed credential updates", () => {
  it("rejects alteration of the KDF, authentication key or key blob after the account signed them", async () => {
    for (const field of ["kdf", "auth_key", "key_blob"]) {
      const { service, deps } = setup();
      const record = (await createAccount({ handle: "credentials", password: "first password" }, deps())).record;
      const before = { ...service.accounts.get("credentials")! };
      const malicious = { ...deps(), api: new ConnectApi("https://c", (async (url, init) => {
        if (String(url).endsWith("/account/password")) {
          const body = JSON.parse(init!.body as string);
          if (field === "kdf") body.kdf.salt = "AAAAAAAAAAAAAAAAAAAAAA";
          else body[field] = field === "auth_key" ? "A".repeat(43) : "A".repeat(80);
          init = { ...init, body: JSON.stringify(body) };
        }
        return service.fetch(url, init);
      }) as typeof fetch) };
      await expect(changePassword(record, { password: "first password", newPassword: "new password" }, malicious)).rejects.toMatchObject({ kind: "bad_request" });
      expect(service.accounts.get("credentials")).toEqual(before);
    }
  }, 90_000);
});


it("rotates recovery credentials with an account-signed update", async () => {
  const { deps } = setup();
  const created = await createAccount({ handle: "rotation", password: "right password" }, deps());
  const code = newRecoveryCode();
  await changeRecoveryCode(created.record, "right password", code, deps());
  await expect(recover({ handle: "rotation", code: created.recoveryCode, newPassword: "new password" }, deps())).rejects.toMatchObject({ kind: "unauthorized" });
  const back = await recover({ handle: "rotation", code, newPassword: "new password" }, deps());
  expect(back.accountKey).toBe(created.record.accountKey);
}, 60_000);
