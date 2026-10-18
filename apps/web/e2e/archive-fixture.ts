/** Browser fixture for the key archive e2e: real Connect account flows and an MLS runtime with the archive. */
import { createAccount, recover, signIn, type FlowDeps } from "../src/connect/account";
import { ConnectApi } from "../src/connect/api";
import { MlsRuntime } from "../src/mls/runtime";
import { Connection } from "../src/net/connection";
import { encodeBase64Url as b64 } from "../src/net/base64url";
import { certificateHash } from "../src/net/handshake";
import { identityFromConnect, identitySeed, uidForPublicKey, type ConnectRecord } from "../src/net/identity";
import { messageBodyHash, messageStatement } from "../src/net/message-signature";
import type { MessageSignature } from "../src/proto/MessageSignature";

const enc = new TextEncoder();

function deps(connect: string, deviceName: string): FlowDeps {
  let stored: ConnectRecord | undefined;
  return {
    api: new ConnectApi(connect),
    store: { load: async () => stored, save: async (r) => void (stored = r), clear: async () => void (stored = undefined) },
    deviceName,
  };
}

export const account = {
  create: (connect: string, handle: string, password: string) => createAccount({ handle, password }, deps(connect, "First")),
  signIn: (connect: string, handle: string, password: string) => signIn({ handle, password }, deps(connect, "Second")),
  recover: (connect: string, handle: string, code: string, newPassword: string) => recover({ handle, code, newPassword }, deps(connect, "Recovered")),
};

export async function connect(record: ConnectRecord, ws: string, connectUrl: string, { uploads = true } = {}) {
  let current = record;
  const identity = await identityFromConnect(record);
  const api = new ConnectApi(connectUrl);
  const link = new Connection({ url: ws, identity, nickname: identity.nickname, client: { name: "archive-test", platform: "web", version: "1" }, autoReconnect: false });
  const errors: string[] = [];
  const runtime = await MlsRuntime.connect({
    identity, seed: identitySeed(identity), link, revocations: () => [], onError: error => errors.push(String(error)),
    archive: {
      api,
      session: async () => ({ token: current.token, accountKey: current.accountKey, recovery: current.archiveRecovery, recoverySecret: current.archiveRecoverySecret }),
      used: async (what) => {
        if (what.recovery && current.archiveRecovery?.statement === what.recovery.statement) current = { ...current, archiveRecovery: undefined };
        if (what.recoverySecret && current.archiveRecoverySecret === what.recoverySecret) current = { ...current, archiveRecoverySecret: undefined };
      },
      uploads: () => uploads,
      revocations: () => [],
    },
  });
  const uid = await uidForPublicKey(record.accountKey);
  const certificate = record.certificate;
  return {
    runtime, link, uid, errors, record: () => current,
    async seal(conversation: string, text: string) {
      let signature!: MessageSignature;
      const sealed = await runtime.sealMessage(conversation, async context => {
        const now = Date.now();
        signature = { ...context, author_uid: uid, key: identity.publicKey, certificate, certificate_hash: await certificateHash(certificate), message_id: b64(crypto.getRandomValues(new Uint8Array(16))), revision: 0, previous_revision_hash: "-", created_at: now, signed_at: now, body_hash: await messageBodyHash(text, [], []), signature: "" };
        signature.signature = await identity.sign(enc.encode(messageStatement(context.server_key, context.conversation, uid, signature, signature.body_hash)));
        return { text, mentions: [], attachments: [], signature };
      });
      return { ...sealed, conversation, message_id: signature.message_id, revision: 0 };
    },
  };
}
