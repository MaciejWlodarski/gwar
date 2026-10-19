import { p256 } from "@noble/curves/nist.js";
import { describe, expect, it } from "vitest";
import { identityScalar, signClaim, teamspeakPublicKey, teamspeakUid } from "./ts-key";

// Made by the desktop's Rust code (`gwar_client::ts_identity`): the export string, the uid and the
// public key `sign_claim` sends. The second has a private scalar starting with a zero byte.
const FIXTURES = [
  {
    identity:
      "10VdE9XURaL9YZFTbfGpp0+S8X4YKpvVlVXUXItaTpbYwdTTX1/QGRtKFdwB1VnJ1NDFGZaQ3tQWywufgAie3paYglfeVdTK0hTElddERQPJFIGVFJlN2RyIX0+DVN5MkFIE3FGZlJDTWNBaUJTMEI2M0diYzgxWkh5SnltNmpzRXBOcEUvdVN3K1JQdkJCTFVXZ04wbU9BPT0=",
    uid: "3YBwdz4DhjMxHwWPyY0raQShMBk=",
    key: "MEwDAgcAAgEgAiEAiNaZZ0dghFLXYjV26zIHsVZJ4Ba0RCfrwSkqKbhJKK4CIBjP9nIggNqcs3luunE77dePSQACJZ8dHQrxvApURCMc",
  },
  {
    identity:
      "624VbDJ87aZS3pkNm6Qrs3eedYhEPKV9U1wIXmEIRzlLR11UUgccfGVdGitVPwEME1RTEEAEcGdXXwhRYQAie2l0UF5aAnV3Pw1dMiMIUjUzGwFBWgF0NX96Ll88T3hALQZyBnJkYzRDMUZSZ0lmTVMvVnNxN0lEeVUyaTRQbWNHd285QlZoWC94Q01zaTdGQ3dCR0NZdnZRPT0=",
    uid: "yi92tp8L4gNDDWogBzxHVMiFEcs=",
    key: "ME0DAgcAAgEgAiEAyQT5H5m8gUivZzrh1e3+OWjxHgYd9wabsu5BWeln4T4CIQDbnk2ECZ4mSG96TRzdpj6AQJILhXzOqN5BcBRP4C1FRg==",
  },
];

const STATEMENT = "gwar teamspeak claim v1\nU2VydmVyS2V5\nAbCd-_09\nbm9uY2U";

describe("TeamSpeak identities in the browser", () => {
  it("decode to the same key and uid as the desktop's", () => {
    for (const f of FIXTURES) {
      const key = teamspeakPublicKey(identityScalar(f.identity));
      expect(key).toBe(f.key);
      expect(teamspeakUid(key)).toBe(f.uid);
    }
  });

  it("sign a claim the way the server checks it: ECDSA P-256 over SHA-256, DER", () => {
    for (const f of FIXTURES) {
      const { key, signature } = signClaim(f.identity, STATEMENT);
      expect(key).toBe(f.key);
      const point = p256.getPublicKey(identityScalar(f.identity), false);
      const der = Uint8Array.from(atob(signature), (c) => c.charCodeAt(0));
      expect(p256.verify(der, new TextEncoder().encode(STATEMENT), point, { format: "der" })).toBe(true);
    }
  });

  it("sign nothing but a claim statement", () => {
    for (const other of [
      "gwar teamspeak claim v2\na\nb\nc",
      "gwar teamspeak claim v1\na\nb",
      "gwar teamspeak claim v1\na\nb\nc\nd",
      "gwar teamspeak claim v1\na\nb\n",
      "gwar teamspeak claim v1\na b\nb\nc",
      "gwar teamspeak claim v1\na=\nb\nc",
      `gwar teamspeak claim v1\n${"a".repeat(129)}\nb\nc`,
      "clientinitiv alpha=x",
    ]) {
      expect(() => signClaim(FIXTURES[0]!.identity, other)).toThrow();
    }
  });

  it("refuse what is not an identity", () => {
    for (const bad of ["", "V", "12", "xVabc", "12Vnot base64!", "12VAAAA", `12V${btoa("x".repeat(40))}`]) {
      expect(() => identityScalar(bad)).toThrow();
    }
  });
});
