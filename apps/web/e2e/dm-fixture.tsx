import * as Tooltip from "@radix-ui/react-tooltip";
import { createRoot } from "react-dom/client";
import { ed25519 } from "@noble/curves/ed25519.js";
import { ChatArea } from "../src/ui/Chat";
import { Dialogs } from "../src/ui/Dialogs";
import { controller } from "../src/state/controller";
import { useSession, useUi } from "../src/state/stores";
import { useSettings } from "../src/state/settings";
import { messageStorage } from "../src/net/message-storage";
import { encodeBase64Url as b64 } from "../src/net/base64url";
import { uidForPublicKey, identityFromConnect } from "../src/net/identity";
import type { MlsRuntime } from "../src/mls/runtime";
import type { SealedMessage } from "../src/proto/SealedMessage";
const enc = new TextEncoder();
const runtime = () => (controller as unknown as { runtime: MlsRuntime }).runtime;
export async function connect(account: number, device: number, address: string) {
  const seed = new Uint8Array(32).fill(device), root = new Uint8Array(32).fill(account);
  const publicKey = b64(ed25519.getPublicKey(seed)), accountKey = b64(ed25519.getPublicKey(root));
  const issued_at = Date.now() - 60_000, expires_at = issued_at + 7 * 86400_000;
  const certificate = { account_key: accountKey, device_key: publicKey, issued_at, expires_at, signature: b64(ed25519.sign(enc.encode(`gwar device v1\n${accountKey}\n${publicKey}\n${issued_at}\n${expires_at}`), root)) };
  const identity = await identityFromConnect({ version: 1, handle: `user${account}`, accountKey,
    deviceName: `device${device}`, token: "local-test", nickname: `User${account}`, certificate,
    deviceJwk: { kty: "OKP", crv: "Ed25519", d: b64(seed), x: publicKey } });
  await controller.switchIdentity(identity);
  await controller.connect({ address });
  // Mark the local fake feed fresh; no external service participates.
  await messageStorage.mergeFeed([], undefined, Date.now());
  const uid = await uidForPublicKey(accountKey);
  return { uid, publicKey,
    debug: () => ({ toasts: useUi.getState().toasts, threads: useSession.getState().threads, phase: useSession.getState().phase, closeReason: useSession.getState().closeReason, mls: !!runtime() }),
    open(peer: string) { useSession.getState().dispatch({ type: "openDm", uid: peer, name: "Peer" }); },
    sync: () => runtime().sync(),
    close: () => controller.disconnect(),
    selectServer() { useSession.getState().dispatch({ type: "setActive", key: "server" }); },
    server: () => useSession.getState().threads.server,
    serverReady: () => runtime().status("server") === "ready",
    rows: (peer: string) => useSession.getState().threads[`dm:${peer}`]?.items ?? [],
    openRaw: (row: SealedMessage) => runtime().openMessage(row),
    redeem: (token: string) => (controller as unknown as { conn: { request: (op: string, data: object) => Promise<unknown> } }).conn.request("token.redeem", { token }),
    openCreate() { useUi.getState().openDialog({ kind: "channelEdit", mode: "create", parent: null }); },
    channelId: (name: string) => Object.values(useSession.getState().channels).find(c => c.name === name)?.id,
    options(channel: number) { const c = useSession.getState().channels[channel]; return { encrypted: c?.encrypted, history: c?.history }; },
    select(channel: number) { useSession.getState().dispatch({ type: "selectChannel", channel }); },
    channel: (channel: number) => useSession.getState().threads[`ch:${channel}`],
    mention: (text: string, uid: string, thread: "channel" | "server" = "channel") => controller.sendChat(thread, text, { mentions: [uid] }),
    groups: () => (controller as unknown as { conn: { request: (op: string, data: object) => Promise<unknown> } }).conn.request("mls.groups", {}),
    async revoke(account: number, device: number) {
      const root = new Uint8Array(32).fill(account), device_key = b64(ed25519.getPublicKey(new Uint8Array(32).fill(device))), account_key = b64(ed25519.getPublicKey(root));
      const revoked_at = Date.now(), seq = (await messageStorage.cursor()) + 1;
      const signature = b64(ed25519.sign(enc.encode(`gwar revoke v1\n${account_key}\n${device_key}\n${revoked_at}`), root));
      await messageStorage.mergeFeed([{ seq, account_key, device_key, revoked_at, signature }]);
      await runtime().sync();
    },
  };
}
useSettings.setState({ language: "en" });
createRoot(document.getElementById("root")!).render(<Tooltip.Provider><ChatArea /><Dialogs /></Tooltip.Provider>);
