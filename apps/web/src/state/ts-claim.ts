/**
 * Bringing a TeamSpeak member's roles to this person (docs/teamspeak-import.md, "Claim"). A server
 * moved from TeamSpeak has members `ts:<uid>`; the person's TeamSpeak identities are the desktop
 * app's, or, in the browser, the account's (the vault). `teamspeak.claimable` lists the TeamSpeak
 * members with roles, and the app compares it with the identities here, so the server learns which
 * identities are the person's only if they accept: the list is asked for on every visit whatever
 * matches or was answered before, so not even the requests depend on the identities. Asked once per
 * server, whatever the answer; the member profile can claim again later. The desktop also asks
 * natively before each signature.
 */
import { claimSigner, type ClaimSigner, type TsEntryInfo } from "../connect/teamspeak";
import type { Op, RequestInput, ResponseMap } from "../net/protocol";
import { tNow } from "../i18n";
import { useSettings } from "./settings";
import { useUi } from "./stores";

export interface ClaimLink {
  request<O extends Op>(op: O, d: RequestInput<O>): Promise<ResponseMap[O]>;
}

/** Asks whether to claim these identities' roles here. */
function ask(identities: TsEntryInfo[]): Promise<boolean> {
  return new Promise((resolve) => useUi.getState().openDialog({ kind: "tsClaim", identities, resolve }));
}

/** The person's identities that are members of this server with a role. */
export async function claimableIdentities(link: ClaimLink, ts: ClaimSigner = claimSigner()): Promise<TsEntryInfo[]> {
  const mine = await ts.identities();
  if (mine.length === 0) return [];
  const { claimable } = await link.request("teamspeak.claimable", {});
  const here = new Set(claimable);
  return mine.filter((i) => here.has(`ts:${i.uid}`));
}

/** Proves `identity` to the server and links it to `myUid` (the server checks the statement too). */
export async function claimIdentity(link: ClaimLink, serverKey: string, myUid: string, identity: string, ts: ClaimSigner): Promise<string> {
  const { nonce } = await link.request("teamspeak.challenge", {});
  const statement = `gwar teamspeak claim v1\n${serverKey}\n${myUid}\n${nonce}`;
  const { key, signature } = await ts.claimSign(identity, statement);
  return (await link.request("teamspeak.claim", { key, signature })).teamspeak;
}

/** Claims each identity in turn; returns how many were linked. */
export async function claimAll(link: ClaimLink, serverKey: string, myUid: string, identities: TsEntryInfo[], ts: ClaimSigner = claimSigner()): Promise<number> {
  let linked = 0;
  for (const identity of identities) {
    try {
      await claimIdentity(link, serverKey, myUid, identity.uid, ts);
      linked++;
    } catch (e) {
      console.warn("could not claim the TeamSpeak identity", identity.uid, e);
    }
  }
  return linked;
}

/** After joining a server: offers the claim once, when there is something to claim. Failures are logged. */
export async function offerClaims(link: ClaimLink, serverKey: string, myUid: string, ts: ClaimSigner = claimSigner()): Promise<void> {
  try {
    const identities = await claimableIdentities(link, ts);
    if (identities.length === 0 || useSettings.getState().tsClaimAsked[serverKey]) return;
    useSettings.getState().setTsClaimAsked(serverKey);
    if (!(await ask(identities))) return;
    const linked = await claimAll(link, serverKey, myUid, identities, ts);
    const toast = useUi.getState().toast;
    if (linked === identities.length) toast("success", tNow("tsClaim.done"));
    else toast("error", tNow("tsClaim.failed"));
  } catch (e) {
    console.warn("could not offer the TeamSpeak claim", e);
  }
}
