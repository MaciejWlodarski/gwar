import { describe, expect, it } from "vitest";
import { decodeBase64Url, encodeBase64Url } from "../net/base64url";
import {
  certifyDevice,
  formatRecoveryCode,
  keyFromSeed,
  normalizePassword,
  openKey,
  parseRecoveryCode,
  passwordSecrets,
  passwordStrength,
  recoverySecrets,
  sealKey,
  signRevocation,
  generateRecoveryCode,
  newKdf,
  revokeStatement,
  deviceStatement,
} from "./crypto";

// The test vectors of docs/connect.md (the same as `client_crypto_vectors` in the service's tests).
const ENC_KEY = "qXcjpHbTOr46vWBB7wO-b3l-lYMp7lsl8sa_SekxVr0";
const AUTH_KEY = "wWSVGXCU8xhR5oEnsTNapLPwB1BEFO42MQySTQK-LK0";
const ACCOUNT_KEY = "iojj3XQJ8ZX9UtstPLpdcspnCb8dlBIb83SIAbQPb1w";
const RECOVERY_ENC = "WA19s_g5IcLVVJ_xQ3KHzTpUn7mkdbLdTKOA3GCok10";
const RECOVERY_AUTH = "42kqZaRu_Jtg8r8S7P-39L9-QR5IQM8eSwza09oTy3M";
const KEY_BLOB = "AwMDAwMDAwMDAwMDEz57zhVdpeE9DuPc-Z8DMvCEty7oHW-BIq7g3-5P-1LKErFt1chgOHB2VhOzkcXy";
const DEVICE_KEY = "gTl3Dqh9F19Wo1Rmw0x-zMuNipG07jeiXfYPW4_Js5Q";
const CERT_SIGNATURE = "hrl_m1SlEOKgUYV7RMz4dnlZ5wa5OY_m5-ZS9qWcZU82oGdpwkQdm5wVyUgxplRzxEiMoJnAs-BN6ktG0ufaDA";

const fill = (n: number, byte: number) => new Uint8Array(n).fill(byte);
const b64 = encodeBase64Url;

describe("test vectors", () => {
  it("derives the password secrets with Argon2id", async () => {
    const s = await passwordSecrets("correct horse", { salt: b64(fill(16, 7)), m: 65_536, t: 3, p: 1 });
    expect(b64(s.encKey)).toBe(ENC_KEY);
    expect(b64(s.authKey)).toBe(AUTH_KEY);
  });

  it("derives the account key from a seed", async () => {
    expect((await keyFromSeed(fill(32, 1))).publicKey).toBe(ACCOUNT_KEY);
    expect((await keyFromSeed(fill(32, 2))).publicKey).toBe(DEVICE_KEY);
  });

  it("derives the recovery secrets with HKDF", async () => {
    const s = await recoverySecrets(fill(20, 9), decodeBase64Url(ACCOUNT_KEY));
    expect(b64(s.encKey)).toBe(RECOVERY_ENC);
    expect(b64(s.authKey)).toBe(RECOVERY_AUTH);
  });

  it("seals the key blob exactly", async () => {
    expect(await sealKey(decodeBase64Url(ENC_KEY), fill(32, 1), ACCOUNT_KEY, fill(12, 3))).toBe(KEY_BLOB);
  });

  it("opens the key blob and refuses a wrong key or account", async () => {
    expect(await openKey(decodeBase64Url(ENC_KEY), KEY_BLOB, ACCOUNT_KEY)).toEqual(fill(32, 1));
    await expect(openKey(decodeBase64Url(AUTH_KEY), KEY_BLOB, ACCOUNT_KEY)).rejects.toThrow();
    await expect(openKey(decodeBase64Url(ENC_KEY), KEY_BLOB, DEVICE_KEY)).rejects.toThrow();
  });

  it("signs the device certificate exactly", async () => {
    const account = await keyFromSeed(fill(32, 1));
    const cert = await certifyDevice(account, DEVICE_KEY, 1, 2);
    expect(cert).toEqual({ device_key: DEVICE_KEY, issued_at: 1, expires_at: 2, signature: CERT_SIGNATURE });
  });
});

describe("statements", () => {
  it("are the documented text", () => {
    expect(deviceStatement("A", "D", 1, 2)).toBe("gwar device v1\nA\nD\n1\n2");
    expect(revokeStatement("A", "D", 3)).toBe("gwar revoke v1\nA\nD\n3");
  });

  it("revocations verify against the account key", async () => {
    const account = await keyFromSeed(fill(32, 1));
    const r = await signRevocation(account, DEVICE_KEY, 5);
    const pub = await crypto.subtle.importKey("raw", decodeBase64Url(ACCOUNT_KEY) as BufferSource, { name: "Ed25519" }, false, ["verify"]);
    const ok = await crypto.subtle.verify(
      { name: "Ed25519" },
      pub,
      decodeBase64Url(r.signature) as BufferSource,
      new TextEncoder().encode(revokeStatement(ACCOUNT_KEY, DEVICE_KEY, 5)),
    );
    expect(ok).toBe(true);
  });
});

describe("recovery code", () => {
  it("is 8 groups of 4 base32 characters", () => {
    const text = formatRecoveryCode(fill(20, 9));
    expect(text).toMatch(/^([A-Z2-7]{4}-){7}[A-Z2-7]{4}$/);
    expect(formatRecoveryCode(new Uint8Array(20))).toBe("AAAA-AAAA-AAAA-AAAA-AAAA-AAAA-AAAA-AAAA");
    expect(formatRecoveryCode(fill(20, 255))).toBe("7777-7777-7777-7777-7777-7777-7777-7777");
  });

  it("round-trips random codes", () => {
    for (let i = 0; i < 20; i++) {
      const code = generateRecoveryCode();
      expect(parseRecoveryCode(formatRecoveryCode(code))).toEqual(code);
    }
  });

  it("is tolerant of case, spaces and dashes", () => {
    const code = fill(20, 9);
    const text = formatRecoveryCode(code);
    expect(parseRecoveryCode(text.toLowerCase())).toEqual(code);
    expect(parseRecoveryCode(text.replace(/-/g, " "))).toEqual(code);
    expect(parseRecoveryCode(`  ${text.replace(/-/g, "")}\n`)).toEqual(code);
    expect(parseRecoveryCode(text.replace(/-/g, " - "))).toEqual(code);
  });

  it("rejects the wrong length or alphabet", () => {
    expect(parseRecoveryCode("")).toBeNull();
    expect(parseRecoveryCode("AAAA-AAAA")).toBeNull();
    expect(parseRecoveryCode(`${formatRecoveryCode(fill(20, 9))}A`)).toBeNull();
    expect(parseRecoveryCode("1111-1111-1111-1111-1111-1111-1111-1111")).toBeNull();
  });
});

describe("passwords", () => {
  it("are NFKC-normalized", async () => {
    expect(normalizePassword("ｐａｓｓ")).toBe("pass");
    expect(normalizePassword("é")).toBe("é");
    const kdf = { salt: b64(fill(16, 7)), m: 19_456, t: 2, p: 1 };
    const a = await passwordSecrets("é", kdf);
    const b = await passwordSecrets("é", kdf);
    expect(b64(a.authKey)).toBe(b64(b.authKey));
  });

  it("new kdf parameters follow the spec", () => {
    const kdf = newKdf();
    expect([kdf.m, kdf.t, kdf.p]).toEqual([65_536, 3, 1]);
    expect(decodeBase64Url(kdf.salt)).toHaveLength(16);
  });

  it("rates strength roughly", () => {
    expect(passwordStrength("abc")).toBe("weak");
    expect(passwordStrength("abcdefgh")).toBe("weak");
    expect(passwordStrength("abcdefg1")).toBe("fair");
    expect(passwordStrength("correct horse battery")).toBe("strong");
  });
});
