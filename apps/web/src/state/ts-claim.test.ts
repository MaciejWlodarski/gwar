import { beforeEach, describe, expect, it, vi } from "vitest";
import type { TsBridge } from "../connect/teamspeak";
import { claimAll, offerClaims, type ClaimLink } from "./ts-claim";
import { useSettings } from "./settings";
import { useUi } from "./stores";

const SERVER = "server-key";
const ME = "my-uid";

function bridge(uids: string[], fail: string[] = []) {
  const signed: Array<{ uid: string; statement: string }> = [];
  const ts: Partial<TsBridge> = {
    list: async () => ({ source: "device", default: uids[0] ?? null, identities: uids.map((uid) => ({ uid, name: `name-${uid}` })) }),
    claimSign: async (uid, statement) => {
      signed.push({ uid, statement });
      if (fail.includes(uid)) throw new Error("no such identity");
      return { key: `key-${uid}`, signature: `sig-${uid}` };
    },
  };
  return { ts: ts as TsBridge, signed };
}

function server(claimable: string[]) {
  const calls: Array<{ op: string; d: unknown }> = [];
  let nonce = 0;
  const link: ClaimLink = {
    request: (async (op: string, d: unknown) => {
      calls.push({ op, d });
      if (op === "teamspeak.claimable") return { claimable };
      if (op === "teamspeak.challenge") return { nonce: `nonce${++nonce}` };
      if (op === "teamspeak.claim") return { teamspeak: `ts:${(d as { key: string }).key.slice(4)}` };
      throw new Error(`unexpected ${op}`);
    }) as ClaimLink["request"],
  };
  return { link, calls };
}

async function answer(accepted: boolean) {
  await vi.waitFor(() => expect(useUi.getState().dialog.kind).toBe("tsClaim"));
  const dialog = useUi.getState().dialog;
  if (dialog.kind !== "tsClaim") throw new Error("no dialog");
  dialog.resolve(accepted);
  useUi.getState().closeDialog();
  return dialog.identities.map((i) => i.uid);
}

beforeEach(() => {
  useSettings.setState({ tsClaimAsked: {} });
  useUi.getState().closeDialog();
});

describe("offering the TeamSpeak claim", () => {
  it("asks only for identities with roles here and claims them with the server's statement", async () => {
    const { ts, signed } = bridge(["A=", "B=", "C="]);
    const { link, calls } = server(["ts:B=", "ts:C=", "ts:Z="]);
    const offered = offerClaims(link, SERVER, ME, ts);
    expect(await answer(true)).toEqual(["B=", "C="]);
    await offered;
    expect(signed).toEqual([
      { uid: "B=", statement: `gwar teamspeak claim v1\n${SERVER}\n${ME}\nnonce1` },
      { uid: "C=", statement: `gwar teamspeak claim v1\n${SERVER}\n${ME}\nnonce2` },
    ]);
    expect(calls.filter((c) => c.op === "teamspeak.claim").map((c) => c.d)).toEqual([
      { key: "key-B=", signature: "sig-B=" },
      { key: "key-C=", signature: "sig-C=" },
    ]);
    expect(useSettings.getState().tsClaimAsked[SERVER]).toBe(true);
  });

  it("sends nothing about the identities when declined, and asks once per server", async () => {
    const { ts, signed } = bridge(["A="]);
    const { link, calls } = server(["ts:A="]);
    const offered = offerClaims(link, SERVER, ME, ts);
    await answer(false);
    await offered;
    expect(signed).toEqual([]);
    expect(calls.map((c) => c.op)).toEqual(["teamspeak.claimable"]);
    // Asked again later: the same request (so it tells the server nothing), but no question.
    await offerClaims(link, SERVER, ME, ts);
    expect(calls.map((c) => c.op)).toEqual(["teamspeak.claimable", "teamspeak.claimable"]);
    expect(useUi.getState().dialog.kind).toBe("none");
  });

  it("takes a dialog closed by something else for no", async () => {
    for (const close of [() => useUi.getState().closeDialog(), () => useUi.getState().openDialog({ kind: "bugReport" })]) {
      useSettings.setState({ tsClaimAsked: {} });
      const { ts, signed } = bridge(["A="]);
      const { link } = server(["ts:A="]);
      const offered = offerClaims(link, SERVER, ME, ts);
      await vi.waitFor(() => expect(useUi.getState().dialog.kind).toBe("tsClaim"));
      close();
      await offered;
      expect(signed).toEqual([]);
      useUi.getState().closeDialog();
    }
  });

  it("stays quiet without a desktop, without identities or without a match, and asks again later then", async () => {
    const { link, calls } = server(["ts:Z="]);
    await offerClaims(link, SERVER, ME, null);
    await offerClaims(link, SERVER, ME, bridge([]).ts);
    expect(calls).toEqual([]);
    await offerClaims(link, SERVER, ME, bridge(["A="]).ts);
    expect(calls.map((c) => c.op)).toEqual(["teamspeak.claimable"]);
    expect(useUi.getState().dialog.kind).toBe("none");
    expect(useSettings.getState().tsClaimAsked[SERVER]).toBeUndefined();
  });

  it("counts only the identities that were linked", async () => {
    const { ts } = bridge(["A=", "B="], ["A="]);
    const { link } = server(["ts:A=", "ts:B="]);
    const identities = [{ uid: "A=", name: "a" }, { uid: "B=", name: "b" }];
    await expect(claimAll(link, SERVER, ME, identities, ts)).resolves.toBe(1);
  });
});
