import { describe, expect, it } from "vitest";
import { changePassword, createAccount, normalizeHandle, recover, renewCertificate, revokeDevice, signIn, signOut, unlockVault } from "./account";
import { ConnectApiError } from "./api";
import { keyFromSeed } from "./crypto";
import { deviceName } from "./device-name";
import { setup } from "./testing";

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
    expect(unlocked.certificate).toEqual(old.certificate);
    expect(stored()?.vaultKey).toBe(created.record.vaultKey);
  }, 90_000);
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
