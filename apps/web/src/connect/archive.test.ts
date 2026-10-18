import { describe, expect, it } from "vitest";
import { encodeBase64Url } from "../net/base64url";
import { recoveryArchiveKey, recoveryArchiveStatement, recoveryVersion } from "./archive";

describe("recovery archive key", () => {
  it("matches the agent's derivation (gwar-mls archive::recovery_key_pair)", async () => {
    // Account key of seed [1; 32], code [5; 20].
    const key = await recoveryArchiveKey(new Uint8Array(20).fill(5), "iojj3XQJ8ZX9UtstPLpdcspnCb8dlBIb83SIAbQPb1w");
    expect(encodeBase64Url(key.secret)).toBe("C_sFFNhsfmfwQFxc144TWXCJqbtqbgNKdF4A9sQlRcs");
    expect(key.publicKey).toBe("gEVFT3gT3iNgrwruByAzMv72aS6ZJdIeyVEknN1uMSk");
  });

  it("reads statement versions", () => {
    expect(recoveryVersion(recoveryArchiveStatement("a", 3, "k", 1))).toBe(3);
    expect(recoveryVersion("gwar archive device v1\na\n3\nk\n1")).toBe(0);
    expect(recoveryVersion(recoveryArchiveStatement("a", 0, "k", 1))).toBe(0);
  });
});
