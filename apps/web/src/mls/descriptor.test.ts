import { expect, it } from "vitest";
import { descriptor } from "./descriptor";
import { generateIdentity, uidForPublicKey } from "../net/identity";
import { decodeBase64Url, encodeBase64Url } from "../net/base64url";
import { ed25519 } from "@noble/curves/ed25519.js";

it("signs a public server descriptor with history and its restart predecessor", async () => {
  const identity = await generateIdentity(), uid = await uidForPublicKey(identity.publicKey);
  const server = encodeBase64Url(new Uint8Array(32).fill(9)), previous = encodeBase64Url(new Uint8Array(32).fill(8));
  for (const successor of [undefined, { incarnation: 1, previous }]) {
    const result = await descriptor(identity, uid, server, "server", 1234, true, successor);
    const bytes = new Uint8Array(result.bytes), fields: Uint8Array[] = [];
    for (let at = 0; at < bytes.length;) {
      const colon = bytes.indexOf(58, at), length = Number(new TextDecoder().decode(bytes.slice(at, colon)));
      fields.push(bytes.slice(colon + 1, colon + 1 + length)); at = colon + length + 2;
    }
    const statement = fields[0]!, text = new TextDecoder().decode(statement).split("\n");
    expect(text.slice(0, 13)).toEqual(["gwar conversation v1", server, "server", "server", successor ? "1" : "0", successor?.previous ?? "-", "0", "1", "-", "-", uid, "1234", text[12]]);
    expect(result.group).toBe(encodeBase64Url(new Uint8Array(await crypto.subtle.digest("SHA-256", new Uint8Array(statement)))));
    expect(ed25519.verify(decodeBase64Url(new TextDecoder().decode(fields[2])), statement, decodeBase64Url(identity.publicKey))).toBe(true);
  }
  await expect(descriptor(identity, uid, server, "server", 1234, false)).rejects.toThrow("history");
});
