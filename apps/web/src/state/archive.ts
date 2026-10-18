/** The key archive's ties to this device's Connect record and settings (docs/e2e-build.md, section 7). */
import { connectApi } from "../connect/api";
import type { ArchiveOptions } from "../mls/archive";
import { indexedDbConnectStore, loadConnectRecord, type Identity } from "../net/identity";
import { messageStorage } from "../net/message-storage";
import { useSettings } from "./settings";

/** For a Connect device identity; none for a local one. */
export function archiveOptions(identity: Identity): Omit<ArchiveOptions, "deviceKey" | "now"> | undefined {
  if (!identity.device) return undefined;
  // The stored record of this very device: a sign-out or another sign-in ends the archive's session.
  const mine = async () => {
    const record = await loadConnectRecord();
    return record?.certificate.device_key === identity.publicKey ? record : undefined;
  };
  return {
    api: connectApi,
    session: async () => {
      const r = await mine();
      return r ? { token: r.token, accountKey: r.accountKey, recovery: r.archiveRecovery, recoverySecret: r.archiveRecoverySecret } : null;
    },
    used: async (what) => {
      const r = await mine();
      if (!r) return;
      const next = { ...r };
      if (what.recovery && r.archiveRecovery?.statement === what.recovery.statement) delete next.archiveRecovery;
      if (what.recoverySecret && r.archiveRecoverySecret === what.recoverySecret) delete next.archiveRecoverySecret;
      await indexedDbConnectStore.save(next);
    },
    uploads: () => useSettings.getState().archiveUploads,
    revocations: messageStorage.currentRevocations,
  };
}
