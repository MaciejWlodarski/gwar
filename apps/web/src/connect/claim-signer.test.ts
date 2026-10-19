import { p256 } from "@noble/curves/nist.js";
import { beforeEach, describe, expect, it, vi } from "vitest";

// The account and its vault, as Gwar Connect would give them.
const state = vi.hoisted(() => ({ record: undefined as { vaultKey?: string } | undefined, identities: [] as Array<{ uid: string; name: string; identity: string }> }));
vi.mock("../platform", async (original) => ({ ...(await original<typeof import("../platform")>()), isDesktop: () => false }));
vi.mock("../net/identity", async (original) => ({ ...(await original<typeof import("../net/identity")>()), loadConnectRecord: async () => state.record }));
vi.mock("./vault", async (original) => ({
  ...(await original<typeof import("./vault")>()),
  loadVault: async () => ({ contents: { teamspeak: { identities: state.identities.map((i) => ({ ...i, updated_at: 1 })), default: null } }, version: 1 }),
}));

import { claimSigner } from "./teamspeak";
import { identityScalar } from "./ts-key";

// From the desktop's Rust code (see ts-key.test.ts).
const IDENTITY =
  "10VdE9XURaL9YZFTbfGpp0+S8X4YKpvVlVXUXItaTpbYwdTTX1/QGRtKFdwB1VnJ1NDFGZaQ3tQWywufgAie3paYglfeVdTK0hTElddERQPJFIGVFJlN2RyIX0+DVN5MkFIE3FGZlJDTWNBaUJTMEI2M0diYzgxWkh5SnltNmpzRXBOcEUvdVN3K1JQdkJCTFVXZ04wbU9BPT0=";
const UID = "3YBwdz4DhjMxHwWPyY0raQShMBk=";
const STATEMENT = "gwar teamspeak claim v1\nU2VydmVyS2V5\nAbCd-_09\nbm9uY2U";

beforeEach(() => {
  state.record = { vaultKey: "key" };
  state.identities = [{ uid: UID, name: "Main", identity: IDENTITY }];
});

describe("claiming in the browser", () => {
  it("uses the account's identities from the vault and signs with them", async () => {
    const signer = claimSigner();
    expect(await signer.identities()).toEqual([{ uid: UID, name: "Main" }]);
    const { signature } = await signer.claimSign(UID, STATEMENT);
    const der = Uint8Array.from(atob(signature), (c) => c.charCodeAt(0));
    const point = p256.getPublicKey(identityScalar(IDENTITY), false);
    expect(p256.verify(der, new TextEncoder().encode(STATEMENT), point, { format: "der" })).toBe(true);
  });

  it("has none without the vault key, and never signs for an identity whose key is another's", async () => {
    state.record = {};
    expect(await claimSigner().identities()).toEqual([]);
    state.record = { vaultKey: "key" };
    state.identities = [{ uid: "someone-else=", name: "Wrong", identity: IDENTITY }];
    await expect(claimSigner().claimSign("someone-else=", STATEMENT)).rejects.toThrow("does not match");
    await expect(claimSigner().claimSign(UID, STATEMENT)).rejects.toThrow("not in your account");
  });
});
